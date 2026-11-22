use std::{fs, path::Path, process::Command};

#[test]
fn build_subcommand_round_trips_and_supports_layout_options() {
    const BRANCH_FANOUT_OFFSET: usize = 100;
    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    let temporary = builder.prefix("regol-cli-build-").tempdir().unwrap();
    let dir = temporary.path();
    let sample_gol = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ids.gol");
    let pbf_path = dir.join("sample.osm.pbf");

    // Export packaged fixture (with retained way-node IDs) to PBF using `regol query -f pbf`.
    let exported = Command::new(env!("CARGO_BIN_EXE_regol"))
        .args(["query", sample_gol.to_str().unwrap(), "*", "-f", "pbf"])
        .output()
        .unwrap();
    assert!(
        exported.status.success(),
        "export failed: {}",
        String::from_utf8_lossy(&exported.stderr)
    );
    fs::write(&pbf_path, &exported.stdout).unwrap();

    let expected_ids = Command::new(env!("CARGO_BIN_EXE_regol"))
        .args(["query", sample_gol.to_str().unwrap(), "*", "-f", "ids"])
        .output()
        .unwrap();
    assert!(expected_ids.status.success());
    let mut expected: Vec<String> = String::from_utf8(expected_ids.stdout)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    expected.sort();

    // Build with default options.
    let built_default = dir.join("default.gol");
    let build_out = Command::new(env!("CARGO_BIN_EXE_regol"))
        .args([
            "build",
            pbf_path.to_str().unwrap(),
            built_default.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        build_out.status.success(),
        "regol build failed: {}",
        String::from_utf8_lossy(&build_out.stderr)
    );
    let stderr = String::from_utf8(build_out.stderr).unwrap();
    assert!(stderr.contains("Wrote ") && stderr.contains(" tiles."));
    assert_eq!(fs::read(&built_default).unwrap()[BRANCH_FANOUT_OFFSET], 8);

    // Verify with `regol check` and `regol query`.
    let check = Command::new(env!("CARGO_BIN_EXE_regol"))
        .args(["check", built_default.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(check.status.success());
    assert!(
        String::from_utf8(check.stdout)
            .unwrap()
            .contains("Verified tiles: 1.")
    );

    let query = Command::new(env!("CARGO_BIN_EXE_regol"))
        .args(["query", built_default.to_str().unwrap(), "*", "-f", "ids"])
        .output()
        .unwrap();
    assert!(query.status.success());
    let mut actual: Vec<String> = String::from_utf8(query.stdout)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    actual.sort();
    assert_eq!(actual, expected);

    // Build with all S3/layout tuning flags (including --flag=value syntax).
    let built_tuned = dir.join("tuned.gol");
    let tuned = Command::new(env!("CARGO_BIN_EXE_regol"))
        .args([
            "build",
            "--compact-dictionary",
            "--even-zooms",
            "--target-tile-kib=4096",
            "--max-duplicate-tiles",
            "16",
            "--rtree-fanout=32",
            "--rtree-leaf-size",
            "64",
            pbf_path.to_str().unwrap(),
            built_tuned.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        tuned.status.success(),
        "tuned build failed: {}",
        String::from_utf8_lossy(&tuned.stderr)
    );
    assert_eq!(fs::read(&built_tuned).unwrap()[BRANCH_FANOUT_OFFSET], 32);
    let check_tuned = Command::new(env!("CARGO_BIN_EXE_regol"))
        .args(["check", built_tuned.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(check_tuned.status.success());

    // Build with layout presets and verify explicit flags can override them.
    for preset in ["vector-tiles", "compact"] {
        let built_preset = dir.join(format!("{preset}.gol"));
        let out = Command::new(env!("CARGO_BIN_EXE_regol"))
            .args([
                "build",
                "--preset",
                preset,
                pbf_path.to_str().unwrap(),
                built_preset.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "preset {preset} build failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let check_preset = Command::new(env!("CARGO_BIN_EXE_regol"))
            .args(["check", built_preset.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(check_preset.status.success());
    }
    assert!(
        !Command::new(env!("CARGO_BIN_EXE_regol"))
            .args([
                "build",
                "--preset=unknown",
                pbf_path.to_str().unwrap(),
                dir.join("bad.gol").to_str().unwrap(),
            ])
            .status()
            .unwrap()
            .success()
    );

    // Build with forced disk spilling (--memory-limit-mib 0) and confirm identical IDs.
    let built_spilled = dir.join("spilled.gol");
    let spilled = Command::new(env!("CARGO_BIN_EXE_regol"))
        .args([
            "build",
            "--memory-limit-mib",
            "0",
            pbf_path.to_str().unwrap(),
            built_spilled.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        spilled.status.success(),
        "spilled build failed: {}",
        String::from_utf8_lossy(&spilled.stderr)
    );
    let check_spilled = Command::new(env!("CARGO_BIN_EXE_regol"))
        .args(["check", built_spilled.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(check_spilled.status.success());
    let query_spilled = Command::new(env!("CARGO_BIN_EXE_regol"))
        .args(["query", built_spilled.to_str().unwrap(), "*", "-f", "ids"])
        .output()
        .unwrap();
    assert!(query_spilled.status.success());
    let mut actual_spilled: Vec<String> = String::from_utf8(query_spilled.stdout)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    actual_spilled.sort();
    assert_eq!(actual_spilled, expected);

    // Help and invalid invocation checks.
    let help = Command::new(env!("CARGO_BIN_EXE_regol"))
        .args(["build", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    let help_text = String::from_utf8(help.stdout).unwrap();
    assert!(help_text.contains("--max-duplicate-tiles"));
    assert!(help_text.contains("--memory-limit-mib"));
    assert!(
        !Command::new(env!("CARGO_BIN_EXE_regol"))
            .arg("build")
            .status()
            .unwrap()
            .success()
    );
}
