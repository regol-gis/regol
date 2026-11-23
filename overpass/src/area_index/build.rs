use super::SHARD_COUNT;
use crate::named_area::{shard, snapshot};
use regol::{Feature, Library, Predicate, Query, Selector, Types, source::Source};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    error::Error,
    fs::{self, File},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    process,
};

const SHARDS: usize = SHARD_COUNT as usize;

#[derive(Serialize)]
struct Manifest<'a> {
    version: u8,
    snapshot: &'a str,
    shards: usize,
}

fn shard_file(id: usize) -> String {
    format!("{id:02x}.json")
}

fn write_json(path: &Path, data: &impl Serialize) -> io::Result<()> {
    let mut writer = BufWriter::new(File::create(path)?);
    serde_json::to_writer(&mut writer, data)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn area_candidate<S: Source>(feature: Feature<'_, S>) -> regol::Result<bool> {
    if feature.is_area() {
        return Ok(true);
    }
    // The selector admits only ways and area features. A named linear way can
    // still map to an area if it is closed. The runtime checks node identity
    // and validates geometry after finding candidates by exact name.
    let mut coordinates = feature.coordinates()?;
    let Some(first) = coordinates.next().transpose()? else {
        return Ok(false);
    };
    let mut count = 1;
    let mut last = first;
    for point in coordinates {
        last = point?;
        count += 1;
    }
    Ok(count >= 4 && last == first)
}

fn write_shards<S: Source>(library: &Library<S>, directory: &Path) -> Result<(), Box<dyn Error>> {
    fs::create_dir(directory)?;
    let mut shards: [_; SHARDS] = std::array::from_fn(|_| BTreeMap::<String, Vec<[i32; 2]>>::new());
    let query =
        Query::new(Selector::new(Types::WAY.union(Types::AREA)).and(Predicate::present("name")));
    let prepared = query.prepare(library)?;
    for feature in prepared.iter() {
        let feature = feature?;
        if area_candidate(feature).map_err(|error| format!("{}: {error}", feature.id()))? {
            let name = feature
                .tag("name")?
                .ok_or_else(|| format!("{} has no readable name tag.", feature.id()))?
                .to_string();
            let point = feature.bounds()?.min;
            shards[usize::from(shard(&name))]
                .entry(name)
                .or_default()
                .push([point.x, point.y]);
        }
    }
    for (id, mut shard) in shards.into_iter().enumerate() {
        for points in shard.values_mut() {
            points.sort_unstable();
            points.dedup();
        }
        // Write entries as pairs. Exact names remain data even when they match
        // a built-in JavaScript property.
        write_json(
            &directory.join(shard_file(id)),
            &shard.into_iter().collect::<Vec<_>>(),
        )?;
    }
    Ok(())
}

fn create_staging(output: &Path) -> io::Result<PathBuf> {
    for attempt in 0.. {
        let path = output.join(format!(".area-index-{}-{attempt}", process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    unreachable!()
}

/// Build the named-area index for an immutable GOL, publishing the matching
/// manifest only after all shards are available.
pub fn build<S: Source>(library: &Library<S>, output: &Path) -> Result<(), Box<dyn Error>> {
    let snapshot = snapshot(library.source())?;
    fs::create_dir_all(output)?;
    let generation = output.join(&snapshot);
    let staging = create_staging(output)?;
    let result = (|| -> Result<(), Box<dyn Error>> {
        if !generation.is_dir() {
            let shards = staging.join("shards");
            write_shards(library, &shards)?;
            if let Err(error) = fs::rename(&shards, &generation)
                && !generation.is_dir()
            {
                return Err(error.into());
            }
        }
        if !(0..SHARDS).all(|id| generation.join(shard_file(id)).is_file()) {
            return Err(format!("Incomplete area-index generation {snapshot}.").into());
        }
        let manifest = staging.join("manifest.json");
        write_json(
            &manifest,
            &Manifest {
                version: 1,
                snapshot: &snapshot,
                shards: SHARDS,
            },
        )?;
        fs::rename(manifest, output.join("manifest.json"))?;
        Ok(())
    })();
    let cleanup = fs::remove_dir_all(staging);
    result?;
    cleanup?;
    Ok(())
}
