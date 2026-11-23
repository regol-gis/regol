use regol::Library;
use regol_overpass::{Counts, GroupData, OutputMode, execute_groups, feature_geometry};
use serde_json::{Value, json};

const GOL: &[u8] = include_bytes!("fixtures/geography.gol");

fn select(name: &str, kind: &str, key: &str, value: &str, spatial: Value) -> Value {
    let mut selection = json!({
        "kind": kind, "tags": [{"key": key, "op": "eq", "value": value}],
    });
    selection
        .as_object_mut()
        .unwrap()
        .extend(spatial.as_object().unwrap().clone());
    json!({"name": name, "source": "*", "overpass": selection})
}

fn assert_selected(sets: Vec<Value>, expected: &[&str]) {
    let lib = Library::new(GOL).unwrap();
    let mut plan = json!({
        "windows": [[7.41, 43.72, 7.44, 43.75]],
        "sets": sets,
        "show": ["selected"],
        "output": "center",
    });
    let mut groups = execute_groups(&lib, serde_json::from_value(plan.clone()).unwrap(), &|| {
        Ok(())
    })
    .unwrap();
    assert_eq!(groups.len(), 1);
    let GroupData::Features(features) = groups.remove(0).data else {
        panic!("expected selected features");
    };
    let mut ids: Vec<_> = features
        .into_iter()
        .map(|feature| {
            feature_geometry(feature, OutputMode::Center).unwrap();
            feature.id().to_string()
        })
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, expected);

    plan["output"] = json!("count");
    let mut groups =
        execute_groups(&lib, serde_json::from_value(plan).unwrap(), &|| Ok(())).unwrap();
    assert_eq!(groups.len(), 1);
    let GroupData::Count(counts) = groups.remove(0).data else {
        panic!("expected count output");
    };
    let by_type = |prefix| expected.iter().filter(|id| id.starts_with(prefix)).count();
    assert_eq!(
        counts,
        Counts {
            nodes: by_type('N'),
            ways: by_type('W'),
            relations: by_type('R'),
            total: expected.len(),
            ..Counts::default()
        }
    );
}

#[test]
fn antimeridian_polygon_finds_both_halves_and_a_way_crossing_between_them() {
    let polygon = [
        [179.8, -0.25],
        [-179.8, -0.25],
        [-179.8, 0.25],
        [179.8, 0.25],
    ];
    let selected = select(
        "selected",
        "nw",
        "test:group",
        "dateline",
        json!({"polygon": polygon, "bbox": false}),
    );
    assert_selected(vec![selected], &["N11", "N12", "W103"]);
}

#[test]
fn wide_sparse_node_polygon_can_keep_its_original_scan_window() {
    let selected = select(
        "selected",
        "node",
        "test:group",
        "dateline",
        json!({"polygon": [[-80,-19], [80,-19], [80,19], [-80,19]], "bbox": false}),
    );
    assert_selected(vec![selected], &["N14"]);
}

#[test]
fn area_blocks_preserve_identity_and_type_through_chained_set_operations() {
    let library = Library::new(GOL).unwrap();
    let declarations = json!([
        {"name":"r", "source":"a[name=Courtyard]"},
        {"name":"w", "source":"a[name=Shell]"},
        {"name":"courtyard", "source":"$r", "map_to_area":true},
        {"name":"shell", "source":"$w", "map_to_area":true},
        {"name":"all", "source":"", "union":["courtyard","shell","courtyard"]},
        {"name":"remaining", "source":"", "difference":["all","shell"]},
        {"name":"none", "source":"", "difference":["remaining","courtyard"]}
    ]);
    let request = |show: &str| {
        json!({
            "windows":[[-180,-85,180,85]], "sets":declarations, "show":[show], "output":"count"
        })
    };
    for (name, ways, areas) in [("all", 1, 1), ("remaining", 0, 1), ("none", 0, 0)] {
        let mut output = execute_groups(
            &library,
            serde_json::from_value(request(name)).unwrap(),
            &|| Ok(()),
        )
        .unwrap();
        assert_eq!(output.len(), 1);
        let GroupData::Count(counts) = output.remove(0).data else {
            panic!("expected count output");
        };
        assert_eq!(
            counts,
            Counts {
                ways,
                areas: Some(areas),
                total: ways + areas,
                ..Counts::default()
            }
        );
    }
    let mut mixed = request("all");
    mixed["sets"][4]["union"] = json!(["courtyard", "r"]);
    let result = execute_groups(&library, serde_json::from_value(mixed).unwrap(), &|| Ok(()));
    assert!(result.err().unwrap().contains("all area sets"));
}

