use crate::{
    Coordinates, Error, Feature, FeatureType, Library, Point, Result, WayNode, WayNodes,
    decode::Decode, links::read_original_way_node_id, source::Source,
};
use sha2::{Digest, Sha256};

const MAGIC: [u8; 8] = *b"RGOLWAY\x01";
const HEADER_BYTES: usize = 64;
const ENTRY_BYTES: usize = 40;
const CHECKPOINT_BYTES: usize = 24;
const DIGEST_BYTES: usize = 32;

#[derive(Clone, Copy)]
struct Entry {
    anchor: u64,
    body: u64,
    way_id: u64,
    count: u32,
    ids_start: u32,
    ids_end: u32,
    first_checkpoint: u32,
}

#[derive(Clone, Copy)]
struct Checkpoint {
    coordinate_offset: u32,
    id_offset: u32,
    point: Point,
    id: u64,
}

/// Optional restart points for a single immutable GOL with original node IDs.
///
/// A sidecar indexes the way records returned by an unbounded traversal. An
/// alternate tile copy of a way or a way below the minimum size uses ordinary
/// decoding. Ordinals are zero-based and include the repeated closing vertex
/// of an area way. The GOL itself stays readable by existing readers. Keep the
/// source bytes stable for the entire lifetime of the index.
pub struct WayCheckpointIndex<'a, S> {
    library: &'a Library<S>,
    stride: u32,
    minimum: u32,
    source_digest: [u8; DIGEST_BYTES],
    entries: Vec<Entry>,
    checkpoints: Vec<Checkpoint>,
}

impl<'a, S: Source> WayCheckpointIndex<'a, S> {
    /// Scan ways and save one restart after every `stride`th stored vertex,
    /// starting with vertex zero. The minimum excludes an area's closing vertex.
    ///
    /// `max_bytes` caps the serialized index size. The scan validates indexed
    /// coordinate and ID streams, and hashes the complete GOL for binding.
    pub fn build(
        library: &'a Library<S>,
        stride: u32,
        minimum: u32,
        max_bytes: usize,
    ) -> Result<Self> {
        require_ids(library)?;
        parameters(stride, minimum)?;
        check_budget(0, 0, max_bytes)?;
        let mut entries = Vec::new();
        let mut checkpoints = Vec::new();
        for item in library.features().restrict(6, 0) {
            let way = item?;
            if way.kind() != FeatureType::Way {
                continue;
            }
            let mut coordinates = way.coordinates()?;
            let count = coordinates.stored_count();
            if count < u64::from(minimum) {
                continue;
            }
            let count = u32::try_from(count).map_err(|_| Error::Limit("way checkpoints"))?;
            let checkpoint_count = count.div_ceil(stride) as usize;
            let first_checkpoint = checkpoints.len();
            check_budget(
                entries.len() + 1,
                first_checkpoint.saturating_add(checkpoint_count),
                max_bytes,
            )?;
            let body = way.body()?;
            for ordinal in 0..count {
                let point = coordinates.next().expect("declared coordinate")?;
                if ordinal % stride == 0 {
                    checkpoints.push(Checkpoint {
                        coordinate_offset: relative(coordinates.position(), body)?,
                        id_offset: 0,
                        point,
                        id: 0,
                    });
                }
            }
            let mut ids = OriginalWayNodeIds::at(way, coordinates.position(), u64::from(count));
            let ids_start = relative(ids.position, body)?;
            for ordinal in 0..count {
                let id = ids.next().expect("declared node ID")?;
                if ordinal % stride == 0 {
                    let checkpoint =
                        &mut checkpoints[first_checkpoint + (ordinal / stride) as usize];
                    checkpoint.id_offset = relative(ids.position, body)?;
                    checkpoint.id = id;
                }
            }
            entries.push(Entry {
                anchor: way.anchor(),
                body,
                way_id: way.id().id,
                count,
                ids_start,
                ids_end: relative(ids.position, body)?,
                first_checkpoint: first_checkpoint as u32,
            });
        }
        // Write groups in directory order, irrespective of traversal order.
        entries.sort_unstable_by_key(|entry| entry.anchor);
        let mut sorted_checkpoints = Vec::with_capacity(checkpoints.len());
        for entry in &mut entries {
            let start = entry.first_checkpoint as usize;
            let end = start + entry.count.div_ceil(stride) as usize;
            entry.first_checkpoint = sorted_checkpoints.len() as u32;
            sorted_checkpoints.extend_from_slice(&checkpoints[start..end]);
        }
        let index = Self {
            library,
            stride,
            minimum,
            source_digest: digest_source(library.source())?,
            entries,
            checkpoints: sorted_checkpoints,
        };
        index.validate_layout()?;
        Ok(index)
    }

