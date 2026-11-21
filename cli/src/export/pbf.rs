use super::{Export, ExportResult, osm_coordinates};
use miniz_oxide::{
    DataFormat,
    deflate::core::{CompressorOxide, TDEFLFlush, TDEFLStatus, compress},
};
use regol::{Feature, Point, TagValue, source::Source};
use rustc_hash::FxHashMap;
use std::{
    io::{self, Write},
    sync::mpsc,
    thread::{self, Scope},
};

const BLOCK_TARGET: usize = 512 * 1024;
const BLOCK_LIMIT: usize = 32 * 1024 * 1024;

pub(super) fn write<S: Source>(
    export: &Export<'_, S>,
    writer: &mut impl Write,
    threads: Option<usize>,
) -> ExportResult<()> {
    let parallel = threads != Some(1) && thread::available_parallelism().is_ok_and(|n| n.get() > 1);
    thread::scope(|scope| {
        let mut file = Pipeline::new(writer, scope, parallel);
        write_blocks(export, &mut file)?;
        file.finish()?;
        Ok(())
    })
}

fn write_blocks<S: Source>(
    export: &Export<'_, S>,
    file: &mut Pipeline<'_, '_, impl Write>,
) -> ExportResult<()> {
    let mut header = Vec::new();
    bytes(&mut header, 4, b"OsmSchema-V0.6");
    bytes(&mut header, 4, b"DenseNodes");
    bytes(&mut header, 5, b"Sort.Type_then_ID");
    if !export.has_way_node_ids {
        bytes(&mut header, 5, b"LocationsOnWays");
    }
    bytes(&mut header, 16, b"regol");
    file.encoder.block(b"OSMHeader", &header, file.writer)?;

    let mut block = Block::default();
    let mut dense = Dense::default();
    export.visit_nodes(|id, point, feature| {
        dense.add(id, point, feature, &mut block)?;
        if dense.size() + block.table.len() >= BLOCK_TARGET {
            dense.flush(&mut block, file)?;
        }
        Ok(())
    })?;
    dense.flush(&mut block, file)?;

    let mut object = Vec::new();
    let mut fields = Fields::default();
    for (index, &way) in export.ways.iter().enumerate() {
        object.clear();
        scalar(&mut object, 1, way.id().id);
        fields.tags(way, &mut block, &mut object)?;
        let mut previous = 0;
        if let Some(refs) = export.way_refs(index) {
            for &id in refs {
                delta(&mut fields.first, id as i64, &mut previous);
            }
        } else {
            let (mut lat, mut lon) = (0, 0);
            for vertex in way.way_nodes()? {
                let vertex = vertex?;
                delta(
                    &mut fields.first,
                    vertex.id.unwrap_or(0) as i64,
                    &mut previous,
                );
                let (y, x) = osm_coordinates(vertex.point);
                delta(&mut fields.second, y, &mut lat);
                delta(&mut fields.third, x, &mut lon);
            }
        }
        fields.pack(&mut object, 8, 9, 10);
        block.add(3, &object, file)?;
    }
    block.flush(file)?;

    for &relation in &export.relations {
        object.clear();
        scalar(&mut object, 1, relation.id().id);
        fields.tags(relation, &mut block, &mut object)?;
        let mut previous = 0;
        for member in relation.members()? {
            let member = member?;
            let id = member.feature.id();
            varint(&mut fields.first, block.intern(&member.role, true));
            delta(&mut fields.second, id.id as i64, &mut previous);
            varint(&mut fields.third, id.kind as u64);
        }
        fields.pack(&mut object, 8, 9, 10);
        block.add(4, &object, file)?;
    }
    block.flush(file)?;
    Ok(())
}

// OSM-binary uses protobuf varints and length-delimited messages.
fn varint(buffer: &mut Vec<u8>, mut value: u64) {
    while value >= 128 {
        buffer.push(value as u8 | 128);
        value >>= 7;
    }
    buffer.push(value as u8);
}

fn scalar(buffer: &mut Vec<u8>, field: u8, value: u64) {
    varint(buffer, u64::from(field) << 3);
    varint(buffer, value);
}

