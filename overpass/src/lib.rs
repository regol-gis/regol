//! Compile and evaluate bounded Overpass QL queries on local and range-backed GOL sources.
pub mod area_index;
pub mod compiler;
pub mod geometry;
pub mod named_area;
mod overpass;
mod program;
pub mod read_hints;
mod relationship;
pub use program::live::Output;
pub use program::outputs::{
    Counts, ExecutedGroup, GroupData, OutputDetail, OutputMode, OutputOrder,
};
use program::{Filter, Plan, Set, outputs::Sink};

use geometry::GeoGeometry;
use regol::{Bounds, Error, Feature, FeatureId, Library, source::Source};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const MAX_INTERMEDIATE_FEATURES: usize = 250_000;
const MAX_VERTICES: usize = 1_500_000;
const MAX_INTERMEDIATE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;

pub fn query_limits() -> regol::Limits {
    regol::Limits {
        max_coordinates: 150_000,
        max_tags: 2048,
        max_members: 50_000,
        max_traversal_steps: 8_000_000,
        ..Default::default()
    }
}

#[derive(Deserialize)]
pub struct Request {
    #[serde(default)]
    windows: Vec<[f64; 4]>,
    #[serde(flatten)]
    body: Body,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Body {
    Program(Plan),
    Legacy {
        selector: String,
        spatial: Option<LegacyRelationship>,
    },
}
#[derive(Deserialize)]
struct LegacyRelationship {
    predicate: String,
    geometry: Option<GeoGeometry>,
    selector: Option<String>,
    meters: Option<f64>,
}

// Relation members can live in tiles far outside the current viewport.
fn with_geometry_reads<S: Source, T>(
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

pub fn execute<S: Source>(lib: &Library<S>, request: Request) -> Result<Output, String> {
    execute_cancellable(lib, request, &|| Ok(()))
}

pub fn execute_cancellable<S: Source>(
    lib: &Library<S>,
    request: Request,
    check: &impl Fn() -> Result<(), String>,
) -> Result<Output, String> {
    execute_with(lib, request, check, &program::candidates::Serial, &|_| {})
}

/// A snapshot of the current set's scan, before spatial filters and deduplication.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryProgress {
    pub set_index: usize,
    pub candidates_read: usize,
}

#[cfg(not(target_arch = "wasm32"))]
/// Run a Live query on a reusable CPU pool. Candidate work can run concurrently;
/// outputs, failures and budgets are committed in the original traversal order.
pub fn execute_parallel<S: Source + Sync>(
    lib: &Library<S>,
    request: Request,
    check: &(impl Fn() -> Result<(), String> + Sync),
    pool: &rayon::ThreadPool,
) -> Result<Output, String> {
    execute_parallel_with_progress(lib, request, check, pool, &|_| {})
}

#[cfg(not(target_arch = "wasm32"))]
/// Reports the active set when it starts and samples its candidate count during iteration.
pub fn execute_parallel_with_progress<S: Source + Sync>(
    lib: &Library<S>,
    request: Request,
    check: &(impl Fn() -> Result<(), String> + Sync),
    pool: &rayon::ThreadPool,
    progress: &(impl Fn(QueryProgress) + Sync),
) -> Result<Output, String> {
    pool.install(|| {
        execute_with(
            lib,
            request,
            check,
            &program::candidates::Parallel,
            progress,
        )
    })
}

fn execute_with<S: Source, C: Fn() -> Result<(), String>>(
    lib: &Library<S>,
    request: Request,
    check: &C,
    executor: &impl program::candidates::Candidates<S, C>,
    progress: &impl Fn(QueryProgress),
) -> Result<Output, String> {
    check()?;
    let (plan, windows, legacy_overlay) = request.prepare(false)?;
    let mut sink = Sink::Live(program::live::LiveOutput::new(legacy_overlay));
    plan.execute(lib, &windows, &mut sink, check, executor, progress)?;
    let Sink::Live(live) = sink else {
        unreachable!()
    };
    Ok(live.into_output())
}

/// Resolve an already compiled request to ordered feature or count groups.
/// A request without viewport windows uses the world as its fallback extent.
pub fn execute_groups<'a, S: Source>(
    lib: &'a Library<S>,
    request: Request,
    check: &impl Fn() -> Result<(), String>,
) -> Result<Vec<ExecutedGroup<'a, S>>, String> {
    check()?;
    let (plan, windows, _) = request.prepare(true)?;
    let mut sink = Sink::groups(&plan);
    plan.execute(
        lib,
        &windows,
        &mut sink,
        check,
        &program::candidates::Serial,
        &|_| {},
    )?;
    let Sink::Groups(groups) = sink else {
        unreachable!()
    };
    Ok(groups)
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
        windows: viewport.unwrap_or_default().to_vec(),
        body: Body::Program(plan),
    })
}

