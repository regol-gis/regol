//! Streaming GOL 2.0 store writer with parallel in-memory tile encoding.
use super::{
    BuildOptions,
    dictionary::Dictionary,
    directory::{Directory, Placement},
    encode::{MAX_KEY_INDEXES, put, string, tile},
    spill::MemoryBudget,
    stage::BuiltDataset,
    worker_count,
};
use anyhow::{Result, anyhow, ensure};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Seek, SeekFrom, Write},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

const PAGE: u64 = 4096;
const SEGMENT: u64 = 1 << 30;

// A GeoDesk mapped reader accesses a whole blob in one 1-GiB segment.
fn placement(next: u64, len: u64) -> Result<u64> {
    ensure!(len <= SEGMENT, "blob exceeds GOL segment size");
    let start = next.next_multiple_of(PAGE);
    Ok(if start % SEGMENT + len > SEGMENT {
        start.next_multiple_of(SEGMENT)
    } else {
        start
    })
}

#[cfg(test)]
pub(super) fn write(
    file: &mut File,
    dataset: &BuiltDataset,
    placement_data: &Placement,
    dir: Directory,
    source: &super::pbf::Header,
    dictionary: &Dictionary,
) -> Result<u32> {
    let budget = MemoryBudget::unlimited();
    write_parallel(
        file,
        dataset,
        placement_data,
        dir,
        source,
        dictionary,
        BuildOptions::default(),
        &budget,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn write_parallel(
    file: &mut File,
    dataset: &BuiltDataset,
    placement_data: &Placement,
    mut dir: Directory,
    source: &super::pbf::Header,
    dictionary: &Dictionary,
    options: BuildOptions,
    budget: &MemoryBudget,
) -> Result<u32> {
    let revision = u32::try_from(source.sequence)
        .map_err(|_| anyhow!("PBF replication sequence does not fit the GOL revision field"))?;
    let mut header = vec![0; PAGE as usize];
    put(&mut header, 0, 0x1ce5_0d6e);
    put(&mut header, 4, 2);
    getrandom::fill(&mut header[8..16]).map_err(|e| anyhow!("generate store ID: {e}"))?;
    getrandom::fill(&mut header[64..80]).map_err(|e| anyhow!("generate tileset ID: {e}"))?;
    put(&mut header, 80, 1); // IDs follow every way coordinate stream
    put(&mut header, 88, PAGE as u32);
    let mut meta = (dictionary.schema.len() as u32).to_le_bytes().to_vec();
    for &(code, category) in &dictionary.schema {
        meta.extend((u32::from(category) << 16 | u32::from(code)).to_le_bytes());
    }
    put(&mut header, 84, (PAGE as usize + meta.len()) as u32);
    meta.extend((dictionary.words.len() as u16).to_le_bytes());
    for s in &dictionary.words {
        string(&mut meta, s);
    }
    put(&mut header, 92, (PAGE as usize + meta.len()) as u32);
    meta.extend(2u16.to_le_bytes());
    for s in [
        "copyright",
        "(C) OpenStreetMap contributors",
        "license",
        "Open Database License 1.0",
    ] {
        string(&mut meta, s);
    }
    put(&mut header, 20, meta.len() as u32);
    put(&mut header, 96, dir.zooms);
    put(&mut header, 100, options.rtree_branch_fanout as u32); // spatial index fanout
    header[103] = MAX_KEY_INDEXES;
    put(&mut header, 104, 1); // an indexed group may contain one feature
    put(&mut header, 112, crc32c::crc32c(&meta));
    put(&mut header, 108, dir.words[0] / 4);
    let catalog_crc = dir
        .words
        .iter()
        .fold(0, |crc, w| crc32c::crc32c_append(crc, &w.to_le_bytes()));
    put(&mut header, 116, catalog_crc);
    let timestamp = match source.timestamp {
        Some(seconds) => seconds
            .checked_mul(1000)
            .ok_or_else(|| anyhow!("PBF timestamp out of range"))?,
        None => u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?,
    };
    put(&mut header, 128, revision);
    header[136..144].copy_from_slice(&timestamp.to_le_bytes());
    file.seek(SeekFrom::Start(PAGE))?;
    file.write_all(&meta)?;
    let mut next = PAGE + meta.len() as u64;

    let workers = budget.max_workers(worker_count().min(dir.tips.len()).min(32));
    if workers > 1 && dir.tips.len() > 1 {
        let tips = &dir.tips;
        // Dispatch heavier tiles earlier within bounded waves to avoid tail stragglers
        // while keeping the reorder buffer strictly bounded.
        let wave = budget.tile_wave_limit(options.target_tile_bytes, workers * 2);
        let mut order: Vec<usize> = (0..tips.len()).collect();
        for chunk in order.chunks_mut(wave) {
            chunk.sort_unstable_by(|&a, &b| {
                placement_data
                    .tile_slice(b)
                    .len()
                    .cmp(&placement_data.tile_slice(a).len())
            });
        }
        let order = &order;
        let next_pos = AtomicUsize::new(0);
        let cancelled = AtomicBool::new(false);
        let (tx, rx) = mpsc::sync_channel::<(usize, Result<Vec<u8>>)>(wave);
        std::thread::scope(|scope| -> Result<()> {
            for _ in 0..workers {
                let tx = tx.clone();
                let next_pos = &next_pos;
                let cancelled = &cancelled;
                scope.spawn(move || {
                    loop {
                        if cancelled.load(Ordering::Relaxed) {
                            break;
                        }
                        let pos = next_pos.fetch_add(1, Ordering::Relaxed);
                        if pos >= order.len() {
                            break;
                        }
                        let idx = order[pos];
                        let encoded = tile(
                            dataset,
                            placement_data,
                            idx,
                            tips[idx],
                            options.rtree_branch_fanout,
                            options.rtree_leaf_size,
                        );
                        let failed = encoded.is_err();
                        if tx.send((idx, encoded)).is_err() || failed {
                            cancelled.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                });
            }
            drop(tx);
            let mut reorder: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
            let mut write_idx = 0usize;
            while let Ok((idx, encoded)) = rx.recv() {
                let bytes = match encoded {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        cancelled.store(true, Ordering::Relaxed);
                        return Err(err);
                    }
                };
                reorder.insert(idx, bytes);
                while let Some(bytes) = reorder.remove(&write_idx) {
                    let tip = tips[write_idx];
                    let start = placement(next, bytes.len() as u64)?;
                    let page = start / PAGE;
                    ensure!(page < 1 << 30, "GOL tile page exceeds format limit");
                    dir.words[tip as usize] = (page as u32) << 2 | 2;
                    file.seek(SeekFrom::Start(start))?;
                    file.write_all(&bytes)?;
                    next = start + bytes.len() as u64;
                    write_idx += 1;
                }
            }
            ensure!(write_idx == tips.len(), "missing encoded GOL tile");
            Ok(())
        })?;
    } else {
        for (idx, &tip) in dir.tips.iter().enumerate() {
            let bytes = tile(
                dataset,
                placement_data,
                idx,
                tip,
                options.rtree_branch_fanout,
                options.rtree_leaf_size,
            )?;
            let start = placement(next, bytes.len() as u64)?;
            let page = start / PAGE;
            ensure!(page < 1 << 30, "GOL tile page exceeds format limit");
            dir.words[tip as usize] = (page as u32) << 2 | 2;
            file.seek(SeekFrom::Start(start))?;
            file.write_all(&bytes)?;
            next = start + bytes.len() as u64;
        }
    }

    let count = u32::try_from(dir.tips.len())?;
    let len = dir.words.len() as u64 * 4;
    let start = placement(next, len)?;
    put(&mut header, 152, u32::try_from(start / PAGE)?);
    put(&mut header, 156, count);
    let pages = (start + len).div_ceil(PAGE);
    put(&mut header, 24, u32::try_from(pages)?);
    file.seek(SeekFrom::Start(start))?;
    {
        let mut out = std::io::BufWriter::new(&mut *file);
        for word in dir.words {
            out.write_all(&word.to_le_bytes())?;
        }
        out.flush()?;
    }
    file.set_len(pages * PAGE)?;
    let checksum = crc32c::crc32c(&header[..504]);
    put(&mut header, 504, checksum);
    file.rewind()?;
    file.write_all(&header)?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Library,
        build::{BuildOptions, Tags, TileZooms, pbf::Header, stage},
    };

    #[test]
    fn snapshot_revision_uses_one_word() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut raw = stage::RawStage::new();
        raw.add_node(1, 0.0, 0.0, &Tags::default())?;
        let dictionary = raw.build_dictionary(BuildOptions::default());
        let (dataset, _) = stage::resolve(
            raw,
            &dictionary,
            &crate::build::spill::MemoryBudget::unlimited(),
        )?;
        let (directory, placement_data) =
            Directory::with_target(&dataset, u64::MAX, TileZooms::Consecutive)?;
        let path = temp.path().join("revision.gol");
        let mut file = File::create(&path)?;
        let mut source = Header::default();
        source.sequence = u64::from(u32::MAX) + 1;
        let overflow = write(
            &mut file,
            &dataset,
            &placement_data,
            Directory {
                words: directory.words.clone(),
                tips: directory.tips.clone(),
                zooms: directory.zooms,
            },
            &source,
            &dictionary,
        )
        .unwrap_err();
        assert!(
            overflow
                .to_string()
                .contains("replication sequence does not fit")
        );
        assert_eq!(file.metadata()?.len(), 0);

        source.sequence = u64::from(u32::MAX);
        write(
            &mut file,
            &dataset,
            &placement_data,
            directory,
            &source,
            &dictionary,
        )?;
        let bytes = std::fs::read(path)?;
        assert_eq!(&bytes[128..132], &u32::MAX.to_le_bytes());
        assert_eq!(&bytes[132..136], &[0; 4]);
        assert_eq!(Library::new(bytes)?.revision()?, source.sequence);
        Ok(())
    }
}