    /// Open a serialized sidecar built for this exact immutable GOL.
    ///
    /// The loader checks the sidecar checksum and record layout, and reads the
    /// complete GOL once to verify its SHA-256. Use a sidecar built by a trusted
    /// caller, since seeking uses its saved decoder states directly. The
    /// checksums detect corruption and mismatched input files.
    pub fn from_bytes(library: &'a Library<S>, bytes: &[u8]) -> Result<Self> {
        require_ids(library)?;
        let header = bytes
            .first_chunk::<HEADER_BYTES>()
            .ok_or(Error::WayCheckpoints("truncated header"))?;
        if header[..8] != MAGIC {
            return Err(Error::WayCheckpoints("unsupported sidecar format"));
        }
        let stride = u32_at(header, 8);
        let minimum = u32_at(header, 12);
        parameters(stride, minimum)?;
        let entry_count = u32_at(header, 16) as usize;
        let checkpoint_count = u32_at(header, 20) as usize;
        let expected_len = serialized_size(entry_count, checkpoint_count);
        if expected_len != Some(bytes.len()) {
            return Err(Error::WayCheckpoints("invalid sidecar length"));
        }
        let payload_end = bytes.len() - DIGEST_BYTES;
        if Sha256::digest(&bytes[..payload_end])[..] != bytes[payload_end..] {
            return Err(Error::WayCheckpoints("invalid sidecar checksum"));
        }
        let source_digest = header[32..64].try_into().expect("digest bytes");
        if u64_at(header, 24) != library.source().len() {
            return Err(Error::WayCheckpoints("sidecar belongs to a different GOL"));
        }

        let mut entries = Vec::with_capacity(entry_count);
        let mut checkpoints = Vec::with_capacity(checkpoint_count);
        let rows_end = HEADER_BYTES + entry_count * ENTRY_BYTES;
        for row in bytes[HEADER_BYTES..rows_end].chunks_exact(ENTRY_BYTES) {
            entries.push(Entry {
                anchor: u64_at(row, 0),
                body: u64_at(row, 8),
                way_id: u64_at(row, 16),
                count: u32_at(row, 24),
                ids_start: u32_at(row, 28),
                ids_end: u32_at(row, 32),
                first_checkpoint: u32_at(row, 36),
            });
        }
        for row in bytes[rows_end..payload_end].chunks_exact(CHECKPOINT_BYTES) {
            checkpoints.push(Checkpoint {
                coordinate_offset: u32_at(row, 0),
                id_offset: u32_at(row, 4),
                point: Point {
                    x: u32_at(row, 8) as i32,
                    y: u32_at(row, 12) as i32,
                },
                id: u64_at(row, 16),
            });
        }
        let index = Self {
            library,
            stride,
            minimum,
            source_digest,
            entries,
            checkpoints,
        };
        index.validate_layout()?;
        if digest_source(library.source())? != source_digest {
            return Err(Error::WayCheckpoints("sidecar belongs to a different GOL"));
        }
        Ok(index)
    }

