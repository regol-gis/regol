use regol_overpass::{Request, compile_request, compiler::compile};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
struct Case {
    text: String,
    viewport: Option<Vec<[f64; 4]>>,
    plan: Option<Value>,
    error: Option<String>,
}

#[test]
fn compiler_matches_the_live_javascript_oracle() {
    for (i, line) in include_str!("compiler-cases.jsonl").lines().enumerate() {
        let case: Case = serde_json::from_str(line).unwrap();
        let label = format!("case {}\n{}", i + 1, case.text);
        let actual = compile(&case.text, case.viewport.as_deref());
        match (case.plan, case.error) {
            (Some(expected), None) => {
                let actual = actual.unwrap_or_else(|error| panic!("{label}\n{error}"));
                assert!(
                    same_plan(&actual, &expected, false),
                    "{label}\nexpected: {expected}\nactual: {actual}"
                );
                let viewport = case.viewport.as_deref();
                let cli = compile_request(&case.text, viewport)
                    .unwrap_or_else(|error| panic!("{label}\n{error}"));
                let live: Request = serde_json::from_slice(&serde_json::to_vec(&actual).unwrap())
                    .unwrap_or_else(|error| panic!("{label}\n{error}"));
                assert_eq!(cli.timeout(), expected["timeout"].as_u64(), "{label}");
                assert_eq!(live.timeout(), cli.timeout(), "{label}");
            }
            (None, Some(error)) => assert_eq!(actual, Err(error), "{label}"),
            _ => panic!("{label}\ninvalid oracle fixture"),
        }
    }
}

fn same_plan(actual: &Value, expected: &Value, coordinate: bool) -> bool {
    if actual == expected {
        return true;
    }
    match (actual, expected) {
        (Value::Number(actual), Value::Number(expected)) => {
            let both_integers = !actual.is_f64() && !expected.is_f64();
            if both_integers {
                return false;
            }
            let diff = (actual.as_f64().unwrap() - expected.as_f64().unwrap()).abs();
            // JS and Rust can round the trigonometry in a read hint differently.
            diff <= if coordinate { 1e-10 } else { 0.0 }
        }
        (Value::Array(actual), Value::Array(expected)) => {
            actual.len() == expected.len()
                && actual
                    .iter()
                    .zip(expected)
                    .all(|(a, b)| same_plan(a, b, coordinate))
        }
        (Value::Object(actual), Value::Object(expected)) => {
            actual.len() == expected.len()
                && expected.iter().all(|(key, b)| {
                    let coordinate = coordinate
                        || matches!(key.as_str(), "windows" | "polygon" | "point" | "polyline");
                    actual.get(key).is_some_and(|a| same_plan(a, b, coordinate))
                })
        }
        _ => false,
    }
}

#[test]
fn viewport_is_required_only_for_a_bbox_placeholder() {
    let literal = compile(
        r#"// {{bbox}}
        node[name="{{bbox}}"](50,179,52,-179); out count;"#,
        None,
    )
    .unwrap();
    assert!(same_plan(
        &literal["sets"][0]["overpass"]["windows"],
        &json!([[179, 50, 180, 52], [-180, 50, -179, 52]]),
        false,
    ));
    assert_eq!(
        literal["sets"][0]["overpass"]["tags"][0]["value"],
        "{{bbox}}"
    );
    assert_eq!(literal["sets"][0]["overpass"]["bbox"], true);

    let named_area = compile(r#"area[name="East Midlands"]; out count;"#, None).unwrap();
    assert_eq!(named_area["sets"][0]["overpass"]["windows"], json!([]));
    assert_eq!(named_area["sets"][1]["map_to_area"], true);

    for (text, line) in [
        ("node(\n  {{bbox}}); out count;", 2),
        ("[out:json]\n[bbox:{{bbox}}]; node; out count;", 2),
    ] {
        assert_eq!(
            compile(text, None).unwrap_err(),
            format!("Line {line}: Supply --bbox for the {{{{bbox}}}} placeholder.")
        );
    }

    assert_eq!(
        compile("node({{bbox}}); out count;", Some(&[])),
        compile("node({{bbox}}); out count;", None),
    );
    assert!(
        compile("node({{bbox}}); out count;", Some(&[[0., 12., 1., 3.]]))
            .unwrap_err()
            .contains("Invalid viewport")
    );
    let view = [[179.0, 50.0, 180.0, 52.0], [-180.0, 50.0, -179.0, 52.0]];
    let plan = compile("[bbox:{{bbox}}]; node; out count;", Some(&view)).unwrap();
    assert_eq!(plan["sets"][0]["overpass"]["windows"], json!(view));
    let supplied = [[0.0, 0.0, 1.0, 1.0]];
    let query = r#"node[name="café"](50,179,52,-179); out count;"#;
    assert_eq!(compile(query, Some(&supplied)), compile(query, None));
}

#[test]
fn high_precision_spatial_literals_survive_the_live_json_round_trip() {
    let plan = compile("node(around:18.860222312132986,50,0); out count;", None).unwrap();
    let radius = &plan["sets"][0]["overpass"]["around"]["meters"];
    assert_eq!(radius.as_f64(), Some(18.860222312132986));
    let decoded: Value = serde_json::from_slice(&serde_json::to_vec(&plan).unwrap()).unwrap();
    assert_eq!(plan, decoded);
}

#[test]
fn set_and_foreach_blocks_preserve_their_depth_boundaries() {
    let set_query = |depth| {
        format!(
            "{}node(0,0,1,1);{}out count;",
            "(".repeat(depth),
            ");".repeat(depth)
        )
    };
    assert!(compile_request(&set_query(32), None).is_ok());
    assert_eq!(
        compile(&set_query(33), None).unwrap_err(),
        "Line 1: Set block nesting limit reached."
    );

    let foreach_query = |depth| {
        format!(
            "node(0,0,1,1)->.input; foreach.input->.item {{{}(.result; .item;)->.result;{}}} .result out count;",
            "if(item.count(nodes) == 1) {".repeat(depth),
            "}".repeat(depth)
        )
    };
    assert!(compile_request(&foreach_query(15), None).is_ok());
    assert_eq!(
        compile(&foreach_query(16), None).unwrap_err(),
        "Line 1: Foreach block nesting limit reached."
    );
}

#[test]
fn quote_scanning_handles_long_unterminated_strings_and_comment_precedence() {
    assert_eq!(
        compile("node(0,0,1,1); out;\n/*/", None).unwrap_err(),
        "Line 2: unclosed comment."
    );
    assert!(compile("node(0,0,1,1); out; /**/", None).is_ok());
    let quoted = r#"node[name="an escaped \"quote\" with /* punctuation */"](0,0,1,1); out;"#;
    let plan = compile(quoted, None).unwrap();
    assert_eq!(
        plan["sets"][0]["overpass"]["tags"][0]["value"],
        r#"an escaped "quote" with /* punctuation */"#
    );

    let unfinished = format!("node(0,0,1,1)[name=\"{}", r#"\""#.repeat(16_384));
    assert_eq!(
        compile(&unfinished, None).unwrap_err(),
        "Line 1: Unterminated quoted string."
    );
    assert_eq!(
        compile(&format!("{unfinished}\n/*"), None).unwrap_err(),
        "Line 2: unclosed comment."
    );
}
