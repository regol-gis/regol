#![cfg(all(unix, feature = "experimental-tag-sidecars"))]

use regol::{
    Bounds, Query, Result,
    tag_sidecar::{IndexedLibrary, Kind, build},
};
use std::{fs, io::Write, path::Path};

#[test]
fn sidecars_keep_spatial_and_predicate_semantics() -> Result<()> {
    let work = tempfile::tempdir()?;
    let source = Path::new("tests/fixtures/tiled_global.gol");
    let pairs = [
        ("amenity", "cafe"),
        ("amenity", "bench"),
        ("building", "yes"),
        ("name", "Test Road"),
        ("capacity", "12"),
    ]
    .map(|(key, value)| (String::from(key), String::from(value)));
    for kind in [Kind::Exact, Kind::Words] {
        let output = work.path().join(format!("{kind:?}"));
        // SAFETY: The checked-in fixture and freshly built index are immutable.
        let library = unsafe {
            build(source, &output, kind, &pairs)?;
            IndexedLibrary::open(source, Some(&output))?
        };
        let windows = [
            None,
            Some(Bounds::WORLD),
            Some(Bounds::new(i32::MIN, i32::MIN, -1, i32::MAX)?),
            Some(Bounds::new(0, i32::MIN, i32::MAX, i32::MAX)?),
            Some(Bounds::new(-300_000, -300_000, 300_000, 300_000)?),
        ];
        for (selector, eligible) in [
            ("*[amenity=cafe]", true),
            ("n[amenity=cafe,bench]", true),
            ("n[amenity=cafe],a[building=yes]", true),
            ("a[building=yes][name~\".*\"]", true),
            ("*[amenity=cafe][capacity>=12]", true),
            ("*[name='Test Road']", true),
            ("*[capacity='12']", true),
            ("*[capacity=12]", false),
            ("n[amenity=cafe],a[building]", false),
            ("*[amenity!=cafe]", false),
        ] {
            let query = Query::parse(selector)?;
            let prepared = library.prepare(&query)?;
            for bounds in windows {
                assert_eq!(prepared.plan(bounds) != "gol", eligible, "{selector}");
                let ids = |feature: Result<regol::Feature<'_, _>>| feature.map(|f| f.id());
                assert_eq!(
                    prepared.iter(bounds).map(ids).collect::<Result<Vec<_>>>()?,
                    prepared
                        .ordinary_iter(bounds)
                        .map(ids)
                        .collect::<Result<Vec<_>>>()?,
                    "{kind:?} {selector} {bounds:?}",
                );
            }
        }
    }
    Ok(())
}

#[test]
fn sidecar_binding_rejects_copies_and_changed_gol() -> Result<()> {
    let work = tempfile::tempdir()?;
    let original = "tests/fixtures/basic.gol";
    let source = work.path().join("input.gol");
    let copy = work.path().join("copy.gol");
    let output = work.path().join("postings");
    fs::copy(original, &source)?;
    fs::copy(original, &copy)?;
    // SAFETY: No mutation occurs while a mapping exists.
    unsafe {
        build(
            &source,
            &output,
            Kind::Exact,
            &[("amenity".into(), "cafe".into())],
        )?;
        assert!(IndexedLibrary::open(&source, Some(&output)).is_ok());
        assert!(IndexedLibrary::open(&copy, Some(&output)).is_err());
    }
    fs::OpenOptions::new()
        .append(true)
        .open(&source)?
        .write_all(&[0])?;
    // SAFETY: The append above is complete and there is no remaining writer.
    unsafe {
        assert!(IndexedLibrary::open(&source, Some(&output)).is_err());
        assert!(IndexedLibrary::open(&source, None).is_ok());
    }
    Ok(())
}

#[test]
fn exact_sidecars_follow_the_gol_encoding_of_dictionary_keys()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let work = tempfile::tempdir()?;
    let legacy = Path::new("tests/fixtures/stress.gol");
    check_dictionary_key_sidecars(legacy, work.path(), "legacy", &[])?;
    #[cfg(feature = "build")]
    {
        let native = work.path().join("native.gol");
        regol::build::build_pbf("tests/fixtures/stress.osm.pbf", &native)?;
        check_dictionary_key_sidecars(&native, work.path(), "native", &[1])?;
    }
    Ok(())
}

fn check_dictionary_key_sidecars(
    source: &Path,
    work: &Path,
    generation: &str,
    expected: &[u64],
) -> Result<()> {
    // Issue #2. In the GeoDesk 2.0.0 fixture, N1's local key "bench" also
    // occurs in the global dictionary as a common value of "amenity".
    let value = "rare-global-value-as-local-key";
    let source_library = regol::Library::open(source)?;
    let mut raw = Vec::new();
    for feature in source_library.features() {
        let feature = feature?;
        if let Some(stored) = feature.tag("bench")? {
            assert_eq!(stored.to_string(), value);
            raw.push(feature.id().id);
        }
    }
    assert_eq!(raw, [1]);
    let pairs = [("bench".into(), value.into())];
    let query = Query::parse("n[bench=rare-global-value-as-local-key]")?;
    for kind in [Kind::Exact, Kind::Words] {
        let output = work.join(format!("{generation}-{kind:?}"));
        // SAFETY: The GOL and sidecar stay immutable throughout the test.
        let library = unsafe {
            build(source, &output, kind, &pairs)?;
            IndexedLibrary::open(source, Some(&output))?
        };
        let prepared = library.prepare(&query)?;
        for bounds in [None, Some(Bounds::WORLD)] {
            assert_ne!(prepared.plan(bounds), "gol");
            let ids = |feature: Result<regol::Feature<'_, _>>| feature.map(|f| f.id().id);
            assert_eq!(
                prepared.iter(bounds).map(ids).collect::<Result<Vec<_>>>()?,
                expected
            );
            assert_eq!(
                prepared
                    .ordinary_iter(bounds)
                    .map(ids)
                    .collect::<Result<Vec<_>>>()?,
                expected
            );
        }
    }
    Ok(())
}
