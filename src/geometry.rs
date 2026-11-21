use crate::{Bounds, Error, Feature, FeatureId, FeatureType, Point, Result, source::Source};
use std::collections::{HashMap, hash_map::Entry};

/// An exterior ring followed by zero or more interior rings. Rings are closed;
/// their input orientation is preserved. Coordinates remain integer Mercator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Polygon {
    /// The closed outer boundary.
    pub exterior: Vec<Point>,
    /// Closed rings representing holes in the exterior.
    pub interiors: Vec<Vec<Point>>,
}

/// Explicitly owned geometry. Streaming coordinates/members avoid these allocations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Geometry {
    /// A single node position.
    Point(Point),
    /// An ordered sequence of way vertices.
    LineString(Vec<Point>),
    /// One exterior ring with its holes.
    Polygon(Polygon),
    /// Multiple polygon components.
    MultiPolygon(Vec<Polygon>),
    /// Member geometries of a non-area relation.
    Collection(Vec<Geometry>),
}

impl<'a, S: Source> Feature<'a, S> {
    /// Assemble owned geometry, rejecting cyclic relation expansion and incomplete
    /// rings. Total coordinates and recursion are bounded by the library limits.
    pub fn geometry(self) -> Result<Geometry> {
        let mut budget = Budget {
            remaining: self.library.limits.max_coordinates,
            steps: self.library.limits.max_traversal_steps,
            path: Vec::new(),
        };
        assemble(self, &mut budget)
    }
}

struct Budget {
    remaining: u64,
    steps: u64,
    path: Vec<FeatureId>,
}
impl Budget {
    #[inline]
    fn spend(&mut self, count: u64) -> Result<()> {
        self.remaining = self
            .remaining
            .checked_sub(count)
            .ok_or(Error::Limit("assembled geometry coordinates"))?;
        Ok(())
    }
}
fn points<S: Source>(feature: Feature<'_, S>, budget: &mut Budget) -> Result<Vec<Point>> {
    let coordinates = feature.coordinates()?;
    // Avoid repeated growth for ordinary ways, without eagerly allocating from
    // an arbitrarily large declared count in untrusted input.
    let upper = coordinates.size_hint().1;
    let capacity = upper.unwrap_or(0).min(budget.remaining.min(1024) as usize);
    if upper.is_some_and(|count| count as u64 <= budget.remaining) {
        // The count includes any closing point. A valid way fitting this
        // budget can charge the entire collection once.
        let out = coordinates.collect_owned(capacity)?;
        budget.spend(out.len() as u64)?;
        return Ok(out);
    }
    // Preserve the original limit and error order when the way may exceed
    // the budget, including when a malformed coordinate consumes the last slot.
    let mut out = Vec::with_capacity(capacity);
    for point in coordinates {
        budget.spend(1)?;
        out.push(point?);
    }
    Ok(out)
}
fn assemble<S: Source>(feature: Feature<'_, S>, budget: &mut Budget) -> Result<Geometry> {
    budget.steps = budget
        .steps
        .checked_sub(1)
        .ok_or(Error::Limit("geometry traversal steps"))?;
    match feature.kind() {
        FeatureType::Node => {
            budget.spend(1)?;
            Ok(Geometry::Point(
                feature.point()?.ok_or(Error::WrongFeatureType)?,
            ))
        }
        FeatureType::Way => {
            let points = points(feature, budget)?;
            if feature.is_area() {
                validate_ring(&points)?;
                Ok(Geometry::Polygon(Polygon {
                    exterior: points,
                    interiors: Vec::new(),
                }))
            } else {
                Ok(Geometry::LineString(points))
            }
        }
        FeatureType::Relation => {
            if budget.path.len() >= feature.library.limits.max_tree_depth {
                return Err(Error::Limit("geometry relation depth"));
            }
            if budget.path.contains(&feature.id()) {
                return Err(Error::Geometry("cyclic relation"));
            }
            budget.path.push(feature.id());
            let result = if feature.is_area() {
                multipolygon(feature, budget)
            } else {
                let mut collection = Vec::new();
                for member in feature.members()? {
                    match assemble(member?.feature, budget)? {
                        Geometry::Collection(children) => collection.extend(children),
                        geometry => collection.push(geometry),
                    }
                }
                Ok(Geometry::Collection(collection))
            };
            budget.path.pop();
            result
        }
    }
}

