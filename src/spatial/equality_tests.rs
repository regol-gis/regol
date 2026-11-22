use super::*;
use crate::{FeatureId, Limits, Query};
use std::io;

struct Positioned<'a>(&'a [u8], usize, bool);
impl Source for Positioned<'_> {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }

    fn read_exact_at(&self, p: u64, output: &mut [u8]) -> io::Result<()> {
        self.0.read_exact_at(p, output)
    }
    fn retained_tail(&self, p: u64) -> Option<&[u8]> {
        if self.2 {
            self.0.get(usize::try_from(p).ok()?..)
        } else {
            None
        }
    }
    fn segment(&self, p: u64) -> io::Result<Option<(u64, &[u8])>> {
        if self.1 == 0 {
            return Ok(None);
        }
        let offset = p as usize / self.1 * self.1;
        let end = (offset + self.1).min(self.0.len());
        let segment = self
            .0
            .get(offset..end)
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        Ok(Some((offset as u64, segment)))
    }
}

type Trace = Vec<(u64, Option<std::result::Result<FeatureId, String>>)>;

fn walk<S: Source>(
    lib: &Library<S>,
    query: &Query,
    tile: TileExtent,
    node: bool,
    bounds: Option<Bounds>,
    pause: u64,
) -> Result<Trace> {
    let prepared = query.prepare(lib)?;
    let mut scan = Features::new(lib, bounds);
    scan.tile = tile;
    scan.work = vec![Work::Leaf {
        p: tile.start(),
        node,
    }];
    let mut trace = Vec::new();
    let mut callbacks = 0;
    while !scan.done {
        let next = prepared
            .next_in(&mut scan, || {
                callbacks += 1;
                Ok(callbacks != pause)
            })
            .map(|feature| feature.map(Feature::id).map_err(|e| e.to_string()));
        trace.push((callbacks, next));
    }
    Ok(trace)
}

#[test]
fn required_pair_preserves_validation_of_the_first_value() {
    let pair = DictionaryPair {
        first: 2,
        second: 4,
    };
    let rejects =
        |bytes: &[u8], limit| matches!(pair.probe(bytes, 8, limit, 0), EqualityProbe::Reject);
    let terminal = 0x8001u32.to_le_bytes();
    let mut entries = ((2u32 << 2) | 1 | (3 << 16)).to_le_bytes().to_vec();
    entries.extend_from_slice(&terminal);
    assert!(rejects(&entries, 2));
    assert!(!rejects(&entries, 1)); // The second lookup would exceed its limit.
    entries[2..4].copy_from_slice(&8u16.to_le_bytes());
    assert!(!rejects(&entries, 2)); // Undefined global string for the first key.
    assert!(!rejects(&(0x8000u32 | (2 << 2) | 3).to_le_bytes(), 1));
    let wide = (0x8000u32 | (2 << 2) | 2).to_le_bytes();
    assert!(!rejects(&wide, 1));
    assert!(rejects(&[wide.as_slice(), &[0, 0]].concat(), 1));
    assert!(rejects(&(0x8000u32 | (4 << 2)).to_le_bytes(), 1));
}

#[test]
fn equality_probe_respects_entry_widths_and_limits() {
    let test = DictionaryEquality { key: 5, value: 7 };
    let mut tags = Vec::new();
    tags.extend_from_slice(&((1u32 << 2) | 2).to_le_bytes()); // wide numeric
    tags.extend_from_slice(&[0, 0]);
    tags.extend_from_slice(&(2u32 << 2).to_le_bytes()); // narrow numeric
    tags.extend_from_slice(&((5u32 << 2) | 1 | (8 << 16)).to_le_bytes());
    let rejects = |tags: &[u8], strings, limit| {
        matches!(test.probe(tags, strings, limit, 0), EqualityProbe::Reject)
    };
    for limit in 0..3 {
        assert!(!rejects(&tags, 10, limit));
    }
    assert!(rejects(&tags, 10, 3)); // global value settles the key
    assert!(!rejects(&tags[..13], 10, 3));
    assert!(!rejects(&tags, 8, 3)); // undefined value code
    tags[10..].copy_from_slice(&((5u32 << 2) | 1 | (7 << 16)).to_le_bytes());
    assert!(matches!(test.probe(&tags, 10, 3, 0), EqualityProbe::Match));
    assert!(matches!(test.probe(&tags, 7, 3, 0), EqualityProbe::Unknown));
    assert!(matches!(
        test.probe(&tags, 10, 2, 0),
        EqualityProbe::Unknown
    ));
    tags[10..].copy_from_slice(&0x8001u32.to_le_bytes());
    assert!(rejects(&tags, 10, 3));
    tags[10..].copy_from_slice(&(3u32 << 2).to_le_bytes());
    for _ in 3..DictionaryEquality::TAG_PROBE_LIMIT {
        tags.extend_from_slice(&(3u32 << 2).to_le_bytes());
    }
    tags.extend_from_slice(&0x8001u32.to_le_bytes());
    assert!(!rejects(&tags, 10, DictionaryEquality::TAG_PROBE_LIMIT + 1)); // speculative scan stops at TAG_PROBE_LIMIT
}

