mod build;
pub use build::build;

use crate::{
    Request,
    named_area::{IndexEntry, Lookup, shard, snapshot},
};
use regol::source::Source;
use serde::{Deserialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

const SHARD_COUNT: u16 = 256;
const MAX_MANIFEST_BYTES: usize = 4096;
const MAX_SHARD_BYTES: usize = 64 * 1024 * 1024;
const MAX_PROBES: usize = 250_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u16,
    snapshot: String,
    shards: u16,
}

pub struct AreaIndex {
    generation: PathBuf,
    snapshot: String,
}

impl AreaIndex {
    pub fn open(directory: &Path, source: &impl Source) -> Result<Self, String> {
        let path = directory.join("manifest.json");
        let manifest = read_file(&path, MAX_MANIFEST_BYTES)?;
        let snapshot = validate_manifest(&path.display().to_string(), &manifest, source)?;
        let generation = directory.join(&snapshot);
        if !generation.is_dir() {
            return Err(format!(
                "Named-area index generation {} is missing.",
                generation.display()
            ));
        }
        Ok(Self {
            generation,
            snapshot,
        })
    }

    pub fn hydrate(
        &self,
        request: &mut Request,
        check: &impl Fn() -> Result<(), String>,
    ) -> Result<(), String> {
        hydrate_shards(
            group_lookups(request, check)?,
            &self.snapshot,
            |file, max| read_file(&self.generation.join(file), max),
            check,
        )
    }
}

/// Hydrate exact-name lookups from an index whose files are provided by the caller.
/// `read` receives a path relative to the index root and a limit on its size in
/// bytes. Lookups share shard reads, and the manifest must match the open GOL
/// before any shards are read. Requests without named areas read no assets.
pub fn hydrate_from(
    request: &mut Request,
    source: &impl Source,
    mut read: impl FnMut(&str, usize) -> Result<Vec<u8>, String>,
    check: &impl Fn() -> Result<(), String>,
) -> Result<(), String> {
    let lookups = group_lookups(request, check)?;
    if lookups.is_empty() {
        return Ok(());
    }
    let file = "manifest.json";
    let manifest = read(file, MAX_MANIFEST_BYTES)?;
    check()?;
    let snapshot = validate_manifest(file, &manifest, source)?;
    hydrate_shards(
        lookups,
        &snapshot,
        |file, max| read(&format!("{snapshot}/{file}"), max),
        check,
    )
}

fn validate_manifest(file: &str, bytes: &[u8], source: &impl Source) -> Result<String, String> {
    let manifest: Manifest = parse_json(file, bytes, MAX_MANIFEST_BYTES)?;
    if manifest.version != 1 || manifest.shards != SHARD_COUNT {
        return Err("Expected a named-area index v1 with 256 shards.".into());
    }
    let actual =
        snapshot(source).map_err(|error| format!("Identifying the GOL snapshot: {error}"))?;
    if manifest.snapshot != actual {
        return Err("The named-area index belongs to a different GOL snapshot. Rebuild the index for this file.".into());
    }
    Ok(actual)
}

fn group_lookups<'a>(
    request: &'a mut Request,
    check: &impl Fn() -> Result<(), String>,
) -> Result<BTreeMap<u8, Vec<&'a mut Lookup>>, String> {
    let mut by_shard = BTreeMap::<u8, Vec<&mut Lookup>>::new();
    for lookup in request.named_areas_mut() {
        check()?;
        by_shard
            .entry(shard(&lookup.name))
            .or_default()
            .push(lookup);
    }
    Ok(by_shard)
}

fn hydrate_shards(
    by_shard: BTreeMap<u8, Vec<&mut Lookup>>,
    snapshot: &str,
    mut read: impl FnMut(&str, usize) -> Result<Vec<u8>, String>,
    check: &impl Fn() -> Result<(), String>,
) -> Result<(), String> {
    let mut probe_count = 0usize;
    // Read and parse one shard at a time. One parse covers every lookup of
    // that shard, including repeated names.
    for (id, lookups) in by_shard {
        check()?;
        let file = format!("{id:02x}.json");
        let bytes = read(&file, MAX_SHARD_BYTES)?;
        check()?;
        let rows: Vec<(String, Vec<[i32; 2]>)> = parse_json(&file, &bytes, MAX_SHARD_BYTES)?;
        let mut entries = HashMap::with_capacity(rows.len());
        for (name, probes) in rows {
            check()?;
            if shard(&name) != id || entries.insert(name, probes).is_some() {
                return Err(format!(
                    "Misplaced or duplicate name in area-index shard {id:02x}."
                ));
            }
        }
        for lookup in lookups {
            check()?;
            let probes = entries
                .get(&lookup.name)
                .map(Vec::as_slice)
                .unwrap_or_default();
            probe_count += probes.len();
            if probe_count > MAX_PROBES {
                return Err(
                    "Named-area lookups exceed the probe budget. Use fewer or more specific names."
                        .into(),
                );
            }
            lookup.index = Some(IndexEntry {
                snapshot: snapshot.into(),
                probes: probes.to_vec(),
            });
        }
    }
    Ok(())
}

