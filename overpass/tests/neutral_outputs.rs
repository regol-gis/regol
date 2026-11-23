use regol::{Library, Limits};
use regol_overpass::{
    Counts, GroupData, OutputDetail, OutputMode, OutputOrder, compile_request, execute,
    execute_groups, feature_geometry,
};
use serde_json::{Value, json};

const MONACO: &[u8] = include_bytes!("fixtures/monaco-2026-01-01.gol");
const WINDOW: [f64; 4] = [7.407, 43.72, 7.44, 43.75];

#[test]
fn ordered_ids_and_counts_do_not_assemble_output_geometry() {
    let library = Library::with_limits(
        MONACO,
        Limits {
            max_coordinates: 4,
            ..Limits::default()
        },
    )
    .unwrap();
    let wire = json!({
        "windows": [WINDOW],
        "sets": [{"name": "boundary", "source": "a[boundary=administrative][name=Monaco]"}],
        "outputs": [
            {"after": 1, "source": "boundary", "name": "admin", "mode": "geom", "order": "asc"},
            {"after": 1, "source": "boundary", "name": "admin", "mode": "count"},
            {"after": 1, "source": "boundary", "name": "admin", "mode": "center", "detail": "skel", "order": "asc"}
        ]
    });
    let read = || serde_json::from_value(wire.clone()).unwrap();
    let groups = execute_groups(&library, read(), &|| Ok(())).unwrap();
    assert_eq!(groups.len(), 3);
    assert!(groups.iter().all(|group| group.name == "admin"));
    assert_eq!(groups[0].mode, OutputMode::Geom);
    assert_eq!(groups[2].mode, OutputMode::Center);
    assert_eq!(groups[2].detail, Some(OutputDetail::Skel));
    assert_eq!(groups[0].order, Some(OutputOrder::Asc));
    assert_eq!(groups[2].order, Some(OutputOrder::Asc));
    let GroupData::Features(geom) = &groups[0].data else {
        panic!("expected geom features");
    };
    let GroupData::Count(counts) = &groups[1].data else {
        panic!("expected count group");
    };
    let GroupData::Features(center) = &groups[2].data else {
        panic!("expected center features");
    };
    assert_eq!(
        *counts,
        Counts {
            relations: 2,
            total: 2,
            ..Counts::default()
        }
    );
    assert!(geom.windows(2).all(|pair| pair[0].id() < pair[1].id()));
    for (geom, center) in geom.iter().zip(center) {
        assert_eq!(geom.id(), center.id());
        let point = feature_geometry(*center, OutputMode::Center).unwrap();
        assert_eq!(serde_json::to_value(point).unwrap()["type"], "Point");
    }
    let error = execute(&library, read()).err().unwrap();
    assert!(error.contains("resource limit exceeded"));
}

#[test]
fn output_name_and_timeout_survive_the_single_output_plan() {
    let library = Library::new(MONACO).unwrap();
    let prefix = "[timeout:12]; node(43.72,7.407,43.75,7.44)->.monaco;";
    let run = |out| {
        let request = compile_request(&format!("{prefix}.monaco out {out};"), None).unwrap();
        assert_eq!(request.timeout(), Some(12));
        execute_groups(&library, request, &|| Ok(()))
            .unwrap()
            .remove(0)
    };
    let shown = run("geom");
    let counted = run("count");
    assert_eq!(shown.name, "monaco");
    assert_eq!(counted.name, "monaco");
    let GroupData::Features(features) = &shown.data else {
        panic!("expected features");
    };
    let GroupData::Count(counts) = &counted.data else {
        panic!("expected counts");
    };
    assert!(!features.is_empty());
    assert_eq!(counts.nodes, features.len());
    assert_eq!(counts.total, features.len());
}

#[test]
fn neutral_groups_continue_past_the_live_display_cap() {
    let library = Library::new(MONACO).unwrap();
    let sets = json!([{"name": "nodes", "source": "n"}]);
    let count = json!({"after": 1, "source": "nodes", "name": "nodes", "mode": "count"});
    let read = |outputs: Vec<Value>| {
        serde_json::from_value(json!({"windows": [WINDOW], "sets": sets, "outputs": outputs}))
            .unwrap()
    };
    let groups = execute_groups(&library, read(vec![count.clone()]), &|| Ok(())).unwrap();
    let GroupData::Count(counts) = &groups[0].data else {
        panic!("expected counts");
    };
    let nodes = counts.nodes;
    let repeats = 100_000 / nodes + 1;
    let mut outputs = vec![
        json!({"after": 1, "source": "nodes", "name": "nodes", "mode": "center", "detail": "skel"});
        repeats
    ];
    outputs.push(count);
    let live = serde_json::to_value(execute(&library, read(outputs.clone())).unwrap()).unwrap();
    assert_eq!(live["limited"], "feature");
    assert_eq!(live["features"].as_array().unwrap().len(), 100_000);

    let groups = execute_groups(&library, read(outputs), &|| Ok(())).unwrap();
    assert_eq!(groups.len(), repeats + 1);
    assert!(groups[..repeats].iter().all(|group| {
        matches!(&group.data, GroupData::Features(features) if features.len() == nodes)
    }));
    assert!(matches!(&groups[repeats].data, GroupData::Count(counts) if counts.nodes == nodes));
}