#[test]
fn equality_probe_checks_the_whole_word_at_a_tile_offset() {
    let probe = DictionaryEquality { key: 5, value: 7 };
    let mut tile = vec![0; 6];
    tile.extend_from_slice(&((7u32 << 16) | 0x8015).to_le_bytes());
    assert!(matches!(probe.probe(&tile, 8, 1, 6), EqualityProbe::Match));
    for offset in [7, u32::MAX as usize, usize::MAX - 1] {
        assert!(matches!(
            probe.probe(&tile, 8, 1, offset),
            EqualityProbe::Unknown
        ));
    }
}

#[test]
fn truth_probe_checks_numeric_width_and_preserves_local_lookup() {
    let probe = DictionarySet {
        key: 5,
        values: &[0b0111],
        numbers_match: true,
        local_match: None,
    };
    let entry = |flags: u32, value: u16| {
        (0x8000 | (5 << 2) | flags | (u32::from(value) << 16)).to_le_bytes()
    };
    let full = |flags| [entry(flags, 0).as_slice(), &[0, 0]].concat();
    let inspect = |probe: DictionarySet<'_>, bytes: &[u8]| probe.probe(bytes, 4, 1, 0);
    assert!(matches!(inspect(probe, &entry(0, 0)), EqualityProbe::Match));
    assert!(matches!(
        inspect(probe, &entry(1, 3)),
        EqualityProbe::Reject
    ));
    assert!(matches!(
        inspect(probe, &entry(1, 4)),
        EqualityProbe::Unknown
    ));
    assert!(matches!(
        inspect(probe, &entry(2, 0)),
        EqualityProbe::Unknown
    ));
    assert!(matches!(inspect(probe, &full(2)), EqualityProbe::Match));
    assert!(matches!(inspect(probe, &full(3)), EqualityProbe::Unknown));
    let known_local = DictionarySet {
        local_match: Some(true),
        ..probe
    };
    assert!(matches!(
        inspect(known_local, &full(3)),
        EqualityProbe::Match
    ));
    assert!(matches!(
        inspect(known_local, &entry(3, 0)),
        EqualityProbe::Unknown
    ));
}