#[test]
fn key_presence_in_a_dateline_view_matches_both_encodings() {
    let selected = json!({
        "name": "selected", "source": "*", "overpass": {
            "kind": "node", "tags": [{"key":"test:group","op":"exists"}],
            "windows": [[179.8, -0.1, 180, 0.1], [-180, -0.1, -179.8, 0.1]]
        }
    });
    assert_selected(vec![selected], &["N11", "N12"]);
}

#[test]
fn antimeridian_mapped_areas_keep_the_hole_and_include_its_boundary() {
    for (kind, name, expected) in [
        (
            "way",
            "Shell",
            &["N10", "N11", "N12", "N13", "N9", "W103", "W107"][..],
        ),
        ("relation", "Courtyard", &["N10", "N13", "N9", "W107"][..]),
    ] {
        let reference = select(
            "outline",
            kind,
            "name",
            name,
            json!({"windows": [[177, -3, 180, 3], [-180, -3, -177, 3]]}),
        );
        let mapped = json!({"name": "area", "source": "$outline", "map_to_area": true});
        let selected = select(
            "selected",
            "nw",
            "test:group",
            "dateline",
            json!({"area": "area"}),
        );
        assert_selected(vec![reference, mapped, selected], expected);
    }
}

#[test]
fn around_finds_direct_paths_whose_great_circle_leaves_the_indexed_bounds() {
    // Both endpoints are at 80°N and outside the 25 m search radius. The
    // shortest arc passes through 80.3342036°N, over 37 km north of the bounds.
    let point = [0.0, 80.3342036];
    let literal = select(
        "selected",
        "wr",
        "test:group",
        "arc",
        json!({"around": {"point": point, "meters": 25}}),
    );
    assert_selected(vec![literal], &["R202", "W105"]);

    let reference = select(
        "midpoint",
        "node",
        "test:group",
        "arc_midpoint",
        json!({"windows": [[-0.001, 80.334, 0.001, 80.335]]}),
    );
    let referenced = select(
        "selected",
        "wr",
        "test:group",
        "arc",
        json!({"around": {"reference": "midpoint", "meters": 25}}),
    );
    assert_selected(vec![reference, referenced], &["R202", "W105"]);
}

#[test]
fn around_polyline_finds_the_middle_of_great_circle_arcs() {
    let node = select(
        "selected",
        "node",
        "test:group",
        "arc_midpoint",
        json!({"around": {"polyline": [[-15, 80], [15, 80]], "meters": 25}}),
    );
    assert_selected(vec![node], &["N23"]);

    let crossing = select(
        "selected",
        "wr",
        "test:group",
        "arc",
        json!({"around": {"polyline": [[0, 80.32], [0, 80.35]], "meters": 0}}),
    );
    assert_selected(vec![crossing], &["R202", "W105"]);
}

#[test]
fn around_polyline_follows_each_segment_across_the_dateline() {
    let selected = select(
        "selected",
        "nw",
        "test:group",
        "dateline",
        json!({"around": {"polyline": [[-179.7, 0], [179.7, 0], [179.7, 0.5]], "meters": 25}}),
    );
    assert_selected(vec![selected], &["N11", "N12", "N13", "W103"]);
}

#[test]
fn around_polyline_uses_polar_coordinates_above_the_mercator_grid() {
    for (polyline, meters, expected) in [
        ([[0.0, 88.8], [0.0, 89.1]], 0.0, &["R208", "W111"][..]),
        // Projected way endpoints can leave the stored arc millimetres short of the pole.
        ([[0.0, 89.5], [130.0, 90.0]], 0.01, &["R209", "W113"][..]),
    ] {
        let selected = select(
            "selected",
            "wr",
            "test:group",
            "polar_arc",
            json!({"around": {"polyline": polyline, "meters": meters}}),
        );
        assert_selected(vec![selected], expected);
    }
}

