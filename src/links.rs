use crate::{
    Bounds, Coordinates, Error, Feature, FeatureType, Features, Library, Point, Result,
    decode::{Decode, add},
    source::Source,
};

impl<'a, S: Source> Feature<'a, S> {
    /// Relations directly containing this feature, in stored table order.
    pub fn parent_relations(self) -> Result<FeatureLinks<'a, S>> {
        let next = if self.flags() & 4 == 0 {
            None
        } else {
            let pp = if self.kind() == FeatureType::Node {
                self.anchor() + 12
            } else {
                add(self.body()?, -4)?
            };
            self.check(pp, 4)?;
            Some(self.library.source.relative(pp, !0)?)
        };
        Ok(FeatureLinks::new(self, next, false))
    }
    /// Stored feature nodes of a way. Geometry-only vertices are exposed by
    /// `way_nodes`; this iterator contains nodes that have feature records.
    pub fn feature_nodes(self) -> Result<FeatureLinks<'a, S>> {
        if self.kind() != FeatureType::Way {
            return Err(Error::WrongFeatureType);
        }
        let next = if self.flags() & 32 == 0 {
            None
        } else {
            Some(add(self.body()?, -4 - i64::from(self.flags() & 4))?)
        };
        Ok(FeatureLinks::new(self, next, true))
    }
    /// Stream a way's vertices with their positions and any retained node IDs.
    pub fn way_nodes(self) -> Result<WayNodes<'a, S>> {
        WayNodes::new(self)
    }
    /// Ways containing this node by identity. Uses the spatial index to select
    /// candidates and verifies their vertex references; coincident nodes are
    /// not treated as identical when their IDs differ.
    pub fn parent_ways(self) -> Result<ParentWays<'a, S>> {
        let point = self.point()?.ok_or(Error::WrongFeatureType)?;
        Ok(self.library.ways_containing_vertex(WayNode {
            point,
            id: Some(self.id().id),
        }))
    }
}

impl<S: Source> Library<S> {
    /// Parent ways of a vertex, including geometry-only nodes. With no stored ID,
    /// the vertex is identified by its integer coordinate location.
    pub fn ways_containing_vertex(&self, vertex: WayNode) -> ParentWays<'_, S> {
        ParentWays {
            candidates: self
                .in_bounds(Bounds {
                    min: vertex.point,
                    max: vertex.point,
                })
                .restrict(6, 0),
            vertex,
            done: false,
        }
    }
}
/// Fallible iterator over ways containing the requested vertex.
pub struct ParentWays<'a, S> {
    candidates: Features<'a, S>,
    vertex: WayNode,
    done: bool,
}
impl<'a, S: Source> Iterator for ParentWays<'a, S> {
    type Item = Result<Feature<'a, S>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        for candidate in self.candidates.by_ref() {
            let result = candidate.and_then(|way| {
                if way.kind() != FeatureType::Way {
                    return Ok(None);
                }
                for node in way.way_nodes()? {
                    let node = node?;
                    if node.point == self.vertex.point
                        && self.vertex.id.is_none_or(|id| node.id == Some(id))
                    {
                        return Ok(Some(way));
                    }
                }
                Ok(None)
            });
            match result {
                Ok(Some(way)) => return Some(Ok(way)),
                Ok(None) => {}
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
        self.done = true;
        None
    }
}
impl<S: Source> std::iter::FusedIterator for ParentWays<'_, S> {}

/// A foreign reference names a numbered slot. Empty slots survive tile updates.
#[derive(Clone, Copy)]
pub(crate) struct ExportTable {
    tip: u32,
    tile: u64,
    end: u64,
    entries: u64,
    count: u32,
}

