use crate::{
    Error, Feature, Library, Result,
    decode::{Decode, ReadWindow, add},
    feature::TileExtent,
    source::Source,
};

mod dictionary;
mod integrity;

pub(crate) use dictionary::{
    DictionaryEquality, DictionaryKey, DictionaryPair, DictionaryProbe, DictionarySet,
    DictionaryTruthy, DictionaryUnion, EqualityProbe,
};

/// Integer Mercator coordinates used by GOL, without lossy projection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Point {
    /// East-west position in the GOL integer Mercator projection.
    pub x: i32,
    /// North-south position in the GOL integer Mercator projection.
    pub y: i32,
}
/// An axis-aligned, inclusive rectangle in integer Mercator coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    /// Southwest corner, including its boundary.
    pub min: Point,
    /// Northeast corner, including its boundary.
    pub max: Point,
}
impl Bounds {
    /// The full range of representable projected coordinates.
    pub const WORLD: Self = Self {
        min: Point {
            x: i32::MIN,
            y: i32::MIN,
        },
        max: Point {
            x: i32::MAX,
            y: i32::MAX,
        },
    };
    /// Construct a bounding box. Either axis being inverted returns an error.
    #[inline]
    pub fn new(min_x: i32, min_y: i32, max_x: i32, max_y: i32) -> Result<Self> {
        if min_x > max_x || min_y > max_y {
            return Err(Error::invalid(0, "inverted bounds"));
        }
        Ok(Self {
            min: Point { x: min_x, y: min_y },
            max: Point { x: max_x, y: max_y },
        })
    }
    /// Whether the two rectangles overlap or touch.
    #[inline]
    pub fn intersects(self, b: Self) -> bool {
        self.min.x <= b.max.x
            && self.max.x >= b.min.x
            && self.min.y <= b.max.y
            && self.max.y >= b.min.y
    }
    /// Whether the rectangle contains the point, including its boundary.
    #[inline]
    pub fn contains(self, p: Point) -> bool {
        p.x >= self.min.x && p.x <= self.max.x && p.y >= self.min.y && p.y <= self.max.y
    }
    pub(crate) fn read<S: Source>(s: &S, p: u64) -> Result<Self> {
        let b = s.array::<16>(p)?;
        Self::from_record(&b)
    }
    #[inline]
    pub(crate) fn from_record(b: &[u8; 16]) -> Result<Self> {
        Self::new(
            i32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            i32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            i32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            i32::from_le_bytes([b[12], b[13], b[14], b[15]]),
        )
    }
}

struct LeafRecord<'a> {
    flags: u16,
    coordinates: &'a [u8],
    header: &'a [u8; 12],
}

impl<'a> LeafRecord<'a> {
    #[inline(always)]
    fn checked<const NODE: bool>(bytes: &'a [u8]) -> Option<Self> {
        let record = Self::read::<NODE>(bytes)?;
        // A kind that disagrees with the leaf layout needs the full decoder.
        record.has_layout::<NODE>().then_some(record)
    }

    #[inline(always)]
    fn read<const NODE: bool>(bytes: &'a [u8]) -> Option<Self> {
        let (coordinates, header) = if NODE {
            bytes.first_chunk::<20>()?.split_at(8)
        } else {
            bytes.first_chunk::<32>()?.split_at(16)
        };
        let flags = u16::from_le_bytes(header[..2].try_into().unwrap());
        Some(Self {
            flags,
            coordinates,
            header: header[..12].try_into().unwrap(),
        })
    }

    #[inline(always)]
    fn has_layout<const NODE: bool>(&self) -> bool {
        if NODE {
            self.flags & 24 == 0
        } else {
            matches!(self.flags & 24, 8 | 16)
        }
    }

    #[inline(always)]
    fn pointer(&self) -> i32 {
        i32::from_le_bytes(self.header[8..].try_into().unwrap())
    }

    #[inline(always)]
    fn bbox(&self) -> Result<Bounds> {
        Bounds::from_record(self.coordinates.try_into().expect("box in non-node record"))
    }

    #[inline(always)]
    fn point(&self) -> Point {
        Point {
            x: i32::from_le_bytes(self.coordinates[..4].try_into().unwrap()),
            y: i32::from_le_bytes(self.coordinates[4..8].try_into().unwrap()),
        }
    }
}

#[inline(always)]
fn next_leaf(p: u64, node: bool, flags: u16) -> Option<u64> {
    (flags & 1 == 0).then(|| p + if node { 20 + u64::from(flags & 4) } else { 32 })
}

#[derive(Clone, Copy)]
struct Tile {
    entry: u64,
    level: usize,
    col: u32,
    row: u32,
}
impl Tile {
    fn bounds(self, zoom: u8) -> Bounds {
        let size = 1i64 << (32 - zoom);
        let left = i64::from(i32::MIN) + i64::from(self.col) * size;
        let top = i64::from(i32::MAX) - i64::from(self.row) * size;
        Bounds {
            min: Point {
                x: left as i32,
                y: (top - size + 1) as i32,
            },
            max: Point {
                x: (left + size - 1) as i32,
                y: top as i32,
            },
        }
    }
}

#[derive(Clone, Copy)]
enum Work {
    Tile(Tile),
    // The tile-index step was already charged during partitioning. Only its
    // payload/header reads remain; workers check cancellation before starting it.
    #[cfg(feature = "parallel")]
    Payload {
        start: u64,
        bounds: Bounds,
    },
    Children {
        parent: Tile,
        mask: u64,
        entries: u64,
        extent: u32,
        ordinal: u32,
    },
    Roots {
        p: u64,
        node: bool,
    },
    Branch {
        p: u64,
        node: bool,
        depth: usize,
    },
    Leaf {
        p: u64,
        node: bool,
    },
}

#[cfg(feature = "parallel")]
#[derive(Clone, Copy)]
pub(crate) struct DeferredTile {
    pub(crate) start: u64,
    pub(crate) bounds: Bounds,
}

#[cfg(feature = "parallel")]
pub(crate) enum Partitions<'a, S> {
    Wide {
        template: Box<Features<'a, S>>,
        tiles: Vec<DeferredTile>,
    },
    Split(Vec<Features<'a, S>>),
}

enum Advance<'a, S> {
    Feature(Feature<'a, S>),
    End,
    Paused,
}

#[cfg(all(feature = "mmap", target_os = "linux"))]
mod mapped_ahead;