fn multipolygon<S: Source>(feature: Feature<'_, S>, budget: &mut Budget) -> Result<Geometry> {
    let mut outer = Vec::new();
    let mut inner = Vec::new();
    for member in feature.members()? {
        let member = member?;
        if member.feature.kind() != FeatureType::Way {
            continue;
        }
        match member.role.as_ref() {
            "outer" | "" => outer.push(points(member.feature, budget)?),
            "inner" => inner.push(points(member.feature, budget)?),
            _ => {}
        }
    }
    polygon_geometry(outer, inner)
}

fn polygon_geometry(outer: Vec<Vec<Point>>, inner: Vec<Vec<Point>>) -> Result<Geometry> {
    let mut polygons: Vec<_> = rings(outer)?
        .into_iter()
        .map(|exterior| Polygon {
            exterior,
            interiors: Vec::new(),
        })
        .collect();
    assign_holes(&mut polygons, rings(inner)?)?;
    // Exterior components retain their OSM boundaries. Only holes assigned to
    // the same shell take part in removing shared inner edges.
    for polygon in &mut polygons {
        if polygon.interiors.len() > 1 {
            polygon.interiors = dissolve_holes(std::mem::take(&mut polygon.interiors))?;
        }
    }
    if polygons.len() == 1 {
        Ok(Geometry::Polygon(polygons.remove(0)))
    } else {
        Ok(Geometry::MultiPolygon(polygons))
    }
}

fn assign_holes(polygons: &mut [Polygon], holes: Vec<Vec<Point>>) -> Result<()> {
    if holes.is_empty() {
        return Ok(());
    }
    // Preserve explicit inner roles, including invalid OSM topology. With one
    // shell the attachment is unambiguous even if the rings do not nest.
    if let [polygon] = polygons {
        polygon.interiors = holes;
        return Ok(());
    }
    if polygons.is_empty() {
        return Err(Error::Geometry("inner ring has no exterior"));
    }
    // Only build this temporary index when there are holes and competing
    // exteriors. These rings have already been validated and budgeted.
    let mut exteriors: Vec<_> = polygons
        .iter()
        .map(|polygon| {
            let first = polygon.exterior[0];
            let mut bounds = Bounds {
                min: first,
                max: first,
            };
            for point in &polygon.exterior[1..] {
                bounds.min.x = bounds.min.x.min(point.x);
                bounds.min.y = bounds.min.y.min(point.y);
                bounds.max.x = bounds.max.x.max(point.x);
                bounds.max.y = bounds.max.y.max(point.y);
            }
            (bounds, None)
        })
        .collect();
    for hole in holes {
        let point = hole[0];
        let mut best: Option<(usize, i128)> = None;
        for (i, (polygon, (bounds, area))) in polygons.iter().zip(&mut exteriors).enumerate() {
            if bounds.contains(point) && point_in_ring(point, &polygon.exterior) {
                let area = *area.get_or_insert_with(|| signed_area(&polygon.exterior).abs());
                if best.is_none_or(|(_, smallest)| area < smallest) {
                    best = Some((i, area));
                }
            }
        }
        // GeoDesk attaches an orphan to the first exterior in assembly order.
        let target = best.map_or(0, |(i, _)| i);
        polygons[target].interiors.push(hole);
    }
    Ok(())
}

pub(crate) fn validate_ring(ring: &[Point]) -> Result<()> {
    if ring.len() < 4 || ring.first() != ring.last() {
        return Err(Error::Geometry(
            "ring is unclosed or has fewer than four coordinates",
        ));
    }
    Ok(())
}

