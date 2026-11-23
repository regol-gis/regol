use geo::{
    Area as _, BoundingRect, Relate,
    coordinate_position::CoordPos,
    dimensions::Dimensions,
    line_intersection::{LineIntersection, line_intersection},
};
use regol::{Error, Result};
use rstar::{
    AABB, RTree,
    primitives::{GeomWithData, Rectangle},
};

type IndexedEdge = GeomWithData<Rectangle<[f64; 2]>, (usize, geo::Line)>;

pub(super) fn edge_envelope(edge: geo::Line) -> AABB<[f64; 2]> {
    AABB::from_corners([edge.start.x, edge.start.y], [edge.end.x, edge.end.y])
}

pub(super) fn edge_index(lines: impl Iterator<Item = geo::Line>) -> RTree<IndexedEdge> {
    RTree::bulk_load(
        lines
            .enumerate()
            .map(|(index, edge)| {
                GeomWithData::new(Rectangle::from_aabb(edge_envelope(edge)), (index, edge))
            })
            .collect(),
    )
}

pub(super) fn touches_only_at(
    ring: &geo::LineString,
    translated: &geo::LineString,
    contact: geo::Coord,
) -> bool {
    let edges = edge_index(ring.lines());
    translated.lines().all(|edge| {
        edges
            .locate_in_envelope_intersecting(&edge_envelope(edge))
            .all(|other| match line_intersection(edge, other.data.1) {
                None => true,
                Some(LineIntersection::SinglePoint { intersection, .. }) => intersection == contact,
                Some(LineIntersection::Collinear { .. }) => false,
            })
    })
}

pub(super) fn contains_ring(shell: &geo::Polygon, ring: &geo::LineString) -> bool {
    let relation = shell.relate(ring);
    relation.is_contains()
        && relation.get(CoordPos::OnBoundary, CoordPos::Inside) != Dimensions::OneDimensional
}

pub(super) fn validate_rings(
    exterior: &geo::LineString,
    interiors: &[geo::LineString],
) -> Result<()> {
    // geo's ring validation compares every pair of edges. Index edges
    // because a periodic polar polygon can contain several copies of a ring.
    for ring in std::iter::once(exterior).chain(interiors) {
        let edges = edge_index(ring.lines());
        let last = ring.0.len().saturating_sub(2);
        if last < 2
            || edges.iter().any(|item| {
                let (i, edge) = item.data;
                let envelope = edge_envelope(edge);
                edges
                    .locate_in_envelope_intersecting(&envelope)
                    .any(|other| {
                        let (j, other) = other.data;
                        if i >= j {
                            return false;
                        }
                        let Some(hit) = line_intersection(edge, other) else {
                            return false;
                        };
                        !(j == i + 1 || (i == 0 && j == last))
                            || !matches!(hit, LineIntersection::SinglePoint { .. })
                    })
            })
        {
            return Err(Error::Geometry(
                "polygon ring intersects itself or has too few vertices",
            ));
        }
    }
    let mut holes = interiors
        .iter()
        .map(|ring| {
            let polygon = geo::Polygon::new(ring.clone(), Vec::new());
            let extent = ring.bounding_rect().unwrap();
            (extent, polygon)
        })
        .collect::<Vec<_>>();
    holes.sort_unstable_by(|(a, _), (b, _)| a.min().x.total_cmp(&b.min().x));
    for (i, (bounds, hole)) in holes.iter().enumerate() {
        if hole.unsigned_area() == 0.0 {
            return Err(Error::Geometry("polygon hole has zero area"));
        }
        for (other, polygon) in holes[i + 1..]
            .iter()
            .take_while(|(other, _)| other.min().x <= bounds.max().x)
        {
            if bounds.min().y <= other.max().y && bounds.max().y >= other.min().y {
                let relation = hole.relate(polygon);
                if relation.get(CoordPos::Inside, CoordPos::Inside) == Dimensions::TwoDimensional
                    || relation.get(CoordPos::OnBoundary, CoordPos::OnBoundary)
                        == Dimensions::OneDimensional
                {
                    return Err(Error::Geometry("polygon holes overlap or share an edge"));
                }
            }
        }
    }
    Ok(())
}
