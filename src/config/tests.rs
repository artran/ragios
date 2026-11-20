use super::Config;
use std::fs;
use tempfile::TempDir;

fn load(yaml: &str) -> anyhow::Result<Config> {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ragios.yaml");
    fs::write(
        &path,
        format!("plugin_dir: .\ndatabase: state.sqlite3\n{yaml}"),
    )
    .unwrap();
    Config::load(&path)
}

#[test]
fn hosts_select_ping_or_tcp_with_literal_arguments_and_resolved_timing() {
    for probe in ["ping", "tcp"] {
        let config = load(&format!(
            "interval_seconds: 90\ntimeout_seconds: 20\nhosts:\n  - id: a\n    name: A\n    address: 'target $(literal)'\n    interval_seconds: 60\n    reachability:\n      probe: {probe}\n      executable: check_{probe}\n      args: ['-H', '{{{{address}}}}', '-p', '443']\n      timeout_seconds: 5\n    services: [{{id: check, name: Check, executable: check}}]\n"
        )).unwrap();
        let reachability = config.hosts[0].reachability.as_ref().unwrap();
        assert_eq!(reachability.probe.label(), probe);
        assert_eq!(
            reachability.check.executable,
            std::path::Path::new(&format!("check_{probe}"))
        );
        assert_eq!(
            reachability.check.args,
            ["-H", "target $(literal)", "-p", "443"]
        );
        assert_eq!(reachability.check.interval_seconds, 60);
        assert_eq!(reachability.check.timeout_seconds, 5);
    }
}

#[test]
fn invalid_reachability_definitions_reject_the_whole_fleet() {
    for definition in [
        "{probe: udp, executable: check}",
        "{executable: check}",
        "{probe: ping}",
        "{probe: ping, executable: ''}",
        "{probe: tcp, executable: check, interval_seconds: 0}",
        "{probe: tcp, executable: check, timeout_seconds: 4294967296}",
        "{probe: ping, executable: check, args: ['{{unset}}']}",
        "{probe: tcp, executable: check, port: 443}",
        "null",
    ] {
        let error = load(&format!("hosts:\n  - id: a\n    name: A\n    reachability: {definition}\n    services: [{{id: check, name: Check, executable: check}}]\n")).expect_err(definition);
        assert!(format!("{error:#}").contains("ragios.yaml"), "{error:#}");
    }
    let legacy =
        load("hosts: [{id: a, name: A, services: [{id: check, name: Check, executable: check}]}]")
            .unwrap();
    assert!(legacy.hosts[0].reachability.is_none());
}

#[test]
fn blank_override_maps_and_entries_are_rejected() {
    for value in [
        "overrides:",
        "overrides:\n      check:",
        "overrides: null",
        "overrides: {check: null}",
    ] {
        let yaml = format!(
            "hosts:\n  - id: a\n    name: A\n    services: [{{id: check, name: Check, executable: check}}]\n    {value}\n"
        );
        let error = load(&yaml).expect_err(&format!("accepted {value:?}"));
        assert!(
            format!("{error:#}").contains("null is not allowed"),
            "{error:#}"
        );
    }
}

#[test]
fn recipients_require_yaml_strings_not_coerced_nulls_numbers_or_booleans() {
    for setting in [
        "recipients: [null]",
        "recipients: [123]",
        "recipients: [true]",
        "recipients: [~]",
    ] {
        let error = load(&format!("{setting}\nhosts:\n  - id: a\n    name: A\n    services: [{{id: check, name: Check, executable: check}}]\n")).unwrap_err();
        assert!(
            format!("{error:#}").contains("recipient must be a string"),
            "{error:#}"
        );
    }
}

