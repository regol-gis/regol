//! Snapshot identity and search windows for the build-time named-area index.
use regol::{Bounds, source::Source};
use serde::Deserialize;
use std::{fmt::Write, io};

/// Identify an immutable GOL by store and tileset IDs, header generation and length.
pub fn snapshot(source: &impl Source) -> io::Result<String> {
    let mut header = [0u8; 256];
    source.read_exact_at(0, &mut header)?;
    let mut key = String::from("v1-");
    for byte in header[8..16].iter().chain(&header[64..80]) {
        write!(&mut key, "{byte:02x}").unwrap();
    }
    let hash = header.iter().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    });
    write!(&mut key, "-{hash:016x}-{}", source.len()).unwrap();
    Ok(key)
}

/// Choose a shard by hashing the UTF-8 name with FNV-1a.
pub fn shard(name: &str) -> u8 {
    name.bytes().fold(0x811c9dc5u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x01000193)
    }) as u8
}

#[derive(Deserialize)]
pub struct Lookup {
    pub name: String,
    pub index: Option<IndexEntry>,
    #[serde(skip)]
    scan_local_file: bool,
}

#[derive(Deserialize)]
pub struct IndexEntry {
    pub snapshot: String,
    pub probes: Vec<[i32; 2]>,
}

impl crate::Request {
    /// Allow named-area lookup by a whole-file scan for a known local source.
    /// A positioned file reader can use this just like an immutable memory map.
    /// This is a consumer choice and cannot be enabled by a serialized request.
    pub fn allow_local_area_scan(&mut self) {
        for lookup in self.named_areas_mut() {
            lookup.scan_local_file = true;
        }
    }

    /// Access named-area lookups so a local data source can supply its snapshot index.
    pub fn named_areas_mut(&mut self) -> impl Iterator<Item = &mut Lookup> {
        let sets = match &mut self.body {
            crate::Body::Program(plan) => plan.sets.as_mut_slice(),
            crate::Body::Legacy { .. } => &mut [],
        };
        sets.iter_mut()
            .filter_map(|set| set.overpass.as_mut()?.named_area.as_mut())
    }
}

impl Lookup {
    pub fn windows(&self, source: &impl Source) -> Result<Vec<Bounds>, String> {
        let Some(index) = &self.index else {
            // Native callers may open any local immutable GOL without building a sidecar.
            if self.scan_local_file || source.as_bytes().is_some() {
                return Ok(vec![Bounds::WORLD]);
            }
            return Err("Named-area lookup needs an area index for this GOL.".into());
        };
        if index.snapshot != snapshot(source).map_err(|e| e.to_string())? {
            return Err("The named-area index belongs to a different GOL snapshot. Rebuild the index for this file.".into());
        }
        if index.probes.len() > crate::MAX_INTERMEDIATE_FEATURES {
            return Err("This name exceeds the named-area lookup limit.".into());
        }
        Ok(index
            .probes
            .iter()
            .map(|&[x, y]| {
                let point = regol::Point { x, y };
                Bounds {
                    min: point,
                    max: point,
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn even_an_empty_hit_list_must_match_the_open_generation() {
        let header = &[21u8; 512][..];
        let index = IndexEntry {
            snapshot: snapshot(&header).unwrap(),
            probes: Vec::new(),
        };
        let lookup = Lookup {
            name: "Absent".into(),
            index: Some(index),
            scan_local_file: false,
        };
        assert!(lookup.windows(&header).unwrap().is_empty());
        assert!(
            lookup
                .windows(&&[22u8; 512][..])
                .unwrap_err()
                .contains("different GOL")
        );
        assert!(
            lookup
                .windows(&&header[..300])
                .unwrap_err()
                .contains("different GOL")
        );
    }
}
