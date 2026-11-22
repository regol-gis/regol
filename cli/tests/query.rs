use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Command, Output},
};

fn query(arguments: &[&str]) -> Output {
    fixture_query("basic.gol", arguments)
}

fn fixture_query(fixture: &str, arguments: &[&str]) -> Output {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    Command::new(env!("CARGO_BIN_EXE_regol"))
        .arg("query")
        .arg(fixture)
        .args(arguments)
        .output()
        .unwrap()
}

#[test]
fn invalid_query_arguments_fail_without_stdout() {
    let output = query(&["*", "--bbox", "1,1,0,0"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("bbox is inverted"));

    let output = query(&["*", "--complete-relations"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing value"));
}

#[test]
fn spatial_query_exports_binary_pbf_to_stdout() {
    let output = query(&[
        "*",
        "--bbox=-0.002,-0.002,0.003,0.003",
        "-f",
        "pbf",
        "-R=all",
    ]);
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stderr.is_empty());
    let header_size = u32::from_be_bytes(output.stdout[..4].try_into().unwrap()) as usize;
    let header = &output.stdout[4..4 + header_size];
    assert!(header.starts_with(b"\x0a\x09OSMHeader"));
    assert!(
        output.stdout[4 + header_size..]
            .windows(7)
            .any(|window| window == b"OSMData")
    );
}

#[test]
fn storage_and_dictionary_modes_preserve_filtered_and_unfiltered_queries() {
    for selector in ["*", "n[name]"] {
        let expected = query(&[selector]);
        assert!(expected.status.success(), "{:?}", expected.stderr);
        for backend in [
            vec![selector, "--threads", "1"],
            vec![selector, "--threads", "2"],
            vec![selector, "--no-mmap"],
            vec![selector, "--eager-dictionary"],
            vec![selector, "--no-mmap", "--eager-dictionary"],
            vec![selector, "--lazy-dictionary"],
            vec![selector, "--mmap", "--lazy-dictionary"],
        ] {
            let result = query(&backend);
            assert!(result.status.success(), "{:?}", result.stderr);
            assert_eq!(result.stdout, expected.stdout);
        }
    }
}

fn successful_query(arguments: &[&str]) -> String {
    successful_stdout(query(arguments))
}

fn successful_stdout(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    String::from_utf8(output.stdout).unwrap()
}

fn dateline_features(format: &str, precision: &str) -> Vec<Value> {
    let output = successful_stdout(fixture_query(
        "antimeridian.gol",
        &["*", "-f", format, "--precision", precision, "--keys=*,bbox"],
    ));
    if format == "geojsonl" {
        output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    } else {
        let collection: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(collection["type"], "FeatureCollection");
        collection["features"].as_array().unwrap().clone()
    }
}

fn feature_by_id<'a>(features: &'a [Value], id: &str) -> &'a Value {
    features.iter().find(|feature| feature["id"] == id).unwrap()
}

#[test]
fn geojson_cuts_lines_in_path_order_and_handles_existing_seam_vertices() {
    let features = dateline_features("geojsonl", "5");
    assert_eq!(features, dateline_features("geojson", "5"));
    let crossing = feature_by_id(&features, "W10");
    assert_eq!(crossing["bbox"], json!([170.0, 45.0, -170.0, 65.0]));
    assert_eq!(crossing["geometry"]["type"], "MultiLineString");
    assert_eq!(
        crossing["geometry"]["coordinates"],
        json!([
            [[170.0, 45.0], [180.0, 50.0]],
            [[-180.0, 50.0], [-170.0, 55.0], [-180.0, 61.66667]],
            [[180.0, 61.66667], [175.0, 65.0]],
        ])
    );

    let vertex = feature_by_id(&features, "W16");
    assert_eq!(
        vertex["geometry"]["coordinates"],
        json!([
            [[-175.0, -3.0], [-180.0, -2.0]],
            [[180.0, -2.0], [175.0, -1.0]]
        ])
    );
    let endpoint = feature_by_id(&features, "W17");
    assert_eq!(endpoint["geometry"]["type"], "LineString");
    assert_eq!(
        endpoint["geometry"]["coordinates"],
        json!([[180.0, -2.0], [175.0, -1.0]])
    );
}

fn lon_lat(position: &Value) -> (f64, f64) {
    (position[0].as_f64().unwrap(), position[1].as_f64().unwrap())
}

