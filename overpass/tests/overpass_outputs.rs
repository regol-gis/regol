use regol::{Library, source::Source};
use regol_overpass::{
    Counts, ExecutedGroup, GroupData, OutputDetail, OutputMode, OutputOrder, execute_groups,
};
use serde_json::json;

const GOL: &[u8] = include_bytes!("fixtures/geography.gol");

fn ids<S: Source>(group: &ExecutedGroup<'_, S>) -> Vec<String> {
    let GroupData::Features(features) = &group.data else {
        panic!("expected features");
    };
    features
        .iter()
        .map(|feature| feature.id().to_string())
        .collect()
}

fn counts<'g, S: Source>(group: &'g ExecutedGroup<'_, S>) -> &'g Counts {
    let GroupData::Count(counts) = &group.data else {
        panic!("expected counts");
    };
    counts
}

#[test]
fn mixed_outputs_keep_the_set_at_each_statement_and_allow_repeated_features() {
    let library = Library::new(GOL).unwrap();
    let plan = json!({
        "windows": [[-1,-1,1,1]],
        "sets": [
            {"name": "first", "source": "*", "overpass": {
                "kind": "way",
                "tags": [{"key": "test:group", "op": "eq", "value": "dateline"}],
                "windows": [[-1,-1,1,1]],
            }},
            {"name": "second", "source": "n[\"test:group\"=dateline]"},
        ],
        "outputs": [
            {"after": 1, "source": "first", "name": "a", "mode": "geom", "detail": "body"},
            {"after": 1, "source": "first", "name": "a", "mode": "center", "detail": "skel"},
            {"after": 1, "source": "first", "name": "a", "mode": "count"},
            {"after": 2, "source": "second", "name": "a", "mode": "count"},
            {"after": 2, "source": "first", "name": "a", "mode": "geom"},
        ],
    });
    let groups =
        execute_groups(&library, serde_json::from_value(plan).unwrap(), &|| Ok(())).unwrap();
    let [body, center, ways, nodes, geom] = groups.as_slice() else {
        panic!("expected five output groups");
    };
    assert!(groups.iter().all(|group| group.name == "a"));
    assert_eq!(
        (body.mode, body.detail),
        (OutputMode::Geom, Some(OutputDetail::Body))
    );
    assert_eq!(
        (center.mode, center.detail),
        (OutputMode::Center, Some(OutputDetail::Skel))
    );
    assert_eq!((geom.mode, geom.detail), (OutputMode::Geom, None));
    for group in [body, center, geom] {
        assert_eq!(ids(group), ["W104"]);
    }
    assert_eq!(
        *counts(ways),
        Counts {
            ways: 1,
            total: 1,
            ..Counts::default()
        }
    );
    assert_eq!(
        *counts(nodes),
        Counts {
            nodes: 1,
            total: 1,
            ..Counts::default()
        }
    );
}

#[test]
fn ascending_and_quadtile_outputs_apply_different_orders() {
    let library = Library::new(GOL).unwrap();
    let plan = json!({
        "windows": [[-180,-1,180,1]],
        "sets": [
            {"name": "nodes", "source": "n[\"test:group\"=dateline]"},
        ],
        "outputs": [
            {"after": 1, "source": "nodes", "name": "_", "mode": "geom", "order": "asc"},
            {"after": 1, "source": "nodes", "name": "_", "mode": "geom", "order": "qt"},
        ],
    });
    let groups =
        execute_groups(&library, serde_json::from_value(plan).unwrap(), &|| Ok(())).unwrap();
    let [ascending, quadtile] = groups.as_slice() else {
        panic!("expected two output groups");
    };
    assert_eq!(ascending.order, Some(OutputOrder::Asc));
    assert_eq!(quadtile.order, Some(OutputOrder::Qt));
    let ascending = ids(ascending);
    let quadtile = ids(quadtile);
    assert_eq!(ascending, ["N9", "N10", "N11", "N12", "N13", "N14"]);
    assert_eq!(quadtile.len(), 6);
    assert_eq!(&quadtile[..3], ["N12", "N10", "N14"]);
    assert_ne!(ascending, quadtile);
}

#[test]
fn output_cannot_use_a_future_set_or_count_with_spatial_modifiers() {
    let lib = Library::new(GOL).unwrap();
    for (after, mode, modifiers, diagnostic) in [
        (0, "geom", json!({"order": "qt"}), "Undefined set"),
        (
            1,
            "count",
            json!({"order": "qt"}),
            "without other output modifiers",
        ),
        (
            1,
            "count",
            json!({"detail": "body"}),
            "without other output modifiers",
        ),
    ] {
        let mut output = json!({"after": after, "source": "nodes", "name": "_", "mode": mode});
        output
            .as_object_mut()
            .unwrap()
            .extend(modifiers.as_object().unwrap().clone());
        let plan = json!({
            "windows": [[-1,-1,1,1]],
            "sets": [{"name": "nodes", "source": "n"}],
            "outputs": [output],
        });
        let error = execute_groups(&lib, serde_json::from_value(plan).unwrap(), &|| Ok(()))
            .err()
            .unwrap();
        assert!(error.contains(diagnostic), "{error}");
    }
}
