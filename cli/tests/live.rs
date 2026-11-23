use std::process::Command;

#[test]
fn missing_live_helper_explains_how_to_install_it() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_regol"))
        .args(["live", "--help"])
        .env("PATH", directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("cargo install --locked regol-live")
    );
}

#[cfg(unix)]
#[test]
fn forwards_arguments_and_exit_status() {
    use std::{fs, os::unix::fs::PermissionsExt};
    let directory = tempfile::tempdir().unwrap();
    let helper = directory.path().join("regol-live");
    fs::write(&helper, "#!/bin/sh\nprintf '%s\\n' \"$@\"\nexit 23\n").unwrap();
    fs::set_permissions(helper, fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_regol"))
        .args(["live", "a file.gol", "--port", "1234"])
        .env("PATH", directory.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(23));
    assert_eq!(output.stdout, b"a file.gol\n--port\n1234\n");
}