    /// Encode the versioned sidecar, including its source digest and checksum.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.len_bytes());
        bytes.extend(MAGIC);
        bytes.extend(self.stride.to_le_bytes());
        bytes.extend(self.minimum.to_le_bytes());
        bytes.extend((self.entries.len() as u32).to_le_bytes());
        bytes.extend((self.checkpoints.len() as u32).to_le_bytes());
        bytes.extend(self.library.source().len().to_le_bytes());
        bytes.extend(self.source_digest);
        for entry in &self.entries {
            bytes.extend(entry.anchor.to_le_bytes());
            bytes.extend(entry.body.to_le_bytes());
            bytes.extend(entry.way_id.to_le_bytes());
            bytes.extend(entry.count.to_le_bytes());
            bytes.extend(entry.ids_start.to_le_bytes());
            bytes.extend(entry.ids_end.to_le_bytes());
            bytes.extend(entry.first_checkpoint.to_le_bytes());
        }
        for checkpoint in &self.checkpoints {
            bytes.extend(checkpoint.coordinate_offset.to_le_bytes());
            bytes.extend(checkpoint.id_offset.to_le_bytes());
            bytes.extend(checkpoint.point.x.to_le_bytes());
            bytes.extend(checkpoint.point.y.to_le_bytes());
            bytes.extend(checkpoint.id.to_le_bytes());
        }
        let digest = Sha256::digest(&bytes);
        bytes.extend(digest);
        bytes
    }

    /// Size of the serialized index in bytes.
    pub fn len_bytes(&self) -> usize {
        serialized_size(self.entries.len(), self.checkpoints.len()).expect("checked sidecar length")
    }

    /// Number of ways with checkpoint entries.
    pub fn indexed_ways(&self) -> usize {
        self.entries.len()
    }

    /// Total number of saved positions in the index.
    pub fn checkpoint_count(&self) -> usize {
        self.checkpoints.len()
    }

    /// A way's vertices in order, using the saved ID-stream offset when present.
    pub fn way_nodes(&self, way: Feature<'a, S>) -> Result<WayNodes<'a, S>> {
        let Some(entry) = self.entry(way)? else {
            return way.way_nodes();
        };
        let coordinates = way.coordinates()?;
        Self::check_count(entry, coordinates.stored_count())?;
        WayNodes::with_ids(
            way,
            coordinates,
            Some(entry.body + u64::from(entry.ids_start)),
            u64::from(entry.count),
        )
    }

    /// The original node IDs in way order, including an area's closing ID.
    pub fn original_node_ids(&self, way: Feature<'a, S>) -> Result<OriginalWayNodeIds<'a, S>> {
        let Some(entry) = self.entry(way)? else {
            return way.original_way_node_ids();
        };
        let mut position = entry.body;
        Self::check_count(entry, way.library.source.varint(&mut position)?)?;
        Ok(OriginalWayNodeIds::at(
            way,
            entry.body + u64::from(entry.ids_start),
            u64::from(entry.count),
        ))
    }

    /// Find one vertex, including its original ID. Returns `None` for an
    /// ordinal beyond the last vertex. On an indexed way, decodes at most
    /// `stride - 1` coordinate and ID pairs after a restart point.
    pub fn vertex(&self, way: Feature<'a, S>, mut ordinal: usize) -> Result<Option<WayNode>> {
        let Some(entry) = self.entry(way)? else {
            for (index, node) in way.way_nodes()?.enumerate() {
                let node = node?;
                if index == ordinal {
                    return Ok(Some(node));
                }
            }
            return Ok(None);
        };
        let mut position = entry.body;
        Self::check_count(entry, way.library.source.varint(&mut position)?)?;
        if ordinal == entry.count as usize && way.is_area() {
            ordinal = 0;
        }
        if ordinal >= entry.count as usize {
            return Ok(None);
        }
        let ordinal = ordinal as u32;
        let group = ordinal / self.stride;
        let checkpoint = self.checkpoints[(entry.first_checkpoint + group) as usize];
        let skip = ordinal % self.stride;
        if skip == 0 {
            return Ok(Some(WayNode {
                point: checkpoint.point,
                id: Some(checkpoint.id),
            }));
        }
        let count = u64::from(entry.count - group * self.stride - 1);
        let mut coordinates = Coordinates::restarted(
            way,
            entry.body + u64::from(checkpoint.coordinate_offset),
            checkpoint.point,
            count,
        );
        let mut ids =
            OriginalWayNodeIds::at(way, entry.body + u64::from(checkpoint.id_offset), count);
        ids.current = checkpoint.id as i64;
        let mut node = WayNode {
            point: checkpoint.point,
            id: Some(checkpoint.id),
        };
        for _ in 0..skip {
            node.point = coordinates.next().expect("checked vertex ordinal")?;
            node.id = Some(ids.next().expect("checked vertex ordinal")?);
        }
        if coordinates.position() > entry.body + u64::from(entry.ids_start)
            || ids.position > entry.body + u64::from(entry.ids_end)
        {
            return Err(Error::WayCheckpoints("restart exceeds way stream"));
        }
        Ok(Some(node))
    }

    fn check_count(entry: Entry, count: u64) -> Result<()> {
        if count != u64::from(entry.count) {
            return Err(Error::WayCheckpoints("way vertex count mismatch"));
        }
        Ok(())
    }

    fn entry(&self, way: Feature<'a, S>) -> Result<Option<Entry>> {
        if !std::ptr::eq(way.library, self.library) {
            return Err(Error::WayCheckpoints(
                "feature belongs to a different library",
            ));
        }
        if way.kind() != FeatureType::Way {
            return Err(Error::WrongFeatureType);
        }
        let Ok(index) = self
            .entries
            .binary_search_by_key(&way.anchor(), |entry| entry.anchor)
        else {
            return Ok(None);
        };
        let entry = self.entries[index];
        if way.id().id != entry.way_id || way.body()? != entry.body {
            return Err(Error::WayCheckpoints("sidecar way record mismatch"));
        }
        if u64::from(entry.count) > way.library.limits.max_coordinates {
            return Err(Error::Limit("way coordinates"));
        }
        way.check(entry.body, u64::from(entry.ids_end))?;
        Ok(Some(entry))
    }

    fn validate_layout(&self) -> Result<()> {
        let mut previous_anchor = None;
        let mut first_checkpoint = 0usize;
        let size = self.library.source().len();
        for entry in &self.entries {
            let checkpoints = entry.count.div_ceil(self.stride) as usize;
            if previous_anchor.is_some_and(|anchor| anchor >= entry.anchor)
                || entry.anchor.checked_add(16).is_none_or(|end| end > size)
                || entry
                    .body
                    .checked_add(u64::from(entry.ids_end))
                    .is_none_or(|end| end > size)
                || entry.count < self.minimum
                || u64::from(entry.ids_start) <= u64::from(entry.count) * 2
                || entry
                    .ids_end
                    .checked_sub(entry.ids_start)
                    .is_none_or(|bytes| bytes < entry.count)
                || entry.first_checkpoint as usize != first_checkpoint
            {
                return Err(Error::WayCheckpoints("invalid way directory entry"));
            }
            let remaining = self
                .checkpoints
                .get(first_checkpoint..)
                .and_then(|rows| rows.get(..checkpoints))
                .ok_or(Error::WayCheckpoints("invalid checkpoint count"))?;
            let mut prev_coordinate_offset = 0;
            let mut prev_id_offset = entry.ids_start;
            for (group, checkpoint) in remaining.iter().enumerate() {
                let vertices = if group == 0 { 1 } else { self.stride };
                if checkpoint
                    .coordinate_offset
                    .checked_sub(prev_coordinate_offset)
                    .is_none_or(|bytes| u64::from(bytes) < u64::from(vertices) * 2)
                    || checkpoint.coordinate_offset > entry.ids_start
                    || checkpoint
                        .id_offset
                        .checked_sub(prev_id_offset)
                        .is_none_or(|bytes| bytes < vertices)
                    || checkpoint.id_offset > entry.ids_end
                    || checkpoint.id > i64::MAX as u64
                {
                    return Err(Error::WayCheckpoints("invalid checkpoint state"));
                }
                prev_coordinate_offset = checkpoint.coordinate_offset;
                prev_id_offset = checkpoint.id_offset;
            }
            first_checkpoint += checkpoints;
            previous_anchor = Some(entry.anchor);
        }
        if first_checkpoint != self.checkpoints.len() {
            return Err(Error::WayCheckpoints("unused checkpoints"));
        }
        Ok(())
    }
}

impl<'a, S: Source> Feature<'a, S> {
    /// The original node IDs in way order, without decoding coordinates twice.
    /// Requires a GOL with original way-node IDs.
    pub fn original_way_node_ids(self) -> Result<OriginalWayNodeIds<'a, S>> {
        if self.kind() != FeatureType::Way {
            return Err(Error::WrongFeatureType);
        }
        require_ids(self.library)?;
        let mut position = self.body()?;
        let source = self.library.source();
        let count = source.varint(&mut position)?;
        if count > self.library.limits.max_coordinates {
            return Err(Error::Limit("way node IDs"));
        }
        for _ in 0..count {
            source.varint(&mut position)?;
            source.varint(&mut position)?;
            self.check(position - 1, 1)?;
        }
        Ok(OriginalWayNodeIds::at(self, position, count))
    }
}

