#![doc = include_str!("../README.md")]
pub mod area_index;
pub mod compiler;
pub mod geometry;
pub mod named_area;
mod overpass;
mod program;
pub mod read_hints;
mod relationship;
pub use program::outputs::{
    Counts, ExecutedGroup, ExecutionStats, FeaturePreparation, FeatureStatus, GroupData,
    OutputDetail, OutputGroup, OutputMode, OutputOrder, OutputSink,
};
use program::{Plan, outputs::GroupCollector};

use geometry::GeoGeometry;
use regol::{Bounds, Error, Feature, FeatureId, Library, source::Source};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const MAX_INTERMEDIATE_FEATURES: usize = 250_000;
const MAX_VERTICES: usize = 1_500_000;
const MAX_INTERMEDIATE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Deserialize)]
pub struct Request {
    #[serde(default)]
    windows: Vec<[f64; 4]>,
    #[serde(flatten)]
    plan: Plan,
}

/// Permit sparse reads when decoding a relation's members in a browser.
pub fn with_geometry_reads<S: Source, T>(
    feature: Feature<'_, S>,
    read: impl FnOnce(Feature<'_, S>) -> regol::Result<T>,
) -> regol::Result<T> {
    #[cfg(target_arch = "wasm32")]
    if feature.kind() == regol::FeatureType::Relation {
        return read_hints::with_relation_reads(|| read(feature));
    }
    read(feature)
}

fn usable_geometry<T>(
    id: FeatureId,
    result: regol::Result<T>,
    skipped: &mut HashSet<FeatureId>,
) -> Result<Option<T>, String> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(Error::Geometry(_)) => {
            skipped.insert(id);
            Ok(None)
        }
        Err(error) => Err(format!("{id}: {error}")),
    }
}

/// A snapshot of the current set's scan, before spatial filters and deduplication.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryProgress {
    pub set_index: usize,
    pub candidates_read: usize,
}

/// Execute a request and pass each output to the sink as it is found.
/// Empty request windows use the world as the fallback extent.
pub fn execute_stream<'a, S: Source>(
    lib: &'a Library<S>,
    request: Request,
    sink: &mut impl OutputSink<'a, S>,
    check: &impl Fn() -> Result<(), String>,
) -> Result<ExecutionStats, String> {
    check()?;
    let windows = request.projected_windows()?;
    request.plan.execute(
        lib,
        &windows,
        sink,
        check,
        &program::candidates::Serial,
        &|_| {},
    )
}

/// Execute on a reusable pool. Preparation can run concurrently while output
/// callbacks, failures, and limits are applied in traversal order.
#[cfg(not(target_arch = "wasm32"))]
pub fn execute_stream_parallel<'a, S: Source + Sync>(
    lib: &'a Library<S>,
    request: Request,
    sink: &mut (impl OutputSink<'a, S> + Send),
    check: &(impl Fn() -> Result<(), String> + Sync),
    pool: &rayon::ThreadPool,
    progress: &(impl Fn(QueryProgress) + Sync),
) -> Result<ExecutionStats, String> {
    check()?;
    let windows = request.projected_windows()?;
    pool.install(|| {
        request.plan.execute(
            lib,
            &windows,
            sink,
            check,
            &program::candidates::Parallel,
            progress,
        )
    })
}

/// Resolve a request to ordered feature or count groups without reading output geometry.
pub fn execute_groups<'a, S: Source>(
    lib: &'a Library<S>,
    request: Request,
    check: &impl Fn() -> Result<(), String>,
) -> Result<Vec<ExecutedGroup<'a, S>>, String> {
    let mut collector = GroupCollector(Vec::new());
    execute_stream(lib, request, &mut collector, check)?;
    Ok(collector.0)
}

/// Decode a matched feature's geometry using the requested `out` geometry mode.
pub fn feature_geometry<S: Source>(
    feature: Feature<'_, S>,
    mode: OutputMode,
) -> regol::Result<GeoGeometry> {
    with_geometry_reads(feature, |feature| match mode {
        OutputMode::Geom => feature.geometry().map(|g| GeoGeometry::from_regol(&g)),
        OutputMode::Center => overpass::center(feature),
        OutputMode::Count => Err(Error::Geometry("out count has no feature geometry")),
    })
}

/// Compile Overpass QL into a typed executable request. The optional viewport is
/// used to expand `{{bbox}}` in the query.
pub fn compile_request(text: &str, viewport: Option<&[[f64; 4]]>) -> Result<Request, String> {
    let value = compiler::compile(text, viewport)?;
    let plan = serde_json::from_value(value).map_err(|e| e.to_string())?;
    Ok(Request {
        // The compiler already expanded {{bbox}} into the relevant selections.
        // Empty fallback windows let explicit QL extents span the whole source.
        windows: Vec::new(),
        plan,
    })
}

impl Request {
    /// Fallback query windows in west, south, east, north order.
    pub fn windows(&self) -> &[[f64; 4]] {
        &self.windows
    }

