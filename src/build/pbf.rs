// The fields of fileformat.proto and osmformat.proto needed for current snapshots.
// Metadata fields are skipped by protobuf. Each frame is decoded independently.
use super::{
    object_id,
    spill::{MemoryBudget, RawSpillSink},
    stage::{self, RawMember, RawNode, RawRelation, RawStage, RawTaggedNode, RawWay, WorkerStage},
    worker_count,
};
use anyhow::{Context, Result, bail, ensure};
use flate2::{Decompress, FlushDecompress, Status};
use prost::Message;
use std::{
    io::BufRead,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

#[derive(Message, Clone)]
struct BlobHeader {
    #[prost(string, tag = "1")]
    kind: String,
    #[prost(int32, tag = "3")]
    size: i32,
}
#[derive(Message, Clone)]
struct Blob {
    #[prost(bytes, optional, tag = "1")]
    raw: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "2")]
    raw_size: Option<i32>,
    #[prost(bytes, optional, tag = "3")]
    zlib: Option<Vec<u8>>,
}
#[derive(Message, Clone)]
pub(super) struct Header {
    #[prost(string, repeated, tag = "4")]
    required: Vec<String>,
    #[prost(uint64, optional, tag = "32")]
    pub timestamp: Option<u64>,
    #[prost(uint64, tag = "33")]
    pub sequence: u64,
}
#[derive(Message, Clone)]
struct Block {
    #[prost(message, optional, tag = "1")]
    table: Option<StringTable>,
    #[prost(message, repeated, tag = "2")]
    groups: Vec<Group>,
    #[prost(int32, optional, tag = "17", default = "100")]
    granularity: Option<i32>,
    #[prost(int64, tag = "19")]
    lat_offset: i64,
    #[prost(int64, tag = "20")]
    lon_offset: i64,
}
#[derive(Message, Clone)]
struct StringTable {
    #[prost(bytes, repeated, tag = "1")]
    strings: Vec<Vec<u8>>,
}
#[derive(Message, Clone)]
struct Group {
    #[prost(message, repeated, tag = "1")]
    nodes: Vec<Node>,
    #[prost(message, optional, tag = "2")]
    dense: Option<Dense>,
    #[prost(message, repeated, tag = "3")]
    ways: Vec<Way>,
    #[prost(message, repeated, tag = "4")]
    relations: Vec<Relation>,
}
#[derive(Message, Clone)]
struct Node {
    #[prost(sint64, tag = "1")]
    id: i64,
    #[prost(uint32, repeated, tag = "2")]
    keys: Vec<u32>,
    #[prost(uint32, repeated, tag = "3")]
    vals: Vec<u32>,
    #[prost(sint64, optional, tag = "8")]
    lat: Option<i64>,
    #[prost(sint64, optional, tag = "9")]
    lon: Option<i64>,
}
#[derive(Message, Clone)]
struct Dense {
    #[prost(sint64, repeated, tag = "1")]
    ids: Vec<i64>,
    #[prost(sint64, repeated, tag = "8")]
    lats: Vec<i64>,
    #[prost(sint64, repeated, tag = "9")]
    lons: Vec<i64>,
    #[prost(int32, repeated, tag = "10")]
    tags: Vec<i32>,
}
#[derive(Message, Clone)]
struct Way {
    #[prost(int64, tag = "1")]
    id: i64,
    #[prost(uint32, repeated, tag = "2")]
    keys: Vec<u32>,
    #[prost(uint32, repeated, tag = "3")]
    vals: Vec<u32>,
    #[prost(sint64, repeated, tag = "8")]
    refs: Vec<i64>,
}
#[derive(Message, Clone)]
struct Relation {
    #[prost(int64, tag = "1")]
    id: i64,
    #[prost(uint32, repeated, tag = "2")]
    keys: Vec<u32>,
    #[prost(uint32, repeated, tag = "3")]
    vals: Vec<u32>,
    #[prost(int32, repeated, tag = "8")]
    roles: Vec<i32>,
    #[prost(sint64, repeated, tag = "9")]
    refs: Vec<i64>,
    #[prost(int32, repeated, tag = "10")]
    types: Vec<i32>,
}

const MAX_BLOB: usize = 32 * 1024 * 1024;