/// Iterator over the stored original node IDs of a way.
pub struct OriginalWayNodeIds<'a, S> {
    way: Feature<'a, S>,
    position: u64,
    current: i64,
    remaining: u64,
    first: Option<u64>,
    done: bool,
}

impl<'a, S: Source> OriginalWayNodeIds<'a, S> {
    fn at(way: Feature<'a, S>, position: u64, remaining: u64) -> Self {
        Self {
            way,
            position,
            current: 0,
            remaining,
            first: None,
            done: false,
        }
    }
}

impl<S: Source> Iterator for OriginalWayNodeIds<'_, S> {
    type Item = Result<u64>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if self.remaining == 0 {
            self.done = true;
            return if self.way.is_area() {
                self.first.map(Ok)
            } else {
                None
            };
        }
        let result = read_original_way_node_id(self.way, &mut self.position, &mut self.current);
        match result {
            Ok(id) => {
                self.first.get_or_insert(id);
                self.remaining -= 1;
            }
            Err(_) => self.done = true,
        }
        Some(result)
    }
}

impl<S: Source> std::iter::FusedIterator for OriginalWayNodeIds<'_, S> {}

fn require_ids<S: Source>(library: &Library<S>) -> Result<()> {
    if library.has_way_node_ids() {
        Ok(())
    } else {
        Err(Error::WayCheckpoints("GOL has no original way node IDs"))
    }
}

