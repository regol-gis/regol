use super::{Body, Feature, Kind, Link, Member, Role, Tags, Tile, Value, invalid, push};
use crate::{Bounds, Error, Point, Result, decode::Decode};

const TAGS: u8 = 1;
const SHARED_TAGS: u8 = 2;
const PARENTS: u8 = 4;
const GEOMETRY: u8 = 8;
const MEMBERS: u8 = 16;
const IDS_OR_BOUNDS: u8 = 32;
const AREA_OR_EXCEPTION: u8 = 64;

pub(super) fn read<'a>(
    data: &'a [u8],
    bounds: Bounds,
    way_node_ids: bool,
    categories: &[(u16, u32)],
    string_count: usize,
) -> Result<Tile<'a>> {
    let mut reader = Reader {
        data,
        pos: 0,
        previous: bounds.min,
        bounds,
        way_node_ids,
        string_count,
        categories: vec![0; 8191],
    };
    for &(code, mask) in categories {
        if let Some(bits) = reader.categories.get_mut(usize::from(code)) {
            *bits |= mask;
        }
    }

    let index = reader.feature_index()?;
    let string_entries = reader.count()?;
    let mut strings = Vec::new();
    for _ in 0..string_entries {
        push(&mut strings, reader.string()?)?;
    }
    let shared_tags = reader.count()?;
    let mut tags = Vec::new();
    for _ in 0..shared_tags {
        push(&mut tags, reader.tags(&strings)?)?;
    }
    let shared_parents = reader.count()?;
    let mut parents = Vec::new();
    for _ in 0..shared_parents {
        let size = reader.number()? as usize;
        push(&mut parents, reader.parents(size, &index)?)?;
    }
    let mut tile = Tile {
        strings,
        tags,
        parents,
        features: Vec::new(),
        exports: Vec::new(),
    };
    tile.features
        .try_reserve_exact(index.ids.len())
        .map_err(|_| Error::Limit("GOB tile features"))?;
    for &key in &index.ids {
        let f = reader.feature(key, &index, &mut tile, shared_tags, shared_parents)?;
        tile.features.push(f);
    }
    if reader.number()? != 0 {
        return Err(reader.invalid("tile export contains removed features"));
    }
    tile.exports = reader.exports(tile.features.len())?;
    if reader.pos != data.len() {
        return Err(reader.invalid("trailing bytes in tile export"));
    }
    Ok(tile)
}

struct FeatureIndex {
    ids: Vec<(Kind, u64)>,
    first_relation: usize,
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    previous: Point,
    bounds: Bounds,
    way_node_ids: bool,
    string_count: usize,
    categories: Vec<u32>,
}

impl<'a> Reader<'a> {
    fn invalid(&self, message: &'static str) -> Error {
        invalid(self.pos, message)
    }

    fn byte(&mut self) -> Result<u8> {
        let byte = self
            .data
            .get(self.pos)
            .copied()
            .ok_or_else(|| self.invalid("truncated tile export"))?;
        self.pos += 1;
        Ok(byte)
    }

    fn varint(&mut self) -> Result<u64> {
        let mut pos = self.pos as u64;
        let value = self
            .data
            .varint(&mut pos)
            .map_err(|_| self.invalid("truncated or overflowing tile export varint"))?;
        self.pos = pos as usize;
        Ok(value)
    }

    fn number(&mut self) -> Result<u32> {
        u32::try_from(self.varint()?).map_err(|_| self.invalid("tile export integer exceeds u32"))
    }

    fn signed(&mut self) -> Result<i64> {
        let n = self.varint()?;
        Ok(((n >> 1) as i64) ^ -((n & 1) as i64))
    }

    fn count(&mut self) -> Result<usize> {
        let count = self.number()? as usize;
        if count > self.data.len() - self.pos {
            return Err(self.invalid("tile export count exceeds remaining bytes"));
        }
        Ok(count)
    }