/// Decompose the endpoint graph into closed walks. When a walk revisits an
/// endpoint, emit that cycle immediately; a junction shared by touching rings
/// therefore does not splice the rings into a self-touching figure eight.
/// Each segment and endpoint occurrence is consumed once.
fn rings(segments: Vec<Vec<Point>>) -> Result<Vec<Vec<Point>>> {
    // A set of already closed member ways would enter and close each chain
    // in member order without joining any coordinates.
    if segments
        .iter()
        .all(|segment| segment.len() >= 2 && segment.first() == segment.last())
    {
        for segment in &segments {
            validate_ring(segment)?;
        }
        return Ok(segments);
    }
    let mut endpoints: HashMap<Point, Vec<usize>> = HashMap::new();
    for (i, segment) in segments.iter().enumerate().rev() {
        if segment.len() < 2 {
            return Err(Error::Geometry(
                "polygon member has fewer than two coordinates",
            ));
        }
        endpoints.entry(segment[0]).or_default().push(i);
        endpoints
            .entry(segment[segment.len() - 1])
            .or_default()
            .push(i);
    }
    let mut segments: Vec<_> = segments.into_iter().map(Some).collect();
    let mut out = Vec::new();
    for start in 0..segments.len() {
        let Some(segment) = &segments[start] else {
            continue;
        };
        let mut chain = vec![segment[0]];
        let mut positions = HashMap::from([(segment[0], 0)]);
        let mut visited = vec![segment[0]];
        loop {
            let end = *chain.last().ok_or(Error::Geometry("empty ring"))?;
            let candidates = endpoints
                .get_mut(&end)
                .ok_or(Error::Geometry("unconnected polygon member"))?;
            let next = loop {
                let i = candidates
                    .pop()
                    .ok_or(Error::Geometry("unclosed polygon chain"))?;
                if segments[i].is_some() {
                    break i;
                }
            };
            let mut segment = segments[next]
                .take()
                .ok_or(Error::Geometry("reused polygon member"))?;
            if segment[0] != end {
                segment.reverse();
            }
            chain.extend(segment.into_iter().skip(1));
            let end = *chain.last().ok_or(Error::Geometry("empty ring"))?;
            if let Some(&offset) = positions.get(&end) {
                let ring = chain.split_off(offset);
                validate_ring(&ring)?;
                out.push(ring);
                chain.push(end);
                while visited.last() != Some(&end) {
                    if let Some(point) = visited.pop() {
                        positions.remove(&point);
                    }
                }
                if offset == 0 {
                    break;
                }
            } else {
                positions.insert(end, chain.len() - 1);
                visited.push(end);
            }
        }
    }
    Ok(out)
}

/// Remove inner edges with multiple occurrences, irrespective of ring winding.
fn dissolve_holes(input: Vec<Vec<Point>>) -> Result<Vec<Vec<Point>>> {
    if input.len() < 2 {
        return Ok(input);
    }
    let removed = repeated_edges(&input);
    if removed.is_empty() {
        return Ok(input);
    }
    let mut segments = Vec::new();
    for (ring, removed) in input.into_iter().zip(removed) {
        if removed.is_empty() {
            segments.push(ring);
            continue;
        }
        let mut segment = Vec::new();
        for (edge, removed) in ring.windows(2).zip(removed) {
            if removed {
                if !segment.is_empty() {
                    segments.push(std::mem::take(&mut segment));
                }
            } else {
                if segment.is_empty() {
                    segment.push(edge[0]);
                }
                segment.push(edge[1]);
            }
        }
        if !segment.is_empty() {
            segments.push(segment);
        }
    }
    rings(segments)
}

// A large ring usually has many edges that occur just once. Each bucket uses
// one bit for "seen" and one for "seen again". Only the latter can contain
// an edge that needs an exact lookup. Collisions add work but cannot remove
// or merge any edges. Both maps are discarded before rebuilding the rings.
struct EdgeBuckets {
    bits: Vec<[u64; 2]>,
    shift: u32,
    collisions: usize,
}

impl EdgeBuckets {
    fn for_rings(input: &[Vec<Point>], largest: usize) -> Option<Self> {
        if largest < 1024 {
            return None;
        }
        // Round the largest ring up to a power of two, then allow 16 two-bit
        // buckets per edge, up to 4 MiB for the entire filter.
        let slots = largest.min(1 << 20).next_power_of_two() * 16;
        let mut filter = Self {
            bits: vec![[0; 2]; slots / 64],
            shift: u64::BITS - slots.ilog2(),
            collisions: 0,
        };
        for ring in input {
            for edge in ring.windows(2) {
                let a = pack_point(edge[0]);
                let b = pack_point(edge[1]);
                if a != b {
                    let (group, mask) = filter.bucket(a, b);
                    let bits = &mut filter.bits[group];
                    let seen = bits[0] & mask;
                    bits[0] |= mask;
                    bits[1] |= seen;
                    filter.collisions += usize::from(seen != 0);
                }
            }
        }
        Some(filter)
    }

