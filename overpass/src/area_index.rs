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
const MAX_MANIFEST_BYTES: u64 = 4096;
const MAX_SHARD_BYTES: u64 = 64 * 1024 * 1024;
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
        let manifest: Manifest = read_json(&directory.join("manifest.json"), MAX_MANIFEST_BYTES)?;
        if manifest.version != 1 || manifest.shards != SHARD_COUNT {
            return Err("Expected a named-area index v1 with 256 shards.".into());
        }
        let actual =
            snapshot(source).map_err(|error| format!("Identifying the GOL snapshot: {error}"))?;
        if manifest.snapshot != actual {
            return Err("The named-area index belongs to a different GOL snapshot. Rebuild the index for this file.".into());
        }
        let generation = directory.join(&actual);
        if !generation.is_dir() {
            return Err(format!(
                "Named-area index generation {} is missing.",
                generation.display()
            ));
        }
        Ok(Self {
            generation,
            snapshot: actual,
        })
    }

    pub fn hydrate(
        &self,
        request: &mut Request,
        check: &impl Fn() -> Result<(), String>,
    ) -> Result<(), String> {
        // Only one shard needs to be in memory at a time. Multiple lookups of
        // the same shard share its parse, including repeated names.
        let mut by_shard = BTreeMap::<u8, Vec<&mut Lookup>>::new();
        for lookup in request.named_areas_mut() {
            check()?;
            by_shard
                .entry(shard(&lookup.name))
                .or_default()
                .push(lookup);
        }
        let mut probe_count = 0usize;
        for (id, lookups) in by_shard {
            check()?;
            let entries = self.read_shard(id, check)?;
            for lookup in lookups {
                check()?;
                let probes = entries
                    .get(&lookup.name)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                probe_count += probes.len();
                if probe_count > MAX_PROBES {
                    return Err("Named-area lookups exceed the probe budget. Use fewer or more specific names.".into());
                }
                lookup.index = Some(IndexEntry {
                    snapshot: self.snapshot.clone(),
                    probes: probes.to_vec(),
                });
            }
        }
        Ok(())
    }

    fn read_shard(
        &self,
        id: u8,
        check: &impl Fn() -> Result<(), String>,
    ) -> Result<HashMap<String, Vec<[i32; 2]>>, String> {
        let path = self.generation.join(format!("{id:02x}.json"));
        let rows: Vec<(String, Vec<[i32; 2]>)> = read_json(&path, MAX_SHARD_BYTES)?;
        let mut entries = HashMap::with_capacity(rows.len());
        for (name, probes) in rows {
            check()?;
            if shard(&name) != id || entries.insert(name, probes).is_some() {
                return Err(format!(
                    "Misplaced or duplicate name in area-index shard {id:02x}."
                ));
            }
        }
        Ok(entries)
    }
}

fn read_json<T: DeserializeOwned>(path: &Path, max: u64) -> Result<T, String> {
    let display = path.display();
    let meta = path
        .metadata()
        .map_err(|e| format!("Reading {display}: {e}"))?;
    if !meta.is_file() || meta.len() > max {
        return Err(format!(
            "Expected {display} to be a regular file no larger than {max} bytes."
        ));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(max + 1).read_to_end(&mut bytes))
        .map_err(|e| format!("Reading {display}: {e}"))?;
    if bytes.len() as u64 > max {
        return Err(format!(
            "The area index file {display} exceeds {max} bytes."
        ));
    }
    serde_json::from_slice(&bytes).map_err(|e| format!("Parsing {display}: {e}"))
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
                    "regol-live-area-index-{}-{}",
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
}