fn parameters(stride: u32, minimum: u32) -> Result<()> {
    if stride == 0 || minimum == 0 {
        return Err(Error::WayCheckpoints("stride and minimum must be positive"));
    }
    Ok(())
}

fn relative(position: u64, body: u64) -> Result<u32> {
    u32::try_from(position - body).map_err(|_| Error::Limit("way checkpoints"))
}

fn check_budget(entries: usize, checkpoints: usize, max_bytes: usize) -> Result<()> {
    if u32::try_from(entries).is_err()
        || u32::try_from(checkpoints).is_err()
        || serialized_size(entries, checkpoints).is_none_or(|size| size > max_bytes)
    {
        return Err(Error::Limit("way checkpoints"));
    }
    Ok(())
}

fn serialized_size(entries: usize, checkpoints: usize) -> Option<usize> {
    (HEADER_BYTES + DIGEST_BYTES)
        .checked_add(entries.checked_mul(ENTRY_BYTES)?)?
        .checked_add(checkpoints.checked_mul(CHECKPOINT_BYTES)?)
}

fn digest_source(source: &impl Source) -> Result<[u8; DIGEST_BYTES]> {
    if let Some(bytes) = source.as_bytes() {
        return Ok(Sha256::digest(bytes).into());
    }
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    let mut offset = 0;
    while offset < source.len() {
        let length = (source.len() - offset).min(buffer.len() as u64) as usize;
        source.read_exact_at(offset, &mut buffer[..length])?;
        hasher.update(&buffer[..length]);
        offset += length as u64;
    }
    Ok(hasher.finalize().into())
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("u32 field"))
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("u64 field"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_rejects_malformed_coordinates_and_original_ids() -> Result<()> {
        let original = &include_bytes!("../tests/fixtures/coincident_ids.gol")[..];
        let library = Library::new(original)?;
        let way = library
            .features()
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .find(|feature| feature.kind() == FeatureType::Way)
            .expect("fixture way");
        let mut coordinates = way.coordinates()?;
        let coordinate_offset = coordinates.position() as usize;
        for _ in 0..coordinates.stored_count() {
            coordinates.next().expect("declared coordinate")?;
        }
        let id_offset = coordinates.position() as usize;
        for offset in [coordinate_offset, id_offset] {
            let mut malformed = original.to_vec();
            malformed[offset..offset + 10].fill(0xff);
            let library = Library::new(malformed)?;
            assert!(matches!(
                WayCheckpointIndex::build(&library, 2, 1, 4096),
                Err(Error::Invalid { .. }),
            ));
        }
        Ok(())
    }
}