#[test]
fn null_or_blank_overrides_are_rejected_instead_of_silently_clearing_values() {
    for field in ["args", "name", "interval_seconds", "timeout_seconds"] {
        for null in ["null", "~", ""] {
            let yaml = format!(
                "hosts:\n  - id: a\n    name: A\n    services: [{{id: check, name: Check, executable: check}}]\n    overrides:\n      check:\n        {field}: {null}\n"
            );
            let error = load(&yaml).expect_err(&format!("accepted {field}: {null}"));
            assert!(
                format!("{error:#}").contains("null is not allowed"),
                "{error:#}"
            );
        }
    }
    for null in ["null", "~", ""] {
        let yaml = format!(
            "hosts:\n  - id: a\n    name: A\n    recipients: {null}\n    services: [{{id: check, name: Check, executable: check}}]\n"
        );
        assert!(load(&yaml).is_err(), "accepted recipients: {null}");
    }
}

#[test]
fn escaped_opening_delimiters_reach_plugins_as_literal_braces() {
    let config = load("hosts:\n  - id: a\n    name: A\n    services:\n      - id: format\n        name: Format\n        executable: check\n        args: ['{{{{.Names}}', '{{{{', 'plain {braces}', '{{host_id}}/{{{{literal}}']\n").unwrap();
    assert_eq!(
        config.hosts[0].services[0].check.args,
        ["{{.Names}}", "{{", "plain {braces}", "a/{{literal}}"]
    );
}

#[test]
fn fragment_diagnostics_identify_the_source_of_errors_and_both_duplicate_definitions() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("ragios.yaml");
    let first = dir.path().join("first.yaml");
    let second = dir.path().join("second.yaml");
    fs::write(
        &root,
        "plugin_dir: .\ndatabase: state.sqlite3\nincludes: [first.yaml, second.yaml]\n",
    )
    .unwrap();
    let host = "hosts:\n  - id: a\n    name: A\n    services: [{id: check, name: Check, executable: check}]\n";
    fs::write(&first, host).unwrap();
    fs::write(&second, host).unwrap();
    let duplicate = format!("{:#}", Config::load(&root).unwrap_err());
    assert!(duplicate.contains("duplicate host ID"));
    assert!(duplicate.contains(first.to_str().unwrap()), "{duplicate}");
    assert!(duplicate.contains(second.to_str().unwrap()), "{duplicate}");
    fs::write(
        &second,
        "hosts:\n  - id: b\n    name: B\n    bundles: [absent]\n",
    )
    .unwrap();
    let unknown = format!("{:#}", Config::load(&root).unwrap_err());
    assert!(unknown.contains("unknown bundle"));
    assert!(unknown.contains(second.to_str().unwrap()), "{unknown}");
    fs::write(
        &first,
        "commands:\n  - {id: probe, executable: check, args: ['{{missing}}']}\n",
    )
    .unwrap();
    fs::write(&second, "hosts:\n  - id: a\n    name: A\n    services: [{id: check, name: Check, command: probe}]\n").unwrap();
    let argument = format!("{:#}", Config::load(&root).unwrap_err());
    assert!(argument.contains("command \"probe\""), "{argument}");
    assert!(argument.contains(first.to_str().unwrap()), "{argument}");
    assert!(argument.contains(second.to_str().unwrap()), "{argument}");
}

#[test]
fn shipped_inline_and_split_fleet_examples_load_without_secrets_or_external_services() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let inline = Config::load(&root.join("examples/ragios.yaml")).unwrap();
    assert_eq!(inline.hosts[0].group, "ungrouped");
    let fleet = Config::load(&root.join("examples/fleet/ragios.yaml")).unwrap();
    assert_eq!(fleet.hosts.len(), 3);
    assert_eq!(fleet.hosts[0].recipients, ["production@example.invalid"]);
    assert_eq!(fleet.hosts[1].recipients, ["website-owner@example.invalid"]);
    assert!(fleet.hosts[2].recipients.is_empty());
    let lab_hours = fleet.hosts[2].services[0].check.window.as_ref().unwrap();
    assert_eq!(lab_hours.id, "lab-hours");
    assert_eq!(
        fleet.hosts[1].services[0].check.args,
        ["--target", "web-02.example.invalid", "--port", "8443"]
    );
}