impl ExportTable {
    fn read(source: &impl Source, tip: u32, tile: u64) -> Result<Self> {
        let end = tile + 4 + u64::from(source.u32(tile)? & 0x3fff_ffff);
        if end < tile + 28 || end > source.len() {
            return Err(Error::invalid(tile, "invalid foreign tile extent"));
        }
        let entries = source.relative(tile + 24, !0)?;
        // The pointer targets the first slot, immediately after the count.
        if entries < tile + 32 || entries > end {
            return Err(Error::invalid(tile + 24, "invalid export table"));
        }
        let count = source.u32(entries - 4)?;
        if u64::from(count) > (end - entries) / 4 {
            return Err(Error::invalid(entries, "export table exceeds tile"));
        }
        Ok(Self {
            tip,
            tile,
            end,
            entries,
            count,
        })
    }

    fn resolve(&self, source: &impl Source, tex: u32) -> Result<u64> {
        if tex >= self.count {
            return Err(Error::invalid(self.entries, "export index outside table"));
        }
        let entry = self.entries + u64::from(tex) * 4;
        let delta = source.i32(entry)?;
        if delta == 0 {
            return Err(Error::invalid(entry, "missing export"));
        }
        add(entry, i64::from(delta))
    }
}

pub(crate) fn foreign_feature<'a, S: Source>(
    lib: &'a Library<S>,
    cache: &mut Option<ExportTable>,
    tip: u32,
    tex: u32,
) -> Result<Feature<'a, S>> {
    let table = match *cache {
        Some(table) if table.tip == tip => table,
        _ => {
            let table = ExportTable::read(&lib.source, tip, lib.tile_page(tip)?)?;
            *cache = Some(table);
            table
        }
    };
    Feature::at(lib, table.resolve(&lib.source, tex)?, table.tile, table.end)
}

/// Bounded lazy traversal of a feature-reference table. Shared decoding handles
/// forward parent tables and backward way-node tables.
pub struct FeatureLinks<'a, S> {
    owner: Feature<'a, S>,
    next: Option<u64>,
    backwards: bool,
    tip: i64,
    tex: i64,
    cached_foreign: Option<ExportTable>,
    steps: u64,
}
impl<'a, S: Source> FeatureLinks<'a, S> {
    fn new(owner: Feature<'a, S>, next: Option<u64>, backwards: bool) -> Self {
        Self {
            owner,
            next,
            backwards,
            tip: 0x4000,
            tex: 0x800,
            cached_foreign: None,
            steps: 0,
        }
    }
    fn read(&mut self, start: u64) -> Result<Feature<'a, S>> {
        let f = self.owner;
        let s = &f.library.source;
        self.steps += 1;
        if self.steps > f.library.limits.max_members {
            return Err(Error::Limit("feature references"));
        }
        let (word, mut cursor) = if self.backwards {
            f.check(start, 4)?;
            let raw = s.u32(start)?;
            if raw & (2 << 16) != 0 && raw & (8 << 16) == 0 {
                ((raw as i32) >> 16, start + 2)
            } else {
                (raw.rotate_left(16) as i32, start)
            }
        } else {
            f.check(start, 2)?;
            let raw = s.u16(start)?;
            if raw & 2 != 0 && raw & 8 == 0 {
                (i32::from(raw as i16), start + 2)
            } else {
                f.check(start, 4)?;
                (s.i32(start)?, start + 4)
            }
        };
        let target = if word & 2 != 0 {
            self.tex += i64::from(word >> 4);
            if word & 4 != 0 {
                let delta = if self.backwards {
                    cursor = add(cursor, -2)?;
                    f.check(cursor, 2)?;
                    let low = s.u16(cursor)? as i16;
                    if low & 1 != 0 {
                        cursor = add(cursor, -2)?;
                        f.check(cursor, 2)?;
                        (i32::from(s.u16(cursor)? as i16) << 15) | ((i32::from(low) >> 1) & 0x7fff)
                    } else {
                        i32::from(low) >> 1
                    }
                } else {
                    f.check(cursor, 2)?;
                    let low = s.u16(cursor)? as i16;
                    if low & 1 != 0 {
                        f.check(cursor, 4)?;
                        let d = s.i32(cursor)? >> 1;
                        cursor += 4;
                        d
                    } else {
                        cursor += 2;
                        i32::from(low) >> 1
                    }
                };
                self.tip += i64::from(delta);
            }
            let tip =
                u32::try_from(self.tip).map_err(|_| Error::invalid(start, "invalid tile delta"))?;
            let tex = u32::try_from(self.tex)
                .map_err(|_| Error::invalid(start, "invalid export delta"))?;
            foreign_feature(f.library, &mut self.cached_foreign, tip, tex)?
        } else {
            let target = if self.backwards {
                add(start, i64::from(word >> 1) + 2)?
            } else {
                add(start & !1, i64::from(word >> 2) * 2)?
            };
            Feature::at_source(f.library, &f.library.source, target, f.tile)?
        };
        if target.kind()
            != if self.backwards {
                FeatureType::Node
            } else {
                FeatureType::Relation
            }
        {
            return Err(Error::invalid(start, "reference has wrong feature type"));
        }
        self.next = if word & 1 != 0 {
            None
        } else if self.backwards {
            Some(add(cursor, -4)?)
        } else {
            Some(cursor)
        };
        Ok(target)
    }
}
impl<'a, S: Source> Iterator for FeatureLinks<'a, S> {
    type Item = Result<Feature<'a, S>>;
    fn next(&mut self) -> Option<Self::Item> {
        let start = self.next?;
        let result = self.read(start);
        if result.is_err() {
            self.next = None;
        }
        Some(result)
    }
}
impl<S: Source> std::iter::FusedIterator for FeatureLinks<'_, S> {}