fn ring_area(ring: &Value) -> f64 {
    let ring = ring.as_array().unwrap();
    assert!(ring.len() >= 4);
    assert_eq!(ring.first(), ring.last());
    let (x, y) = lon_lat(&ring[0]);
    let cross_products: f64 = ring
        .windows(2)
        .map(|edge| {
            let (ax, ay) = lon_lat(&edge[0]);
            let (bx, by) = lon_lat(&edge[1]);
            assert!((-180.0..=180.0).contains(&ax));
            assert!((-90.0..=90.0).contains(&ay));
            assert!((ax - bx).abs() <= 180.0);
            (ax - x) * (by - y) - (bx - x) * (ay - y)
        })
        .sum();
    cross_products / 2.0
}

#[test]
fn geojson_cuts_polygons_and_holes_and_preserves_mainland_and_collections() {
    let features = dateline_features("geojson", "15");
    let relation = feature_by_id(&features, "R20");
    assert_eq!(relation["geometry"]["type"], "MultiPolygon");
    let parts = relation["geometry"]["coordinates"].as_array().unwrap();
    assert_eq!(parts.len(), 3);

    let mut area = 0.0;
    let mut holes = 0;
    for polygon in parts {
        for (index, ring) in polygon.as_array().unwrap().iter().enumerate() {
            let signed_area = ring_area(ring);
            assert!(if index == 0 {
                signed_area > 0.0
            } else {
                signed_area < 0.0
            });
            holes += usize::from(index != 0);
            area += signed_area;
        }
    }
    // The second hole spans the seam and leaves an indentation in each shell.
    assert_eq!(holes, 2);
    assert!((area - 140.0).abs() < 1e-4);

    let mainland = feature_by_id(&features, "W14");
    let positions = |ring: &Value| {
        let mut positions: Vec<_> = ring
            .as_array()
            .unwrap()
            .iter()
            .map(Value::to_string)
            .collect();
        positions.sort_unstable();
        positions
    };
    assert_eq!(
        positions(&parts[0][0]),
        positions(&mainland["geometry"]["coordinates"])
    );
    let tiny_hole = feature_by_id(&features, "W21");
    assert!(
        parts
            .iter()
            .flat_map(|polygon| polygon.as_array().unwrap().iter().skip(1))
            .any(|ring| positions(ring) == positions(&tiny_hole["geometry"]["coordinates"]))
    );

    let concave = feature_by_id(&features, "W15");
    let parts = concave["geometry"]["coordinates"].as_array().unwrap();
    assert_eq!(parts.len(), 3);
    assert!(parts.iter().all(|polygon| ring_area(&polygon[0]) > 0.0));

    let collection = feature_by_id(&features, "R21");
    assert_eq!(collection["geometry"]["type"], "GeometryCollection");
    assert_eq!(
        collection["geometry"]["geometries"][1],
        relation["geometry"]
    );
    assert!(collection["bbox"][0].as_f64().unwrap() > collection["bbox"][2].as_f64().unwrap());
}

#[test]
fn geojson_closes_polar_caps_through_the_smaller_side_and_cuts_polar_holes() {
    let features = dateline_features("geojsonl", "5");
    for (id, expected_bbox) in [
        ("W18", json!([-180.0, 80.0, 180.0, 90.0])),
        ("W19", json!([-180.0, -90.0, 180.0, -80.0])),
        ("R22", json!([-180.0, 80.0, 180.0, 83.0])),
    ] {
        let feature = feature_by_id(&features, id);
        assert_eq!(feature["geometry"]["type"], "Polygon");
        assert_eq!(feature["bbox"], expected_bbox);
        let rings = feature["geometry"]["coordinates"].as_array().unwrap();
        assert_eq!(rings.len(), 1);
        assert!(ring_area(&rings[0]) > 0.0);
    }

    let backtracking = feature_by_id(&features, "W22");
    assert_eq!(backtracking["geometry"]["type"], "MultiPolygon");
    assert_eq!(backtracking["bbox"], json!([-180.0, 76.0, 180.0, 90.0]));
    let parts = backtracking["geometry"]["coordinates"].as_array().unwrap();
    assert_eq!(parts.len(), 2);
    assert!(parts.iter().all(|part| ring_area(&part[0]) > 0.0));
}