    fn bucket(&self, a: u64, b: u64) -> (usize, u64) {
        // The sum and XOR are independent of the direction of the edge.
        let hash = a.wrapping_add(b) ^ (a ^ b).rotate_left(27);
        let index = (hash.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> self.shift) as usize;
        (index / 64, 1 << (index % 64))
    }

    fn may_repeat(&self, a: u64, b: u64) -> bool {
        let (group, mask) = self.bucket(a, b);
        self.bits[group][1] & mask != 0
    }
}

fn pack_point(point: Point) -> u64 {
    // Keep the lanes in Point's logical x, y order. On targets whose
    // byte order matches, the optimizer can combine the two field loads.
    (u64::from(point.y as u32) << 32) | u64::from(point.x as u32)
}

fn repeated_edges(input: &[Vec<Point>]) -> Vec<Vec<bool>> {
    let largest = input.iter().map(|ring| ring.len() - 1).max().unwrap_or(0);
    let filter = EdgeBuckets::for_rings(input, largest);
    // Each repeated bucket has at most twice as many entries as collisions.
    // Bound the initial reservation by the largest ring and grow as required.
    let reserve = filter
        .as_ref()
        .map_or(largest, |f| largest.min(f.collisions.saturating_mul(2)));
    let mut removed: Vec<Vec<bool>> = Vec::new();
    if reserve == 0 {
        return removed;
    }
    let mut edges = HashMap::with_capacity(reserve);
    for (i, ring) in input.iter().enumerate() {
        for (j, edge) in ring.windows(2).enumerate() {
            let a = pack_point(edge[0]);
            let b = pack_point(edge[1]);
            if a == b || filter.as_ref().is_some_and(|f| !f.may_repeat(a, b)) {
                continue;
            }
            // Both directions use the same key, so one lookup suffices.
            let key = if a < b { (a, b) } else { (b, a) };
            match edges.entry(key) {
                Entry::Vacant(entry) => {
                    entry.insert((i, j));
                }
                Entry::Occupied(entry) => {
                    if removed.is_empty() {
                        removed.resize_with(input.len(), Vec::new);
                    }
                    for (ring_index, offset) in [*entry.get(), (i, j)] {
                        let flags = &mut removed[ring_index];
                        if flags.is_empty() {
                            flags.resize(input[ring_index].len() - 1, false);
                        }
                        flags[offset] = true;
                    }
                }
            }
        }
    }
    removed
}

fn cross(a: Point, b: Point, p: Point) -> i128 {
    (i128::from(b.x) - i128::from(a.x)) * (i128::from(p.y) - i128::from(a.y))
        - (i128::from(b.y) - i128::from(a.y)) * (i128::from(p.x) - i128::from(a.x))
}
fn on_segment(a: Point, b: Point, p: Point) -> bool {
    p.x >= a.x.min(b.x)
        && p.x <= a.x.max(b.x)
        && p.y >= a.y.min(b.y)
        && p.y <= a.y.max(b.y)
        && cross(a, b, p) == 0
}
fn intersects_segment(a: Point, b: Point, c: Point, d: Point) -> bool {
    if on_segment(a, b, c) || on_segment(a, b, d) || on_segment(c, d, a) || on_segment(c, d, b) {
        return true;
    }
    let ca = cross(a, b, c);
    let da = cross(a, b, d);
    let ac = cross(c, d, a);
    let bc = cross(c, d, b);
    ca.signum() != da.signum()
        && ac.signum() != bc.signum()
        && ca != 0
        && da != 0
        && ac != 0
        && bc != 0
}
fn point_in_ring(p: Point, ring: &[Point]) -> bool {
    let mut inside = false;
    for edge in ring.windows(2) {
        let a = edge[0];
        let b = edge[1];
        if on_segment(a, b, p) {
            return true;
        }
        if (a.y > p.y) != (b.y > p.y) {
            let orientation = cross(a, b, p);
            if (orientation > 0) == (b.y > a.y) {
                inside = !inside;
            }
        }
    }
    inside
}
const AREA_CHUNK_EDGES: usize = 1 << 19;