/// Streaming spatial traversal. Uses memory proportional to tree depth, not
/// result count. An error terminates the iterator.
pub struct Features<'a, S> {
    library: &'a Library<S>,
    records: ReadWindow<'a, S>,
    tags: ReadWindow<'a, S>,
    strings: ReadWindow<'a, S>,
    work: Vec<Work>,
    leaf: Option<(u64, bool)>,
    bounds: Bounds,
    prune: bool,
    tile: TileExtent,
    duplicate_flags: u16,
    steps: u64,
    done: bool,
    types: u8,
    required_keys: u32,
    alternative_keys: u32,
    #[cfg(all(feature = "mmap", target_os = "linux"))]
    read_ahead: Option<crate::source::MappedScan<'a>>,
    #[cfg(all(feature = "mmap", target_os = "linux"))]
    mapped_ahead: Option<Box<mapped_ahead::Ahead>>,
}
impl<'a, S: Source> Features<'a, S> {
    pub(crate) fn new(library: &'a Library<S>, bounds: Option<Bounds>) -> Self {
        let work = vec![Work::Tile(Tile {
            entry: library.tile_index + 4,
            level: 0,
            col: 0,
            row: 0,
        })];
        Self {
            library,
            records: ReadWindow::new(&library.source),
            tags: ReadWindow::new(&library.source),
            strings: ReadWindow::new(&library.source),
            work,
            leaf: None,
            bounds: bounds.unwrap_or(Bounds::WORLD),
            prune: bounds.is_some(),
            tile: TileExtent::default(),
            duplicate_flags: 0,
            steps: 0,
            done: false,
            types: 15,
            required_keys: 0,
            alternative_keys: 0,
            #[cfg(all(feature = "mmap", target_os = "linux"))]
            read_ahead: if bounds.is_none() {
                library
                    .source
                    .mapped_file()
                    .map(crate::source::MappedScan::new)
            } else {
                None
            },
            #[cfg(all(feature = "mmap", target_os = "linux"))]
            mapped_ahead: None,
        }
    }
    pub(crate) fn steps(&self) -> u64 {
        self.steps
    }
    pub(crate) fn restrict(mut self, types: u8, required_keys: u32) -> Self {
        self.types = types;
        self.required_keys = required_keys;
        self
    }
    pub(crate) fn require_any(mut self, keys: u32) -> Self {
        self.alternative_keys = keys;
        self
    }
    fn in_tile(&self, p: u64, len: u64) -> Result<()> {
        if p < self.tile.start() || p.checked_add(len).is_none_or(|e| e > self.tile.end()) {
            return Err(Error::invalid(p, "pointer outside tile"));
        }
        Ok(())
    }
    fn start_tile(&mut self, tile: Tile) -> Result<()> {
        if let Some((start, bounds)) = self.index_tile(tile)? {
            #[cfg(all(feature = "mmap", target_os = "linux"))]
            if let Some(scan) = &mut self.read_ahead {
                scan.leave_tile();
                scan.sample(self.steps);
            }
            #[cfg(all(feature = "mmap", target_os = "linux"))]
            self.maybe_start_mapped_ahead();
            #[cfg(all(feature = "mmap", target_os = "linux"))]
            if let Some(ahead) = self.mapped_ahead.as_mut() {
                ahead.advance();
                if ahead.due() {
                    self.refill_mapped_ahead();
                }
            }
            self.load_tile(start, bounds)?;
            #[cfg(all(feature = "mmap", target_os = "linux"))]
            if let Some(scan) = &mut self.read_ahead {
                scan.enter_tile(self.tile.start(), self.tile.end());
            }
        }
        Ok(())
    }
    // Walk only the compact tile index, validating each accessed entry. Payload reads can be
    // dispatched independently after child entries have been discovered.
    fn index_tile(&mut self, mut tile: Tile) -> Result<Option<(u64, Bounds)>> {
        self.resolve_index_tile(&mut tile)
    }

    fn resolve_index_tile(&mut self, tile: &mut Tile) -> Result<Option<(u64, Bounds)>> {
        let lib = self.library;

        let bbox = tile.bounds(lib.zooms[tile.level]);
        if !self.bounds.intersects(bbox) {
            return Ok(None);
        }
        let mut word = lib.index_word(tile.entry)?;
        let branch = word & 3 == 1;
        if branch {
            tile.entry = add(tile.entry, (word as i32 & !3) as i64)?;
            word = lib.index_word(tile.entry)?;
        }
        if branch || tile.level == 0 {
            if let Some(&next_zoom) = lib.zooms.get(tile.level + 1) {
                let extent = 1u32 << (next_zoom - lib.zooms[tile.level]);
                let mask_bytes = if extent == 8 { 8 } else { 4 };
                let mask_lo = u64::from(lib.index_word(tile.entry + 4)?);
                let mask = if mask_bytes == 8 {
                    mask_lo | (u64::from(lib.index_word(tile.entry + 8)?) << 32)
                } else {
                    mask_lo
                };
                if extent < 8 && mask >> (extent * extent) != 0 {
                    return Err(Error::invalid(tile.entry + 4, "invalid child mask"));
                }
                if mask != 0 {
                    self.work.push(Work::Children {
                        parent: *tile,
                        mask,
                        entries: tile.entry + 4 + mask_bytes,
                        extent,
                        ordinal: 0,
                    });
                }
            } else if branch {
                return Err(Error::invalid(tile.entry, "tile depth exceeds zoom levels"));
            }
        }
        if word & 3 != 2 || word >> 2 == 0 {
            return Err(Error::MissingTile(
                ((tile.entry - lib.tile_index) / 4) as u32,
            ));
        }
        Ok(Some((u64::from(word >> 2) * 4096, bbox)))
    }