fn decompress_blob_into<'a>(
    bytes: &[u8],
    decoder: &mut Decompress,
    buf: &'a mut Vec<u8>,
) -> Result<&'a [u8]> {
    let blob = Blob::decode(bytes)?;
    match (blob.raw, blob.zlib) {
        (Some(data), None) => {
            *buf = data;
            Ok(buf.as_slice())
        }
        (None, Some(zlib)) => {
            let size = blob.raw_size.context("missing raw_size")?;
            ensure!((0..=MAX_BLOB as i32).contains(&size), "invalid raw_size");
            let target = size as usize;
            buf.clear();
            buf.resize(target + 1, 0);
            decoder.reset(true);
            let status = decoder.decompress(&zlib, buf, FlushDecompress::Finish)?;
            ensure!(
                status == Status::StreamEnd
                    && decoder.total_out() == size as u64
                    && decoder.total_in() == zlib.len() as u64,
                "incomplete zlib stream or PBF size mismatch"
            );
            buf.truncate(target);
            Ok(buf.as_slice())
        }
        _ => bail!("expected a raw or zlib-compressed PBF blob"),
    }
}

fn read_frame(input: &mut impl BufRead) -> Result<Option<(String, Vec<u8>)>> {
    if input.fill_buf()?.is_empty() {
        return Ok(None);
    }
    let mut len = [0; 4];
    input.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    ensure!(
        (1..64 * 1024).contains(&len),
        "invalid PBF blob header size"
    );
    let mut bytes = vec![0; len];
    input.read_exact(&mut bytes)?;
    let frame = BlobHeader::decode(&*bytes)?;
    ensure!(
        (1..=(MAX_BLOB as i32)).contains(&frame.size),
        "invalid PBF blob size"
    );
    bytes.resize(frame.size as usize, 0);
    input.read_exact(&mut bytes)?;
    Ok(Some((frame.kind, bytes)))
}

fn parse_header(bytes: &[u8]) -> Result<Header> {
    let mut decoder = Decompress::new(true);
    let mut buf = Vec::new();
    let data = decompress_blob_into(bytes, &mut decoder, &mut buf)?;
    let decoded = Header::decode(data)?;
    ensure!(
        decoded.required.iter().any(|s| s == "OsmSchema-V0.6"),
        "missing OsmSchema-V0.6"
    );
    for feature in &decoded.required {
        ensure!(
            matches!(feature.as_str(), "OsmSchema-V0.6" | "DenseNodes"),
            "unsupported required PBF feature {feature}"
        );
    }
    Ok(decoded)
}

pub(super) fn read(mut input: impl BufRead, budget: &MemoryBudget) -> Result<(Header, RawStage)> {
    let workers = budget.max_workers(worker_count().min(32));
    let mut header = None;
    let sink = RawSpillSink::new();

    if workers <= 1 {
        let mut worker = WorkerStage::new();
        let mut decoder = Decompress::new(true);
        let mut buf = Vec::new();
        let mut seq = 0usize;
        while let Some((kind, bytes)) = read_frame(&mut input)? {
            if kind == "OSMHeader" {
                ensure!(header.is_none(), "duplicate OSMHeader");
                header = Some(parse_header(&bytes)?);
            } else if kind == "OSMData" {
                ensure!(header.is_some(), "OSMData before OSMHeader");
                let data = decompress_blob_into(&bytes, &mut decoder, &mut buf)?;
                Block::decode(data)?.decode_into(seq, &mut worker)?;
                worker.maybe_spill(budget, &sink, false)?;
                seq += 1;
            }
        }
        if budget.is_spilling() {
            worker.maybe_spill(budget, &sink, true)?;
        }
        let header = header.context("missing OSMHeader")?;
        return Ok((header, RawStage::from_workers(vec![worker], sink)));
    }

    let capacity = workers * 2;
    let (raw_tx, raw_rx) = mpsc::sync_channel::<(usize, Vec<u8>)>(capacity);
    let raw_rx = Arc::new(Mutex::new(raw_rx));
    let cancelled = AtomicBool::new(false);

    let (header, worker_stages): (Option<Header>, Vec<WorkerStage>) =
        std::thread::scope(|scope| -> Result<(Option<Header>, Vec<WorkerStage>)> {
            let mut handles = Vec::with_capacity(workers);
            for _ in 0..workers {
                let raw_rx = Arc::clone(&raw_rx);
                let cancelled = &cancelled;
                let sink = &sink;
                handles.push(scope.spawn(move || -> Result<WorkerStage> {
                    let mut worker = WorkerStage::new();
                    let mut decoder = Decompress::new(true);
                    let mut buf = Vec::new();
                    loop {
                        if cancelled.load(Ordering::Relaxed) {
                            break;
                        }
                        let job = {
                            let Ok(rx) = raw_rx.lock() else { break };
                            rx.recv()
                        };
                        let Ok((seq, bytes)) = job else { break };
                        if cancelled.load(Ordering::Relaxed) {
                            break;
                        }
                        let res = decompress_blob_into(&bytes, &mut decoder, &mut buf)
                            .and_then(|data| Block::decode(data)?.decode_into(seq, &mut worker))
                            .and_then(|()| worker.maybe_spill(budget, sink, false));
                        if let Err(err) = res {
                            cancelled.store(true, Ordering::Relaxed);
                            return Err(err);
                        }
                    }
                    if !cancelled.load(Ordering::Relaxed) && budget.is_spilling() {
                        worker.maybe_spill(budget, sink, true)?;
                    }
                    Ok(worker)
                }));
            }

            // Only workers own the receiver while the producer is sending.
            // Their exit must wake a producer blocked on a full queue.
            drop(raw_rx);
            let mut seq = 0usize;
            let read_res = (|| -> Result<()> {
                while let Some((kind, bytes)) = read_frame(&mut input)? {
                    if kind == "OSMHeader" {
                        ensure!(header.is_none(), "duplicate OSMHeader");
                        header = Some(parse_header(&bytes)?);
                        continue;
                    }
                    if kind != "OSMData" {
                        continue;
                    }
                    ensure!(header.is_some(), "OSMData before OSMHeader");
                    if cancelled.load(Ordering::Relaxed) {
                        break;
                    }
                    if raw_tx.send((seq, bytes)).is_err() {
                        break;
                    }
                    seq += 1;
                }
                Ok(())
            })();
            drop(raw_tx);

            let mut stages = Vec::with_capacity(workers);
            let mut worker_err = None;
            for handle in handles {
                match handle.join() {
                    Ok(Ok(stage)) => stages.push(stage),
                    Ok(Err(err)) => {
                        if worker_err.is_none() {
                            worker_err = Some(err);
                        }
                    }
                    Err(_) => {
                        if worker_err.is_none() {
                            worker_err = Some(anyhow::anyhow!("PBF decoder worker panicked"));
                        }
                    }
                }
            }
            read_res?;
            if let Some(err) = worker_err {
                return Err(err);
            }
            Ok((header, stages))
        })?;

    let header = header.context("missing OSMHeader")?;
    Ok((header, RawStage::from_workers(worker_stages, sink)))
}

