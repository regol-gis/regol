use crate::{Bounds, Error, Point, Result};

mod decode;
mod encode;

#[cfg(test)]
mod tests;

/// Materialize a full tile export stream in the layout used by the GOL reader.
/// Export slots and foreign deltas retain their original numbering, so adjacent
/// tiles can be converted and loaded in any order.
pub(super) fn convert(
    payload: &[u8],
    bounds: Bounds,
    way_node_ids: bool,
    categories: &[(u16, u32)],
    string_count: usize,
    max_output_bytes: usize,
) -> Result<Vec<u8>> {
    let tile = decode::read(payload, bounds, way_node_ids, categories, string_count)?;
    encode::write(tile, max_output_bytes)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Node,
    Way,
    Relation,
}

struct Tile<'a> {
    strings: Vec<&'a [u8]>,
    tags: Vec<Tags>,
    parents: Vec<Vec<Link>>,
    features: Vec<Feature<'a>>,
    exports: Vec<Option<usize>>,
}

struct Tags {
    local: Vec<(usize, Value)>,
    global: Vec<(u16, Value)>,
    categories: u32,
}

#[derive(Clone, Copy)]
enum Value {
    Narrow(u16),
    Global(u16),
    Wide(u32),
    Local(usize),
}

impl Value {
    fn kind(self) -> u32 {
        match self {
            Self::Narrow(_) => 0,
            Self::Global(_) => 1,
            Self::Wide(_) => 2,
            Self::Local(_) => 3,
        }
    }

    fn width(self) -> usize {
        2 + (self.kind() & 2) as usize
    }
}

struct Feature<'a> {
    id: u64,
    flags: u16,
    bounds: Bounds,
    tags: usize,
    parents: Option<usize>,
    body: Body<'a>,
}

impl Feature<'_> {
    const AREA: u16 = 1 << 1;
    const HAS_PARENTS: u16 = 1 << 2;
    const WAY_NODE: u16 = 1 << 5;
    const WEST: u16 = 1 << 6;
    const NORTH: u16 = 1 << 7;
    const SHARED_LOCATION: u16 = 1 << 8;
    const EXCEPTION_NODE: u16 = 1 << 9;

    fn index(&self) -> usize {
        match (&self.body, self.flags & Self::AREA != 0) {
            (Body::Node, _) => 0,
            (_, true) => 2,
            (Body::Way { .. }, false) => 1,
            (Body::Relation(_), false) => 3,
        }
    }
}

enum Body<'a> {
    Node,
    Way {
        count: u32,
        first: Point,
        // The wire format after the first point is already the GOL encoding.
        deltas_and_ids: &'a [u8],
        nodes: Vec<Link>,
    },
    Relation(Vec<Member>),
}

#[derive(Clone, Copy)]
enum Link {
    Local(usize),
    Foreign {
        export_delta: i32,
        tile_delta: Option<i32>,
    },
}

impl Link {
    fn width(self, member: bool) -> usize {
        match self {
            Self::Local(_) => 4,
            Self::Foreign {
                export_delta,
                tile_delta,
            } => {
                let export_shift = if member { 5 } else { 4 };
                let export_width = delta_width(export_delta << export_shift);
                export_width + tile_delta.map_or(0, |d| delta_width(d << 1))
            }
        }
    }
}

struct Member {
    link: Link,
    role: Option<Role>,
}

#[derive(Clone, Copy)]
enum Role {
    Global(u16),
    Local(usize),
}

fn delta_width(delta: i32) -> usize {
    if i16::try_from(delta).is_ok() { 2 } else { 4 }
}

fn invalid(pos: usize, message: &'static str) -> Error {
    super::invalid(pos as u64, message)
}

fn push<T>(vec: &mut Vec<T>, value: T) -> Result<()> {
    vec.try_reserve(1)
        .map_err(|_| Error::Limit("GOB tile conversion allocation"))?;
    vec.push(value);
    Ok(())
}