    pub(crate) fn visit_tile_pages(
        mut self,
        mut visit: impl FnMut(u64, u64) -> Result<()>,
    ) -> Result<()> {
        while let Some(work) = self.work.pop() {
            self.steps += 1;
            if self.steps > self.library.limits.max_traversal_steps {
                return Err(Error::Limit("tile directory traversal"));
            }
            match work {
                Work::Tile(mut tile) => {
                    if let Some((start, _)) = self.resolve_index_tile(&mut tile)? {
                        visit(tile.entry, start)?;
                    }
                }
                Work::Children {
                    parent,
                    mask,
                    entries,
                    extent,
                    ordinal,
                } => {
                    self.push_child(parent, mask, entries, extent, ordinal);
                }
                _ => unreachable!("only tile directory entries are scheduled"),
            }
        }
        Ok(())
    }
    fn load_tile(&mut self, start: u64, bbox: Bounds) -> Result<()> {
        let s = &self.library.source;
        // Read the adjacent length/root fields once. Positioned-I/O sources
        // avoid separate calls, and stable sources perform one checked lookup.
        let header = s.array::<24>(start)?;
        let payload = u64::from(
            u32::from_le_bytes(header[..4].try_into().expect("header length")) & 0x3fff_ffff,
        );
        s.check(start, payload + 4)?;
        if payload < 24 {
            return Err(Error::invalid(start, "short tile"));
        }
        self.tile = TileExtent::new(start, start + 4 + payload)?;
        self.duplicate_flags = if self.bounds.min.x < bbox.min.x {
            64
        } else {
            0
        } | if self.bounds.max.y > bbox.max.y {
            128
        } else {
            0
        };
        for (ofs, node) in [(20, false), (16, false), (12, false), (8, true)] {
            let type_bit = match ofs {
                8 => 1,
                12 => 2,
                16 => 4,
                _ => 8,
            };
            if self.types & type_bit == 0 {
                continue;
            }
            let delta = i32::from_le_bytes(
                header[ofs..ofs + 4]
                    .try_into()
                    .expect("root pointer length"),
            );
            if delta != 0 {
                self.work.push(Work::Roots {
                    p: add(start + ofs as u64, i64::from(delta))?,
                    node,
                });
            }
        }
        Ok(())
    }
    fn push_child(&mut self, parent: Tile, mask: u64, entries: u64, extent: u32, ordinal: u32) {
        let bit = mask.trailing_zeros();
        let remaining = mask & (mask - 1);
        if remaining != 0 {
            self.work.push(Work::Children {
                parent,
                mask: remaining,
                entries,
                extent,
                ordinal: ordinal + 1,
            });
        }
        self.work.push(Work::Tile(Tile {
            entry: entries + u64::from(ordinal) * 4,
            level: parent.level + 1,
            col: parent.col * extent + bit % extent,
            row: parent.row * extent + bit / extent,
        }));
    }
    #[cfg(feature = "parallel")]
    pub(crate) fn partitions(mut self, target: usize) -> Result<(Partitions<'a, S>, u64)> {
        #[cfg(all(feature = "mmap", target_os = "linux"))]
        {
            self.read_ahead = None;
            self.mapped_ahead = None;
        }
        let mut tiles = Vec::new();
        // A bounded serial prefix may have stopped inside the current tile.
        // Preserve its remaining tree work before walking later tile entries.
        let mut current = Vec::new();
        while self
            .work
            .last()
            .is_some_and(|work| !matches!(work, Work::Tile(_) | Work::Children { .. }))
        {
            current.push(self.work.pop().expect("remaining tree work"));
        }
        current.reverse();
        if let Some((p, node)) = self.leaf.take() {
            current.push(Work::Leaf { p, node });
        }
        while let Some(work) = self.work.pop() {
            self.steps += 1;
            if self.steps > self.library.limits.max_traversal_steps {
                return Err(Error::Limit("spatial traversal steps"));
            }
            match work {
                Work::Tile(tile) => {
                    if let Some((start, bounds)) = self.index_tile(tile)? {
                        tiles.push(DeferredTile { start, bounds });
                    }
                }
                Work::Children {
                    parent,
                    mask,
                    entries,
                    extent,
                    ordinal,
                } => {
                    self.push_child(parent, mask, entries, extent, ordinal);
                }
                _ => unreachable!("partitioning visits only tile-index work"),
            }
        }
        // Scanning tile payloads to subdivide them would serialize the first
        // range GET for each tile on a high-latency source.
        if self.library.source().prefers_concurrent_tile_reads()
            || tiles.len() + usize::from(!current.is_empty()) >= target
        {
            let steps = self.steps;
            return Ok((
                Partitions::Wide {
                    template: Box::new(self.fork_work(current)),
                    tiles,
                },
                steps,
            ));
        }
        // A city window may intersect only a few large tiles. Split their
        // R-tree work so tile boundaries do not cap useful parallelism.
        let mut pending = std::collections::VecDeque::new();
        if !current.is_empty() {
            pending.push_back(self.fork_work(current));
        }
        pending.extend(tiles.into_iter().map(|tile| {
            self.fork_work(vec![Work::Payload {
                start: tile.start,
                bounds: tile.bounds,
            }])
        }));
        let mut terminal = Vec::new();
        while pending.len() + terminal.len() < target {
            let Some(mut partition) = pending.pop_front() else {
                break;
            };
            if partition.work.len() == 1 && matches!(partition.work[0], Work::Leaf { .. }) {
                terminal.push(partition);
                continue;
            }
            if partition.work.len() == 1 {
                let work = partition.work.pop().expect("one work item");
                // Loading a deferred payload completes the tile-index step
                // already counted above. Tree expansion still costs one step.
                if !matches!(work, Work::Payload { .. }) {
                    self.steps += 1;
                    if self.steps > self.library.limits.max_traversal_steps {
                        return Err(Error::Limit("spatial traversal steps"));
                    }
                }
                match work {
                    Work::Payload { start, bounds } => partition.load_tile(start, bounds)?,
                    Work::Roots { p, node } => partition.expand_root(p, node)?,
                    Work::Branch { p, node, depth } => partition.expand_branch(p, node, depth)?,
                    _ => unreachable!("tile work was already partitioned"),
                }
            }
            if let Some((p, node)) = partition.leaf.take() {
                partition.work.push(Work::Leaf { p, node });
            }
            let work = std::mem::take(&mut partition.work);
            for work in work {
                pending.push_back(partition.fork_work(vec![work]));
            }
        }
        terminal.extend(pending);
        Ok((Partitions::Split(terminal), self.steps))
    }
    #[cfg(feature = "parallel")]
    pub(crate) fn worker_cursor(&self) -> Self {
        self.fork_work(Vec::new())
    }
    #[cfg(feature = "parallel")]
    pub(crate) fn has_remaining_work(&self) -> bool {
        !self.work.is_empty()
    }
    #[cfg(feature = "parallel")]
    pub(crate) fn fork_remaining(&self) -> Self {
        self.fork_work(self.work.clone())
    }
    #[cfg(feature = "parallel")]
    pub(crate) fn defer_tile(&mut self, tile: DeferredTile) {
        self.records.clear();
        self.tags.clear();
        self.strings.clear();
        self.work.clear();
        self.work.push(Work::Payload {
            start: tile.start,
            bounds: tile.bounds,
        });
        self.leaf = None;
        self.steps = 0;
        self.done = false;
    }
    #[cfg(feature = "parallel")]
    fn fork_work(&self, work: Vec<Work>) -> Self {
        Self {
            library: self.library,
            records: ReadWindow::new(&self.library.source),
            tags: ReadWindow::new(&self.library.source),
            strings: ReadWindow::new(&self.library.source),
            work,
            leaf: None,
            bounds: self.bounds,
            prune: self.prune,
            tile: self.tile,
            duplicate_flags: self.duplicate_flags,
            steps: 0,
            done: false,
            types: self.types,
            required_keys: self.required_keys,
            alternative_keys: self.alternative_keys,
            #[cfg(all(feature = "mmap", target_os = "linux"))]
            read_ahead: None,
            #[cfg(all(feature = "mmap", target_os = "linux"))]
            mapped_ahead: None,
        }
    }
    #[cfg(feature = "parallel")]
    pub(crate) fn finished(&self) -> bool {
        self.done
    }
    fn root(&self, p: u64) -> Result<(bool, Option<u64>)> {
        let s = &self.library.source;

        self.in_tile(p, 8)?;
        let record = s.array::<8>(p)?;
        let word = i32::from_le_bytes(record[..4].try_into().expect("four root-pointer bytes"));
        let categories = u32::from_le_bytes(record[4..].try_into().expect("four category bytes"));
        let matches = categories & self.required_keys == self.required_keys
            && (self.alternative_keys == 0 || categories & self.alternative_keys != 0);
        let target = matches.then(|| add(p, (word & !3) as i64)).transpose()?;
        Ok((word & 1 == 0, target))
    }
    fn push_root(&mut self, p: u64, node: bool, more: bool, target: Option<u64>) {
        if more {
            self.work.push(Work::Roots { p: p + 8, node });
        }
        if let Some(p) = target {
            self.work.push(Work::Branch { p, node, depth: 0 });
        }
    }
    #[cfg(feature = "parallel")]
    fn expand_root(&mut self, p: u64, node: bool) -> Result<()> {
        let (more, target) = self.root(p)?;
        self.push_root(p, node, more, target);
        Ok(())
    }
    // Rejected roots are consecutive records. Keep scanning in place until
    // there is a subtree to visit or the caller's traversal budget runs out.
    fn scan_roots(
        &mut self,
        mut p: u64,
        node: bool,
        step: &mut impl FnMut() -> Result<bool>,
    ) -> Result<bool> {
        loop {
            let (more, target) = self.root(p)?;
            if !more || target.is_some() {
                self.push_root(p, node, more, target);
                return Ok(true);
            }
            p += 8;
            if !self.try_step(step)? {
                self.work.push(Work::Roots { p, node });
                return Ok(false);
            }
        }
    }
    #[inline]
    fn branch_record(&self, p: u64) -> Result<(i32, Bounds)> {
        self.in_tile(p, 20)?;
        let record = self.library.source.array::<20>(p)?;
        let word = i32::from_le_bytes(record[..4].try_into().expect("four branch-pointer bytes"));
        let bbox = Bounds::from_record(record[4..].try_into().expect("sixteen bounds bytes"))?;
        Ok((word, bbox))
    }
    fn expand_branch(&mut self, p: u64, node: bool, depth: usize) -> Result<()> {
        if depth >= self.library.limits.max_tree_depth {
            return Err(Error::Limit("R-tree depth"));
        }
        let (word, bbox) = self.branch_record(p)?;
        let target = if !self.prune || self.bounds.intersects(bbox) {
            Some(add(p, (word & !3) as i64)?)
        } else {
            None
        };
        if word & 1 == 0 {
            self.work.push(Work::Branch {
                p: p + 20,
                node,
                depth,
            });
        }
        if let Some(target) = target {
            if word & 2 != 0 {
                // The child is next in traversal order, including after a pause.
                self.leaf = Some((target, node));
            } else {
                self.work.push(Work::Branch {
                    p: target,
                    node,
                    depth: depth + 1,
                });
            }
        }
        Ok(())
    }
    // A conclusive probe returns the record flags and the first predicate's result.
    // A missing or incomplete page takes the ordinary checked decoding path.
    #[inline(always)]
    fn probe_window<const NODE: bool>(
        &self,
        p: u64,
        equality: impl DictionaryProbe,
    ) -> Option<(u16, bool)> {
        let end = self.tile.end();
        let leaf = LeafRecord::checked::<NODE>(self.records.tail(p, end)?)?;
        if !NODE {
            leaf.bbox().ok()?;
        }
        let ptr = leaf.pointer();
        let table = (p + if NODE { 16 } else { 24 }).checked_add_signed(i64::from(ptr & !1))?;
        if table < self.tile.start() {
            return None;
        }
        let table = self
            .records
            .tail(table, end)
            .filter(|bytes| bytes.len() >= 4)
            .or_else(|| self.tags.tail(table, end).filter(|bytes| bytes.len() >= 4))
            .or_else(|| {
                let length = usize::try_from(end.checked_sub(table)?).unwrap_or(usize::MAX);
                let tail = self.library.source.retained_tail(table)?;
                Some(&tail[..length.min(tail.len())])
            })?;
        match equality.probe(
            table,
            self.library.string_count,
            self.library.limits.max_tags,
            0,
        ) {
            EqualityProbe::Reject => Some((leaf.flags, false)),
            EqualityProbe::Match => Some((leaf.flags, true)),
            EqualityProbe::Unknown => None,
        }
    }

