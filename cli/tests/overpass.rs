use serde_json::{Value, json};
use std::{
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};

fn command(fixture: &str) -> Command {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    let mut command = Command::new(env!("CARGO_BIN_EXE_regol"));
    command.arg("overpass").arg(fixture);
    command
}

fn stdin_query(fixture: &str, query: &[u8], args: &[&str]) -> Output {
    let mut child = command(fixture)
        .args(["--file", "-"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(query).unwrap();
    child.wait_with_output().unwrap()
}

fn rows(output: Output) -> Vec<Value> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn failed(output: Output, diagnostic: &str) {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(diagnostic), "{stderr}");
}

#[test]
fn file_query_preserves_feature_and_subsequent_count_groups() {
    let query = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        query.path(),
        "node[amenity=cafe](0,0,0.002,0.002)->.cafes; .cafes out body asc; .cafes out count;",
    )
    .unwrap();
    let output = command("basic.gol")
        .arg("--file")
        .arg(query.path())
        .output()
        .unwrap();
    assert_eq!(
        rows(output),
        [
            json!({"name":"cafes","mode":"geom","detail":"body","order":"asc","ids":["N1"]}),
            json!({"name":"cafes","mode":"count","counts":{"nodes":1,"ways":0,"relations":0,"total":1}}),
        ]
    );
}

#[test]
fn stdin_bbox_is_substituted_only_for_the_macro() {
    let selection = b"node[amenity=cafe]({{bbox}}); out count;";
    failed(stdin_query("basic.gol", selection, &[]), "--bbox");
    let args = &["--bbox", "0,0,0.002,0.002", "--no-mmap"];
    let bounded = rows(stdin_query("basic.gol", selection, args));
    assert_eq!(bounded[0]["counts"]["total"], 1);
    let outside = &["--bbox=-20,-10,-19,-9"];
    let bounded = rows(stdin_query("basic.gol", selection, outside));
    assert_eq!(bounded[0]["counts"]["total"], 0);
    let literal = b"node[amenity=cafe](0,0,0.002,0.002); out count;";
    let unbounded_by_option = rows(stdin_query("basic.gol", literal, outside));
    assert_eq!(unbounded_by_option[0]["counts"]["total"], 1);

    let wrapped = rows(stdin_query(
        "antimeridian.gol",
        b"way[name=Crossing]({{bbox}}); out;",
        &["--bbox=167,40,-167,75"],
    ));
    assert_eq!(wrapped[0]["ids"], json!(["W10"]));
}

#[test]
fn output_formats_respect_counts_center_and_skeleton() {
    let query = b"node[amenity=cafe](0,0,0.002,0.002)->.cafes;
        .cafes out geom; .cafes out center skel; .cafes out count;";
    let geojson = rows(stdin_query("basic.gol", query, &["-f", "geojson"]));
    assert_eq!(geojson[0]["type"], "FeatureCollection");
    assert_eq!(geojson[0]["features"][0]["properties"]["name"], "Café 東京");
    assert_eq!(geojson[1]["mode"], "center");
    assert_eq!(geojson[1]["detail"], "skel");
    assert_eq!(geojson[1]["features"][0]["geometry"]["type"], "Point");
    assert_eq!(geojson[1]["features"][0]["properties"], json!({}));
    assert_eq!(geojson[2]["counts"]["total"], 1);
    let counts = rows(stdin_query("basic.gol", query, &["--format=count"]));
    assert_eq!(counts.len(), 3);
    assert!(counts.iter().all(|group| group["counts"]["total"] == 1));
}

#[test]
fn bad_input_fails_with_a_diagnostic() {
    failed(command("basic.gol").output().unwrap(), "missing --file");
    failed(
        command("basic.gol").arg("--file").output().unwrap(),
        "missing value for --file",
    );
    let missing = tempfile::tempdir().unwrap().path().join("missing.ql");
    failed(
        command("basic.gol")
            .arg("--file")
            .arg(missing)
            .output()
            .unwrap(),
        "reading query",
    );
    failed(stdin_query("basic.gol", &[0xff], &[]), "UTF-8");
    failed(
        stdin_query(
            "basic.gol",
            b"node[amenity=cafe](0,0,0.002,0.002);\nout meta;",
            &[],
        ),
        "Line 2:",
    );
    failed(
        stdin_query(
            "basic.gol",
            b"node[amenity=cafe]; out;",
            &["--bbox=0,50,0,40"],
        ),
        "bbox",
    );
}

#[test]
fn named_areas_scan_local_files_and_require_matching_indexes() {
    let query = b"area[name=Courtyard]; out count;";
    let expected = json!({"name":"_","mode":"count","counts":{
        "nodes":0,"ways":0,"relations":0,"areas":1,"total":1
    }});
    for args in [&[][..], &["--no-mmap"]] {
        assert_eq!(
            rows(stdin_query("basic.gol", query, args)).as_slice(),
            std::slice::from_ref(&expected),
        );
    }

    let index = tempfile::tempdir().unwrap();
    std::fs::write(
        index.path().join("manifest.json"),
        r#"{"version":1,"snapshot":"wrong","shards":256}"#,
    )
    .unwrap();
    failed(
        stdin_query(
            "basic.gol",
            query,
            &["--area-index", index.path().to_str().unwrap()],
        ),
        "different GOL snapshot",
    );
}
