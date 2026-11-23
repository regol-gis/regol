use regol::Library;
use regol_overpass::execute;
use serde_json::{Value, json};

const STRESS: &[u8] = include_bytes!("fixtures/stress.gol");
const GEOGRAPHY: &[u8] = include_bytes!("fixtures/geography.gol");
const MONACO: &[u8] = include_bytes!("fixtures/monaco-2026-01-01.gol");

fn recurse(data: &[u8], window: [f64; 4], selector: &str) -> Result<Vec<String>, String> {
    let lib = Library::new(data).unwrap();
    let request = json!({
        "windows": [window],
        "sets": [
            {"name": "input", "source": selector},
            {"name": "result", "source": "", "recurse": "input"}
        ],
        "show": ["result"],
        "output": "center"
    });
    let output = execute(&lib, serde_json::from_value(request).unwrap())?;
    let output = serde_json::to_value(output).unwrap();
    let mut ids: Vec<_> = output["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|feature: &Value| feature["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort_unstable();
    Ok(ids)
}

#[test]
fn follows_way_and_direct_relation_members_outside_the_viewport() {
    let window = [-0.00161, 50.99999, -0.00159, 51.00001];
    assert!(
        recurse(STRESS, window, r#"n[name="Grid 1"]"#)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        recurse(STRESS, window, r#"a[name="Area 0"]"#).unwrap(),
        ["N1", "N2", "N33", "N34"]
    );
    assert_eq!(
        recurse(STRESS, window, r#"r[name="Route 0"]"#).unwrap(),
        [
            "N1", "N2", "N3", "N33", "N34", "N35", "N36", "N4", "W2000", "W2001", "W2002"
        ]
    );
}

#[test]
fn does_not_traverse_relation_members_of_relations() {
    assert!(
        recurse(GEOGRAPHY, [-16., 78., 16., 81.], "r[type=collection]")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn reports_geometry_only_vertices_even_if_their_ids_are_retained() {
    assert!(Library::new(GEOGRAPHY).unwrap().has_way_node_ids());
    let error = recurse(GEOGRAPHY, [-16., 79.9, 16., 80.1], "w[test:group=arc]").unwrap_err();
    assert!(
        error.contains("requires node 19 from way 105 to have a feature record"),
        "{error}"
    );
}

#[test]
fn reports_input_relations_with_known_membership_omissions() {
    let error = recurse(
        MONACO,
        [7.39, 43.7, 7.5, 43.79],
        "ar[geodesk:missing_members]",
    )
    .unwrap_err();
    assert!(
        error.contains("requires complete membership for relation"),
        "{error}"
    );
}
