use geo::{
    Line,
    line_intersection::{LineIntersection, line_intersection},
};
use rstar::{AABB, RTree, RTreeObject};

pub(super) type EdgeIndex = RTree<IndexedEdge>;

pub(super) struct IndexedEdge {
    pub(super) index: usize,
    pub(super) line: Line,
}

impl RTreeObject for IndexedEdge {
    type Envelope = AABB<[f64; 2]>;

    fn envelope(&self) -> Self::Envelope {
        AABB::from_corners(
            [self.line.start.x, self.line.start.y],
            [self.line.end.x, self.line.end.y],
        )
    }
}

pub(super) fn build(lines: impl IntoIterator<Item = Line>) -> EdgeIndex {
    RTree::bulk_load(
        lines
            .into_iter()
            .enumerate()
            .map(|(index, line)| IndexedEdge { index, line })
            .collect(),
    )
}

/// Enumerate segment pairs once, including intersections at their endpoints.
pub(super) fn intersections(
    edges: &EdgeIndex,
) -> impl Iterator<Item = (usize, usize, LineIntersection<f64>)> + '_ {
    edges.iter().flat_map(|left| {
        edges
            .locate_in_envelope_intersecting(left.envelope())
            .filter_map(move |right| {
                if left.index < right.index {
                    line_intersection(left.line, right.line)
                        .map(|hit| (left.index, right.index, hit))
                } else {
                    None
                }
            })
    })
}