/// A vertex in a way, with its original node ID when recoverable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WayNode {
    /// The vertex position in integer Mercator coordinates.
    pub point: Point,
    /// The OSM node ID, if the file retains its identity.
    pub id: Option<u64>,
}
/// Fallible iterator over a way's vertices in geometry order.
pub struct WayNodes<'a, S> {
    owner: Feature<'a, S>,
    coords: Coordinates<'a, S>,
    feature_nodes: FeatureLinks<'a, S>,
    next_feature: Option<Feature<'a, S>>,
    id_pos: Option<u64>,
    ids_remaining: u64,
    id: i64,
    first: Option<WayNode>,
    done: bool,
}
impl<'a, S: Source> WayNodes<'a, S> {
    fn new(owner: Feature<'a, S>) -> Result<Self> {
        let coords = owner.coordinates()?;
        let mut id_pos = None;
        let mut ids_remaining = 0;
        if owner.library.has_way_node_ids() {
            let mut p = owner.body()?;
            ids_remaining = owner.library.source.varint(&mut p)?;
            if ids_remaining > owner.library.limits.max_coordinates {
                return Err(Error::Limit("way node IDs"));
            }
            // IDs follow the coordinate stream; locate them without retaining
            // a second geometry buffer. Subsequent iteration is allocation-free.
            let mut skipped = false;
            if let Some(bytes) = owner.library.source.as_bytes() {
                let start = p as usize;
                let end = (owner.tile_end() as usize).min(bytes.len());
                if start <= end {
                    let slice = &bytes[start..end];
                    let mut idx = 0usize;
                    let mut count = ids_remaining.saturating_mul(2);
                    let mut ok = true;
                    while count != 0 {
                        let mut shift = 0u32;
                        loop {
                            let Some(&b) = slice.get(idx) else {
                                ok = false;
                                break;
                            };
                            idx += 1;
                            if shift == 9 && b > 1 {
                                ok = false;
                                break;
                            }
                            if b < 128 {
                                break;
                            }
                            shift += 1;
                            if shift >= 10 {
                                ok = false;
                                break;
                            }
                        }
                        if !ok {
                            break;
                        }
                        count -= 1;
                    }
                    if ok {
                        p += idx as u64;
                        skipped = true;
                    }
                }
            }
            if !skipped {
                for _ in 0..ids_remaining {
                    owner.library.source.signed_varint(&mut p)?;
                    owner.library.source.signed_varint(&mut p)?;
                    owner.check(p - 1, 1)?;
                }
            }
            id_pos = Some(p);
        }
        Self::with_ids(owner, coords, id_pos, ids_remaining)
    }

