use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    transport::smtp::AsyncSmtpTransportBuilder,
};
use ragios::{
    config::{Config, Smtp},
    store::{DeliveryOutcome, Notification, Store},
};
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::watch,
    time::{sleep, timeout},
};

struct Relay {
    from: lettre::Address,
    transport: AsyncSmtpTransport<Tokio1Executor>,
    builder: AsyncSmtpTransportBuilder,
}

impl Relay {
    fn new(config: &Smtp) -> anyhow::Result<Self> {
        let builder = config.transport_builder()?;
        Ok(Self {
            from: config.from.clone(),
            transport: builder.clone().build(),
            builder,
        })
    }

    async fn deliver(&mut self, notification: &Notification) -> DeliveryOutcome {
        let Ok(recipient) = notification.recipient.parse::<lettre::Address>() else {
            return DeliveryOutcome::Failed("Invalid SMTP recipient address".into());
        };
        let label = notification.label();
        let check = if notification.service_id.is_empty() {
            "Check"
        } else {
            "Service"
        };
        let Ok(message) = Message::builder()
            .from(self.from.clone().into())
            .to(recipient.into())
            .subject(format!("Ragios: {label}"))
            .body(format!(
                "{label}\nHost: {}\n{check}: {}\nObserved at: {}\n",
                notification.host_id,
                notification.target_label(),
                notification.occurred_at
            ))
        else {
            return DeliveryOutcome::Failed("Could not compose notification email".into());
        };
        match timeout(Duration::from_secs(15), self.transport.send(message)).await {
            Ok(Ok(_)) => DeliveryOutcome::Delivered,
            Err(_) => {
                // Cancellation may have returned a connection with unread
                // replies to the pool. Drop that pool before the next send.
                self.transport = self.builder.clone().build();
                DeliveryOutcome::Retry("SMTP delivery timed out".into())
            }
            Ok(Err(error)) => {
                // SMTP servers can echo AUTH values in replies. Persist only a
                // numeric reply code or a locally defined error description.
                // 535 (invalid credentials) can be repaired by an operator.
                // Keep it retryable, unlike a permanent rejection of a recipient.
                let status = error.status().map(u16::from);
                if status == Some(535) {
                    return DeliveryOutcome::Retry(
                        "SMTP rejected authentication (reply 535)".into(),
                    );
                }
                let diagnostic = match status {
                    Some(code) => format!("SMTP rejected delivery (reply {code})"),
                    None if error.is_tls() => "SMTP TLS negotiation or verification failed".into(),
                    None if error.is_timeout() => "SMTP delivery timed out".into(),
                    None => "SMTP connection or protocol failure".into(),
                };
                if error.is_permanent() {
                    DeliveryOutcome::Failed(diagnostic)
                } else {
                    DeliveryOutcome::Retry(diagnostic)
                }
            }
        }
    }
}

pub async fn run(store: Store, mut active: watch::Receiver<Arc<Config>>) {
    while active.has_changed().is_ok() {
        let Err(error) = store.resume_notifications().await else {
            break;
        };
        eprintln!("Resume email delivery: {error:#}");
        sleep(Duration::from_secs(1)).await;
    }
    while active.has_changed().is_ok() {
        let config = active.borrow_and_update().clone();
        if config.smtp.is_some()
            && let Err(error) = store.hold_silenced_deliveries(config.clone()).await
        {
            eprintln!("Reconcile notification silences: {error:#}");
            sleep(Duration::from_secs(1)).await;
            continue;
        }
        let mut relay = match config.smtp.as_ref().map(Relay::new).transpose() {
            Ok(relay) => relay,
            Err(error) => {
                eprintln!("Configure email delivery: {error:#}");
                sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        loop {
            if !matches!(active.has_changed(), Ok(false)) {
                break;
            }
            match store.next_notification(active.clone()).await {
                Ok(Some(candidate)) => {
                    let notification = match store
                        .begin_notification(candidate, active.clone(), config.clone())
                        .await
                    {
                        Ok(Some(notification)) => notification,
                        Ok(None) => continue,
                        Err(error) => {
                            eprintln!("Dispatch email delivery: {error:#}");
                            sleep(Duration::from_secs(1)).await;
                            continue;
                        }
                    };
                    let outcome = if !matches!(active.has_changed(), Ok(false)) {
                        DeliveryOutcome::RecheckConfiguration
                    } else {
                        relay
                            .as_mut()
                            .expect("SMTP enabled for dispatch")
                            .deliver(&notification)
                            .await
                    };
                    // If SQLite is temporarily unavailable, retry only the
                    // write; do not send the email a second time.
                    while let Err(error) = store
                        .finish_notification(notification.clone(), outcome.clone())
                        .await
                    {
                        eprintln!("Record email delivery: {error:#}");
                        sleep(Duration::from_secs(1)).await;
                    }
                }
                Ok(None) if relay.is_none() => {
                    let _ = active.changed().await;
                    break;
                }
                Ok(None) => sleep(Duration::from_millis(250)).await,
                Err(error) => {
                    eprintln!("Read email delivery queue: {error:#}");
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
}