fn bytes(buffer: &mut Vec<u8>, field: u8, value: &[u8]) {
    varint(buffer, (u64::from(field) << 3) | 2);
    varint(buffer, value.len() as u64);
    buffer.extend_from_slice(value);
}

fn delta(buffer: &mut Vec<u8>, value: i64, previous: &mut i64) {
    let difference = value - *previous;
    varint(buffer, ((difference << 1) ^ (difference >> 63)) as u64);
    *previous = value;
}

#[derive(Default)]
struct Buffers {
    data: Vec<u8>,
    encoded: Vec<u8>,
}

struct Worker {
    submit: mpsc::SyncSender<Buffers>,
    completed: mpsc::Receiver<io::Result<Buffers>>,
}

impl Worker {
    fn start<'scope>(scope: &'scope Scope<'scope, '_>) -> io::Result<Self> {
        let (submit, pending) = mpsc::sync_channel::<Buffers>(1);
        let (completed, receive) = mpsc::sync_channel(1);
        thread::Builder::new().spawn_scoped(scope, move || {
            let mut encoder = Encoder::new();
            while let Ok(mut buffers) = pending.recv() {
                buffers.encoded.clear();
                let result = encoder
                    .block(b"OSMData", &buffers.data, &mut buffers.encoded)
                    .map(|()| buffers);
                if completed.send(result).is_err() {
                    break;
                }
            }
        })?;
        Ok(Self {
            submit,
            completed: receive,
        })
    }

    fn write_completed(&self, writer: &mut impl Write) -> io::Result<Buffers> {
        let buffers = self.completed.recv().map_err(io::Error::other)??;
        writer.write_all(&buffers.encoded)?;
        Ok(buffers)
    }
}

struct Pipeline<'scope, 'env, W> {
    writer: &'env mut W,
    scope: &'scope Scope<'scope, 'env>,
    encoder: Encoder,
    worker: Option<Worker>,
    parallel: bool,
}

impl<'scope, 'env, W: Write> Pipeline<'scope, 'env, W> {
    fn new(writer: &'env mut W, scope: &'scope Scope<'scope, 'env>, parallel: bool) -> Self {
        Self {
            writer,
            scope,
            encoder: Encoder::new(),
            worker: None,
            parallel,
        }
    }

    fn block(&mut self, data: &mut Vec<u8>) -> io::Result<()> {
        // Keep one block in flight so that compression overlaps with encoding.
        // Small exports and runs on a single CPU use the caller's encoder.
        let (worker, mut buffers) = match &mut self.worker {
            Some(worker) => {
                let buffers = worker.write_completed(self.writer)?;
                (worker, buffers)
            }
            None if self.parallel && data.len() >= BLOCK_TARGET => {
                match Worker::start(self.scope) {
                    Ok(worker) => (self.worker.insert(worker), Buffers::default()),
                    Err(_) => {
                        self.parallel = false;
                        return self.encoder.block(b"OSMData", data, self.writer);
                    }
                }
            }
            None => return self.encoder.block(b"OSMData", data, self.writer),
        };
        buffers.data.clear();
        std::mem::swap(data, &mut buffers.data);
        worker
            .submit
            .send(buffers)
            .map_err(|_| io::Error::other("OSM PBF compression worker stopped"))
    }

    fn finish(self) -> io::Result<()> {
        if let Some(worker) = self.worker {
            worker.write_completed(self.writer)?;
        }
        Ok(())
    }
}

struct Encoder {
    compressor: CompressorOxide,
    compressed: Vec<u8>,
    header: Vec<u8>,
    blob: Vec<u8>,
}

impl Encoder {
    fn new() -> Self {
        let mut compressor = CompressorOxide::default();
        compressor.set_format_and_level(DataFormat::Zlib, 1);
        Self {
            compressor,
            compressed: Vec::new(),
            header: Vec::with_capacity(32),
            blob: Vec::with_capacity(32),
        }
    }