#[test]
fn around_polyline_rejects_invalid_json_plans() {
    let lib = Library::new(GOL).unwrap();
    for (around, expected) in [
        (
            json!({"meters": 0}),
            "Supply one reference set, point or polyline",
        ),
        (
            json!({"meters": 0, "polyline": [[0, 0]], "point": [0, 0]}),
            "Supply one reference set, point or polyline",
        ),
        (
            json!({"meters": 0, "polyline": [[0, 0], [1, 1]], "reference": "reference"}),
            "Supply one reference set, point or polyline",
        ),
        (json!({"meters": 0, "polyline": []}), "2 to 10,000"),
        (json!({"meters": 0, "polyline": [[0, 0]]}), "2 to 10,000"),
        (
            json!({"meters": 0, "polyline": vec![[0, 0]; 10_001]}),
            "2 to 10,000",
        ),
        (
            json!({"meters": 0, "polyline": [[0, 0], [0, 91]]}),
            "latitude",
        ),
        (
            json!({"meters": 0, "polyline": [[0, 0], [181, 0]]}),
            "longitude",
        ),
        (
            json!({"meters": -1, "polyline": [[0, 0], [0, 1]]}),
            "nonnegative",
        ),
        (
            json!({"meters": 0, "polyline": [[0, 0], [180, 0]]}),
            "antipodal",
        ),
    ] {
        let selected = select(
            "selected",
            "node",
            "test:group",
            "arc_midpoint",
            json!({"around": around}),
        );
        let reference = select(
            "reference",
            "node",
            "test:group",
            "dateline",
            json!({"windows": [[179, -1, 180, 1]]}),
        );
        let plan = json!({
            "windows": [[179, -1, 180, 1]], "sets": [reference, selected],
            "show": ["selected"], "output": "count",
        });
        let error = match execute_groups(&lib, serde_json::from_value(plan).unwrap(), &|| Ok(())) {
            Err(error) => error,
            Ok(_) => panic!("Invalid proximity plan succeeded"),
        };
        assert!(
            error.contains(expected),
            "Expected {expected:?}, got {error:?}"
        );
    }
}

#[test]
fn explicit_bbox_finds_seam_paths_outside_their_stored_bounds() {
    // The closed way and its parent route reach 179.95°E across the seam,
    // despite their indexed maximum longitude being only 179.7°E.
    let direct_bbox = select(
        "selected",
        "wr",
        "test:group",
        "seam_path",
        json!({"windows": [[179.85, -0.15, 179.95, 0.15]]}),
    );
    assert_selected(vec![direct_bbox], &["R205", "W109"]);
}

#[test]
fn mapped_area_finds_seam_paths_outside_their_stored_bounds() {
    let reference = select(
        "outline",
        "way",
        "name",
        "Narrow",
        json!({"windows": [[179.84, -0.3, 179.96, 0.3]]}),
    );
    let mapped = json!({"name": "area", "source": "$outline", "map_to_area": true});
    let dynamic_area = select(
        "selected",
        "wr",
        "test:group",
        "seam_path",
        json!({"area": "area"}),
    );
    assert_selected(vec![reference, mapped, dynamic_area], &["R205", "W109"]);
}

#[test]
fn mapped_multipolygon_assigns_the_hole_to_its_dateline_shell() {
    let reference = select(
        "outline",
        "relation",
        "name",
        "Two Shells",
        json!({"windows": [[-2, -2, 2, 2]]}),
    );
    let mapped = json!({"name": "area", "source": "$outline", "map_to_area": true});
    let selected = select(
        "selected",
        "nw",
        "test:group",
        "dateline",
        json!({"area": "area"}),
    );
    assert_selected(
        vec![reference, mapped, selected],
        &["N10", "N13", "N14", "N9", "W104", "W107"],
    );
}

#[test]
fn around_uses_the_true_literal_center_above_the_mercator_grid() {
    for (point, expected) in [
        ([0.0, 88.9544017], &["R208", "W111"][..]),
        ([130.0, 90.0], &["R209", "W113"][..]),
        ([0.0, 85.0511287], &["W112"][..]),
    ] {
        let selected = select(
            "selected",
            "wr",
            "test:group",
            "polar_arc",
            json!({"around": {"point": point, "meters": 25}}),
        );
        assert_selected(vec![selected], expected);
    }
}

#[test]
fn around_finds_a_way_and_its_route_in_the_dateline_gap() {
    for longitude in [-180.0, 180.0] {
        let selected = select(
            "selected",
            "wr",
            "test:group",
            "seam_path",
            json!({"around": {"point": [longitude, 0.1], "meters": 10}}),
        );
        assert_selected(vec![selected], &["R205", "W109"]);
    }
}

#[test]
fn inclusive_boxes_on_either_side_include_both_seam_encodings() {
    for window in [[179.9, 0.9, 180.0, 1.1], [-180.0, 0.9, -179.9, 1.1]] {
        let selected = select(
            "selected",
            "node",
            "test:group",
            "seam_point",
            json!({"windows": [window]}),
        );
        assert_selected(vec![selected], &["N44", "N45"]);
    }
}