impl Request {
    /// Timeout in seconds, if set by an Overpass `[timeout:…]` directive.
    pub fn timeout(&self) -> Option<u64> {
        match &self.body {
            Body::Program(plan) => plan.timeout,
            Body::Legacy { .. } => None,
        }
    }

    fn prepare(self, allow_world: bool) -> Result<(Plan, Vec<Bounds>, bool), String> {
        if (!allow_world && self.windows.is_empty()) || self.windows.len() > 2 {
            return Err("A query needs a viewport.".into());
        }
        let mut windows = self
            .windows
            .into_iter()
            .map(geometry::project_box)
            .collect::<Result<Vec<_>, String>>()?;
        if windows.is_empty() {
            windows.push(Bounds::WORLD);
        }
        let (plan, legacy_overlay) = match self.body {
            Body::Program(plan) => (plan, false),
            Body::Legacy { selector, spatial } => {
                let mut sets = Vec::new();
                let mut filters = Vec::new();
                let mut geometry = None;
                if let Some(s) = spatial {
                    let reference = match (s.selector, s.geometry) {
                        (Some(source), None) => {
                            sets.push(Set::new("reference", source));
                            "reference"
                        }
                        (None, Some(g)) => {
                            geometry = Some(g);
                            "geometry"
                        }
                        _ => {
                            return Err(
                                "Supply one reference geometry or reference GOQL selector.".into(),
                            );
                        }
                    };
                    filters.push(Filter {
                        predicate: s.predicate,
                        reference: reference.into(),
                        meters: s.meters,
                    });
                }
                let overlay = !sets.is_empty();
                sets.push(Set {
                    filters,
                    ..Set::new("result", selector)
                });
                (
                    Plan {
                        sets,
                        show: vec!["result".into()],
                        geometry,
                        output: Default::default(),
                        output_name: None,
                        outputs: Vec::new(),
                        timeout: None,
                    },
                    overlay,
                )
            }
        };
        Ok((plan, windows, legacy_overlay))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::geographic_point;
    use regol::Bounds;
    use regol::{
        Point, Query, SpatialFilter, SpatialPredicate,
        units::{Length, length::meter},
    };
    use serde_json::{Value, json};

    // Upstream's synthetic conformance fixture includes a point on a polygon boundary.
    const GOL: &[u8] = include_bytes!("../tests/fixtures/stress.gol");
    const MONACO: &[u8] = include_bytes!("../tests/fixtures/monaco-2026-01-01.gol");

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn parallel_results_match_serial_order_across_batches_and_repeated_windows() {
        let lib = Library::new(MONACO).unwrap();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();
        let window = [7.407, 43.72, 7.44, 43.75];
        let request = json!({
            "windows": [window, window],
            "sets": [
                {"name":"parks","source":"a[leisure=park,garden]"},
                {"name":"inparks","source":"nwa","filters":[{"predicate":"intersects","reference":"parks"}]},
                {"name":"copy","source":"$inparks"},
                {"name":"others","source":"nwa"}
            ],
            "show":["inparks", "copy", "others"]
        });
        let parse = || serde_json::from_value(request.clone()).unwrap();
        let serial = execute(&lib, parse()).unwrap();
        assert!(serial.features.len() > 1024 && serial.skipped_geometries > 0);
        let updates = std::sync::Mutex::new(Vec::new());
        let parallel = execute_parallel_with_progress(&lib, parse(), &|| Ok(()), &pool, &|scan| {
            updates
                .lock()
                .unwrap()
                .push((scan.set_index, scan.candidates_read));
        })
        .unwrap();
        let updates = updates.into_inner().unwrap();
        assert_eq!(
            updates
                .iter()
                .filter(|(_, count)| *count == 0)
                .map(|(index, _)| *index)
                .collect::<Vec<_>>(),
            [0, 1, 2, 3],
        );
        assert!(updates.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(updates.last().unwrap().1 >= 1024);
        assert_eq!(
            serde_json::to_vec(&serial).unwrap(),
            serde_json::to_vec(&parallel).unwrap()
        );
    }

    fn coordinates(p: Point) -> [f64; 2] {
        let pos = p.lon_lat();
        use regol::units::angle::degree;
        [pos.longitude.get::<degree>(), pos.latitude.get::<degree>()]
    }

    fn predicates() -> [(&'static str, SpatialPredicate); 9] {
        [
            ("intersects", SpatialPredicate::Intersects),
            ("contains", SpatialPredicate::Contains),
            ("within", SpatialPredicate::Within),
            ("covered_by", SpatialPredicate::CoveredBy),
            ("crosses", SpatialPredicate::Crosses),
            ("touches", SpatialPredicate::Touches),
            ("overlaps", SpatialPredicate::Overlaps),
            ("disjoint", SpatialPredicate::Disjoint),
            (
                "within_distance",
                SpatialPredicate::WithinDistance(Length::new::<meter>(30.0)),
            ),
        ]
    }

    fn feature_ids(output: &Output) -> HashSet<String> {
        output
            .features
            .iter()
            .map(|f| {
                serde_json::from_str::<Value>(f.get()).unwrap()["id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
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
            execute(&lib, serde_json::from_value(request).unwrap())
                .map(|out| out.counts.unwrap().total)
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
                let out = execute(&lib, serde_json::from_value(request).unwrap()).unwrap();
                assert!(out.features.is_empty());
                assert!(out.limited.is_none());
                out.counts.unwrap()
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

    #[test]
    fn bounding_box_centers_and_counts_work_without_assembling_geometries() {
        let lib = Library::with_limits(
            MONACO,
            regol::Limits {
                max_coordinates: 4,
                ..Default::default()
            },
        )
        .unwrap();
        let window = [7.407, 43.72, 7.44, 43.75];
        let selector = "a[boundary=administrative][name=Monaco]";
        let run = |output| {
            let request = json!({
                "windows":[window], "sets":[{"name":"boundary", "source":selector}],
                "show":["boundary"], "output":output
            });
            execute(&lib, serde_json::from_value(request).unwrap())
        };
        assert!(
            run("geom")
                .err()
                .unwrap()
                .contains("resource limit exceeded")
        );
        let counted = run("count").unwrap();
        assert_eq!(counted.counts.unwrap().total, 2);
        assert!(counted.features.is_empty());
        assert_eq!(counted.skipped_geometries, 0);

        let centered = run("center").unwrap();
        assert_eq!(centered.features.len(), 2);
        let expected: std::collections::HashMap<_, _> = lib
            .select_in(
                &Query::parse(selector).unwrap(),
                geometry::project_box(window).unwrap(),
            )
            .map(|f| {
                let f = f.unwrap();
                let bounds = f.bounds().unwrap();
                let [w, s] = coordinates(bounds.min);
                let [e, n] = coordinates(bounds.max);
                (f.id().to_string(), [(w + e) / 2.0, (s + n) / 2.0])
            })
            .collect();
        for feature in centered.features {
            let feature: Value = serde_json::from_str(feature.get()).unwrap();
            assert_eq!(feature["geometry"]["type"], "Point");
            assert_eq!(feature["tags"]["name"], "Monaco");
            let expected = expected[feature["id"].as_str().unwrap()];
            for (i, expected) in expected.iter().enumerate() {
                assert!(
                    (feature["geometry"]["coordinates"][i].as_f64().unwrap() - expected).abs()
                        < 1e-8
                );
            }
        }
    }

    #[test]
    fn chained_sets_use_filtered_members_and_show_only_the_requested_union() {
        let lib = Library::new(MONACO).unwrap();
        let windows = json!([[7.407, 43.72, 7.44, 43.75]]);
        let run = |mut request: Value| {
            request["windows"] = windows.clone();
            execute(&lib, serde_json::from_value(request).unwrap()).unwrap()
        };
        let parks = "a[leisure=park,garden]";
        let benches = "n[amenity=bench]";
        let cafes = "na[amenity=cafe]";
        let park_benches =
            run(json!({"selector":benches, "spatial":{"selector":parks,"predicate":"within"}}));
        assert!(!park_benches.features.is_empty());
        let geometries: Vec<_> = park_benches
            .features
            .iter()
            .map(|f| serde_json::from_str::<Value>(f.get()).unwrap()["geometry"].clone())
            .collect();
        let near = run(
            json!({"selector":cafes,"spatial":{"geometry":{"type":"GeometryCollection","geometries":geometries},"predicate":"within_distance","meters":150}}),
        );
        let expected = feature_ids(&near);
        assert!(!expected.is_empty());
        assert!(expected.len() < run(json!({"selector":cafes})).features.len());
        let sets = json!([
            {"name":"parks","source":parks},
            {"name":"benches","source":benches,"filters":[{"predicate":"within","reference":"parks"}]},
            {"name":"cafes","source":cafes,"filters":[{"predicate":"within_distance","reference":"benches","meters":150}]},
            {"name":"also","source":"$cafes"}
        ]);
        let only = run(json!({"sets":sets,"show":["cafes"]}));
        assert_eq!(feature_ids(&only), expected);
        assert!(only.references.is_none());
        let both = run(json!({"sets":sets,"show":["benches","also","cafes"]}));
        let union: HashSet<_> = expected
            .union(&feature_ids(&park_benches))
            .cloned()
            .collect();
        assert_eq!(feature_ids(&both), union);
        assert_eq!(both.features.len(), union.len());
    }

    #[test]
    fn incomplete_boundaries_are_counted_once_across_windows_and_selectors() {
        let lib = Library::new(MONACO).unwrap();
        let window = [7.407, 43.72, 7.44, 43.75];
        let valid = "a[boundary=administrative][name=Monaco]";
        let mixed = "a[boundary=administrative][name=France,Monaco]";
        let run = |selector: &str, spatial: Value| {
            let request =
                json!({"selector": selector, "windows": [window, window], "spatial": spatial});
            execute(&lib, serde_json::from_value(request).unwrap()).unwrap()
        };
        let expected = feature_ids(&run(valid, Value::Null));
        assert_eq!(expected.len(), 2);
        // The France relation in the extract has an unclosed polygon chain.
        let plain = run(mixed, Value::Null);
        assert_eq!(feature_ids(&plain), expected);
        assert_eq!((plain.skipped_geometries, plain.skipped_references), (1, 0));
        let both = run(mixed, json!({"predicate": "intersects", "selector": mixed}));
        assert_eq!(feature_ids(&both), expected);
        assert_eq!(both.references.unwrap().len(), 2);
        assert_eq!((both.skipped_geometries, both.skipped_references), (1, 1));
        let invalid = "a[boundary=administrative][name=France]";
        for predicate in ["intersects", "disjoint"] {
            let out = run(valid, json!({"predicate": predicate, "selector": invalid}));
            assert_eq!(
                out.features.len(),
                if predicate == "disjoint" { 2 } else { 0 }
            );
            assert!(out.references.unwrap().is_empty());
            assert_eq!((out.skipped_geometries, out.skipped_references), (1, 1));
        }
    }

    #[test]
    fn invalid_topology_is_skipped_during_matching_and_reference_preparation() {
        let lib = Library::new(MONACO).unwrap();
        let window = [7.407, 43.72, 7.44, 43.75];
        let run = |selector: &str, reference: &str| {
            let request = json!({"selector": selector, "windows": [window, window],
                "spatial": {"predicate": "within", "selector": reference}});
            execute(&lib, serde_json::from_value(request).unwrap()).unwrap()
        };
        let monaco = "a[boundary=administrative][name=Monaco]";
        // The museum relation assembles, but two polygon components share an edge.
        let mixed = format!(r#"{monaco},r[name="Nouveau Musée National de Monaco"]"#);
        let expected = feature_ids(&run(monaco, monaco));
        assert_eq!(expected.len(), 2);
        let candidates = run(&mixed, monaco);
        assert_eq!(feature_ids(&candidates), expected);
        assert_eq!(
            (candidates.skipped_geometries, candidates.skipped_references),
            (1, 0)
        );
        let references = run(monaco, &mixed);
        assert_eq!(feature_ids(&references), expected);
        assert_eq!(references.references.unwrap().len(), 2);
        assert_eq!(
            (references.skipped_geometries, references.skipped_references),
            (1, 1)
        );
    }

    #[test]
    fn a_geometry_read_hitting_a_resource_limit_still_fails_the_query() {
        let lib = Library::with_limits(
            MONACO,
            regol::Limits {
                max_coordinates: 4,
                ..Default::default()
            },
        )
        .unwrap();
        let selector = "a[boundary=administrative][name=Monaco]";
        for spatial in [
            Value::Null,
            json!({"predicate": "within", "selector": selector}),
        ] {
            let request = json!({"selector": selector, "windows": [[7.407, 43.72, 7.44, 43.75]], "spatial": spatial});
            let error = execute(&lib, serde_json::from_value(request).unwrap())
                .err()
                .unwrap();
            assert!(error.contains("resource limit exceeded"), "{error}");
        }
    }

    #[test]
    fn web_predicates_match_native_feature_filters() {
        let lib = Library::new(GOL).unwrap();
        let operand = lib
            .select(&Query::parse(r#"a[name="Area 81"]"#).unwrap())
            .next()
            .unwrap()
            .unwrap();
        let geometry = GeoGeometry::from_regol(&operand.geometry().unwrap());
        let bounds = operand.bounds().unwrap();
        let [min_x, min_y] = coordinates(bounds.min);
        let [max_x, max_y] = coordinates(bounds.max);
        let window = [min_x - 0.002, min_y - 0.002, max_x + 0.002, max_y + 0.002];
        let bbox = Bounds::from_lon_lat(
            geographic_point([window[0], window[1]]).unwrap(),
            geographic_point([window[2], window[3]]).unwrap(),
        )
        .unwrap();
        let query = Query::parse("nwa").unwrap();
        for (wire, native) in predicates() {
            let request = serde_json::from_value(
                json!({"selector":"nwa", "windows":[window,window], "spatial":{
                    "predicate":wire,"meters":30.0,"geometry":geometry
                }}),
            )
            .unwrap();
            let out = execute(&lib, request).unwrap();
            let expected_filter = SpatialFilter::from_feature(native, operand).unwrap();
            let expected: HashSet<_> = lib
                .select_in(&query, bbox)
                .map(Result::unwrap)
                .filter(|&f| expected_filter.matches(f).unwrap())
                .map(|f| f.id().to_string())
                .collect();
            let actual = feature_ids(&out);
            assert_eq!(actual.len(), out.features.len(), "duplicate IDs for {wire}");
            assert_eq!(actual, expected, "{wire}");
        }
    }

    #[test]
    fn reference_queries_match_individual_native_operands() {
        let lib = Library::new(GOL).unwrap();
        let window = [-0.00166, 51.00020, -0.00128, 51.00061];
        let bbox = Bounds::from_lon_lat(
            geographic_point([window[0], window[1]]).unwrap(),
            geographic_point([window[2], window[3]]).unwrap(),
        )
        .unwrap();
        let refs: Vec<_> = lib
            .select_in(&Query::parse("a[building]").unwrap(), bbox)
            .map(Result::unwrap)
            .collect();
        assert!(refs.len() > 1);
        for (wire, predicate) in predicates() {
            let request = serde_json::from_value(json!({
                "selector": "nwa", "windows": [window, window],
                "spatial": {"selector":"a[building]", "predicate":wire, "meters":30}
            }))
            .unwrap();
            let out = execute(&lib, request).unwrap();
            assert_eq!(out.references.as_ref().unwrap().len(), refs.len());
            let filters: Vec<_> = refs
                .iter()
                .map(|&f| SpatialFilter::from_feature(predicate, f).unwrap())
                .collect();
            let expected = lib
                .select_in(&Query::parse("nwa").unwrap(), bbox)
                .map(Result::unwrap)
                .filter(|&f| {
                    if predicate == SpatialPredicate::Disjoint {
                        filters.iter().all(|filter| filter.matches(f).unwrap())
                    } else {
                        filters.iter().any(|filter| filter.matches(f).unwrap())
                    }
                })
                .map(|f| f.id().to_string())
                .collect();
            let actual = feature_ids(&out);
            assert_eq!(out.features.len(), actual.len());
            assert_eq!(actual, expected, "{wire}");
        }
    }
}
