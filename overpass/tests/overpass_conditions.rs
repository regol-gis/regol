use regol::Library;
use regol_overpass::execute;
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
    let output = execute(&gol, serde_json::from_value(plan).unwrap()).unwrap();
    let output = serde_json::to_value(output).unwrap();
    assert_eq!(output["features"][0]["id"], "N9");
    assert_eq!(output["outputs"][1]["counts"]["total"], 0);
}