#[test]
fn split_configuration_rejects_missing_repeated_or_conflicting_definitions() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("ragios.yaml");
    let fragment = dir.path().join("fleet.yaml");
    let host = "hosts:\n  - id: a\n    name: A\n    services: [{id: check, name: Check, executable: check}]\n";
    let settings = "plugin_dir: .\ndatabase: state.sqlite3\n";
    let cases = [
        (
            "includes: [missing.yaml]\n".to_string(),
            host.to_string(),
            "read included configuration",
        ),
        (
            "includes: [fleet.yaml, ./fleet.yaml]\n".into(),
            host.into(),
            "repeated included configuration",
        ),
        (
            "includes: [ragios.yaml]\n".into(),
            host.into(),
            "repeated included configuration",
        ),
        (
            "includes: [fleet.yaml]\n".into(),
            format!("{host}timeout_seconds: 10\n"),
            "unknown field `timeout_seconds`",
        ),
        (
            "includes: [fleet.yaml]\n".into(),
            format!("{host}includes: [ragios.yaml]\n"),
            "unknown field `includes`",
        ),
        (
            format!("includes: [fleet.yaml]\n{host}"),
            host.into(),
            "duplicate host ID \"a\"",
        ),
        (
            "includes: [fleet.yaml]\ncommands: [{id: check, executable: check}]\n".into(),
            format!("{host}commands: [{{id: check, executable: other}}]\n"),
            "duplicate command ID \"check\"",
        ),
        (
            "includes: [fleet.yaml]\n".into(),
            "hosts: [".into(),
            "parse included configuration",
        ),
    ];
    for (includes, definitions, expected) in cases {
        fs::write(&root, format!("{settings}{includes}")).unwrap();
        fs::write(&fragment, definitions).unwrap();
        let error = format!("{:#}", Config::load(&root).unwrap_err());
        assert!(
            error.contains(expected),
            "expected {expected:?}, got {error}"
        );
    }
}

#[test]
fn different_bundles_and_inline_services_cannot_shadow_the_same_identity_even_with_an_override() {
    let bundles = r#"
bundles:
  - id: web
    services: [{id: shared, name: Web, executable: check_web}]
  - id: database
    services: [{id: shared, name: Database, executable: check_database}]
"#;
    for definitions in [
        "bundles: [web, database]",
        "bundles: [web]\n    services: [{id: shared, name: Inline, executable: check}]",
        "bundles: [web, database]\n    overrides: {shared: {args: []}}",
    ] {
        let error = load(&format!(
            "{bundles}hosts:\n  - id: a\n    name: A\n    {definitions}\n"
        ))
        .unwrap_err();
        assert!(format!("{error:#}").contains("duplicate service ID \"shared\""));
    }
}

