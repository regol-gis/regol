use std::{path::Path, process::Command};

#[test]
fn commands_inspect_the_packaged_sample_and_report_failures() {
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/basic.gol");
    for (command, expected) in [
        ("info", "Way node IDs:"),
        ("check", "Verified tiles: 1. Features: 8."),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_regol"))
            .arg(command)
            .arg(&file)
            .output()
            .unwrap();
        assert!(out.status.success(), "{:?}", out.stderr);
        assert!(String::from_utf8(out.stdout).unwrap().contains(expected));
        assert!(
            !Command::new(env!("CARGO_BIN_EXE_regol"))
                .arg(command)
                .status()
                .unwrap()
                .success()
        );
    }
}