#[inline]
fn advance(value: &mut i64, delta: i64) -> Result<i64> {
    *value = value.checked_add(delta).context("PBF delta overflow")?;
    Ok(*value)
}

struct BlockStrings<'a> {
    raw: &'a [Vec<u8>],
    cache: Vec<Option<u32>>,
}

impl<'a> BlockStrings<'a> {
    fn new(table: &'a StringTable) -> Self {
        Self {
            raw: &table.strings,
            cache: vec![None; table.strings.len()],
        }
    }

    #[inline]
    fn get(&mut self, index: u32, worker: &mut WorkerStage) -> Result<u32> {
        let idx = index as usize;
        let slot = self
            .cache
            .get_mut(idx)
            .context("undefined PBF string index")?;
        if let Some(id) = *slot {
            return Ok(id);
        }
        let bytes = &self.raw[idx];
        ensure!(bytes.len() <= 32767, "string exceeds GOL format limit");
        let s = std::str::from_utf8(bytes)?;
        let id = worker.intern(s);
        *slot = Some(id);
        Ok(id)
    }

    fn push_tags(
        &mut self,
        keys: &[u32],
        vals: &[u32],
        worker: &mut WorkerStage,
    ) -> Result<(usize, u32)> {
        ensure!(keys.len() == vals.len(), "mismatched tag arrays");
        let start = worker.next_tag_offset();
        worker.tags.reserve(keys.len());
        for (&k, &v) in keys.iter().zip(vals) {
            let kid = self.get(k, worker)?;
            let vid = self.get(v, worker)?;
            worker.count(kid, true);
            worker.count(vid, false);
            worker.tags.push((kid, vid));
        }
        Ok((start, keys.len() as u32))
    }
}