#[test]
fn invalid_or_ambiguous_fleet_definitions_fail_with_context() {
    let base = r#"
commands:
  - {id: probe, executable: check, args: ['{{address}}']}
bundles:
  - id: basic
    services:
      - {id: health, name: Health, command: probe}
groups:
  - {id: production, name: Production}
hosts:
  - {id: a, name: A, address: a.invalid, group: production, bundles: [basic]}
"#;
    let cases = [
        (
            base.replace("bundles: [basic]", "bundles: [absent]"),
            "unknown bundle \"absent\"",
        ),
        (
            base.replace("group: production", "group: absent"),
            "unknown group \"absent\"",
        ),
        (
            base.replace("command: probe", "command: absent"),
            "unknown command \"absent\"",
        ),
        (
            base.replace("bundles: [basic]", "bundles: [basic, basic]"),
            "duplicate service ID \"health\"",
        ),
        (
            base.replace(
                "bundles: [basic]",
                "bundles: [basic], overrides: {absent: {args: []}}",
            ),
            "override refers to unknown service \"absent\"",
        ),
        (
            base.replace(
                "bundles: [basic]",
                "bundles: [basic], overrides: {health: {id: renamed}}",
            ),
            "unknown field `id`",
        ),
        (
            base.replace("command: probe", "command: probe, executable: other"),
            "exactly one of command or executable",
        ),
        (
            base.replace("command: probe", "args: []"),
            "exactly one of command or executable",
        ),
        (
            base.replace("address: a.invalid, ", ""),
            "unknown or unset argument placeholder \"address\"",
        ),
        (
            base.replace("{{address}}", "{{missing}}"),
            "unknown or unset argument placeholder \"missing\"",
        ),
        (
            base.replace("{{address}}", "{{address"),
            "unclosed argument placeholder",
        ),
        (
            base.replace(
                "bundles: [basic]",
                "bundles: [basic], vars: {host_id: changed}",
            ),
            "vars cannot redefine",
        ),
        (
            base.replace(
                "bundles: [basic]",
                "bundles: [basic], overrides: {health: {timeout_seconds: 0}}",
            ),
            "timeout_seconds",
        ),
        (
            base.replace("name: Health", "name: ''"),
            "name must not be empty",
        ),
        (
            base.replace("id: production", "id: ungrouped"),
            "group ID ungrouped is reserved",
        ),
        (
            format!("recipients: ['']\n{base}"),
            "recipient must not be empty",
        ),
        (
            base.replace("name: Production", "name: ''"),
            "group \"production\": name must not be empty",
        ),
        (
            base.replace("bundles: [basic]", "bundles: [basic], interval_seconds: 0"),
            "interval_seconds",
        ),
        (base.replace("bundles: [basic]", "bundles: [basic], recipients: null"), "null is not allowed"),
        (base.replace("bundles: [basic]", "bundles: [basic], overrides: {health: {args: null}}"), "null is not allowed"),
        (base.replace("commands:\n", "commands:\n  - {id: probe, executable: other}\n"), "duplicate command ID \"probe\""),
        (base.replace("bundles:\n", "bundles:\n  - {id: basic, services: [{id: other, name: Other, executable: check}]}\n"), "duplicate bundle ID \"basic\""),
        (base.replace("groups:\n", "groups:\n  - {id: production, name: Duplicate}\n"), "duplicate group ID \"production\""),
        // Unused definitions must be validated too, not silently deferred until assigned.
        (
            base.replace(
                "commands:\n",
                "commands:\n  - {id: unused, executable: ''}\n",
            ),
            "command \"unused\": executable must not be empty",
        ),
        (
            base.replace("bundles:\n", "bundles:\n  - {id: unused, services: []}\n"),
            "bundle \"unused\": services must not be empty",
        ),
        (
            base.replace(
                "bundles:\n",
                "bundles:\n  - {id: unused, services: [{id: bad, name: Bad, command: absent}]}\n",
            ),
            "unknown command \"absent\"",
        ),
        (
            base.replace(
                "bundles: [basic]",
                "bundles: [basic], overrides: {health: {args: []}, health: {name: Hidden}}",
            ),
            "duplicate map key",
        ),
    ];
    for (yaml, expected) in cases {
        let error = load(&yaml).expect_err(&format!("accepted invalid configuration: {expected}"));
        let diagnostic = format!("{error:#}");
        assert!(
            diagnostic.contains(expected),
            "expected {expected:?}, got {diagnostic}"
        );
    }
}

#[test]
fn explicit_host_overrides_resolve_after_service_and_host_defaults() {
    let config = load(
        r#"
interval_seconds: 300
timeout_seconds: 30
commands:
  - {id: probe, executable: check, args: [default]}
bundles:
  - id: basic
    services:
      - {id: inherited, name: Inherited, command: probe}
      - {id: tuned, name: Tuned, command: probe, interval_seconds: 60, timeout_seconds: 10}
hosts:
  - id: alpha
    name: Alpha
    bundles: [basic]
  - id: beta
    name: Beta
    bundles: [basic]
    interval_seconds: 120
    timeout_seconds: 20
    overrides:
      tuned:
        name: Host-specific label
        args: ["literal $(not-a-shell)", "{{host_id}}"]
        interval_seconds: 15
        timeout_seconds: 5
      inherited:
        args: []
"#,
    )
    .unwrap();
    let alpha = &config.hosts[0].services;
    assert_eq!(
        (
            alpha[0].check.interval_seconds,
            alpha[0].check.timeout_seconds
        ),
        (300, 30)
    );
    assert_eq!(
        (
            alpha[1].check.interval_seconds,
            alpha[1].check.timeout_seconds
        ),
        (60, 10)
    );
    assert_eq!(alpha[1].check.args, ["default"]);
    let beta = &config.hosts[1].services;
    assert_eq!(
        (
            beta[0].check.interval_seconds,
            beta[0].check.timeout_seconds
        ),
        (120, 20)
    );
    assert!(beta[0].check.args.is_empty());
    assert_eq!(
        (
            beta[1].check.interval_seconds,
            beta[1].check.timeout_seconds
        ),
        (15, 5)
    );
    assert_eq!(beta[1].id, "tuned");
    assert_eq!(beta[1].name, "Host-specific label");
    assert_eq!(beta[1].check.args, ["literal $(not-a-shell)", "beta"]);
}