fn parse_json<T: DeserializeOwned>(file: &str, bytes: &[u8], max: usize) -> Result<T, String> {
    if bytes.len() > max {
        return Err(format!("The area index file {file} exceeds {max} bytes."));
    }
    serde_json::from_slice(bytes).map_err(|e| format!("Parsing {file}: {e}"))
}

fn read_file(path: &Path, max: usize) -> Result<Vec<u8>, String> {
    let display = path.display();
    let meta = path
        .metadata()
        .map_err(|e| format!("Reading {display}: {e}"))?;
    if !meta.is_file() || meta.len() > max as u64 {
        return Err(format!(
            "Expected {display} to be a regular file no larger than {max} bytes."
        ));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(max as u64 + 1).read_to_end(&mut bytes))
        .map_err(|e| format!("Reading {display}: {e}"))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        fs,
        sync::atomic::{AtomicUsize, Ordering},
    };

    const GOL: &[u8] = include_bytes!("../tests/fixtures/geography.gol");
    const NAME: &str = "Courtyard";

    struct TestAreaDirectory(PathBuf);

    impl TestAreaDirectory {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            loop {
                let path = std::env::temp_dir().join(format!(
                    "regol-overpass-area-index-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => panic!("{error}"),
                }
            }
        }

        fn manifest(&self, version: u16, key: &str, shards: u16) {
            fs::write(
                self.0.join("manifest.json"),
                json!({"version":version,"snapshot":key,"shards":shards}).to_string(),
            )
            .unwrap();
        }

        fn matching_index(&self) -> AreaIndex {
            let key = snapshot(&GOL).unwrap();
            self.manifest(1, &key, 256);
            fs::create_dir(self.0.join(key)).unwrap();
            AreaIndex::open(&self.0, &GOL).unwrap()
        }
    }

    impl Drop for TestAreaDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn request(names: &[&str]) -> Request {
        let sets: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                json!({
                    "name": format!("source{i}"), "source":"*", "overpass":{
                        "kind":"wr", "tags":[{"key":"name", "op":"eq", "value":name}],
                        "namedArea":{"name":name},
                    },
                })
            })
            .collect();
        serde_json::from_value(json!({"windows":[], "sets":sets, "show":[]})).unwrap()
    }

    #[test]
    fn validates_manifest_version_shards_and_exact_snapshot() {
        let directory = TestAreaDirectory::new();
        let key = snapshot(&GOL).unwrap();
        directory.manifest(2, &key, 256);
        assert!(
            AreaIndex::open(&directory.0, &GOL)
                .err()
                .unwrap()
                .contains("v1 with 256 shards")
        );
        directory.manifest(1, &key, 255);
        assert!(
            AreaIndex::open(&directory.0, &GOL)
                .err()
                .unwrap()
                .contains("v1 with 256 shards")
        );
        directory.manifest(1, "v1-wrong", 256);
        assert!(
            AreaIndex::open(&directory.0, &GOL)
                .err()
                .unwrap()
                .contains("different GOL snapshot")
        );
    }

    #[test]
    fn hydrates_multiple_names_from_one_shard_and_marks_missing_names_empty() {
        let directory = TestAreaDirectory::new();
        let index = directory.matching_index();
        let missing = (0..)
            .map(|i| format!("Missing {i}"))
            .find(|name| shard(name) == shard(NAME))
            .unwrap();
        let probes = [[17, 29], [41, 53]];
        fs::write(
            index.generation.join(format!("{:02x}.json", shard(NAME))),
            json!([[NAME, probes]]).to_string(),
        )
        .unwrap();
        let mut plan = request(&[NAME, &missing, NAME]);
        index.hydrate(&mut plan, &|| Ok(())).unwrap();
        let entries = plan
            .named_areas_mut()
            .map(|lookup| lookup.index.take().unwrap())
            .collect::<Vec<_>>();
        assert!(entries.iter().all(|entry| entry.snapshot == index.snapshot));
        assert_eq!(entries[0].probes, probes);
        assert!(entries[1].probes.is_empty());
        assert_eq!(entries[2].probes, probes);
    }

    #[test]
    fn queries_without_named_areas_need_no_shards_and_bad_shards_report_errors() {
        let directory = TestAreaDirectory::new();
        let index = directory.matching_index();
        index.hydrate(&mut request(&[]), &|| Ok(())).unwrap();
        let mut plan = request(&[NAME]);
        assert_eq!(
            index
                .hydrate(&mut plan, &|| Err("cancelled".into()))
                .unwrap_err(),
            "cancelled"
        );
        let id = shard(NAME);
        let path = index.generation.join(format!("{id:02x}.json"));
        assert!(
            index
                .hydrate(&mut plan, &|| Ok(()))
                .unwrap_err()
                .contains("Reading")
        );
        fs::write(
            &path,
            json!([[NAME, [[1, 2]]], [NAME, [[3, 4]]]]).to_string(),
        )
        .unwrap();
        let error = index.hydrate(&mut plan, &|| Ok(())).unwrap_err();
        assert!(error.contains("duplicate name"), "{error}");
    }

    #[test]
    fn supplied_assets_share_exact_name_shards_and_check_snapshot_first() {
        let snapshot = snapshot(&GOL).unwrap();
        let manifest = |snapshot: &str| {
            serde_json::to_vec(&json!({"version": 1, "snapshot": snapshot, "shards": 256})).unwrap()
        };
        let mut plan = request(&[NAME]);
        let error = hydrate_from(
            &mut plan,
            &GOL,
            |file, max| {
                assert_eq!(file, "manifest.json");
                assert_eq!(max, MAX_MANIFEST_BYTES);
                Ok(manifest("v1-wrong"))
            },
            &|| Ok(()),
        )
        .unwrap_err();
        assert!(error.contains("different GOL snapshot"));
        assert!(plan.named_areas_mut().next().unwrap().index.is_none());

        let name = "Île-de-France";
        let index_file = format!("{snapshot}/{:02x}.json", shard(name));
        let probes = [[3, -4], [-5, 6]];
        let mut plan = request(&[name, name]);
        let mut files = ["manifest.json", &index_file].into_iter();
        hydrate_from(
            &mut plan,
            &GOL,
            |file, max| {
                assert_eq!(Some(file), files.next());
                if file == "manifest.json" {
                    Ok(manifest(&snapshot))
                } else {
                    assert_eq!(max, MAX_SHARD_BYTES);
                    Ok(serde_json::to_vec(&json!([[name, probes]])).unwrap())
                }
            },
            &|| Ok(()),
        )
        .unwrap();
        assert_eq!(files.next(), None);
        for entry in plan
            .named_areas_mut()
            .map(|lookup| lookup.index.take().unwrap())
        {
            assert_eq!(entry.snapshot, snapshot);
            assert_eq!(entry.probes, probes);
        }
    }

    #[test]
    fn supplied_assets_enforce_size_coordinates_and_aggregate_probe_budget() {
        let snapshot = snapshot(&GOL).unwrap();
        let manifest =
            serde_json::to_vec(&json!({"version": 1, "snapshot": snapshot, "shards": 256}))
                .unwrap();
        let mut plan = request(&[]);
        hydrate_from(
            &mut plan,
            &GOL,
            |_, _| panic!("unexpected asset read"),
            &|| Ok(()),
        )
        .unwrap();

        let error = hydrate_from(
            &mut request(&[NAME]),
            &GOL,
            |_, max| Ok(vec![0; max + 1]),
            &|| Ok(()),
        )
        .unwrap_err();
        assert!(error.contains("exceeds 4096 bytes"));

        for (probes, message) in [
            (json!([[i64::from(i32::MAX) + 1, 0]]), "invalid value"),
            (
                json!(vec![[0, 0]; MAX_PROBES / 2 + 1]),
                "exceed the probe budget",
            ),
        ] {
            let shard = serde_json::to_vec(&json!([[NAME, probes]])).unwrap();
            let error = hydrate_from(
                &mut request(&[NAME, NAME]),
                &GOL,
                |file, _| {
                    Ok(if file == "manifest.json" {
                        manifest.clone()
                    } else {
                        shard.clone()
                    })
                },
                &|| Ok(()),
            )
            .unwrap_err();
            assert!(error.contains(message), "{error}");
        }
    }
}