impl Block {
    fn decode_into(self, seq: usize, worker: &mut WorkerStage) -> Result<()> {
        let granularity = i64::from(self.granularity());
        ensure!(granularity > 0, "invalid coordinate granularity");
        let table = self.table.context("missing PBF string table")?;
        let mut strings = BlockStrings::new(&table);
        let coordinate = |v: i64, offset: i64| -> Result<f64> {
            let n = v
                .checked_mul(granularity)
                .and_then(|v| v.checked_add(offset))
                .context("PBF coordinate overflow")?;
            Ok(n as f64 / 1e9)
        };

        let mut block_nodes = Vec::new();
        let mut prev_node_id = i64::MIN;
        let mut nodes_sorted = true;

        for group in self.groups {
            if !group.nodes.is_empty() {
                block_nodes.reserve(group.nodes.len());
                for n in group.nodes {
                    let lon = coordinate(n.lon.context("missing longitude")?, self.lon_offset)?;
                    let lat = coordinate(n.lat.context("missing latitude")?, self.lat_offset)?;
                    let (x, y) = stage::project(n.id, lon, lat)?;
                    object_id(0, n.id)?;
                    if n.id <= prev_node_id {
                        nodes_sorted = false;
                    }
                    prev_node_id = n.id;
                    block_nodes.push(RawNode { id: n.id, x, y });
                    if !n.keys.is_empty() || !n.vals.is_empty() {
                        let (tag_start, tag_len) = strings.push_tags(&n.keys, &n.vals, worker)?;
                        if tag_len > 0 {
                            worker.tagged_nodes.push(RawTaggedNode {
                                id: n.id,
                                tag_start,
                                tag_len,
                            });
                        }
                    }
                }
            }
            if let Some(d) = group.dense {
                ensure!(
                    d.ids.len() == d.lons.len() && d.ids.len() == d.lats.len(),
                    "mismatched dense node arrays"
                );
                block_nodes.reserve(d.ids.len());
                let (mut id, mut lon, mut lat) = (0i64, 0i64, 0i64);
                let has_tags = !d.tags.is_empty();
                let mut kv = d.tags.iter().copied();
                for ((di, dx), dy) in d.ids.into_iter().zip(d.lons).zip(d.lats) {
                    let tag_start = worker.next_tag_offset();
                    if has_tags {
                        loop {
                            let k = kv.next().context("unterminated dense node tags")?;
                            if k == 0 {
                                break;
                            }
                            let v = kv.next().context("missing dense value")?;
                            let kid = strings.get(k as u32, worker)?;
                            let vid = strings.get(v as u32, worker)?;
                            worker.count(kid, true);
                            worker.count(vid, false);
                            worker.tags.push((kid, vid));
                        }
                    }
                    let tag_len = (worker.next_tag_offset() - tag_start) as u32;
                    let raw_id = advance(&mut id, di)?;
                    let lon_deg = coordinate(advance(&mut lon, dx)?, self.lon_offset)?;
                    let lat_deg = coordinate(advance(&mut lat, dy)?, self.lat_offset)?;
                    let (px, py) = stage::project(raw_id, lon_deg, lat_deg)?;
                    object_id(0, raw_id)?;
                    if raw_id <= prev_node_id {
                        nodes_sorted = false;
                    }
                    prev_node_id = raw_id;
                    block_nodes.push(RawNode {
                        id: raw_id,
                        x: px,
                        y: py,
                    });
                    if tag_len > 0 {
                        worker.tagged_nodes.push(RawTaggedNode {
                            id: raw_id,
                            tag_start,
                            tag_len,
                        });
                    }
                }
                ensure!(kv.next().is_none(), "extra dense node tags");
            }
            if !group.ways.is_empty() {
                worker.ways.reserve(group.ways.len());
                for w in group.ways {
                    object_id(1, w.id)?;
                    let ref_start = worker.way_refs.len();
                    let ref_len = w.refs.len() as u32;
                    worker.way_refs.reserve(w.refs.len());
                    let mut nid = 0i64;
                    for d in w.refs {
                        let n = advance(&mut nid, d)?;
                        object_id(0, n)?;
                        worker.way_refs.push(n);
                    }
                    let (tag_start, tag_len) = if w.keys.is_empty() && w.vals.is_empty() {
                        (0, 0)
                    } else {
                        strings.push_tags(&w.keys, &w.vals, worker)?
                    };
                    worker.ways.push(RawWay {
                        id: w.id,
                        tag_start,
                        ref_start,
                        tag_len,
                        ref_len,
                    });
                }
            }
            if !group.relations.is_empty() {
                worker.relations.reserve(group.relations.len());
                for r in group.relations {
                    ensure!(
                        r.refs.len() == r.types.len() && r.refs.len() == r.roles.len(),
                        "mismatched relation member arrays"
                    );
                    object_id(2, r.id)?;
                    let (tag_start, tag_len) = if r.keys.is_empty() && r.vals.is_empty() {
                        (0, 0)
                    } else {
                        strings.push_tags(&r.keys, &r.vals, worker)?
                    };
                    let mem_start = worker.rel_members.len();
                    let mem_len = r.refs.len() as u32;
                    worker.rel_members.reserve(r.refs.len());
                    let mut mid = 0i64;
                    for ((delta, kind), role) in r.refs.into_iter().zip(r.types).zip(r.roles) {
                        ensure!((0..=2).contains(&kind), "invalid member type");
                        let child_id = advance(&mut mid, delta)?;
                        object_id(i64::from(kind), child_id)?;
                        let role_id = strings.get(role as u32, worker)?;
                        worker.count(role_id, false);
                        worker.rel_members.push(RawMember {
                            child_id,
                            role: role_id,
                            kind: kind as u8,
                        });
                    }
                    worker.relations.push(RawRelation {
                        id: r.id,
                        tag_start,
                        mem_start,
                        tag_len,
                        mem_len,
                    });
                }
            }
        }
        if !block_nodes.is_empty() {
            worker.node_chunks.push((seq, nodes_sorted, block_nodes));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FeatureType, Library, build::build_pbf};
    use std::fs;

    fn frame(kind: &str, data: Vec<u8>) -> Vec<u8> {
        let blob = Blob {
            raw: Some(data),
            ..Default::default()
        }
        .encode_to_vec();
        let head = BlobHeader {
            kind: kind.into(),
            size: blob.len() as i32,
        }
        .encode_to_vec();
        let mut bytes = (head.len() as u32).to_be_bytes().to_vec();
        bytes.extend(head);
        bytes.extend(blob);
        bytes
    }

    #[test]
    fn raw_unordered_subset_with_simple_nodes_and_missing_references() -> Result<()> {
        let mut input = frame(
            "OSMHeader",
            Header {
                required: vec!["OsmSchema-V0.6".into()],
                timestamp: Some(1_500_000_000),
                sequence: 42,
            }
            .encode_to_vec(),
        );
        let id = 9_123_456_789;
        let groups = vec![
            Group {
                relations: vec![Relation {
                    id: 2,
                    roles: vec![0, 1, 1],
                    types: vec![1, 0, 1],
                    refs: vec![4, id - 4, 5 - id],
                    ..Default::default()
                }],
                ..Default::default()
            },
            Group {
                ways: vec![
                    Way {
                        id: 4,
                        refs: vec![id, 1],
                        ..Default::default()
                    },
                    Way {
                        id: 5,
                        refs: vec![id, 9],
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            Group {
                nodes: vec![
                    Node {
                        id,
                        lon: Some(0),
                        lat: Some(0),
                        ..Default::default()
                    },
                    Node {
                        id: id + 1,
                        lon: Some(100),
                        lat: Some(-100),
                        ..Default::default()
                    },
                    Node {
                        id: id + 2,
                        lon: Some(200),
                        lat: Some(100),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
        ];
        let block = Block {
            table: Some(StringTable {
                strings: vec![vec![], b"stop".to_vec()],
            }),
            granularity: Some(1000),
            lat_offset: -1_000_000_000,
            lon_offset: 2_000_000_000,
            groups,
        };
        input.extend(frame("OSMData", block.encode_to_vec()));
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("raw.pbf");
        let dest = dir.path().join("raw.gol");
        fs::write(&src, &input)?;
        let report = build_pbf(&src, &dest)?;
        assert_eq!(
            (report.features, report.omitted_ways, report.omitted_members),
            (4, 1, 1)
        );
        let bytes = fs::read(&dest)?;
        assert_eq!(u64::from_le_bytes(bytes[128..136].try_into()?), 42);
        assert_eq!(
            u64::from_le_bytes(bytes[136..144].try_into()?),
            1_500_000_000_000
        );
        let lib = Library::open(&dest)?;
        for feature in lib.features() {
            let f = feature?;
            if f.kind() == FeatureType::Node && f.id().id == id as u64 {
                use crate::units::angle::degree;
                let position = f.point()?.unwrap().lon_lat();
                let x = position.longitude.get::<degree>();
                let y = position.latitude.get::<degree>();
                assert!((x - 2.0).abs() < 1e-7 && (y + 1.0).abs() < 1e-7);
            }
            if f.kind() == FeatureType::Relation {
                let ids = f
                    .members()?
                    .map(|m| m.map(|m| m.feature.id().id))
                    .collect::<crate::Result<Vec<_>>>()?;
                assert_eq!(ids, [4, id as u64]);
            }
        }
        Ok(())
    }

    #[test]
    fn history_and_invalid_required_features_are_rejected() -> Result<()> {
        for feature in ["HistoricalInformation", "Unknown"] {
            let bytes = frame(
                "OSMHeader",
                Header {
                    required: vec!["OsmSchema-V0.6".into(), feature.into()],
                    ..Default::default()
                }
                .encode_to_vec(),
            );
            assert!(
                read(&*bytes, &MemoryBudget::unlimited())
                    .err()
                    .unwrap()
                    .to_string()
                    .contains(feature)
            );
        }
        Ok(())
    }
}