#[test]
fn host_groups_are_independent_of_service_bundles_and_recipient_overrides_replace_lists() {
    let config = load(
        r#"
recipients: [global@example.invalid, backup@example.invalid]
groups:
  - {id: production, name: Production, recipients: [prod@example.invalid]}
  - {id: lab, name: Lab}
bundles:
  - id: basic
    services:
      - {id: check, name: Health, executable: check}
hosts:
  - {id: a, name: A, group: production, bundles: [basic]}
  - {id: b, name: B, group: lab, bundles: [basic]}
  - {id: c, name: C, group: production, recipients: [owner@example.invalid], bundles: [basic]}
  - {id: d, name: D, group: production, recipients: [], bundles: [basic]}
  - {id: e, name: E, bundles: [basic]}
"#,
    )
    .unwrap();
    assert_eq!(config.hosts[0].group, "production");
    assert_eq!(config.hosts[0].recipients, ["prod@example.invalid"]);
    assert_eq!(config.hosts[1].group, "lab");
    assert_eq!(
        config.hosts[1].recipients,
        ["global@example.invalid", "backup@example.invalid"]
    );
    assert_eq!(config.hosts[2].recipients, ["owner@example.invalid"]);
    assert!(config.hosts[3].recipients.is_empty());
    assert_eq!(config.hosts[4].group, "ungrouped");
    assert_eq!(
        config.hosts[4].recipients,
        ["global@example.invalid", "backup@example.invalid"]
    );
}

#[test]
fn explicit_fragments_resolve_exactly_like_a_single_file_independent_of_working_directory() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("fleet")).unwrap();
    let settings =
        "plugin_dir: plugins\ndatabase: state.sqlite3\nrecipients: [global@example.invalid]\n";
    let commands = "commands:\n  - id: probe\n    executable: check\n    args: ['{{address}}']\n";
    let bundles = "bundles:\n  - id: basic\n    services:\n      - {id: health, name: Health, command: probe}\n";
    let groups = "groups:\n  - {id: prod, name: Production, recipients: [prod@example.invalid]}\n";
    let hosts = "hosts:\n  - {id: a, name: A, address: a.invalid, group: prod, bundles: [basic]}\n  - {id: b, name: B, address: b.invalid, group: prod, recipients: [], bundles: [basic]}\n";
    fs::write(
        dir.path().join("fleet/commands.yaml"),
        format!("{commands}{bundles}{groups}"),
    )
    .unwrap();
    fs::write(dir.path().join("fleet/hosts.yaml"), hosts).unwrap();
    let single = dir.path().join("single.yaml");
    fs::write(
        &single,
        format!("{settings}{commands}{bundles}{groups}{hosts}"),
    )
    .unwrap();
    let split = dir.path().join("split.yaml");
    fs::write(
        &split,
        format!("{settings}includes: [fleet/commands.yaml, fleet/hosts.yaml]\n"),
    )
    .unwrap();
    let resolved = Config::load(&split).unwrap();
    assert_eq!(Config::load(&single).unwrap(), resolved);
    assert_eq!(resolved.hosts[0].recipients, ["prod@example.invalid"]);
    assert!(resolved.hosts[1].recipients.is_empty());
}