    fn block(&mut self, kind: &[u8], data: &[u8], writer: &mut impl Write) -> io::Result<()> {
        if data.len() >= BLOCK_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "OSM PBF block exceeds 32 MiB",
            ));
        }
        self.header.clear();
        self.blob.clear();
        // Require enough savings to cover the compressed blob's extra framing.
        let limit = data.len().saturating_sub(9);
        if self.compressed.len() < limit {
            self.compressed.resize(limit, 0);
        }
        self.compressor.reset();
        let (status, consumed, written) = compress(
            &mut self.compressor,
            data,
            &mut self.compressed[..limit],
            TDEFLFlush::Finish,
        );
        let payload = if status == TDEFLStatus::Done && consumed == data.len() {
            scalar(&mut self.blob, 2, data.len() as u64);
            self.blob.push((3 << 3) | 2);
            &self.compressed[..written]
        } else {
            self.blob.push((1 << 3) | 2);
            data
        };
        varint(&mut self.blob, payload.len() as u64);
        let blob_length = self.blob.len() + payload.len();
        if blob_length >= BLOCK_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "OSM PBF blob exceeds 32 MiB",
            ));
        }
        bytes(&mut self.header, 1, kind);
        scalar(&mut self.header, 3, blob_length as u64);
        writer.write_all(&(self.header.len() as u32).to_be_bytes())?;
        writer.write_all(&self.header)?;
        writer.write_all(&self.blob)?;
        writer.write_all(payload)
    }
}

#[derive(Default)]
struct Block {
    indexes: FxHashMap<Box<str>, u64>,
    table: Vec<u8>,
    group: Vec<u8>,
    data: Vec<u8>,
    numeric_tag: String,
}

impl Block {
    fn intern(&mut self, value: &str, zero_allowed: bool) -> u64 {
        if value.is_empty() && zero_allowed {
            return 0;
        }
        if let Some(&index) = self.indexes.get(value) {
            return index;
        }
        let index = self.indexes.len() as u64 + 1;
        self.indexes.insert(Box::from(value), index);
        if self.table.is_empty() {
            bytes(&mut self.table, 1, b"");
        }
        bytes(&mut self.table, 1, value.as_bytes());
        index
    }

    fn value(&mut self, value: &TagValue<'_>) -> u64 {
        use std::fmt::Write as _;
        match value {
            TagValue::Text(text) => self.intern(text, false),
            number => {
                let mut buffer = std::mem::take(&mut self.numeric_tag);
                buffer.clear();
                write!(buffer, "{number}").expect("writing to a string");
                let index = self.intern(&buffer, false);
                self.numeric_tag = buffer;
                index
            }
        }
    }

    fn add(
        &mut self,
        field: u8,
        object: &[u8],
        file: &mut Pipeline<'_, '_, impl Write>,
    ) -> io::Result<()> {
        bytes(&mut self.group, field, object);
        if self.group.len() + self.table.len() >= BLOCK_TARGET {
            self.flush(file)?;
        }
        Ok(())
    }

    fn flush(&mut self, file: &mut Pipeline<'_, '_, impl Write>) -> io::Result<()> {
        if self.group.is_empty() {
            return Ok(());
        }
        self.data.clear();
        let table = if self.table.is_empty() {
            &[10, 0]
        } else {
            &self.table[..]
        };
        bytes(&mut self.data, 1, table);
        bytes(&mut self.data, 2, &self.group);
        file.block(&mut self.data)?;
        self.group.clear();
        self.table.clear();
        self.indexes.clear();
        Ok(())
    }
}

#[derive(Default)]
struct Dense {
    ids: Vec<u8>,
    lats: Vec<u8>,
    lons: Vec<u8>,
    keys_values: Vec<u8>,
    previous_id: i64,
    previous_lat: i64,
    previous_lon: i64,
    message: Vec<u8>,
}

impl Dense {
    fn size(&self) -> usize {
        self.ids.len() + self.lats.len() + self.lons.len() + self.keys_values.len()
    }

    fn add<S: Source>(
        &mut self,
        id: u64,
        point: Point,
        feature: Option<Feature<'_, S>>,
        block: &mut Block,
    ) -> regol::Result<()> {
        let (lat, lon) = osm_coordinates(point);
        delta(&mut self.ids, id as i64, &mut self.previous_id);
        delta(&mut self.lats, lat, &mut self.previous_lat);
        delta(&mut self.lons, lon, &mut self.previous_lon);
        if let Some(feature) = feature {
            for tag in feature.tags()? {
                let tag = tag?;
                varint(&mut self.keys_values, block.intern(&tag.key, false));
                varint(&mut self.keys_values, block.value(&tag.value));
            }
        }
        self.keys_values.push(0);
        Ok(())
    }