#[test]
fn geojson_cuts_a_ribbon_covering_more_than_one_world_copy() {
    let features = dateline_features("geojson", "7");
    let ribbon = feature_by_id(&features, "W23");
    assert_eq!(ribbon["geometry"]["type"], "MultiPolygon");
    assert_eq!(ribbon["bbox"], json!([-180.0, -30.0, 180.0, -6.5]));
    let pieces = ribbon["geometry"]["coordinates"].as_array().unwrap();
    assert_eq!(pieces.len(), 2);
    let area: f64 = pieces.iter().map(|part| ring_area(&part[0])).sum();
    assert!((area - 405.0).abs() < 1e-4);

    let holed = feature_by_id(&features, "R23");
    let pieces = holed["geometry"]["coordinates"].as_array().unwrap();
    assert_eq!(pieces.len(), 2);
    for piece in pieces {
        let rings = piece.as_array().unwrap();
        assert_eq!(rings.len(), 2);
        assert!(ring_area(&rings[0]) > 0.0);
        assert!((ring_area(&rings[1]) + 0.9).abs() < 1e-5);
    }
}

#[test]
fn geojson_exports_typed_tags_and_optional_bounds() {
    let output = successful_query(&[
        "n[amenity=cafe]",
        "-f",
        "geojson",
        "--keys",
        "name,capacity,height,negative,odd:key,missing,bbox,empty",
        "--precision",
        "3",
    ]);
    let collection: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(
        collection,
        json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "id": "N1",
                "geometry": { "type": "Point", "coordinates": [0.001, 0.001] },
                "bbox": [0.001, 0.001, 0.001, 0.001],
                "properties": {
                    "name": "Café 東京",
                    "capacity": 12,
                    "height": 3.25,
                    "negative": -256,
                    "odd:key": "0012",
                    "empty": "",
                }
            }]
        })
    );
}

#[test]
fn geojsonl_and_wkt_export_matching_geometries_in_id_order() {
    let bbox = "--bbox=0,0,0.0016,0.0018";
    let ids = successful_query(&["*", bbox, "-f", "ids"]);
    let records = successful_query(&["*", bbox, "-f", "geojsonl"]);
    let records: Vec<Value> = records
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        records
            .iter()
            .map(|record| record["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ids.lines().collect::<Vec<_>>()
    );
    let polygon = records.iter().find(|record| record["id"] == "R20").unwrap();
    assert_eq!(polygon["geometry"]["type"], "Polygon");
    assert_eq!(
        polygon["geometry"]["coordinates"].as_array().unwrap().len(),
        2
    );
    let nested = records.iter().find(|record| record["id"] == "R22").unwrap();
    assert_eq!(nested["geometry"]["type"], "GeometryCollection");

    let wkts = successful_query(&["*", bbox, "-f", "wkt", "--precision=3"]);
    assert_eq!(wkts.lines().count(), records.len());
    assert!(wkts.lines().any(|line| line == "POINT (0.001 0.001)"));
    assert!(wkts.lines().any(|line| line.starts_with("LINESTRING (")));
    assert!(wkts.lines().any(|line| line.starts_with("POLYGON ((")));
    assert!(
        wkts.lines()
            .any(|line| line.starts_with("GEOMETRYCOLLECTION ("))
    );
}

#[test]
fn polygon_filter_selects_the_same_features_for_gis_exports() {
    let area = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/area.geojson");
    let area = area.to_str().unwrap();
    let ids = successful_query(&["*", "--area", area, "-f", "ids"]);
    let ids: Vec<_> = ids.lines().collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&"R20"));
    assert!(ids.contains(&"W11"));

    let output = successful_query(&["*", "--area", area, "-f", "geojson"]);
    let collection: Value = serde_json::from_str(&output).unwrap();
    let records = collection["features"].as_array().unwrap();
    let exported_ids: Vec<_> = records
        .iter()
        .map(|record| record["id"].as_str().unwrap())
        .collect();
    assert_eq!(exported_ids, ids);

    let csv = successful_query(&["*", "--area", area, "-f", "csv", "--keys=id"]);
    assert_eq!(csv.lines().skip(1).collect::<Vec<_>>(), ids);
}