#[test]
fn bundles_resolve_named_commands_for_multiple_hosts_without_shell_evaluation() {
    let config = load(
        r#"
commands:
  - id: probe
    executable: check_http
    args: ["-H", "{{address}}", "--label={{host_id}}", "{{port}}"]
bundles:
  - id: web
    services:
      - id: https
        name: HTTPS
        command: probe
hosts:
  - id: alpha
    name: Web server
    address: alpha.invalid
    vars: {port: "443"}
    bundles: [web]
  - id: beta
    name: Web server
    address: "beta.invalid; $(touch /tmp/no)"
    vars: {port: "{{not-expanded-again}}"}
    bundles: [web]
"#,
    )
    .unwrap();
    assert_eq!(config.hosts[0].services[0].id, "https");
    assert_eq!(
        config.hosts[0].services[0].check.executable.to_str(),
        Some("check_http")
    );
    assert_eq!(
        config.hosts[0].services[0].check.args,
        ["-H", "alpha.invalid", "--label=alpha", "443"]
    );
    assert_eq!(
        config.hosts[1].services[0].check.args,
        [
            "-H",
            "beta.invalid; $(touch /tmp/no)",
            "--label=beta",
            "{{not-expanded-again}}"
        ]
    );
}

const WINDOWS: &str = "check_windows:
  - id: work-hours
    timezone: Europe/London
    ranges:
      - {days: [mon, tue, wed, thu, fri], start: '09:00', end: '17:30'}
  - id: sundays
    ranges: [{days: [Sunday], start: '00:00', end: '24:00'}]
";

#[test]
fn check_windows_resolve_with_the_same_precedence_as_timing_and_default_to_utc() {
    let config = load(&format!(
        "check_window: work-hours\n{WINDOWS}hosts:
  - id: a
    name: A
    check_window: sundays
    reachability: {{probe: ping, executable: check, check_window: always}}
    overrides: {{overridden: {{check_window: always}}}}
    services:
      - {{id: host, name: Host, executable: check}}
      - {{id: service, name: Service, executable: check, check_window: work-hours}}
      - {{id: overridden, name: Overridden, executable: check, check_window: work-hours}}
  - id: b
    name: B
    reachability: {{probe: tcp, executable: check}}
    services: [{{id: root, name: Root, executable: check}}]
"
    ))
    .unwrap();
    fn window(check: &super::Check) -> Option<&str> {
        check.window.as_ref().map(|w| w.id.as_str())
    }
    let [a, b] = &config.hosts[..] else { panic!() };
    assert_eq!(window(&a.reachability.as_ref().unwrap().check), None);
    assert_eq!(window(&a.services[0].check), Some("sundays"));
    assert_eq!(window(&a.services[1].check), Some("work-hours"));
    assert_eq!(window(&a.services[2].check), None, "always opts out");
    assert_eq!(
        window(&b.reachability.as_ref().unwrap().check),
        Some("work-hours")
    );
    assert_eq!(window(&b.services[0].check), Some("work-hours"));
    let work = b.services[0].check.window.as_ref().unwrap();
    assert_eq!(work.timezone, chrono_tz::Europe::London);
    assert_eq!(
        work.ranges,
        [super::WindowRange {
            days: vec![
                chrono::Weekday::Mon,
                chrono::Weekday::Tue,
                chrono::Weekday::Wed,
                chrono::Weekday::Thu,
                chrono::Weekday::Fri,
            ],
            start: "09:00".parse().unwrap(),
            end: "17:30".parse().unwrap(),
        }]
    );
    let sundays = a.services[0].check.window.as_ref().unwrap();
    assert_eq!(sundays.timezone, chrono_tz::UTC);
    assert_eq!(sundays.ranges[0].end, super::TimeOfDay::END_OF_DAY);
    let unwindowed =
        load("hosts: [{id: a, name: A, services: [{id: s, name: S, executable: check}]}]").unwrap();
    assert_eq!(unwindowed.hosts[0].services[0].check.window, None);
}