    fn flush(
        &mut self,
        block: &mut Block,
        file: &mut Pipeline<'_, '_, impl Write>,
    ) -> io::Result<()> {
        if !self.ids.is_empty() {
            self.message.clear();
            bytes(&mut self.message, 1, &self.ids);
            bytes(&mut self.message, 8, &self.lats);
            bytes(&mut self.message, 9, &self.lons);
            bytes(&mut self.message, 10, &self.keys_values);
            bytes(&mut block.group, 2, &self.message);
            block.flush(file)?;
            self.ids.clear();
            self.lats.clear();
            self.lons.clear();
            self.keys_values.clear();
            self.previous_id = 0;
            self.previous_lat = 0;
            self.previous_lon = 0;
        }
        Ok(())
    }
}

#[derive(Default)]
struct Fields {
    first: Vec<u8>,
    second: Vec<u8>,
    third: Vec<u8>,
}

impl Fields {
    fn tags<S: Source>(
        &mut self,
        feature: Feature<'_, S>,
        block: &mut Block,
        object: &mut Vec<u8>,
    ) -> regol::Result<()> {
        for tag in feature.tags()? {
            let tag = tag?;
            varint(&mut self.first, block.intern(&tag.key, false));
            varint(&mut self.second, block.value(&tag.value));
        }
        packed(object, 2, &mut self.first);
        packed(object, 3, &mut self.second);
        Ok(())
    }

    fn pack(&mut self, object: &mut Vec<u8>, first: u8, second: u8, third: u8) {
        packed(object, first, &mut self.first);
        packed(object, second, &mut self.second);
        packed(object, third, &mut self.third);
    }
}

fn packed(object: &mut Vec<u8>, field: u8, buffer: &mut Vec<u8>) {
    if !buffer.is_empty() {
        bytes(object, field, buffer);
        buffer.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::{BLOCK_TARGET, Encoder, Pipeline};
    use miniz_oxide::inflate::decompress_to_vec_zlib;
    use std::{io, thread};

    #[test]
    fn raw_blocks_do_not_interrupt_compression() {
        let mut output = Vec::new();
        let mut encoder = Encoder::new();
        let mut seed = 9813u32;
        let incompressible: Vec<_> = (0..8192)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect();

        for data in [&b"tiny"[..], incompressible.as_slice()] {
            encoder.block(b"OSMHeader", data, &mut output).unwrap();
            assert_eq!(encoder.blob[0], (1 << 3) | 2);
            assert!(output.ends_with(data));
        }

        let data = [0; 8192];
        let start = output.len();
        encoder.block(b"OSMData", &data, &mut output).unwrap();
        assert_eq!(encoder.blob[0], 2 << 3);
        let payload = start + 4 + encoder.header.len() + encoder.blob.len();
        assert_eq!(decompress_to_vec_zlib(&output[payload..]).unwrap(), data);
    }

    #[test]
    fn pipelined_blocks_match_serial_order_and_content() {
        let blocks = [
            vec![19; 8],
            vec![7; BLOCK_TARGET],
            vec![255; BLOCK_TARGET],
            vec![91; 32],
        ];
        let mut expected = Vec::new();
        let mut encoder = Encoder::new();
        for data in &blocks {
            encoder.block(b"OSMData", data, &mut expected).unwrap();
        }

        let mut output = Vec::new();
        thread::scope(|scope| {
            let mut file = Pipeline::new(&mut output, scope, true);
            for mut data in blocks {
                file.block(&mut data).unwrap();
            }
            file.finish().unwrap();
        });
        assert_eq!(output, expected);
    }

    #[test]
    fn pipelined_block_propagates_output_error() {
        struct ClosedWriter;

        impl io::Write for ClosedWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut writer = ClosedWriter;
        let error = thread::scope(|scope| {
            let mut file = Pipeline::new(&mut writer, scope, true);
            file.block(&mut vec![0; BLOCK_TARGET]).unwrap();
            file.finish().unwrap_err()
        });
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }
}