    pub(crate) fn with_ids(
        owner: Feature<'a, S>,
        coords: Coordinates<'a, S>,
        id_pos: Option<u64>,
        ids_remaining: u64,
    ) -> Result<Self> {
        let mut feature_nodes = owner.feature_nodes()?;
        let next_feature = feature_nodes.next().transpose()?;
        Ok(Self {
            owner,
            coords,
            feature_nodes,
            next_feature,
            id_pos,
            ids_remaining,
            id: 0,
            first: None,
            done: false,
        })
    }
    fn read(&mut self, point: Point) -> Result<WayNode> {
        let mut id = None;
        if let Some(p) = &mut self.id_pos {
            if self.ids_remaining == 0 {
                if let Some(first) = self.first {
                    return Ok(first);
                }
                return Err(Error::invalid(*p, "missing closing node ID"));
            }
            id = Some(read_original_way_node_id(self.owner, p, &mut self.id)?);
            self.ids_remaining -= 1;
        }
        if let Some(node) = self.next_feature
            && node.point()? == Some(point)
            && id.is_none_or(|id| id == node.id().id)
        {
            id = Some(node.id().id);
            self.next_feature = self.feature_nodes.next().transpose()?;
        }
        let node = WayNode { point, id };
        self.first.get_or_insert(node);
        Ok(node)
    }
}
impl<S: Source> Iterator for WayNodes<'_, S> {
    type Item = Result<WayNode>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let Some(point) = self.coords.next() else {
            self.done = true;
            return None;
        };
        let result = point.and_then(|p| self.read(p));
        if result.is_err() {
            self.done = true;
        }
        Some(result)
    }
}
impl<S: Source> std::iter::FusedIterator for WayNodes<'_, S> {}

#[inline]
pub(crate) fn read_original_way_node_id<S: Source>(
    owner: Feature<'_, S>,
    p: &mut u64,
    current: &mut i64,
) -> Result<u64> {
    if let Some(bytes) = owner.library.source.as_bytes() {
        let start = *p as usize;
        let end = (owner.tile_end() as usize).min(bytes.len());
        if start < end {
            let slice = &bytes[start..end];
            let mut n = 0u64;
            let mut idx = 0usize;
            while idx < 10 {
                let Some(&b) = slice.get(idx) else {
                    break;
                };
                if idx == 9 && b > 1 {
                    break;
                }
                n |= u64::from(b & 127) << (idx * 7);
                idx += 1;
                if b < 128 {
                    *p += idx as u64;
                    let delta = ((n >> 1) as i64) ^ -((n & 1) as i64);
                    *current = current
                        .checked_add(delta)
                        .ok_or_else(|| Error::invalid(*p, "node ID overflow"))?;
                    return u64::try_from(*current)
                        .map_err(|_| Error::invalid(*p, "negative node ID"));
                }
            }
        }
    }
    *current = current
        .checked_add(owner.library.source.signed_varint(p)?)
        .ok_or_else(|| Error::invalid(*p, "node ID overflow"))?;
    owner.check(*p - 1, 1)?;
    u64::try_from(*current).map_err(|_| Error::invalid(*p, "negative node ID"))
}

#[cfg(test)]
mod export_tests {
    use super::*;

    #[test]
    fn sparse_exports_and_table_bounds() -> Result<()> {
        let mut bytes = vec![0; 80];
        bytes[..4].copy_from_slice(&76u32.to_le_bytes());
        bytes[24..28].copy_from_slice(&12u32.to_le_bytes());
        bytes[32..36].copy_from_slice(&2u32.to_le_bytes());
        bytes[40..44].copy_from_slice(&20u32.to_le_bytes());
        let table = ExportTable::read(&bytes, 1, 0)?;
        assert!(table.resolve(&bytes, 0).is_err());
        assert_eq!(table.resolve(&bytes, 1)?, 60);
        assert!(table.resolve(&bytes, 2).is_err());
        bytes[32..36].copy_from_slice(&12u32.to_le_bytes());
        assert!(ExportTable::read(&bytes, 1, 0).is_err());
        bytes[24..28].fill(0);
        assert!(ExportTable::read(&bytes, 1, 0).is_err());
        Ok(())
    }
}