    /// Timeout in seconds, if set by an Overpass `[timeout:…]` directive.
    pub fn timeout(&self) -> Option<u64> {
        self.plan.timeout
    }

    fn projected_windows(&self) -> Result<Vec<Bounds>, String> {
        if self.windows.is_empty() {
            return Ok(vec![Bounds::WORLD]);
        }
        self.windows
            .iter()
            .copied()
            .map(geometry::project_box)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const MONACO: &[u8] = include_bytes!("../tests/fixtures/monaco-2026-01-01.gol");

    fn counts(library: &Library<&[u8]>, plan: Value) -> Result<Counts, String> {
        let mut groups =
            execute_groups(library, serde_json::from_value(plan).unwrap(), &|| Ok(()))?;
        assert_eq!(groups.len(), 1);
        let GroupData::Count(counts) = groups.remove(0).data else {
            panic!("expected counts");
        };
        Ok(counts)
    }

    #[test]
    fn overpass_value_regex_searches_text_and_negates_missing_tags() {
        let lib = Library::new(MONACO).unwrap();
        let window = [7.407, 43.72, 7.44, 43.75];
        let run = |key, test: Value| {
            let mut tag = test;
            tag["key"] = json!(key);
            let request = json!({
                "windows": [window], "show": ["named"], "output": "count",
                "sets": [{"name":"named", "source":"*", "overpass":{
                    "kind":"nwr", "tags":[tag], "windows":[window]
                }}]
            });
            counts(&lib, request).map(|count| count.total)
        };
        let named = run("name", json!({"op":"exists"})).unwrap();
        let unnamed = run("name", json!({"op":"missing"})).unwrap();
        assert!(named > 0 && unnamed > 0);
        assert_eq!(
            run("name", json!({"op":"re", "value":".*"})).unwrap(),
            named
        );
        assert_eq!(
            run("name", json!({"op":"nre", "value":".*"})).unwrap(),
            unnamed
        );
        let monaco = run("name", json!({"op":"eq", "value":"Monaco"})).unwrap();
        assert!(monaco > 0);
        assert!(run("name", json!({"op":"re", "value":"onac"})).unwrap() >= monaco);
        assert_eq!(
            run("name", json!({"op":"re", "value":"^MONACO$"})).unwrap(),
            0
        );
        let insensitive = run(
            "name",
            json!({"op":"re", "value":"^MONACO$", "insensitive":true}),
        )
        .unwrap();
        assert!(insensitive >= monaco);
        assert_eq!(
            run(
                "name",
                json!({"op":"nre", "value":"^MONACO$", "insensitive":true})
            )
            .unwrap(),
            named + unnamed - insensitive
        );
        let invalid = run("name", json!({"op":"re", "value":"[", "line":3})).unwrap_err();
        assert!(invalid.starts_with("Line 3: Unsupported or invalid regular expression"));

        for value in ["no", "yes"] {
            let exact = run("oneway", json!({"op":"eq", "value":value})).unwrap();
            assert!(exact > 0);
            let regex = run("oneway", json!({"op":"re", "value":format!("^{value}$")})).unwrap();
            assert_eq!(regex, exact);
        }
    }

    #[test]
    fn overpass_paired_types_and_count_deduplicate_by_osm_identity() {
        let lib = Library::new(MONACO).unwrap();
        let window = [7.407, 43.72, 7.44, 43.75];
        let run =
            |kinds: &[&str], windows: &[[f64; 4]]| {
                let mut sets: Vec<_> = kinds.iter().enumerate().map(|(i, kind)| json!({
                "name":format!("type{i}"), "source":"*", "overpass":{
                    "kind":kind, "tags":[{"key":"name","op":"exists"}], "windows":windows
                }
            })).collect();
                let output = if sets.len() == 1 {
                    "type0"
                } else {
                    let names: Vec<_> = (0..sets.len()).map(|i| format!("type{i}")).collect();
                    sets.push(json!({"name":"all", "source":"", "union":names}));
                    "all"
                };
                let request =
                    json!({"windows":[window], "sets":sets, "show":[output], "output":"count"});
                counts(&lib, request).unwrap()
            };
        let all = run(&["node", "way", "relation"], &[window, window]);
        assert!(all.nodes > 0 && all.ways > 0 && all.relations > 0);
        for (pair, expected) in [
            ("nw", (all.nodes, all.ways, 0)),
            ("nr", (all.nodes, 0, all.relations)),
            ("wr", (0, all.ways, all.relations)),
            ("nwr", (all.nodes, all.ways, all.relations)),
        ] {
            for windows in [&[window][..], &[window, window][..]] {
                let counts = run(&[pair], windows);
                assert_eq!((counts.nodes, counts.ways, counts.relations), expected);
                assert_eq!(counts.total, expected.0 + expected.1 + expected.2);
            }
        }
        assert_eq!(run(&["nw", "nr", "wr"], &[window, window]).total, all.total);
    }
}
