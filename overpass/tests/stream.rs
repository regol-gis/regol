#![cfg(not(target_arch = "wasm32"))]

use regol::{Feature, FeatureId, Geometry, Library, source::Source};
use regol_overpass::{
    Counts, FeatureStatus, OutputGroup, OutputSink, compile_request, execute_stream,
    execute_stream_parallel,
};
use std::ops::ControlFlow;

#[derive(Default)]
struct FirstFeature {
    groups: Vec<String>,
    ids: Vec<FeatureId>,
}

impl<'a, S: Source> OutputSink<'a, S> for FirstFeature {
    type Prepared = ();
    type Preparation = ();

    fn group(
        &mut self,
        group: OutputGroup<'_>,
        _counts: Option<Counts>,
    ) -> Result<ControlFlow<()>, String> {
        self.groups.push(group.name.to_owned());
        Ok(ControlFlow::Continue(()))
    }

    fn feature(
        &mut self,
        _group: OutputGroup<'_>,
        feature: Feature<'a, S>,
        _geometry: Option<Geometry>,
        _prepared: Option<()>,
    ) -> Result<FeatureStatus, String> {
        self.ids.push(feature.id());
        Ok(FeatureStatus::Stop)
    }
}

#[test]
fn stopping_at_a_feature_skips_the_remainder_and_subsequent_outputs() {
    let lib = Library::new(&include_bytes!("fixtures/monaco-2026-01-01.gol")[..]).unwrap();
    let query = || {
        compile_request(
            "node(43.72,7.407,43.75,7.44)->.places; .places out asc; .places out count;",
            None,
        )
        .unwrap()
    };
    let mut serial = FirstFeature::default();
    execute_stream(&lib, query(), &mut serial, &|| Ok(())).unwrap();
    assert_eq!(serial.groups, ["places"]);
    assert_eq!(serial.ids.len(), 1);

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .unwrap();
    let mut parallel = FirstFeature::default();
    execute_stream_parallel(&lib, query(), &mut parallel, &|| Ok(()), &pool, &|_| ()).unwrap();
    assert_eq!(parallel.groups, serial.groups);
    assert_eq!(parallel.ids, serial.ids);
}

#[test]
fn stored_set_goql_filters_and_step_counts_match_the_indexed_selector() {
    use regol_overpass::{GroupData, execute_groups};
    use serde_json::json;
    let lib = Library::new(&include_bytes!("fixtures/monaco-2026-01-01.gol")[..]).unwrap();
    let window = [7.407, 43.72, 7.44, 43.75];
    let total = |source| {
        let request = json!({"windows":[window], "sets":[{"name":"s","source":source}],
            "show":["s"],"output":"count"});
        let groups =
            execute_groups(&lib, serde_json::from_value(request).unwrap(), &|| Ok(())).unwrap();
        let GroupData::Count(counts) = &groups[0].data else {
            panic!("expected count")
        };
        counts.total
    };
    let all = total("n[amenity]");
    let refined = total("n[amenity=cafe,bar][name]");
    assert!(refined > 0 && refined < all);
    let request = json!({"windows":[window],"sets":[
        {"name":"unused","source":"n"},
        {"name":"places","source":"n[amenity]"},
        {"name":"named","source":"$places","filters":[{"selector":"*[amenity=cafe,bar][name]"}]}
    ],"show":["named"],"output":"count","setStats":true});
    let parse = || serde_json::from_value(request.clone()).unwrap();
    let stats = execute_stream(&lib, parse(), &mut FirstFeature::default(), &|| Ok(())).unwrap();
    assert_eq!(stats.sets.len(), 2);
    let (src, target) = (&stats.sets[0], &stats.sets[1]);
    assert_eq!(
        (src.set_index, src.matches_kept, src.retained),
        (1, all, all)
    );
    assert_eq!(
        (
            target.set_index,
            target.candidates_read,
            target.matches_kept,
            target.retained
        ),
        (2, all, refined, 0)
    );
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .unwrap();
    let parallel = execute_stream_parallel(
        &lib,
        parse(),
        &mut FirstFeature::default(),
        &|| Ok(()),
        &pool,
        &|_| (),
    )
    .unwrap();
    assert_eq!(stats.sets, parallel.sets);

    let mut invalid = request;
    invalid["sets"][0]["filters"] = json!([{"selector":"*[name=]"}]);
    let err = execute_stream(
        &lib,
        serde_json::from_value(invalid).unwrap(),
        &mut FirstFeature::default(),
        &|| Ok(()),
    )
    .err()
    .unwrap();
    assert!(err.contains("$unused: GOQL filter:"), "{err}");
}
