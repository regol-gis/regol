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