#[test]
fn csv_selects_columns_in_order_and_disambiguates_tag_names() {
    let output = successful_query(&[
        "r[type=route]",
        "-f",
        "csv",
        "--keys",
        "name,type,id,tag:type,osm_id,name",
    ]);
    assert_eq!(
        output,
        "name,type,id,tag:type,osm_id\r\nTest Route,relation,R21,route,21\r\n"
    );

    let output = successful_query(&[
        "n[amenity=cafe]",
        "-f",
        "csv",
        "--columns",
        "id,lat,lon,bbox,geom,wkt,tags",
        "--precision=3",
    ]);
    assert!(output.starts_with(
        "id,lat,lon,bbox,geom,tags\r\nN1,0.001,0.001,\"0.001,0.001,0.001,0.001\",\"POINT (0.001 0.001)\",\"{"
    ));
    assert!(output.contains("\"\"capacity\"\":12"));
    assert!(output.contains("\"\"name\"\":\"\"Café 東京\"\""));
    assert!(output.ends_with("}\"\r\n"));
}

#[test]
fn export_empty_results_and_precision_errors() {
    let term = "n[name=DoesNotExist]";
    assert_eq!(
        successful_query(&[term, "-f", "geojson"]),
        "{\"type\":\"FeatureCollection\",\"features\":[]}\n"
    );
    assert!(successful_query(&[term, "-f", "geojsonl"]).is_empty());
    assert!(successful_query(&[term, "-f", "wkt"]).is_empty());
    assert_eq!(
        successful_query(&[term, "-f", "csv"]),
        "id,type,geometry\r\n"
    );

    let output = query(&["*", "-f", "geojson", "--precision", "16"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("precision must be an integer"));
}

#[test]
fn unordered_flag_exports_same_decoded_records_across_formats() {
    let run_tiled = |args: &[&str]| successful_stdout(fixture_query("tiled.gol", args));

    // IDs
    let mut ordered_ids: Vec<_> = run_tiled(&["*", "-f", "ids", "--threads", "1"])
        .lines()
        .map(str::to_owned)
        .collect();
    let mut unordered_ids: Vec<_> = run_tiled(&["*", "-f", "ids", "--threads", "4", "--unordered"])
        .lines()
        .map(str::to_owned)
        .collect();
    ordered_ids.sort();
    unordered_ids.sort();
    assert_eq!(ordered_ids, unordered_ids);

    // GeoJSON
    let ordered_gj: Value =
        serde_json::from_str(&run_tiled(&["*", "-f", "geojson", "--threads", "1"])).unwrap();
    let unordered_gj: Value = serde_json::from_str(&run_tiled(&[
        "*",
        "-f",
        "geojson",
        "--threads",
        "4",
        "--unordered",
    ]))
    .unwrap();
    let mut ordered_features: Vec<_> = ordered_gj["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(Value::to_string)
        .collect();
    let mut unordered_features: Vec<_> = unordered_gj["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(Value::to_string)
        .collect();
    ordered_features.sort();
    unordered_features.sort();
    assert_eq!(ordered_features, unordered_features);

    // GeoJSONL, WKT, CSV
    for args in [
        &["*", "-f", "geojsonl"][..],
        &["*", "-f", "wkt"][..],
        &["*", "-f", "csv", "--keys", "id,name"][..],
    ] {
        let mut ordered_cmd = args.to_vec();
        ordered_cmd.extend(["--threads", "1"]);
        let mut parallel_ordered_cmd = args.to_vec();
        parallel_ordered_cmd.extend(["--threads", "4", "--ordered"]);
        let mut unordered_cmd = args.to_vec();
        unordered_cmd.extend(["--threads", "4", "--unordered"]);
        let ordered_out = run_tiled(&ordered_cmd);
        assert_eq!(run_tiled(&parallel_ordered_cmd), ordered_out);
        let mut ordered_lines: Vec<_> = ordered_out.lines().map(str::to_owned).collect();
        let mut unordered_lines: Vec<_> = run_tiled(&unordered_cmd)
            .lines()
            .map(str::to_owned)
            .collect();
        if args.contains(&"csv") {
            assert_eq!(ordered_lines[0], unordered_lines[0]);
            ordered_lines[1..].sort();
            unordered_lines[1..].sort();
        } else {
            ordered_lines.sort();
            unordered_lines.sort();
        }
        assert_eq!(ordered_lines, unordered_lines);
    }
}
