use anyhow::Result;
use rusqlite::Connection;

pub(super) fn initialize(connection: &mut Connection) -> Result<()> {
    connection.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;")?;
    let transaction = connection.transaction()?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS check_results (
            check_kind TEXT NOT NULL,
            host_id TEXT NOT NULL,
            service_id TEXT NOT NULL,
            status INTEGER NOT NULL,
            output TEXT NOT NULL,
            started_at TEXT NOT NULL,
            finished_at TEXT NOT NULL,
            duration_ms INTEGER NOT NULL,
            probe TEXT,
            PRIMARY KEY (check_kind, host_id, service_id)
         );
         CREATE TABLE IF NOT EXISTS check_problems (
            check_kind TEXT NOT NULL,
            host_id TEXT NOT NULL,
            service_id TEXT NOT NULL,
            confirmed_severity INTEGER,
            pending_severity INTEGER,
            pending_attempts INTEGER NOT NULL,
            PRIMARY KEY (check_kind, host_id, service_id)
         );
         CREATE TABLE IF NOT EXISTS check_transitions (
            id INTEGER PRIMARY KEY,
            check_kind TEXT NOT NULL,
            host_id TEXT NOT NULL,
            service_id TEXT NOT NULL,
            occurred_at TEXT NOT NULL,
            result_status INTEGER NOT NULL,
            kind TEXT NOT NULL,
            confirmed_severity INTEGER,
            pending_severity INTEGER,
            pending_attempts INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS transitions_by_check
            ON check_transitions(check_kind, host_id, service_id, id);
         CREATE INDEX IF NOT EXISTS transitions_by_time ON check_transitions(occurred_at);
         CREATE TABLE IF NOT EXISTS check_problem_episodes (
            host_id TEXT NOT NULL,
            service_id TEXT NOT NULL,
            token TEXT NOT NULL,
            acknowledged INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (host_id, service_id)
         );",
    )?;
    transaction.execute_batch(super::notifications::SCHEMA)?;
    transaction.execute_batch(super::silences::SCHEMA)?;
    super::notifications::migrate(&transaction)?;
    let legacy: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'latest_results')",
        [],
        |row| row.get(0),
    )?;
    if legacy {
        // Both the original result-only schema and the service lifecycle schema
        // are supported. Existing lifecycle state and transition IDs stay intact.
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS problem_states (
                host_id TEXT NOT NULL,
                service_id TEXT NOT NULL,
                confirmed_severity INTEGER,
                pending_severity INTEGER,
                pending_attempts INTEGER NOT NULL,
                PRIMARY KEY (host_id, service_id)
             );
             CREATE TABLE IF NOT EXISTS problem_transitions (
                id INTEGER PRIMARY KEY,
                host_id TEXT NOT NULL,
                service_id TEXT NOT NULL,
                occurred_at TEXT NOT NULL,
                result_status INTEGER NOT NULL,
                kind TEXT NOT NULL,
                confirmed_severity INTEGER,
                pending_severity INTEGER,
                pending_attempts INTEGER NOT NULL
             );
             INSERT INTO problem_states
                SELECT host_id, service_id, NULL,
                       CASE WHEN status = 0 THEN NULL ELSE status END,
                       CASE WHEN status = 0 THEN 0 ELSE 1 END
                FROM latest_results WHERE true
                ON CONFLICT(host_id, service_id) DO NOTHING;
             INSERT INTO check_results
                SELECT 'service', host_id, service_id, status, output,
                       started_at, finished_at, duration_ms, NULL FROM latest_results;
             INSERT INTO check_problems SELECT 'service', * FROM problem_states;
             INSERT INTO check_transitions
                SELECT id, 'service', host_id, service_id, occurred_at, result_status,
                       kind, confirmed_severity, pending_severity, pending_attempts
                FROM problem_transitions;
             DROP TABLE latest_results;
             DROP TABLE problem_states;
             DROP TABLE problem_transitions;",
        )?;
    }
    let has_email_disabled: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('check_transitions') WHERE name = 'email_disabled')",
        [],
        |row| row.get(0),
    )?;
    if !has_email_disabled {
        transaction.execute_batch(
            "ALTER TABLE check_transitions ADD COLUMN email_disabled INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    // Clear episodes on recovery even when the previous binary records it
    // during a rollback. Re-upgrading must never revive an acknowledgement.
    transaction.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS clear_episode_on_recovery
         AFTER UPDATE OF confirmed_severity ON check_problems
         WHEN OLD.confirmed_severity IS NOT NULL AND NEW.confirmed_severity IS NULL
         BEGIN
             DELETE FROM check_problem_episodes
             WHERE host_id = NEW.host_id AND service_id = NEW.service_id;
         END;",
    )?;
    // Backfill currently confirmed problems on upgrade, independently of
    // retained check history and whether SMTP was configured.
    transaction.execute_batch(
        "INSERT INTO check_problem_episodes (host_id, service_id, token)
         SELECT host_id, service_id, lower(hex(randomblob(16))) FROM check_problems
         WHERE confirmed_severity IS NOT NULL
         ON CONFLICT(host_id, service_id) DO NOTHING;",
    )?;
    transaction.commit()?;
    Ok(())
}
