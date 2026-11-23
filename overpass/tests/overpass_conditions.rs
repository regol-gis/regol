use regol::Library;
use regol_overpass::{GroupData, execute_groups};
use serde_json::{Value, json};

#[test]
fn feature_conditions_filter_bounded_scans_and_stored_sets() {
    let gol = Library::new(&include_bytes!("fixtures/geography.gol")[..]).unwrap();
    let tag = |key| json!({"op": "tag", "key": key});
    let value = |value| json!({"op": "value", "value": value});
    let binary = |operator, left: Value, right: Value| json!({"op": "binary", "operator": operator, "left": left, "right": right});
    let plan = json!({
        "windows": [[178.55, -0.05, 178.65, 0.05]],
        "sets": [{"name": "from_box", "source": "*", "overpass": {
            "kind": "node", "tags": [], "windows": [[178.55, -0.05, 178.65, 0.05]],
            "conditions": [binary("==", tag("test:group"), value("dateline"))]
        }}, {"name": "selected", "source": "$from_box", "overpass": {
            "kind": "node", "tags": [], "conditions": [
                binary("==", tag("unknown"), value("")),
                binary("&&", json!({"op": "is_number", "arg": value("20 mph")}),
                    binary(">", json!({"op": "number", "arg": value("20 mph")}), value("10")))
            ]
        }}, {"name": "empty", "source": "$selected", "overpass": {
            "kind": "node", "tags": [], "conditions": [
                binary(">", json!({"op": "number", "arg": tag("unknown")}), value("0"))
            ]
        }}],
        "outputs": [{"after": 2, "source": "selected", "name": "selected", "mode": "geom"},
                    {"after": 3, "source": "empty", "name": "empty", "mode": "count"}]
    });
    let groups = execute_groups(&gol, serde_json::from_value(plan).unwrap(), &|| Ok(())).unwrap();
    let [selected, empty] = groups.as_slice() else {
        panic!("expected two output groups");
    };
    let GroupData::Features(features) = &selected.data else {
        panic!("expected selected features");
    };
    let GroupData::Count(counts) = &empty.data else {
        panic!("expected count");
    };
    assert_eq!(features.len(), 1);
    assert_eq!(features[0].id().to_string(), "N9");
    assert_eq!(counts.total, 0);
}