/// Twice the signed area. A closed ring includes its first point again at the end.
pub(crate) fn signed_area(ring: &[Point]) -> i128 {
    let edges = ring.len().saturating_sub(1);
    let mut total = 0i128;
    for start in (0..edges).step_by(AREA_CHUNK_EDGES) {
        let end = (start + AREA_CHUNK_EDGES).min(edges);
        let mut high = 0i64;
        let mut low = 0i64;
        // An i32 product lies in [-2^62 + 2^31, 2^62], so their difference fits i64.
        // Sum its two halves independently so the edge loop can vectorise. In a
        // chunk, |high| <= 2^50 and low < 2^51, even at the coordinate extremes.
        for edge in ring[start..=end].windows(2) {
            let cross = i64::from(edge[0].x) * i64::from(edge[1].y)
                - i64::from(edge[1].x) * i64::from(edge[0].y);
            high += cross >> 32;
            low += cross & i64::from(u32::MAX);
        }
        total += (i128::from(high) << 32) + i128::from(low);
    }
    total
}
fn line_intersects_box(line: &[Point], bounds: Bounds) -> bool {
    if line.iter().any(|p| bounds.contains(*p)) {
        return true;
    }
    let a = bounds.min;
    let c = bounds.max;
    let b = Point { x: c.x, y: a.y };
    let d = Point { x: a.x, y: c.y };
    line.windows(2).any(|w| {
        [(a, b), (b, c), (c, d), (d, a)]
            .iter()
            .any(|&(p, q)| intersects_segment(w[0], w[1], p, q))
    })
}
impl Polygon {
    /// Test whether a point lies in the exterior ring and outside all hole rings.
    pub fn contains_point(&self, point: Point) -> bool {
        point_in_ring(point, &self.exterior)
            && !self.interiors.iter().any(|ring| point_in_ring(point, ring))
    }
    fn intersects_bounds(&self, bounds: Bounds) -> bool {
        line_intersects_box(&self.exterior, bounds)
            || self
                .interiors
                .iter()
                .any(|r| line_intersects_box(r, bounds))
            || self.contains_point(bounds.min)
    }
}
impl Geometry {
    /// Exact geometry/rectangle intersection in integer Mercator coordinates.
    /// Includes boundary contacts, unlike an interior-only containment test.
    pub fn intersects_bounds(&self, bounds: Bounds) -> bool {
        match self {
            Self::Point(p) => bounds.contains(*p),
            Self::LineString(line) => line_intersects_box(line, bounds),
            Self::Polygon(p) => p.intersects_bounds(bounds),
            Self::MultiPolygon(polys) => polys.iter().any(|p| p.intersects_bounds(bounds)),
            Self::Collection(geoms) => geoms.iter().any(|g| g.intersects_bounds(bounds)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn p(x: i32, y: i32) -> Point {
        Point { x, y }
    }
    #[test]
    fn signed_area_preserves_unit_differences_at_extreme_coordinates() {
        let max = i32::MAX;
        let mut triangle = [p(0, 0), p(max, max - 1), p(max - 1, max - 2), p(0, 0)];
        assert_eq!(signed_area(&[]), 0);
        assert_eq!(signed_area(&triangle[..1]), 0);
        assert_eq!(signed_area(&triangle), -1);
        triangle.reverse();
        assert_eq!(signed_area(&triangle), 1);
    }
    #[test]
    fn signed_area_counts_each_edge_across_chunk_boundaries() {
        let (lo, hi) = (i32::MIN, i32::MAX);
        let corners = [p(lo, lo), p(hi, lo), p(hi, hi), p(lo, hi)];
        let mut points: Vec<_> = corners
            .into_iter()
            .cycle()
            .take(AREA_CHUNK_EDGES + 2)
            .collect();
        for _ in 0..2 {
            for edges in [AREA_CHUNK_EDGES - 1, AREA_CHUNK_EDGES, AREA_CHUNK_EDGES + 1] {
                let ring = &points[..=edges];
                let expected: i128 = ring
                    .windows(2)
                    .map(|edge| {
                        i128::from(edge[0].x) * i128::from(edge[1].y)
                            - i128::from(edge[1].x) * i128::from(edge[0].y)
                    })
                    .sum();
                assert!(expected.abs() > i128::from(i64::MAX));
                assert_eq!(signed_area(ring), expected);
            }
            points.reverse();
        }
    }
    #[test]
    fn hole_assignment_preserves_smallest_exterior_ties_and_boundaries() {
        let square = |lo, hi| vec![p(lo, lo), p(hi, lo), p(hi, hi), p(lo, hi), p(lo, lo)];
        let mut reversed = square(-5, 5);
        reversed.reverse();
        let exteriors = [
            square(-10, 10),
            reversed,
            square(-5, 5), // Equal area: the earlier exterior must win.
            vec![p(20, 0), p(30, 0), p(30, 10), p(25, 2), p(20, 10), p(20, 0)],
            square(i32::MIN, i32::MAX),
        ];
        for count in 0..=exteriors.len() {
            for point in [p(0, 0), p(5, 0), p(8, 0), p(25, 7), p(30, 0), p(50, 50)] {
                let mut actual: Vec<_> = exteriors[..count]
                    .iter()
                    .cloned()
                    .map(|exterior| Polygon {
                        exterior,
                        interiors: Vec::new(),
                    })
                    .collect();
                // Deliberately use the exhaustive containment/area oracle,
                // independent of the bounds and lazy area cache under test.
                let expected = actual
                    .iter()
                    .enumerate()
                    .filter(|(_, poly)| point_in_ring(point, &poly.exterior))
                    .min_by_key(|(_, poly)| signed_area(&poly.exterior).abs())
                    .map(|(i, _)| i)
                    .or_else(|| (!actual.is_empty()).then_some(0));
                let hole = vec![
                    point,
                    p(point.x + 1, point.y),
                    p(point.x, point.y + 1),
                    point,
                ];
                let result = assign_holes(&mut actual, vec![hole.clone(); 3]);
                assert_eq!(result.is_ok(), expected.is_some());
                if let Some(target) = expected {
                    for (i, poly) in actual.iter().enumerate() {
                        assert_eq!(poly.interiors.len(), if i == target { 3 } else { 0 });
                        assert!(poly.interiors.iter().all(|ring| ring == &hole));
                    }
                }
            }
        }
        assert!(assign_holes(&mut [], Vec::new()).is_ok());
    }
    #[test]
    fn joins_reversed_segments_without_reordering_vertices() {
        let result = rings(vec![
            vec![p(0, 0), p(4, 0)],
            vec![p(4, 4), p(4, 0)],
            vec![p(0, 0), p(0, 4), p(4, 4)],
        ])
        .unwrap();
        assert_eq!(
            result,
            vec![vec![p(0, 0), p(4, 0), p(4, 4), p(0, 4), p(0, 0)]]
        );
    }
    #[test]
    fn rejects_unclosed_and_odd_degree_chains() {
        assert!(rings(vec![vec![p(0, 0), p(4, 0)]]).is_err());
        assert!(
            rings(vec![
                vec![p(0, 0), p(4, 0)],
                vec![p(4, 0), p(4, 4)],
                vec![p(4, 0), p(0, 4)]
            ])
            .is_err()
        );
    }
    #[test]
    fn touching_rings_preserve_every_segment() {
        let result = rings(vec![
            vec![p(0, 0), p(4, 0), p(4, 4)],
            vec![p(4, 4), p(8, 4), p(8, 8)],
            vec![p(4, 4), p(4, 8), p(8, 8)],
            vec![p(0, 0), p(0, 4), p(4, 4)],
        ])
        .unwrap();
        assert_eq!(result.len(), 2);
        assert!(
            result
                .iter()
                .all(|r| r.len() == 5 && signed_area(r).abs() == 32)
        );
    }
    #[test]
    fn adjacent_exteriors_retain_shared_edges_and_their_own_holes() {
        let left = vec![p(0, 0), p(5, 0), p(5, 5), p(0, 5), p(0, 0)];
        let right = vec![p(5, 0), p(10, 0), p(10, 5), p(5, 5), p(5, 0)];
        let hole = vec![p(7, 2), p(8, 2), p(8, 3), p(7, 2)];
        let actual =
            polygon_geometry(vec![left.clone(), right.clone()], vec![hole.clone()]).unwrap();
        assert_eq!(
            actual,
            Geometry::MultiPolygon(vec![
                Polygon {
                    exterior: left,
                    interiors: vec![]
                },
                Polygon {
                    exterior: right,
                    interiors: vec![hole]
                },
            ])
        );
    }

    #[test]
    fn orphan_hole_remains_attached_to_the_declared_shell() {
        let shell = vec![p(0, 0), p(2, 0), p(2, 2), p(0, 0)];
        let hole = vec![p(5, 5), p(7, 5), p(7, 7), p(5, 5)];
        assert_eq!(
            polygon_geometry(vec![shell.clone()], vec![hole.clone()]).unwrap(),
            Geometry::Polygon(Polygon {
                exterior: shell,
                interiors: vec![hole]
            })
        );
    }

    #[test]
    fn dissolves_adjacent_holes_and_keeps_zero_length_edges() {
        let (a, b, c, d) = (p(0, 0), p(2, 0), p(2, 2), p(0, 2));
        let (e, f) = (p(4, 0), p(4, 2));
        let left = vec![a, b, b, c, d, a];
        let right = vec![b, c, f, e, b];
        assert_eq!(
            dissolve_holes(vec![left, right]).unwrap(),
            vec![vec![a, b, b, e, f, c, d, a]]
        );
    }

    #[test]
    fn coincident_holes_cancel_across_rotation_winding_and_multiplicity() {
        let (a, b, c, d) = (p(0, 0), p(2, 0), p(2, 2), p(0, 2));
        let ring = vec![a, b, c, d, a];
        let rotated = vec![d, a, b, c, d];
        let reversed = vec![a, d, c, b, a];
        let island = vec![p(4, 0), p(5, 0), p(5, 1), p(4, 0)];
        for copies in [
            vec![ring.clone(), rotated.clone()],
            vec![ring.clone(), reversed.clone()],
            vec![ring, rotated, reversed],
        ] {
            let mut holes = vec![island.clone()];
            holes.extend(copies);
            assert_eq!(
                dissolve_holes(holes).unwrap(),
                std::slice::from_ref(&island)
            );
        }
    }

    #[test]
    fn large_ring_filter_preserves_shared_and_coincident_hole_edges() {
        let mut base: Vec<_> = (0..=2048).map(|x| p(x, 0)).collect();
        let mut expected = base.clone();
        base.extend([p(2048, 2), p(0, 2), p(0, 0)]);
        expected.extend([p(2049, 0), p(2049, 2), p(2048, 2), p(0, 2), p(0, 0)]);
        let side = vec![p(2048, 0), p(2049, 0), p(2049, 2), p(2048, 2), p(2048, 0)];
        assert_eq!(
            dissolve_holes(vec![base.clone(), side]).unwrap(),
            [expected]
        );
        assert!(dissolve_holes(vec![base.clone(), base]).unwrap().is_empty());
    }

    #[test]
    fn hole_and_boundary_intersections() {
        let poly = Polygon {
            exterior: vec![p(0, 0), p(10, 0), p(10, 10), p(0, 10), p(0, 0)],
            interiors: vec![vec![p(3, 3), p(7, 3), p(7, 7), p(3, 7), p(3, 3)]],
        };
        assert!(poly.contains_point(p(1, 1)));
        assert!(!poly.contains_point(p(5, 5)));
        assert!(!poly.intersects_bounds(Bounds::new(4, 4, 6, 6).unwrap()));
        assert!(poly.intersects_bounds(Bounds::new(3, 4, 6, 6).unwrap()));
        assert!(poly.intersects_bounds(Bounds::new(0, 1, 1, 2).unwrap()));
    }
    #[test]
    fn extreme_coordinates_do_not_overflow() {
        let a = p(i32::MIN, i32::MIN);
        let b = p(i32::MAX, i32::MAX);
        assert!(intersects_segment(
            a,
            b,
            p(i32::MIN, i32::MAX),
            p(i32::MAX, i32::MIN)
        ));
    }
}