    fn string(&mut self) -> Result<&'a [u8]> {
        let start = self.pos;
        let n = self.count()?;
        if n >= 1 << 14 || self.pos - start > 2 {
            return Err(self.invalid("tile export string exceeds 14 bits"));
        }
        let bytes = &self.data[self.pos..self.pos + n];
        std::str::from_utf8(bytes).map_err(|_| self.invalid("invalid tile export string"))?;
        self.pos += n;
        Ok(bytes)
    }

    fn feature_index(&mut self) -> Result<FeatureIndex> {
        let count = self.count()?;
        // Each full feature needs an ID delta, flags, at least a tag-table
        // reference, and two coordinate deltas.
        if count > (self.data.len() - self.pos) / 5 {
            return Err(self.invalid("tile export feature count exceeds remaining bytes"));
        }
        let mut ids = Vec::new();
        let mut kind = Kind::Node;
        let mut previous = 0u64;
        let mut first_relation = count;
        while ids.len() < count {
            let word = self.varint()?;
            if word == 0 {
                kind = match kind {
                    Kind::Node => Kind::Way,
                    Kind::Way => {
                        first_relation = ids.len();
                        Kind::Relation
                    }
                    Kind::Relation => {
                        return Err(self.invalid("invalid tile export feature type"));
                    }
                };
                previous = 0;
                continue;
            }
            let delta = word >> 1;
            if word & 1 == 0 {
                return Err(self.invalid("tile export contains incremental feature references"));
            }
            if delta == 0 {
                return Err(self.invalid("duplicate tile export feature ID"));
            }
            previous = previous
                .checked_add(delta)
                .filter(|&id| id < 1 << 52)
                .ok_or_else(|| self.invalid("tile export feature ID exceeds 52 bits"))?;
            push(&mut ids, (kind, previous))?;
        }
        Ok(FeatureIndex {
            ids,
            first_relation,
        })
    }

    fn tags(&mut self, strings: &[&[u8]]) -> Result<Tags> {
        let tagged_size = self.number()?;
        let total_size = (tagged_size & !1) as usize;
        let mut local_size = if tagged_size & 1 != 0 {
            let half = self.number()? as usize;
            half.checked_mul(2)
                .filter(|&size| size != 0)
                .ok_or_else(|| self.invalid("invalid local tile export tag size"))?
        } else {
            0
        };
        let mut global_size = total_size
            .checked_sub(local_size)
            .filter(|&size| size >= 4)
            .ok_or_else(|| self.invalid("invalid tile export tag table size"))?;
        let mut local = Vec::new();
        while local_size != 0 {
            let key = self.number()?;
            let offset = self.local_string(key >> 2, strings)?;
            let value = self.value(key, strings)?;
            self.consume(&mut local_size, 4 + value.width())?;
            push(&mut local, (offset, value))?;
        }
        local.sort_unstable_by_key(|&(number, _)| strings[number]);
        if local
            .windows(2)
            .any(|pair| strings[pair[0].0] == strings[pair[1].0])
        {
            return Err(self.invalid("duplicate local tile export tag key"));
        }

        let mut global = Vec::new();
        let mut categories = 0;
        let mut code = 0u32;
        while global_size != 0 {
            let key = self.number()?;
            if !global.is_empty() && key >> 2 == 0 {
                return Err(self.invalid("duplicate global tile export tag key"));
            }
            code = code
                .checked_add(key >> 2)
                .filter(|&c| c <= 8190)
                .ok_or_else(|| self.invalid("invalid global tile export tag key"))?;
            self.global_string(code)?;
            let value = self.value(key, strings)?;
            self.consume(&mut global_size, 2 + value.width())?;
            categories |= self.categories[code as usize];
            push(&mut global, (code as u16, value))?;
        }
        Ok(Tags {
            local,
            global,
            categories,
        })
    }

    fn value(&mut self, key: u32, strings: &[&[u8]]) -> Result<Value> {
        let n = self.number()?;
        match key & 3 {
            0 => Ok(Value::Narrow(u16::try_from(n).map_err(|_| {
                self.invalid("tile export tag value exceeds u16")
            })?)),
            1 => Ok(Value::Global(self.global_string(n)?)),
            2 => Ok(Value::Wide(n)),
            _ => Ok(Value::Local(self.local_string(n, strings)?)),
        }
    }

    fn global_string(&self, number: u32) -> Result<u16> {
        u16::try_from(number)
            .ok()
            .filter(|&n| usize::from(n) < self.string_count)
            .ok_or_else(|| self.invalid("global string outside dictionary"))
    }

    fn local_string(&self, number: u32, strings: &[&[u8]]) -> Result<usize> {
        self.reference(
            number,
            strings.len(),
            "local string outside tile export table",
        )
    }

    fn reference(&self, number: u32, count: usize, error: &'static str) -> Result<usize> {
        let n = number as usize;
        if n >= count {
            return Err(self.invalid(error));
        }
        Ok(n)
    }

    fn consume(&self, remaining: &mut usize, width: usize) -> Result<()> {
        *remaining = remaining
            .checked_sub(width)
            .ok_or_else(|| self.invalid("tile export entry exceeds its table size"))?;
        Ok(())
    }

    fn feature(
        &mut self,
        (kind, id): (Kind, u64),
        index: &FeatureIndex,
        tile: &mut Tile<'a>,
        shared_tags: usize,
        shared_parents: usize,
    ) -> Result<Feature<'a>> {
        let changes = self.byte()?;
        if changes & (TAGS | GEOMETRY) != TAGS | GEOMETRY || changes & 128 != 0 {
            return Err(self.invalid("invalid full tile export feature flags"));
        }
        let tag = if changes & SHARED_TAGS != 0 {
            let n = self.number()?;
            self.reference(n, shared_tags, "shared tag table outside tile export")?
        } else {
            push(&mut tile.tags, self.tags(&tile.strings)?)?;
            tile.tags.len() - 1
        };
        let parent = if changes & PARENTS == 0 {
            None
        } else {
            match self.number()? {
                0 => None,
                n if n & 1 != 0 => Some(self.reference(
                    n >> 1,
                    shared_parents,
                    "shared parent table outside tile export",
                )?),
                size => {
                    push(&mut tile.parents, self.parents(size as usize, index)?)?;
                    Some(tile.parents.len() - 1)
                }
            }
        };
        let mut flags = if parent.is_some() {
            Feature::HAS_PARENTS
        } else {
            0
        };
        let (bounds, body) = match kind {
            Kind::Node => {
                let point = self.first_point()?;
                if !self.bounds.contains(point) {
                    return Err(self.invalid("node outside tile bounds"));
                }
                if changes & MEMBERS != 0 {
                    flags |= Feature::WAY_NODE;
                }
                if changes & IDS_OR_BOUNDS != 0 {
                    flags |= Feature::SHARED_LOCATION;
                }
                if changes & AREA_OR_EXCEPTION != 0 {
                    flags |= Feature::EXCEPTION_NODE;
                }
                (
                    Bounds {
                        min: point,
                        max: point,
                    },
                    Body::Node,
                )
            }
            Kind::Way => {
                flags |= 8;
                let (bbox, body) = self.way(changes, index)?;
                if matches!(&body, Body::Way { nodes, .. } if !nodes.is_empty()) {
                    flags |= Feature::WAY_NODE;
                }
                (bbox, body)
            }
            Kind::Relation => {
                flags |= 16;
                if changes & (MEMBERS | IDS_OR_BOUNDS) != MEMBERS | IDS_OR_BOUNDS {
                    return Err(self.invalid("full tile export relation lacks a body or bounds"));
                }
                let bbox = self.relation_bounds()?;
                let size = self.number()? as usize;
                (
                    bbox,
                    Body::Relation(self.members(size, index, &tile.strings)?),
                )
            }
        };
        if kind != Kind::Node {
            if changes & AREA_OR_EXCEPTION != 0 {
                flags |= Feature::AREA;
            }
            if bounds.min.x < self.bounds.min.x {
                flags |= Feature::WEST;
            }
            if bounds.max.y > self.bounds.max.y {
                flags |= Feature::NORTH;
            }
        }
        Ok(Feature {
            id,
            flags,
            bounds,
            tags: tag,
            parents: parent,
            body,
        })
    }

    fn point(&mut self, previous: Point) -> Result<Point> {
        let x = self.coordinate_delta()?;
        let y = self.coordinate_delta()?;
        Ok(Point {
            x: previous.x.wrapping_add(x),
            y: previous.y.wrapping_add(y),
        })
    }

    fn coordinate_delta(&mut self) -> Result<i32> {
        let delta = self.signed()?;
        if delta.unsigned_abs() > u64::from(u32::MAX) {
            return Err(self.invalid("tile export coordinate delta exceeds grid"));
        }
        Ok(delta as i32)
    }

    fn first_point(&mut self) -> Result<Point> {
        self.previous = self.point(self.previous)?;
        Ok(self.previous)
    }

    fn relation_bounds(&mut self) -> Result<Bounds> {
        let min = self.first_point()?;
        let width = i64::try_from(self.varint()?)
            .map_err(|_| self.invalid("invalid tile export relation width"))?;
        let height = i64::try_from(self.varint()?)
            .map_err(|_| self.invalid("invalid tile export relation height"))?;
        let axis = |base: i32, delta: i64| {
            i64::from(base)
                .checked_add(delta)
                .and_then(|n| i32::try_from(n).ok())
                .ok_or_else(|| self.invalid("tile export relation bounds exceed grid"))
        };
        Ok(Bounds {
            min,
            max: Point {
                x: axis(min.x, width)?,
                y: axis(min.y, height)?,
            },
        })
    }

    fn way(&mut self, changes: u8, index: &FeatureIndex) -> Result<(Bounds, Body<'a>)> {
        let count = self.number()?;
        if count == 0 || count as usize > (self.data.len() - self.pos) / 2 {
            return Err(self.invalid("invalid tile export way coordinate count"));
        }
        let first = self.first_point()?;
        let mut bounds = Bounds {
            min: first,
            max: first,
        };
        let mut point = first;
        let start = self.pos;
        for _ in 1..count {
            point = self.point(point)?;
            bounds = union(
                bounds,
                Bounds {
                    min: point,
                    max: point,
                },
            );
        }
        if !bounds.intersects(self.bounds) {
            return Err(self.invalid("way outside tile bounds"));
        }
        let coordinates_end = self.pos;
        if changes & IDS_OR_BOUNDS != 0 {
            let mut id = 0i64;
            for _ in 0..count {
                id = id
                    .checked_add(self.signed()?)
                    .filter(|&n| (0..1 << 52).contains(&n))
                    .ok_or_else(|| self.invalid("invalid tile export way node ID"))?;
            }
        } else if self.way_node_ids {
            return Err(self.invalid("tile export way lacks required node IDs"));
        }
        let end = if self.way_node_ids {
            self.pos
        } else {
            coordinates_end
        };
        let nodes = if changes & MEMBERS != 0 {
            let size = self.number()? as usize;
            self.way_nodes(size, index)?
        } else {
            Vec::new()
        };
        Ok((
            bounds,
            Body::Way {
                count,
                first,
                deltas_and_ids: &self.data[start..end],
                nodes,
            },
        ))
    }

    fn way_nodes(&mut self, mut size: usize, index: &FeatureIndex) -> Result<Vec<Link>> {
        let mut nodes = Vec::new();
        let mut foreign = Foreign::new(false);
        while size != 0 {
            let word = self.number()?;
            let link = if word & 1 != 0 {
                self.foreign(&mut foreign, word >> 2, word & 2 != 0)?
            } else {
                let n =
                    self.reference(word >> 1, index.ids.len(), "way node outside tile export")?;
                if index.ids[n].0 != Kind::Node {
                    return Err(self.invalid("way node references a non-node feature"));
                }
                Link::Local(n)
            };
            self.consume(&mut size, link.width(false))?;
            push(&mut nodes, link)?;
        }
        Ok(nodes)
    }

    fn parents(&mut self, mut size: usize, index: &FeatureIndex) -> Result<Vec<Link>> {
        if size == 0 {
            return Err(self.invalid("empty shared tile export parent table"));
        }
        let mut parents = Vec::new();
        let mut foreign = Foreign::new(false);
        let mut foreign_started = false;
        while size != 0 {
            let word = self.number()?;
            let different_tile = word & 1 != 0;
            foreign_started |= different_tile;
            let link = if foreign_started {
                self.foreign(&mut foreign, word >> 1, different_tile)?
            } else {
                let n = self.reference(
                    word >> 1,
                    index.ids.len() - index.first_relation,
                    "parent relation outside tile export",
                )?;
                Link::Local(index.first_relation + n)
            };
            self.consume(&mut size, link.width(false))?;
            push(&mut parents, link)?;
        }
        Ok(parents)
    }

    fn members(
        &mut self,
        mut size: usize,
        index: &FeatureIndex,
        strings: &[&[u8]],
    ) -> Result<Vec<Member>> {
        let mut members = Vec::new();
        let mut foreign = Foreign::new(true);
        while size != 0 {
            let word = self.number()?;
            let link = if word & 1 != 0 {
                self.foreign(&mut foreign, word >> 3, word & 4 != 0)?
            } else {
                Link::Local(self.reference(
                    word >> 2,
                    index.ids.len(),
                    "member outside tile export",
                )?)
            };
            self.consume(&mut size, link.width(true))?;
            let role = if word & 2 != 0 {
                let n = self.number()?;
                if n & 1 != 0 {
                    self.consume(&mut size, 2)?;
                    let code = self.global_string(n >> 1)?;
                    if code >= 1 << 15 {
                        return Err(self.invalid("global role exceeds 15 bits"));
                    }
                    Some(Role::Global(code))
                } else {
                    self.consume(&mut size, 4)?;
                    Some(Role::Local(self.local_string(n >> 1, strings)?))
                }
            } else {
                None
            };
            push(&mut members, Member { link, role })?;
        }
        Ok(members)
    }

    fn foreign(
        &mut self,
        state: &mut Foreign,
        encoded_export: u32,
        different_tile: bool,
    ) -> Result<Link> {
        let export_delta = ((encoded_export >> 1) as i32) ^ -((encoded_export & 1) as i32);
        let tile_delta = if different_tile {
            let delta = self.signed()?;
            state.tip = state
                .tip
                .checked_add(delta)
                .filter(|n| (0..1 << 24).contains(n))
                .ok_or_else(|| self.invalid("foreign tile position exceeds 24 bits"))?;
            Some(delta as i32)
        } else {
            None
        };
        state.export += i64::from(export_delta);
        if !(0..1 << 28).contains(&state.export) {
            return Err(self.invalid("foreign export position outside tile address space"));
        }
        Ok(Link::Foreign {
            export_delta,
            tile_delta,
        })
    }

    fn exports(&mut self, features: usize) -> Result<Vec<Option<usize>>> {
        let header = self.number()?;
        let with_holes = header & 1 != 0;
        if with_holes && header & 2 == 0 {
            return Err(self.invalid("tile export contains an incremental export table"));
        }
        let count = (header >> if with_holes { 2 } else { 1 }) as usize;
        if count > self.data.len() - self.pos {
            return Err(self.invalid("export count exceeds remaining tile bytes"));
        }
        let mut exports = Vec::new();
        for _ in 0..count {
            let n = self.number()?;
            let slot = match (with_holes, n) {
                (true, 0) => None,
                (true, _) => Some(n - 1),
                (false, _) => Some(n),
            };
            let index = slot
                .map(|n| self.reference(n, features, "export references unknown feature"))
                .transpose()?;
            push(&mut exports, index)?;
        }
        Ok(exports)
    }
}

struct Foreign {
    tip: i64,
    export: i64,
}

impl Foreign {
    fn new(member: bool) -> Self {
        Self {
            tip: 0x4000,
            export: if member { 0x400 } else { 0x800 },
        }
    }
}

pub(super) fn union(a: Bounds, b: Bounds) -> Bounds {
    Bounds {
        min: Point {
            x: a.min.x.min(b.min.x),
            y: a.min.y.min(b.min.y),
        },
        max: Point {
            x: a.max.x.max(b.max.x),
            y: a.max.y.max(b.max.y),
        },
    }
}