#[test]
fn invalid_check_windows_and_unknown_references_reject_the_whole_fleet() {
    let service = "hosts: [{id: a, name: A, services: [{id: s, name: S, executable: check}]}]\n";
    let window = |range: &str| format!("check_windows: [{{id: w, ranges: [{range}]}}]\n{service}");
    for (yaml, expected) in [
        (format!("check_window: absent\n{service}"), "unknown check window \"absent\""),
        (
            "hosts: [{id: a, name: A, check_window: absent, services: [{id: s, name: S, executable: check}]}]\n".into(),
            "unknown check window \"absent\"",
        ),
        (
            "hosts: [{id: a, name: A, services: [{id: s, name: S, executable: check, check_window: absent}]}]\n".into(),
            "unknown check window \"absent\"",
        ),
        (
            "hosts: [{id: a, name: A, overrides: {s: {check_window: absent}}, services: [{id: s, name: S, executable: check}]}]\n".into(),
            "unknown check window \"absent\"",
        ),
        (
            "hosts: [{id: a, name: A, reachability: {probe: ping, executable: check, check_window: absent}, services: [{id: s, name: S, executable: check}]}]\n".into(),
            "unknown check window \"absent\"",
        ),
        (
            format!("bundles: [{{id: unused, services: [{{id: s, name: S, executable: check, check_window: absent}}]}}]\n{service}"),
            "unknown check window \"absent\"",
        ),
        (
            format!("check_windows: [{{id: w, timezone: Mars/Olympus, ranges: [{{days: [mon], start: '09:00', end: '17:00'}}]}}]\n{service}"),
            "invalid timezone \"Mars/Olympus\"",
        ),
        (window("{days: [mon], start: '17:00', end: '09:00'}"), "start must be before end"),
        (window("{days: [mon], start: '09:00', end: '09:00'}"), "start must be before end"),
        (window("{days: [mon], start: '09:00', end: '24:01'}"), "HH:MM"),
        (window("{days: [mon], start: '24:00', end: '24:00'}"), "start must be before end"),
        (window("{days: [mon], start: '00:00', end: '00:00'}"), "start must be before end"),
        (window("{days: [mon], start: '9:00', end: '17:00'}"), "HH:MM"),
        (window("{days: [mon], start: '09:60', end: '17:00'}"), "HH:MM"),
        (window("{days: [], start: '09:00', end: '17:00'}"), "days must not be empty"),
        (window("{days: [mon, mon], start: '09:00', end: '17:00'}"), "duplicate day"),
        (window("{days: [funday], start: '09:00', end: '17:00'}"), "days"),
        (format!("check_windows: [{{id: w, ranges: []}}]\n{service}"), "ranges must not be empty"),
        (
            format!("check_windows: [{{id: always, ranges: [{{days: [mon], start: '09:00', end: '17:00'}}]}}]\n{service}"),
            "check window ID always is reserved",
        ),
        (
            format!("check_windows:\n  - {{id: w, ranges: [{{days: [mon], start: '09:00', end: '17:00'}}]}}\n  - {{id: w, ranges: [{{days: [tue], start: '09:00', end: '17:00'}}]}}\n{service}"),
            "duplicate check window ID",
        ),
    ] {
        let error = format!("{:#}", load(&yaml).expect_err(&yaml));
        assert!(error.contains(expected), "{yaml}\n{error}");
        assert!(error.contains("ragios.yaml"), "{error}");
    }
}

#[test]
fn fragments_can_define_and_reference_check_windows() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("ragios.yaml");
    fs::write(
        &root,
        "plugin_dir: .\ndatabase: state.sqlite3\ncheck_window: work-hours\nincludes: [windows.yaml]\nhosts: [{id: a, name: A, services: [{id: s, name: S, executable: check}]}]\n",
    )
    .unwrap();
    fs::write(dir.path().join("windows.yaml"), WINDOWS).unwrap();
    let config = Config::load(&root).unwrap();
    let window = config.hosts[0].services[0].check.window.as_ref().unwrap();
    assert_eq!(window.id, "work-hours");
}