#[test]
fn dictionary_set_checks_all_codes_and_keeps_local_text_for_mixed_sets() {
    let probe = DictionarySet {
        key: 5,
        values: &[1 << 7, 1 << 6],
        numbers_match: false,
        local_match: None,
    };
    let entry = |key: u32, mode: u32, value: u16| {
        ((key << 2) | mode | (u32::from(value) << 16)).to_le_bytes()
    };
    for code in [7, 70] {
        assert!(matches!(
            probe.probe(&entry(5, 1, code), 71, 1, 0),
            EqualityProbe::Match
        ));
    }
    assert!(matches!(
        probe.probe(&entry(5, 1, 71), 71, 1, 0),
        EqualityProbe::Unknown
    ));
    let local = [entry(5, 3, 0).as_slice(), &[0xff, 0xff]].concat();
    let global_only = DictionarySet {
        local_match: Some(false),
        ..probe
    };
    assert!(matches!(
        probe.probe(&local, 71, 1, 0),
        EqualityProbe::Unknown
    ));
    assert!(matches!(
        global_only.probe(&local, 71, 1, 0),
        EqualityProbe::Reject
    ));
    assert!(matches!(
        global_only.probe(&local[..5], 71, 1, 0),
        EqualityProbe::Unknown
    ));
    assert!(matches!(
        probe.probe(&entry(6, 1, 0), 71, 1, 0),
        EqualityProbe::Reject
    ));
    assert!(matches!(
        probe.probe(&entry(5, 1, 70), 71, 0, 0),
        EqualityProbe::Unknown
    ));
}
#[test]
fn equality_runs_preserve_step_boundaries_and_ordered_validation() -> Result<()> {
    let mut bytes = include_bytes!("../../tests/fixtures/global.gol").to_vec();
    let lib = Library::new(bytes.as_slice())?;
    let code = |name| {
        (0..lib.string_count as u16)
            .find(|&i| lib.global_string(i).unwrap() == name)
            .unwrap()
    };
    let amenity = code("amenity");
    let value_code = code("highway");
    let base = bytes.len().next_multiple_of(4096);
    bytes.resize(base + 256, 0);
    let bounds = Bounds::new(-1, -1, 1, 1)?;
    let queries = [
        Query::parse("*[amenity=highway]")?,
        Query::parse("*[amenity=highway,building]")?,
    ];

    for node in [true, false] {
        let anchor = if node { 8 } else { 16 };
        let offsets = if node {
            [0, 20, 44, 64]
        } else {
            [0, 32, 64, 96]
        };
        let mut valid = bytes.clone();
        for (i, p) in offsets.into_iter().enumerate() {
            let flags = if node { (u64::from(i == 1)) << 2 } else { 8 };
            let header = ((i as u64 + 1) << 12) | flags | u64::from(i == 3);
            let ptr = (184 + i * 16 - (p + anchor + 8)) as i32;
            let value = if i % 2 == 0 { 0 } else { value_code };
            let entry = (u32::from(amenity) << 2) | 0x8001 | (u32::from(value) << 16);
            valid[base + p + anchor..][..8].copy_from_slice(&header.to_le_bytes());
            valid[base + p + anchor + 8..][..4].copy_from_slice(&ptr.to_le_bytes());
            valid[base + 184 + i * 16..][..4].copy_from_slice(&entry.to_le_bytes());
        }
        for damage in 0..=14 {
            let mut fixture = valid.clone();
            let second = base + offsets[1];
            let ptr_at = second + anchor + 8;
            let end = match damage {
                1 => {
                    fixture[base + 202..][..2].copy_from_slice(&u16::MAX.to_le_bytes());
                    base + 256
                }
                2 => {
                    fixture[ptr_at..][..4].copy_from_slice(&i32::MIN.to_le_bytes());
                    base + 256
                }
                3 => {
                    fixture[second + anchor] |= 24;
                    base + 256
                }
                4 => {
                    fixture[second..][..4].copy_from_slice(&2i32.to_le_bytes());
                    base + 256
                }
                5 => base + offsets[3] + 3,
                6 | 8..=11 => {
                    let ptr = (200 - (offsets[1] + anchor + 8)) as i32 | 1;
                    fixture[ptr_at..][..4].copy_from_slice(&ptr.to_le_bytes());
                    fixture[base + 200..][..4].copy_from_slice(&0x8001u32.to_le_bytes());
                    let local_ptr = (-24i32 << 1) | 5;
                    fixture[base + 196..][..4].copy_from_slice(&local_ptr.to_le_bytes());
                    fixture[base + 194..][..2].copy_from_slice(&value_code.to_le_bytes());
                    let name: &[u8] = match damage {
                        8 => b"\x04name",
                        9 => &[1, 0xff],
                        10 => &[0xfe, 0xff, 0x40],
                        11 => &[2, 0xc3, 0xa9],
                        _ => b"\x07amenity",
                    };
                    fixture[base + 176..][..name.len()].copy_from_slice(name);
                    base + 256
                }
                // The retained page contains the whole tag word but the tile ends halfway through it.
                7 => base + 186,
                12..=14 => {
                    // Exact global literals ignore local text under the common key.
                    let field = (amenity << 2) | 0x8003;
                    let pointer = base + 202;
                    let string = base + 240;
                    let delta = if damage == 14 {
                        i32::MIN
                    } else {
                        (string - pointer) as i32
                    };
                    let value = if damage == 12 {
                        b"\x07highway"
                    } else {
                        b"\x07highwa\xff"
                    };
                    fixture[base + 200..][..2].copy_from_slice(&field.to_le_bytes());
                    fixture[pointer..][..4].copy_from_slice(&delta.to_le_bytes());
                    fixture[string..][..value.len()].copy_from_slice(value);
                    base + 256
                }
                _ => base + 256,
            };
            let tile = TileExtent::new(base as u64, end as u64)?;
            if matches!(damage, 6 | 8..=11) {
                for max_tags in 1..=2 {
                    let limits = Limits {
                        max_tags,
                        ..Limits::default()
                    };
                    let whole = Library::with_limits(fixture.as_slice(), limits)?;
                    let positioned = Library::with_limits(Positioned(&fixture, 0, false), limits)?;
                    for query in &queries {
                        let actual = walk(&whole, query, tile, node, None, 0)?;
                        assert_eq!(actual, walk(&positioned, query, tile, node, None, 0)?);
                        assert!(actual.iter().all(|(_, item)| {
                            item.as_ref().is_none_or(|result| result.is_ok())
                        }));
                    }
                }
            }
            if damage == 0 {
                let equality = DictionaryEquality {
                    key: amenity,
                    value: value_code,
                };
                let lib = Library::new(Positioned(&fixture, 64, false))?;
                let mut scan = Features::new(&lib, None);
                let p = base as u64;
                scan.tile = tile;
                scan.records.seek(p)?;
                let probe = |scan: &Features<'_, _>, p| {
                    if node {
                        scan.probe_window::<true>(p, equality)
                    } else {
                        scan.probe_window::<false>(p, equality)
                    }
                };
                assert_eq!(probe(&scan, p), None);
                scan.tags.seek(p + 184)?;
                assert_eq!(
                    probe(&scan, p),
                    Some((if node { 0x1000 } else { 0x1008 }, false))
                );
                let second = p + offsets[1] as u64;
                scan.records.seek(second)?;
                scan.tags.seek(p + 200)?;
                assert!(matches!(probe(&scan, second), Some((_, true))));
            }
            for steps in 0..=4 {
                let limits = Limits {
                    max_traversal_steps: steps,
                    ..Limits::default()
                };
                let fast = Library::with_limits(fixture.as_slice(), limits)?;
                let reference = Library::with_limits(Positioned(&fixture, 0, false), limits)?;
                let segmented = Library::with_limits(Positioned(&fixture, 64, false), limits)?;
                let window = Library::with_limits(Positioned(&fixture, 256, false), limits)?;
                let retained = Library::with_limits(Positioned(&fixture, 64, true), limits)?;
                for query in &queries {
                    for bounds in [None, Some(bounds)] {
                        for pause in 0..=4 {
                            let actual = walk(&fast, query, tile, node, bounds, pause)?;
                            let expected = walk(&reference, query, tile, node, bounds, pause)?;
                            assert_eq!(
                                actual, expected,
                                "{node} {damage} {steps} {bounds:?} {pause}"
                            );
                            for lib in [&segmented, &window, &retained] {
                                let actual = walk(lib, query, tile, node, bounds, pause)?;
                                assert_eq!(
                                    actual, expected,
                                    "window {} {node} {damage} {steps} {bounds:?} {pause}",
                                    lib.source.1
                                );
                            }
                            if matches!(damage, 0 | 6 | 8..=14) && steps == 4 && pause == 0 {
                                assert!(actual.iter().all(|(_, item)| {
                                    item.as_ref().is_none_or(|result| result.is_ok())
                                }));
                                assert_eq!(
                                    actual
                                        .iter()
                                        .filter(|(_, v)| matches!(v, Some(Ok(_))))
                                        .count(),
                                    if damage == 0 { 2 } else { 1 },
                                    "{node} {damage} {bounds:?} {actual:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

#[test]
fn proven_first_predicate_still_checks_type_and_second_predicate() -> Result<()> {
    let mut bytes = include_bytes!("../../tests/fixtures/global.gol").to_vec();
    let lib = Library::new(bytes.as_slice())?;
    let amenity = lib.dictionary().unwrap().key_code("amenity").unwrap();
    let building = lib.dictionary().unwrap().key_code("building").unwrap();
    let value = lib.dictionary().unwrap().key_code("highway").unwrap();
    assert!(amenity < building);
    let base = bytes.len().next_multiple_of(4096);
    bytes.resize(base + 256, 0);
    let tile = TileExtent::new(base as u64, bytes.len() as u64)?;
    bytes[base + 8..][..8].copy_from_slice(&0x1001u64.to_le_bytes());
    bytes[base + 16..][..4].copy_from_slice(&112i32.to_le_bytes());
    for (selector, first, second, expected) in [
        ("n", value, value, Some(true)),
        ("n", value, 0, None),
        ("n", 0, u16::MAX, None),
        ("n", value, u16::MAX, Some(false)),
        ("w", value, u16::MAX, None),
    ] {
        let first_word = (u32::from(first) << 16) | (u32::from(amenity) << 2) | 1;
        let second_word = (u32::from(second) << 16) | (u32::from(building) << 2) | 0x8001;
        bytes[base + 128..][..4].copy_from_slice(&first_word.to_le_bytes());
        bytes[base + 132..][..4].copy_from_slice(&second_word.to_le_bytes());
        let query = Query::parse(&format!("{selector}[amenity=highway][building=highway]"))?;
        for max_tags in 1..=3 {
            let limits = Limits {
                max_tags,
                ..Limits::default()
            };
            let whole = Library::with_limits(bytes.as_slice(), limits)?;
            let positioned = Library::with_limits(Positioned(&bytes, 0, false), limits)?;
            let segmented = Library::with_limits(Positioned(&bytes, 4096, false), limits)?;
            let actual = walk(&whole, &query, tile, true, None, 0)?;
            let reference = walk(&positioned, &query, tile, true, None, 0)?;
            assert_eq!(actual, reference, "{selector} {first} {second} {max_tags}");
            let held = walk(&segmented, &query, tile, true, None, 0)?;
            assert_eq!(
                held, reference,
                "segmented {selector} {first} {second} {max_tags}"
            );
            if max_tags == 3 {
                assert_eq!(actual[0].1.as_ref().map(|result| result.is_ok()), expected);
            }
        }
    }
    // The first two tests match. A third test can still reject the feature.
    bytes[base + 128..][..4]
        .copy_from_slice(&((u32::from(value) << 16) | (u32::from(amenity) << 2) | 1).to_le_bytes());
    bytes[base + 132..][..4].copy_from_slice(
        &((u32::from(value) << 16) | (u32::from(building) << 2) | 0x8001).to_le_bytes(),
    );
    let whole = Library::new(bytes.as_slice())?;
    let positioned = Library::new(Positioned(&bytes, 0, false))?;
    for (last, matches) in [("[name]", false), ("[!name]", true)] {
        let query = Query::parse(&format!("n[amenity=highway][building=highway]{last}"))?;
        let actual = walk(&whole, &query, tile, true, None, 0)?;
        assert_eq!(actual, walk(&positioned, &query, tile, true, None, 0)?);
        assert_eq!(actual[0].1.as_ref().is_some_and(|r| r.is_ok()), matches);
    }
    // The first two amenity arms can be combined. The last one must not be
    // moved ahead of a checked building lookup that reports an error.
    bytes[base + 134..][..2].copy_from_slice(&u16::MAX.to_le_bytes());
    let whole = Library::new(bytes.as_slice())?;
    let feature = Feature::at_source(&whole, whole.source(), (base + 8) as u64, tile)?;
    for (first, expected) in [("absent", false), ("highway", true)] {
        let query = Query::parse(&format!(
            "n[amenity={first}],n[amenity=other],n[building=yes],n[amenity=highway]"
        ))?;
        // Repeating the first predicate keeps the original arms
        // separate without adding a different tag read.
        let reference = Query::parse(&format!(
            "n[amenity={first}][amenity={first}],n[amenity=other],n[building=yes],n[amenity=highway]"
        ))?;
        let actual = walk(&whole, &query, tile, true, None, 0)?;
        assert_eq!(actual[0].1.as_ref().unwrap().is_ok(), expected);
        assert_eq!(actual, walk(&whole, &reference, tile, true, None, 0)?);
        let outcome = |query: &Query| query.matches(feature).map_err(|error| error.to_string());
        assert_eq!(outcome(&query), outcome(&reference));
        assert_eq!(query.matches(feature).is_ok(), expected);
        if expected {
            assert!(query.matches(feature)?);
        }
    }
    Ok(())
}

#[test]
fn dictionary_unions_share_only_conclusive_lookups() -> Result<()> {
    let mut bytes = include_bytes!("../../tests/fixtures/global.gol").to_vec();
    let lib = Library::new(bytes.as_slice())?;
    let amenity = lib.dictionary().unwrap().key_code("amenity").unwrap();
    let building = lib.dictionary().unwrap().key_code("building").unwrap();
    let highway = lib.dictionary().unwrap().key_code("highway").unwrap();
    assert!(amenity < building);
    let base = bytes.len().next_multiple_of(4096);
    bytes.resize(base + 256, 0);
    bytes[base + 8..][..8].copy_from_slice(&0x1001u64.to_le_bytes());
    let entry = |key, value, flags| (u32::from(value) << 16) | (u32::from(key) << 2) | flags;
    let cases = [
        (
            entry(amenity, highway, 1),
            entry(building, 0, 0x8001),
            0,
            256,
        ),
        (
            entry(amenity, highway, 1),
            entry(building, u16::MAX, 0x8001),
            0,
            256,
        ),
        (
            entry(amenity, u16::MAX, 1),
            entry(building, highway, 0x8001),
            0,
            256,
        ),
        (
            entry(building, highway, 1),
            entry(amenity, u16::MAX, 0x8001),
            0,
            256,
        ),
        (
            entry(amenity, 0, 1),
            entry(amenity, highway, 0x8001),
            0,
            256,
        ),
        (
            entry(amenity, 0, 2),
            entry(building, highway, 0x8001),
            0,
            132,
        ),
        (
            entry(amenity, 0, 1),
            entry(building, highway, 0x8001),
            0,
            134,
        ),
        (0x8001, 0, 1, 256),
        (0x8001, 0, 2, 256),
        (0x8001, 0, 3, 256),
        (entry(amenity, highway, 0x8001), 0, 3, 256),
        (0x8001, 0, 0, 256),
    ];
    for (first, second, local, end) in cases {
        let ptr = 112i32 | i32::from(local != 0);
        bytes[base + 16..][..4].copy_from_slice(&ptr.to_le_bytes());
        let name: &[u8] = match local {
            2 => b"\x04name",
            3 => &[1, 0xff],
            _ => b"\x07amenity",
        };
        bytes[base + 96..][..name.len()].copy_from_slice(name);
        bytes[base + 122..][..2].copy_from_slice(&highway.to_le_bytes());
        bytes[base + 124..][..4].copy_from_slice(&((-32i32 << 1) | 5).to_le_bytes());
        bytes[base + 128..][..4].copy_from_slice(&first.to_le_bytes());
        bytes[base + 132..][..4].copy_from_slice(&second.to_le_bytes());
        for max_tags in [0, 1, 2, 3, 100] {
            let limits = Limits {
                max_tags,
                ..Limits::default()
            };
            let whole = Library::with_limits(bytes.as_slice(), limits)?;
            let positioned = Library::with_limits(Positioned(&bytes, 0, false), limits)?;
            let paged = Library::with_limits(Positioned(&bytes, 64, true), limits)?;
            let tile = TileExtent::new(base as u64, (base + end) as u64)?;
            for text in [
                "n[amenity=highway],n[building=highway]",
                "n[building=highway],n[amenity=highway]",
                "n[highway=highway],n[building=highway],n[amenity=highway]",
                "n[building=highway],n[amenity=highway],n[highway=highway],n[building=amenity]",
                "w[amenity=highway],w[building=highway]",
                "n[amenity=highway],w[building=highway]",
            ] {
                let query = Query::parse(text)?;
                for bounds in [None, Some(Bounds::new(-1, -1, 1, 1)?)] {
                    let expected = walk(&positioned, &query, tile, true, bounds, 0)?;
                    assert_eq!(
                        walk(&whole, &query, tile, true, bounds, 0)?,
                        expected,
                        "{text}, {max_tags}, {first:#x}, {second:#x}"
                    );
                    assert_eq!(walk(&paged, &query, tile, true, bounds, 0)?, expected);
                }
            }
        }
    }
    Ok(())
}

#[test]
fn dictionary_union_bitset_checks_long_prefixes_and_order() {
    let tests = [
        DictionaryEquality { key: 6, value: 80 },
        DictionaryEquality { key: 7, value: 80 },
        DictionaryEquality {
            key: 123,
            value: 80,
        },
    ];
    let probe = DictionaryUnion::from_tests(&tests).unwrap();
    let entry = |key: u32, value: u32, flags: u32| (key << 2) | (value << 16) | flags;
    let mut bytes = Vec::new();
    for key in [6, 7, 10, 11, 30, 64, 80, 123] {
        bytes.extend_from_slice(&entry(key, 50, 1).to_le_bytes());
    }
    assert!(matches!(
        probe.probe(&bytes, 100, 7, 0),
        EqualityProbe::Unknown
    ));
    assert!(matches!(
        probe.probe(&bytes, 100, 8, 0),
        EqualityProbe::Reject
    ));
    bytes[28..32].copy_from_slice(&entry(123, 80, 1).to_le_bytes());
    assert!(matches!(
        probe.probe(&bytes, 100, 8, 0),
        EqualityProbe::Match
    ));
    let local = [
        entry(123, u32::from(u16::MAX), 0x8003)
            .to_le_bytes()
            .as_slice(),
        &[0xff, 0xff],
    ]
    .concat();
    assert!(matches!(
        probe.probe(&local, 100, 1, 0),
        EqualityProbe::Reject
    ));
    assert!(matches!(
        probe.probe(&local[..5], 100, 1, 0),
        EqualityProbe::Unknown
    ));
    bytes[12..16].copy_from_slice(&entry(7, 80, 1).to_le_bytes());
    assert!(matches!(
        probe.probe(&bytes, 100, 8, 0),
        EqualityProbe::Unknown
    ));
}

#[test]
fn dictionary_unions_match_checked_lookup_across_page_and_tile_edges() -> Result<()> {
    use crate::source::{CachedFile, PagedFile, WindowedFile};

    let mut bytes = include_bytes!("../../tests/fixtures/global.gol").to_vec();
    let dictionary = Library::new(bytes.as_slice())?;
    let amenity = dictionary
        .dictionary()
        .unwrap()
        .key_code("amenity")
        .unwrap();
    let building = dictionary
        .dictionary()
        .unwrap()
        .key_code("building")
        .unwrap();
    let highway = dictionary
        .dictionary()
        .unwrap()
        .key_code("highway")
        .unwrap();
    let page = 32 * 1024;
    let base = bytes.len().next_multiple_of(page);
    bytes.resize(base + page + 16, 0);
    let path = tempfile::NamedTempFile::new()?.into_temp_path();
    let first = u32::from(amenity) << 2 | 1 | u32::from(highway) << 16;
    let first_miss = u32::from(amenity) << 2 | 1;
    let second = u32::from(building) << 2 | 0x8001 | u32::from(u16::MAX) << 16;
    let second_miss = u32::from(building) << 2 | 0x8001;
    let queries = [
        Query::parse("n[amenity=highway],n[building=highway]")?,
        Query::parse("n[building=highway],n[amenity=highway]")?,
        Query::parse("w[building=highway],w[amenity=highway]")?,
    ];
    for (table, stop, header, first, second) in [
        (page - 2, page + 16, 0x1001u64, first, second),
        (page - 4, page + 16, 0x1001, first, second),
        (page - 4, page, 0x1001, first, second),
        (page - 4, page, 0x1801, first, second),
        (32, page + 16, 0x1001, first_miss, second_miss),
    ] {
        bytes[base + 8..][..8].copy_from_slice(&header.to_le_bytes());
        bytes[base + 16..][..4].copy_from_slice(&((table - 16) as i32).to_le_bytes());
        bytes[base + table..][..4].copy_from_slice(&first.to_le_bytes());
        bytes[base + table + 4..][..4].copy_from_slice(&second.to_le_bytes());
        std::fs::write(&path, &bytes)?;
        let tile = TileExtent::new(base as u64, (base + stop) as u64)?;
        for max_tags in [0, 1, 2, 8, 100] {
            let limits = Limits {
                max_tags,
                ..Limits::default()
            };
            let reference = Library::with_limits(Positioned(&bytes, 0, false), limits)?;
            let paged = Library::with_limits(PagedFile::open(&path)?, limits)?;
            let cached = Library::with_limits(CachedFile::open(&path, page)?, limits)?;
            let windowed = Library::with_limits(WindowedFile::open(&path)?, limits)?;
            for query in &queries {
                for bounds in [None, Some(Bounds::new(1, 1, 2, 2)?)] {
                    let expected = walk(&reference, query, tile, true, bounds, 0)?;
                    assert_eq!(walk(&paged, query, tile, true, bounds, 0)?, expected);
                    assert_eq!(walk(&cached, query, tile, true, bounds, 0)?, expected);
                    assert_eq!(walk(&windowed, query, tile, true, bounds, 0)?, expected);
                }
            }
        }
    }
    Ok(())
}

#[test]
fn required_pair_short_circuit_keeps_ordered_lookup_errors() -> Result<()> {
    let mut bytes = include_bytes!("../../tests/fixtures/global.gol").to_vec();
    let lib = Library::new(bytes.as_slice())?;
    let highway = u32::from(lib.dictionary().unwrap().key_code("highway").unwrap());
    let amenity = u32::from(lib.dictionary().unwrap().key_code("amenity").unwrap());
    let base = bytes.len().next_multiple_of(4096);
    bytes.resize(base + 256, 0);
    let query = Query::parse("n[highway][amenity]")?;
    bytes[base + 8..][..8].copy_from_slice(&0x1001u64.to_le_bytes());
    let table = base + 128;
    let word = |key: u32, mode: u32, value: u32| (key << 2) | mode | (value << 16);
    for (first, second, third) in [
        (
            word(amenity, 1, u32::from(u16::MAX)),
            word(highway, 1, 1),
            0x8001,
        ),
        (
            word(highway, 1, u32::from(u16::MAX)),
            word(amenity, 1, 1),
            0x8001,
        ),
        (
            word(highway, 1, 1),
            word(amenity, 1, u32::from(u16::MAX)),
            0x8001,
        ),
        (
            word(highway, 1, 1),
            word(highway, 1, u32::from(u16::MAX)),
            0x8001,
        ),
        (word(highway, 1, 1), word(amenity, 1, 1), 0x8001),
        (word(1, 1, 1), word(2, 1, 2), word(amenity, 1, 1) | 0x8000),
        (word(highway, 2, 0) | 0x8000, 0, 0),
        (word(highway, 3, 0) | 0x8000, 0, 0),
        (0x8001, 0, 0),
    ] {
        for (i, entry) in [first, second, third].into_iter().enumerate() {
            bytes[table + 4 * i..][..4].copy_from_slice(&entry.to_le_bytes());
        }
        for local in [0, 1] {
            let ptr = (table - base - 16) as i32 | local;
            bytes[base + 16..][..4].copy_from_slice(&ptr.to_le_bytes());
            for max_tags in [0, 1, 2, 3, 16, 100] {
                let limits = Limits {
                    max_tags,
                    ..Limits::default()
                };
                let mmap_like = Library::with_limits(bytes.as_slice(), limits)?;
                let positioned = Library::with_limits(Positioned(&bytes, 0, false), limits)?;
                for end in [table + 3, table + 4, table + 8, table + 12, base + 256] {
                    let tile = TileExtent::new(base as u64, end as u64)?;
                    assert_eq!(
                        walk(&mmap_like, &query, tile, true, None, 0)?,
                        walk(&positioned, &query, tile, true, None, 0)?,
                        "{first:#x}, {second:#x}, {third:#x}, {local}, {max_tags}, {end}"
                    );
                }
            }
        }
    }
    Ok(())
}
