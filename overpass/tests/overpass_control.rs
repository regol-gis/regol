use regol::Library;
use regol_overpass::{execute, execute_cancellable};
use serde_json::{Value, json};

const GOL: &[u8] = include_bytes!("fixtures/geography.gol");

fn loop_query(selector: &str, initial: Option<&str>, count: u32) -> Value {
    let mut sets = vec![json!({"name":"candidates","source":selector})];
    if let Some(initial) = initial {
        sets.push(json!({"name":"initial","source":initial}));
    }
    sets.push(json!({
        "name":"accepted", "source":"", "control": {
            "source":"candidates", "item":"item", "inputs":{"all":"candidates"},
            "accumulator":"result", "initial": initial.map(|_| "initial"),
            "body":[
                {"type":"nearby","source":"all","reference":"item","meters":25000,"target":"nearby"},
                {"type":"nearby","source":"all","reference":"nearby","meters":25000,"target":"hop"},
                {"type":"if","comparisons":[
                    {"source":"nearby","op":"==","count":count},
                    {"source":"hop","op":"==","count":count}
                ],"body":[
                    {"type":"union","members":["nearby","item"],"target":"combined"},
                    {"type":"copy","source":"combined","target":"copy"},
                    {"type":"accumulate","members":["copy"]},
                    {"type":"accumulate","members":["nearby"]}
                ]}
            ]
        }
    }));
    sets.push(json!({"name":"display","source":"","union":["accepted"]}));
    json!({
        "windows":[[-180,-85,180,85]],"sets":sets, "show":["display"],"output":"geom"
    })
}

fn ids(request: Value) -> Result<Vec<String>, String> {
    let output = execute(
        &Library::new(GOL).unwrap(),
        serde_json::from_value(request).unwrap(),
    )?;
    let output = serde_json::to_value(output).unwrap();
    assert_eq!(output["limited"], Value::Null);
    let mut ids: Vec<_> = output["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|feature| feature["id"].as_str().unwrap().to_owned())
        .collect();
    ids.sort_unstable();
    Ok(ids)
}

#[test]
fn counts_both_neighborhoods_across_disconnected_sets_and_deduplicates_each_append() {
    assert_eq!(
        ids(loop_query("n[test:group]", None, 2)).unwrap(),
        ["N11", "N12", "N44", "N45"]
    );
}

#[test]
fn zero_iterations_and_singletons_preserve_the_initial_accumulator() {
    let empty = "n[test:group=missing]";
    let singleton = "n[test:group=arc_midpoint]";
    assert!(ids(loop_query(empty, None, 1)).unwrap().is_empty());
    assert_eq!(ids(loop_query(empty, Some(singleton), 1)).unwrap(), ["N23"]);
    assert_eq!(ids(loop_query(singleton, None, 1)).unwrap(), ["N23"]);
    assert!(ids(loop_query(singleton, None, 2)).unwrap().is_empty());
}

#[test]
fn validates_node_only_sources_and_cancellation() {
    let result = ids(loop_query("w[test:group=arc]", None, 1));
    assert!(result.unwrap_err().contains("must contain only nodes"));
    let result = execute_cancellable(
        &Library::new(GOL).unwrap(),
        serde_json::from_value(loop_query("n[test:group]", None, 2)).unwrap(),
        &|| Err("cancelled".into()),
    );
    assert_eq!(result.err().as_deref(), Some("cancelled"));
}
