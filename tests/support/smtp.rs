use super::*;

#[test]
fn smtp_credentials_must_be_in_protected_files() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = dir.path().join("password");
    fs::write(&credentials, "fake-password-never-in-diagnostics\n").unwrap();
    fs::set_permissions(&credentials, fs::Permissions::from_mode(0o644)).unwrap();
    private_file(&dir, "username", "fake-user\n");
    let path = config(&dir, &smtp_yaml("127.0.0.1", 1, "implicit"));
    let validate = || {
        Command::new(env!("CARGO_BIN_EXE_ragios"))
            .args(["--validate", "--config"])
            .arg(&path)
            .output()
            .unwrap()
    };
    let invalid = validate();
    assert!(!invalid.status.success());
    let stderr = String::from_utf8(invalid.stderr).unwrap();
    assert!(stderr.contains("owner-only"), "{stderr}");
    assert!(!stderr.contains("fake-password-never-in-diagnostics"));
    fs::set_permissions(&credentials, fs::Permissions::from_mode(0o600)).unwrap();
    let valid = validate();
    assert!(valid.status.success(), "{:?}", valid);
}

fn private_file(dir: &TempDir, name: &str, value: &str) {
    let path = dir.path().join(name);
    fs::write(&path, value).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn smtp_yaml(server: &str, port: u16, tls: &str) -> String {
    format!(
        "email:\n  mode: smtp\n  server: {server}\n  port: {port}\n  tls: {tls}\n  from: monitor@example.invalid\n  username_file: username\n  password_file: password\n"
    )
}