    #[inline(always)]
    fn try_step(&mut self, step: &mut impl FnMut() -> Result<bool>) -> Result<bool> {
        if !step()? {
            return Ok(false);
        }
        self.steps += 1;
        if self.steps > self.library.limits.max_traversal_steps {
            return Err(Error::Limit("spatial traversal steps"));
        }
        Ok(true)
    }

    #[inline(always)]
    fn leaf_candidate<const EXACT: bool>(
        &mut self,
        p: u64,
        node: bool,
        predicate: &mut impl FnMut(
            Feature<'a, S>,
            &mut ReadWindow<'a, S>,
            &mut ReadWindow<'a, S>,
            bool,
        ) -> Result<bool>,
    ) -> Result<Option<Feature<'a, S>>> {
        self.in_tile(p, if node { 20 } else { 32 })?;
        if EXACT {
            // The tile check above establishes the full record's extent. A
            // contiguous source can borrow it directly at its absolute offset.
            let record = if let Some(bytes) = self.library.source.as_bytes() {
                let tail = usize::try_from(p).ok().and_then(|start| bytes.get(start..));
                if node {
                    tail.and_then(LeafRecord::checked::<true>)
                } else {
                    tail.and_then(LeafRecord::checked::<false>)
                }
            } else {
                self.records.seek(p)?;
                let end = self.tile.end();
                if node {
                    self.records
                        .record::<20, true>(p, end)
                        .and_then(|record| LeafRecord::checked::<true>(record))
                } else {
                    self.records
                        .record::<32, true>(p, end)
                        .and_then(|record| LeafRecord::checked::<false>(record))
                }
            };
            if let Some(record) = record {
                self.leaf = next_leaf(p, node, record.flags).map(|p| (p, node));
                if record.flags & (0x800 | self.duplicate_flags) != 0 {
                    return Ok(None);
                }
                let in_bounds = if node {
                    !self.prune || self.bounds.contains(record.point())
                } else {
                    self.bounds.intersects(record.bbox()?)
                };
                if !in_bounds {
                    return Ok(None);
                }
                let anchor = p + if node { 8 } else { 16 };
                // in_tile covered the full leaf record and checked() matched
                // its layout. Tile lengths fit u32, including this anchor.
                let feature = Feature::from_leaf_header(
                    self.library,
                    record.header,
                    self.tile,
                    (anchor - self.tile.start()) as u32,
                );
                return Ok(
                    predicate(feature, &mut self.tags, &mut self.strings, false)?
                        .then_some(feature),
                );
            }
        }
        self.records.seek(p)?;
        self.decode_leaf_candidate::<EXACT>(p, node, false, predicate)
    }

    #[inline(always)]
    fn decode_leaf_candidate<const EXACT: bool>(
        &mut self,
        p: u64,
        node: bool,
        leading_match: bool,
        predicate: &mut impl FnMut(
            Feature<'a, S>,
            &mut ReadWindow<'a, S>,
            &mut ReadWindow<'a, S>,
            bool,
        ) -> Result<bool>,
    ) -> Result<Option<Feature<'a, S>>> {
        let anchor = p + if node { 8 } else { 16 };
        let feature = Feature::at_window::<EXACT>(self.library, &self.records, anchor, self.tile)?;
        let flags = feature.flags();
        self.leaf = next_leaf(p, node, flags).map(|p| (p, node));
        if flags & (0x800 | self.duplicate_flags) != 0 {
            return Ok(None);
        }
        // A point cannot have inverted bounds. For world queries all other
        // range and feature-header checks still apply.
        let in_bounds = (!self.prune && node)
            || self
                .bounds
                .intersects(feature.bounds_window::<EXACT>(&self.records)?);
        if in_bounds && predicate(feature, &mut self.tags, &mut self.strings, leading_match)? {
            return Ok(Some(feature));
        }
        Ok(None)
    }

    // Choose the tag budget once per leaf so the whole-tile path can probe
    // its usual bounded prefix without a dynamic tag limit.
    #[inline]
    fn equality_whole_leaf<const NODE: bool, P: DictionaryProbe>(
        &mut self,
        p: u64,
        bytes: &[u8],
        equality: P,
        predicate: &mut impl FnMut(
            Feature<'a, S>,
            &mut ReadWindow<'a, S>,
            &mut ReadWindow<'a, S>,
            bool,
        ) -> Result<bool>,
        step: &mut impl FnMut() -> Result<bool>,
    ) -> Result<Option<Advance<'a, S>>> {
        match (
            self.library.limits.max_tags >= P::TAG_PROBE_LIMIT,
            self.prune,
        ) {
            (true, false) => {
                let width = if NODE { 20 } else { 32 };
                let offset = (p - self.tile.start()) as usize;
                let remaining = self.library.limits.max_traversal_steps - self.steps;
                // Each successful read advances at least one record width.
                // Include the final step that could encounter a short record.
                if remaining >= ((bytes.len() - offset) / width) as u64 {
                    self.equality_whole_leaf_with_probe::<NODE, true, false, false, _>(
                        p, bytes, equality, predicate, step,
                    )
                } else {
                    self.equality_whole_leaf_with_probe::<NODE, true, false, true, _>(
                        p, bytes, equality, predicate, step,
                    )
                }
            }
            (true, true) => self.equality_whole_leaf_with_probe::<NODE, true, true, true, _>(
                p, bytes, equality, predicate, step,
            ),
            // The bounded and unbounded forms share the budget-limited loop.
            _ => self.equality_whole_leaf_with_probe::<NODE, false, true, true, _>(
                p, bytes, equality, predicate, step,
            ),
        }
    }

    #[inline]
    fn equality_whole_leaf_with_probe<
        const NODE: bool,
        const FULL_PROBE: bool,
        const MAY_PRUNE: bool,
        const CHECK_TRAVERSAL: bool,
        P: DictionaryProbe,
    >(
        &mut self,
        p: u64,
        bytes: &[u8],
        equality: P,
        predicate: &mut impl FnMut(
            Feature<'a, S>,
            &mut ReadWindow<'a, S>,
            &mut ReadWindow<'a, S>,
            bool,
        ) -> Result<bool>,
        step: &mut impl FnMut() -> Result<bool>,
    ) -> Result<Option<Advance<'a, S>>> {
        let base = self.tile.start();
        // A GOL tile has a 30-bit payload length. Even the next record
        // after a truncated leaf fits in a tile-relative u32.
        let mut offset = (p - base) as u32;
        let strings = self.library.string_count;
        let max_tags = if FULL_PROBE {
            P::TAG_PROBE_LIMIT
        } else {
            self.library.limits.max_tags
        };
        let limit = self.library.limits.max_traversal_steps;
        // The caller charged the first record. Publish the cursor and accumulated
        // steps whenever the caller can resume, including a worker prefix pause.
        let mut steps = self.steps;
        let flags_to_skip = 0x800 | self.duplicate_flags;
        let prune = MAY_PRUNE && self.prune;
        let bounds = self.bounds;
        let types = crate::query::Types(self.types);
        let width = if NODE { 20 } else { 32 };
        loop {
            let mut matched = None;
            let start = offset as usize;
            let record_flags = bytes.get(start..start + width).and_then(|raw| {
                let record = LeafRecord::read::<NODE>(raw)?;
                if NODE {
                    // Most node headers have neither malformed kind bits nor
                    // skip flags. Only the exceptional case needs to distinguish.
                    if record.flags & (24 | flags_to_skip) != 0 {
                        return record.has_layout::<NODE>().then_some(record.flags);
                    }
                    if prune && !bounds.contains(record.point()) {
                        return Some(record.flags);
                    }
                } else {
                    if !matches!(record.flags & (24 | flags_to_skip), 8 | 16) {
                        return record.has_layout::<NODE>().then_some(record.flags);
                    }
                    let bbox = record.bbox().ok()?;
                    if prune && !bounds.intersects(bbox) {
                        return Some(record.flags);
                    }
                }
                if !types.matches_flags(record.flags) {
                    return Some(record.flags);
                }
                let ptr = record.pointer();
                let pointer = offset + if NODE { 16 } else { 24 };
                // A negative target wraps past the 30-bit tile length.
                // Nonnegative sums still fit u32 with a signed delta.
                let table = pointer.wrapping_add((ptr & !1) as u32) as usize;
                match equality.probe(bytes, strings, max_tags, table) {
                    EqualityProbe::Reject => Some(record.flags),
                    EqualityProbe::Match => {
                        matched = Some(record.header);
                        Some(record.flags)
                    }
                    EqualityProbe::Unknown => None,
                }
            });
            if let Some(flags) = record_flags {
                if let Some(header) = matched {
                    let p = base + offset as u64;
                    let feature = Feature::from_leaf_header(
                        self.library,
                        header,
                        self.tile,
                        offset + if NODE { 8 } else { 16 },
                    );
                    if predicate(feature, &mut self.tags, &mut self.strings, true)? {
                        self.leaf = next_leaf(p, NODE, flags).map(|p| (p, NODE));
                        self.steps = steps;
                        return Ok(Some(Advance::Feature(feature)));
                    }
                }
                if flags & 1 != 0 {
                    self.steps = steps;
                    return Ok(None);
                }
                offset += width as u32 + if NODE { u32::from(flags & 4) } else { 0 };
            } else {
                let p = base + offset as u64;
                if let Some(feature) = self.leaf_candidate::<false>(p, NODE, predicate)? {
                    self.steps = steps;
                    return Ok(Some(Advance::Feature(feature)));
                }
                let Some((next, _)) = self.leaf.take() else {
                    self.steps = steps;
                    return Ok(None);
                };
                offset = (next - base) as u32;
            }
            if !step()? {
                self.steps = steps;
                self.leaf = Some((base + offset as u64, NODE));
                return Ok(Some(Advance::Paused));
            }
            steps += 1;
            if CHECK_TRAVERSAL && steps > limit {
                return Err(Error::Limit("spatial traversal steps"));
            }
        }
    }

    // The outer traversal has charged the first record.
    #[inline]
    fn equality_leaf<const NODE: bool>(
        &mut self,
        mut p: u64,
        equality: impl DictionaryProbe,
        predicate: &mut impl FnMut(
            Feature<'a, S>,
            &mut ReadWindow<'a, S>,
            &mut ReadWindow<'a, S>,
            bool,
        ) -> Result<bool>,
        step: &mut impl FnMut() -> Result<bool>,
    ) -> Result<Option<Advance<'a, S>>> {
        loop {
            self.in_tile(p, if NODE { 20 } else { 32 })?;
            self.records.seek(p)?;
            let next = match self.probe_window::<NODE>(p, equality) {
                Some((flags, false)) => next_leaf(p, NODE, flags),
                probe => {
                    let leading_match = probe.is_some();
                    if let Some(feature) =
                        self.decode_leaf_candidate::<false>(p, NODE, leading_match, predicate)?
                    {
                        return Ok(Some(Advance::Feature(feature)));
                    }
                    self.leaf.take().map(|(next, _)| next)
                }
            };
            let Some(next) = next else {
                return Ok(None);
            };
            p = next;
            if !self.try_step(step)? {
                self.leaf = Some((p, NODE));
                return Ok(Some(Advance::Paused));
            }
        }
    }

    fn advance<const EXACT: bool, const EQUALITY: bool>(
        &mut self,
        predicate: &mut impl FnMut(
            Feature<'a, S>,
            &mut ReadWindow<'a, S>,
            &mut ReadWindow<'a, S>,
            bool,
        ) -> Result<bool>,
        step: &mut impl FnMut() -> Result<bool>,
        equality: Option<impl DictionaryProbe>,
    ) -> Result<Advance<'a, S>> {
        let lib = self.library;
        while let Some(work) = self
            .leaf
            .take()
            .map(|(p, node)| Work::Leaf { p, node })
            .or_else(|| self.work.pop())
        {
            #[cfg(feature = "parallel")]
            if let Work::Payload { start, bounds } = work {
                self.load_tile(start, bounds)?;
                continue;
            }
            if !self.try_step(step)? {
                match work {
                    Work::Leaf { p, node } => self.leaf = Some((p, node)),
                    work => self.work.push(work),
                }
                return Ok(Advance::Paused);
            }
            match work {
                Work::Tile(tile) => self.start_tile(tile)?,
                #[cfg(feature = "parallel")]
                Work::Payload { .. } => unreachable!("prepaid payload handled before stepping"),
                Work::Children {
                    parent,
                    mask,
                    entries,
                    extent,
                    ordinal,
                } => {
                    self.push_child(parent, mask, entries, extent, ordinal);
                }
                Work::Roots { p, node } => {
                    if !self.scan_roots(p, node, step)? {
                        return Ok(Advance::Paused);
                    }
                }
                Work::Branch { p, node, depth } => self.expand_branch(p, node, depth)?,
                Work::Leaf { p, node } => {
                    if EQUALITY {
                        if let Some(equality) = equality {
                            self.in_tile(p, if node { 20 } else { 32 })?;
                            let tile = lib.source.as_bytes().and_then(|bytes| {
                                let end = usize::try_from(self.tile.end()).ok()?;
                                bytes.get(self.tile.start() as usize..end)
                            });
                            let outcome = match (tile, node) {
                                (Some(tile), true) => self.equality_whole_leaf::<true, _>(
                                    p, tile, equality, predicate, step,
                                )?,
                                (Some(tile), false) => self.equality_whole_leaf::<false, _>(
                                    p, tile, equality, predicate, step,
                                )?,
                                (None, true) => {
                                    self.equality_leaf::<true>(p, equality, predicate, step)?
                                }
                                (None, false) => {
                                    self.equality_leaf::<false>(p, equality, predicate, step)?
                                }
                            };
                            if let Some(outcome) = outcome {
                                return Ok(outcome);
                            }
                            continue;
                        }
                    }
                    let mut current = p;
                    loop {
                        if let Some(feature) =
                            self.leaf_candidate::<EXACT>(current, node, predicate)?
                        {
                            return Ok(Advance::Feature(feature));
                        }
                        let Some((next, _)) = self.leaf.take() else {
                            break;
                        };
                        if !self.try_step(step)? {
                            self.leaf = Some((next, node));
                            return Ok(Advance::Paused);
                        }
                        current = next;
                    }
                }
            }
        }
        Ok(Advance::End)
    }
    pub(crate) fn next_matching(
        &mut self,
        predicate: impl FnMut(
            Feature<'a, S>,
            &mut ReadWindow<'a, S>,
            &mut ReadWindow<'a, S>,
        ) -> Result<bool>,
    ) -> Option<Result<Feature<'a, S>>> {
        self.next_with_budget::<false>(predicate, || Ok(true))
    }
    pub(crate) fn next_with_budget<const EXACT: bool>(
        &mut self,
        predicate: impl FnMut(
            Feature<'a, S>,
            &mut ReadWindow<'a, S>,
            &mut ReadWindow<'a, S>,
        ) -> Result<bool>,
        step: impl FnMut() -> Result<bool>,
    ) -> Option<Result<Feature<'a, S>>> {
        let mut predicate = predicate;
        self.next_scan::<EXACT, false>(
            |feature, window, strings, _| predicate(feature, window, strings),
            step,
            None::<DictionaryEquality>,
        )
    }
    // The final predicate argument is true only for a checked, matching leading
    // tag. Feature type and every remaining predicate still need evaluation.
    pub(crate) fn next_equality(
        &mut self,
        predicate: impl FnMut(
            Feature<'a, S>,
            &mut ReadWindow<'a, S>,
            &mut ReadWindow<'a, S>,
            bool,
        ) -> Result<bool>,
        step: impl FnMut() -> Result<bool>,
        equality: impl DictionaryProbe,
    ) -> Option<Result<Feature<'a, S>>> {
        self.next_scan::<false, true>(predicate, step, Some(equality))
    }
    fn finish_scan(&mut self) {
        #[cfg(all(feature = "mmap", target_os = "linux"))]
        {
            self.read_ahead = None;
            self.mapped_ahead = None;
        }
        self.done = true;
    }

    #[inline]
    fn count_all_whole_leaf<const NODE: bool>(
        &mut self,
        p: u64,
        bytes: &[u8],
        types: crate::query::Types,
        step: &mut impl FnMut() -> Result<bool>,
    ) -> Result<(u64, bool)> {
        if !self.prune {
            let width = if NODE { 20 } else { 32 };
            let offset = (p - self.tile.start()) as usize;
            let remaining = self.library.limits.max_traversal_steps - self.steps;
            if remaining >= ((bytes.len() - offset) / width) as u64 {
                self.count_all_whole_leaf_inner::<NODE, false, false>(p, bytes, types, step)
            } else {
                self.count_all_whole_leaf_inner::<NODE, false, true>(p, bytes, types, step)
            }
        } else {
            self.count_all_whole_leaf_inner::<NODE, true, true>(p, bytes, types, step)
        }
    }

    #[inline]
    fn count_all_whole_leaf_inner<
        const NODE: bool,
        const MAY_PRUNE: bool,
        const CHECK_TRAVERSAL: bool,
    >(
        &mut self,
        p: u64,
        bytes: &[u8],
        types: crate::query::Types,
        step: &mut impl FnMut() -> Result<bool>,
    ) -> Result<(u64, bool)> {
        let base = self.tile.start();
        let mut offset = (p - base) as u32;
        let limit = self.library.limits.max_traversal_steps;
        let mut steps = self.steps;
        let flags_to_skip = 0x800 | self.duplicate_flags;
        let bounds = self.bounds;
        let width = if NODE { 20 } else { 32 };
        let mut count = 0u64;
        loop {
            let start = offset as usize;
            let flags = if let Some(raw) = bytes.get(start..start + width) {
                let flags = if NODE {
                    u16::from_le_bytes([raw[8], raw[9]])
                } else {
                    u16::from_le_bytes([raw[16], raw[17]])
                };
                if NODE {
                    if flags & (24 | flags_to_skip) == 0 {
                        let in_bounds = !MAY_PRUNE
                            || bounds.contains(Point {
                                x: i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]),
                                y: i32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]),
                            });
                        if in_bounds && types.matches_flags(flags) {
                            count += 1;
                        }
                        Some(flags)
                    } else if flags & 24 == 0 {
                        Some(flags)
                    } else {
                        None
                    }
                } else if matches!(flags & (24 | flags_to_skip), 8 | 16) {
                    let min_x = i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
                    let min_y = i32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
                    let max_x = i32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]);
                    let max_y = i32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]);
                    if min_x <= max_x && min_y <= max_y {
                        let in_bounds = !MAY_PRUNE
                            || bounds.intersects(Bounds {
                                min: Point { x: min_x, y: min_y },
                                max: Point { x: max_x, y: max_y },
                            });
                        if in_bounds && types.matches_flags(flags) {
                            count += 1;
                        }
                        Some(flags)
                    } else {
                        None
                    }
                } else if matches!(flags & 24, 8 | 16) {
                    Some(flags)
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(flags) = flags {
                if flags & 1 != 0 {
                    self.steps = steps;
                    return Ok((count, false));
                }
                offset += width as u32 + if NODE { u32::from(flags & 4) } else { 0 };
            } else {
                let p = base + offset as u64;
                self.steps = steps;
                if self
                    .leaf_candidate::<true>(p, NODE, &mut |f, _, _, _| Ok(types.matches(f)))?
                    .is_some()
                {
                    count += 1;
                }
                let Some((next, _)) = self.leaf.take() else {
                    return Ok((count, false));
                };
                offset = (next - base) as u32;
            }
            if !step()? {
                self.steps = steps;
                self.leaf = Some((base + offset as u64, NODE));
                return Ok((count, true));
            }
            steps += 1;
            if CHECK_TRAVERSAL && steps > limit {
                return Err(Error::Limit("spatial traversal steps"));
            }
        }
    }

    pub(crate) fn count_all(
        &mut self,
        types: crate::query::Types,
        mut step: impl FnMut() -> Result<bool>,
    ) -> Result<u64> {
        if self.done {
            return Ok(0);
        }
        match self.count_all_inner(types, &mut step) {
            Ok((count, true)) => {
                self.finish_scan();
                Ok(count)
            }
            Ok((count, false)) => Ok(count),
            Err(error) => {
                self.finish_scan();
                Err(error)
            }
        }
    }

    fn count_all_inner(
        &mut self,
        types: crate::query::Types,
        step: &mut impl FnMut() -> Result<bool>,
    ) -> Result<(u64, bool)> {
        let lib = self.library;
        let mut total = 0u64;
        while let Some(work) = self
            .leaf
            .take()
            .map(|(p, node)| Work::Leaf { p, node })
            .or_else(|| self.work.pop())
        {
            #[cfg(feature = "parallel")]
            if let Work::Payload { start, bounds } = work {
                self.load_tile(start, bounds)?;
                continue;
            }
            if !self.try_step(step)? {
                match work {
                    Work::Leaf { p, node } => self.leaf = Some((p, node)),
                    work => self.work.push(work),
                }
                return Ok((total, false));
            }
            match work {
                Work::Tile(tile) => self.start_tile(tile)?,
                #[cfg(feature = "parallel")]
                Work::Payload { .. } => unreachable!("prepaid payload handled before stepping"),
                Work::Children {
                    parent,
                    mask,
                    entries,
                    extent,
                    ordinal,
                } => {
                    self.push_child(parent, mask, entries, extent, ordinal);
                }
                Work::Roots { p, node } => {
                    if !self.scan_roots(p, node, step)? {
                        return Ok((total, false));
                    }
                }
                Work::Branch { p, node, depth } => self.expand_branch(p, node, depth)?,
                Work::Leaf { p, node } => {
                    self.in_tile(p, if node { 20 } else { 32 })?;
                    let tile = lib.source.as_bytes().and_then(|bytes| {
                        let end = usize::try_from(self.tile.end()).ok()?;
                        bytes.get(self.tile.start() as usize..end)
                    });
                    if let Some(tile) = tile {
                        let (leaf_count, paused) = if node {
                            self.count_all_whole_leaf::<true>(p, tile, types, step)?
                        } else {
                            self.count_all_whole_leaf::<false>(p, tile, types, step)?
                        };
                        total += leaf_count;
                        if paused {
                            return Ok((total, false));
                        }
                        continue;
                    }
                    let mut current = p;
                    loop {
                        if self
                            .leaf_candidate::<true>(current, node, &mut |f, _, _, _| {
                                Ok(types.matches(f))
                            })?
                            .is_some()
                        {
                            total += 1;
                        }
                        let Some((next, _)) = self.leaf.take() else {
                            break;
                        };
                        if !self.try_step(step)? {
                            self.leaf = Some((next, node));
                            return Ok((total, false));
                        }
                        current = next;
                    }
                }
            }
        }
        Ok((total, true))
    }

    #[inline]
    fn count_dictionary_whole_leaf<const NODE: bool, P: DictionaryProbe>(
        &mut self,
        p: u64,
        bytes: &[u8],
        types: crate::query::Types,
        probe: P,
        fallback: &mut impl FnMut(
            Feature<'a, S>,
            &mut ReadWindow<'a, S>,
            &mut ReadWindow<'a, S>,
        ) -> Result<bool>,
        step: &mut impl FnMut() -> Result<bool>,
    ) -> Result<(u64, bool)> {
        match (
            self.library.limits.max_tags >= P::TAG_PROBE_LIMIT,
            self.prune,
        ) {
            (true, false) => {
                let width = if NODE { 20 } else { 32 };
                let offset = (p - self.tile.start()) as usize;
                let remaining = self.library.limits.max_traversal_steps - self.steps;
                if remaining >= ((bytes.len() - offset) / width) as u64 {
                    self.count_dictionary_whole_leaf_inner::<NODE, true, false, false, _>(
                        p, bytes, types, probe, fallback, step,
                    )
                } else {
                    self.count_dictionary_whole_leaf_inner::<NODE, true, false, true, _>(
                        p, bytes, types, probe, fallback, step,
                    )
                }
            }
            (true, true) => self.count_dictionary_whole_leaf_inner::<NODE, true, true, true, _>(
                p, bytes, types, probe, fallback, step,
            ),
            _ => self.count_dictionary_whole_leaf_inner::<NODE, false, true, true, _>(
                p, bytes, types, probe, fallback, step,
            ),
        }
    }

    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn count_dictionary_whole_leaf_inner<
        const NODE: bool,
        const FULL_PROBE: bool,
        const MAY_PRUNE: bool,
        const CHECK_TRAVERSAL: bool,
        P: DictionaryProbe,
    >(
        &mut self,
        p: u64,
        bytes: &[u8],
        types: crate::query::Types,
        probe: P,
        fallback: &mut impl FnMut(
            Feature<'a, S>,
            &mut ReadWindow<'a, S>,
            &mut ReadWindow<'a, S>,
        ) -> Result<bool>,
        step: &mut impl FnMut() -> Result<bool>,
    ) -> Result<(u64, bool)> {
        let base = self.tile.start();
        let mut offset = (p - base) as u32;
        let strings = self.library.string_count;
        let max_tags = if FULL_PROBE {
            P::TAG_PROBE_LIMIT
        } else {
            self.library.limits.max_tags
        };
        let limit = self.library.limits.max_traversal_steps;
        let mut steps = self.steps;
        let flags_to_skip = 0x800 | self.duplicate_flags;
        let prune = MAY_PRUNE && self.prune;
        let bounds = self.bounds;
        let width = if NODE { 20 } else { 32 };
        let mut count = 0u64;
        loop {
            let start = offset as usize;
            let flags = bytes.get(start..start + width).and_then(|raw| {
                let flags = if NODE {
                    u16::from_le_bytes([raw[8], raw[9]])
                } else {
                    u16::from_le_bytes([raw[16], raw[17]])
                };
                if NODE {
                    if flags & (24 | flags_to_skip) != 0 {
                        return (flags & 24 == 0).then_some(flags);
                    }
                    if prune
                        && !bounds.contains(Point {
                            x: i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]),
                            y: i32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]),
                        })
                    {
                        return Some(flags);
                    }
                } else {
                    if !matches!(flags & (24 | flags_to_skip), 8 | 16) {
                        return matches!(flags & 24, 8 | 16).then_some(flags);
                    }
                    let min_x = i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
                    let min_y = i32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
                    let max_x = i32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]);
                    let max_y = i32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]);
                    if min_x > max_x || min_y > max_y {
                        return None;
                    }
                    if prune
                        && !bounds.intersects(Bounds {
                            min: Point { x: min_x, y: min_y },
                            max: Point { x: max_x, y: max_y },
                        })
                    {
                        return Some(flags);
                    }
                }
                if !types.matches_flags(flags) {
                    return Some(flags);
                }
                let ptr = if NODE {
                    i32::from_le_bytes([raw[16], raw[17], raw[18], raw[19]])
                } else {
                    i32::from_le_bytes([raw[24], raw[25], raw[26], raw[27]])
                };
                let pointer = offset + if NODE { 16 } else { 24 };
                let table = pointer.wrapping_add((ptr & !1) as u32) as usize;
                match probe.probe(bytes, strings, max_tags, table) {
                    EqualityProbe::Reject => Some(flags),
                    EqualityProbe::Match => {
                        count += 1;
                        Some(flags)
                    }
                    EqualityProbe::Unknown => None,
                }
            });
            if let Some(flags) = flags {
                if flags & 1 != 0 {
                    self.steps = steps;
                    return Ok((count, false));
                }
                offset += width as u32 + if NODE { u32::from(flags & 4) } else { 0 };
            } else {
                let p = base + offset as u64;
                self.steps = steps;
                if self
                    .leaf_candidate::<false>(p, NODE, &mut |f, w, s, _| {
                        Ok(types.matches(f) && fallback(f, w, s)?)
                    })?
                    .is_some()
                {
                    count += 1;
                }
                let Some((next, _)) = self.leaf.take() else {
                    return Ok((count, false));
                };
                offset = (next - base) as u32;
            }
            if !step()? {
                self.steps = steps;
                self.leaf = Some((base + offset as u64, NODE));
                return Ok((count, true));
            }
            steps += 1;
            if CHECK_TRAVERSAL && steps > limit {
                return Err(Error::Limit("spatial traversal steps"));
            }
        }
    }

    pub(crate) fn count_dictionary<P: DictionaryProbe>(
        &mut self,
        types: crate::query::Types,
        probe: P,
        mut fallback: impl FnMut(
            Feature<'a, S>,
            &mut ReadWindow<'a, S>,
            &mut ReadWindow<'a, S>,
        ) -> Result<bool>,
        mut step: impl FnMut() -> Result<bool>,
    ) -> Result<u64> {
        if self.done {
            return Ok(0);
        }
        match self.count_dictionary_inner(types, probe, &mut fallback, &mut step) {
            Ok((count, true)) => {
                self.finish_scan();
                Ok(count)
            }
            Ok((count, false)) => Ok(count),
            Err(error) => {
                self.finish_scan();
                Err(error)
            }
        }
    }

    fn count_dictionary_inner<P: DictionaryProbe>(
        &mut self,
        types: crate::query::Types,
        probe: P,
        fallback: &mut impl FnMut(
            Feature<'a, S>,
            &mut ReadWindow<'a, S>,
            &mut ReadWindow<'a, S>,
        ) -> Result<bool>,
        step: &mut impl FnMut() -> Result<bool>,
    ) -> Result<(u64, bool)> {
        let lib = self.library;
        let mut total = 0u64;
        while let Some(work) = self
            .leaf
            .take()
            .map(|(p, node)| Work::Leaf { p, node })
            .or_else(|| self.work.pop())
        {
            #[cfg(feature = "parallel")]
            if let Work::Payload { start, bounds } = work {
                self.load_tile(start, bounds)?;
                continue;
            }
            if !self.try_step(step)? {
                match work {
                    Work::Leaf { p, node } => self.leaf = Some((p, node)),
                    work => self.work.push(work),
                }
                return Ok((total, false));
            }
            match work {
                Work::Tile(tile) => self.start_tile(tile)?,
                #[cfg(feature = "parallel")]
                Work::Payload { .. } => unreachable!("prepaid payload handled before stepping"),
                Work::Children {
                    parent,
                    mask,
                    entries,
                    extent,
                    ordinal,
                } => {
                    self.push_child(parent, mask, entries, extent, ordinal);
                }
                Work::Roots { p, node } => {
                    if !self.scan_roots(p, node, step)? {
                        return Ok((total, false));
                    }
                }
                Work::Branch { p, node, depth } => self.expand_branch(p, node, depth)?,
                Work::Leaf { p, node } => {
                    self.in_tile(p, if node { 20 } else { 32 })?;
                    let tile = lib.source.as_bytes().and_then(|bytes| {
                        let end = usize::try_from(self.tile.end()).ok()?;
                        bytes.get(self.tile.start() as usize..end)
                    });
                    if let Some(tile) = tile {
                        let (leaf_count, paused) = if node {
                            self.count_dictionary_whole_leaf::<true, _>(
                                p, tile, types, probe, fallback, step,
                            )?
                        } else {
                            self.count_dictionary_whole_leaf::<false, _>(
                                p, tile, types, probe, fallback, step,
                            )?
                        };
                        total += leaf_count;
                        if paused {
                            return Ok((total, false));
                        }
                        continue;
                    }
                    let mut current = p;
                    loop {
                        if self
                            .leaf_candidate::<false>(current, node, &mut |f, w, s, _| {
                                Ok(types.matches(f) && fallback(f, w, s)?)
                            })?
                            .is_some()
                        {
                            total += 1;
                        }
                        let Some((next, _)) = self.leaf.take() else {
                            break;
                        };
                        if !self.try_step(step)? {
                            self.leaf = Some((next, node));
                            return Ok((total, false));
                        }
                        current = next;
                    }
                }
            }
        }
        Ok((total, true))
    }

    #[inline]
    fn try_fold_all_whole_leaf<const NODE: bool, T>(
        &mut self,
        p: u64,
        bytes: &[u8],
        types: crate::query::Types,
        acc: T,
        fold: &mut impl FnMut(T, Feature<'a, S>) -> Result<T>,
        step: &mut impl FnMut() -> Result<bool>,
    ) -> Result<(T, bool)> {
        if !self.prune {
            let width = if NODE { 20 } else { 32 };
            let offset = (p - self.tile.start()) as usize;
            let remaining = self.library.limits.max_traversal_steps - self.steps;
            if remaining >= ((bytes.len() - offset) / width) as u64 {
                self.try_fold_all_whole_leaf_inner::<NODE, false, false, T>(
                    p, bytes, types, acc, fold, step,
                )
            } else {
                self.try_fold_all_whole_leaf_inner::<NODE, false, true, T>(
                    p, bytes, types, acc, fold, step,
                )
            }
        } else {
            self.try_fold_all_whole_leaf_inner::<NODE, true, true, T>(
                p, bytes, types, acc, fold, step,
            )
        }
    }

    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn try_fold_all_whole_leaf_inner<
        const NODE: bool,
        const MAY_PRUNE: bool,
        const CHECK_TRAVERSAL: bool,
        T,
    >(
        &mut self,
        p: u64,
        bytes: &[u8],
        types: crate::query::Types,
        mut acc: T,
        fold: &mut impl FnMut(T, Feature<'a, S>) -> Result<T>,
        step: &mut impl FnMut() -> Result<bool>,
    ) -> Result<(T, bool)> {
        let base = self.tile.start();
        let mut offset = (p - base) as u32;
        let limit = self.library.limits.max_traversal_steps;
        let mut steps = self.steps;
        let flags_to_skip = 0x800 | self.duplicate_flags;
        let bounds = self.bounds;
        let width = if NODE { 20 } else { 32 };
        loop {
            let start = offset as usize;
            let mut matched = None;
            let flags = bytes.get(start..start + width).and_then(|raw| {
                let record = LeafRecord::read::<NODE>(raw)?;
                if NODE {
                    if record.flags & (24 | flags_to_skip) != 0 {
                        return record.has_layout::<NODE>().then_some(record.flags);
                    }
                    if MAY_PRUNE && !bounds.contains(record.point()) {
                        return Some(record.flags);
                    }
                } else {
                    if !matches!(record.flags & (24 | flags_to_skip), 8 | 16) {
                        return record.has_layout::<NODE>().then_some(record.flags);
                    }
                    let bbox = record.bbox().ok()?;
                    if MAY_PRUNE && !bounds.intersects(bbox) {
                        return Some(record.flags);
                    }
                }
                if types.matches_flags(record.flags) {
                    matched = Some(record.header);
                }
                Some(record.flags)
            });
            if let Some(flags) = flags {
                if let Some(header) = matched {
                    let feature = Feature::from_leaf_header(
                        self.library,
                        header,
                        self.tile,
                        offset + if NODE { 8 } else { 16 },
                    );
                    acc = fold(acc, feature)?;
                }
                if flags & 1 != 0 {
                    self.steps = steps;
                    return Ok((acc, false));
                }
                offset += width as u32 + if NODE { u32::from(flags & 4) } else { 0 };
            } else {
                let p = base + offset as u64;
                self.steps = steps;
                if let Some(feature) =
                    self.leaf_candidate::<true>(p, NODE, &mut |f, _, _, _| Ok(types.matches(f)))?
                {
                    acc = fold(acc, feature)?;
                }
                let Some((next, _)) = self.leaf.take() else {
                    return Ok((acc, false));
                };
                offset = (next - base) as u32;
            }
            if !step()? {
                self.steps = steps;
                self.leaf = Some((base + offset as u64, NODE));
                return Ok((acc, true));
            }
            steps += 1;
            if CHECK_TRAVERSAL && steps > limit {
                return Err(Error::Limit("spatial traversal steps"));
            }
        }
    }

    pub(crate) fn try_fold_all<T>(
        &mut self,
        types: crate::query::Types,
        initial: T,
        mut fold: impl FnMut(T, Feature<'a, S>) -> Result<T>,
        mut step: impl FnMut() -> Result<bool>,
    ) -> Result<T> {
        if self.done {
            return Ok(initial);
        }
        match self.try_fold_all_inner(types, initial, &mut fold, &mut step) {
            Ok((acc, true)) => {
                self.finish_scan();
                Ok(acc)
            }
            Ok((acc, false)) => Ok(acc),
            Err(error) => {
                self.finish_scan();
                Err(error)
            }
        }
    }

    fn try_fold_all_inner<T>(
        &mut self,
        types: crate::query::Types,
        mut acc: T,
        fold: &mut impl FnMut(T, Feature<'a, S>) -> Result<T>,
        step: &mut impl FnMut() -> Result<bool>,
    ) -> Result<(T, bool)> {
        let lib = self.library;
        while let Some(work) = self
            .leaf
            .take()
            .map(|(p, node)| Work::Leaf { p, node })
            .or_else(|| self.work.pop())
        {
            #[cfg(feature = "parallel")]
            if let Work::Payload { start, bounds } = work {
                self.load_tile(start, bounds)?;
                continue;
            }
            if !self.try_step(step)? {
                match work {
                    Work::Leaf { p, node } => self.leaf = Some((p, node)),
                    work => self.work.push(work),
                }
                return Ok((acc, false));
            }
            match work {
                Work::Tile(tile) => self.start_tile(tile)?,
                #[cfg(feature = "parallel")]
                Work::Payload { .. } => unreachable!("prepaid payload handled before stepping"),
                Work::Children {
                    parent,
                    mask,
                    entries,
                    extent,
                    ordinal,
                } => {
                    self.push_child(parent, mask, entries, extent, ordinal);
                }
                Work::Roots { p, node } => {
                    if !self.scan_roots(p, node, step)? {
                        return Ok((acc, false));
                    }
                }
                Work::Branch { p, node, depth } => self.expand_branch(p, node, depth)?,
                Work::Leaf { p, node } => {
                    self.in_tile(p, if node { 20 } else { 32 })?;
                    let tile = lib.source.as_bytes().and_then(|bytes| {
                        let end = usize::try_from(self.tile.end()).ok()?;
                        bytes.get(self.tile.start() as usize..end)
                    });
                    if let Some(tile) = tile {
                        let (next_acc, paused) = if node {
                            self.try_fold_all_whole_leaf::<true, T>(
                                p, tile, types, acc, fold, step,
                            )?
                        } else {
                            self.try_fold_all_whole_leaf::<false, T>(
                                p, tile, types, acc, fold, step,
                            )?
                        };
                        acc = next_acc;
                        if paused {
                            return Ok((acc, false));
                        }
                        continue;
                    }
                    let mut current = p;
                    loop {
                        if let Some(feature) =
                            self.leaf_candidate::<true>(current, node, &mut |f, _, _, _| {
                                Ok(types.matches(f))
                            })?
                        {
                            acc = fold(acc, feature)?;
                        }
                        let Some((next, _)) = self.leaf.take() else {
                            break;
                        };
                        if !self.try_step(step)? {
                            self.leaf = Some((next, node));
                            return Ok((acc, false));
                        }
                        current = next;
                    }
                }
            }
        }
        Ok((acc, true))
    }

    fn next_scan<const EXACT: bool, const EQUALITY: bool>(
        &mut self,
        mut predicate: impl FnMut(
            Feature<'a, S>,
            &mut ReadWindow<'a, S>,
            &mut ReadWindow<'a, S>,
            bool,
        ) -> Result<bool>,
        mut step: impl FnMut() -> Result<bool>,
        equality: Option<impl DictionaryProbe>,
    ) -> Option<Result<Feature<'a, S>>> {
        if self.done {
            return None;
        }
        match self.advance::<EXACT, EQUALITY>(&mut predicate, &mut step, equality) {
            Ok(Advance::Feature(f)) => Some(Ok(f)),
            Ok(Advance::Paused) => None,
            Ok(Advance::End) => {
                #[cfg(all(feature = "mmap", target_os = "linux"))]
                {
                    self.read_ahead = None;
                    self.mapped_ahead = None;
                }
                self.done = true;
                None
            }
            Err(e) => {
                #[cfg(all(feature = "mmap", target_os = "linux"))]
                {
                    self.read_ahead = None;
                    self.mapped_ahead = None;
                }
                self.done = true;
                Some(Err(e))
            }
        }
    }
}
impl<'a, S: Source> Iterator for Features<'a, S> {
    type Item = Result<Feature<'a, S>>;
    fn next(&mut self) -> Option<Self::Item> {
        self.next_matching(|_, _, _| Ok(true))
    }
}
impl<S: Source> std::iter::FusedIterator for Features<'_, S> {}

#[cfg(all(unix, feature = "experimental-tag-sidecars"))]
mod tag_sidecar;

#[cfg(test)]
mod equality_tests;

#[cfg(test)]
mod tile_tests;
