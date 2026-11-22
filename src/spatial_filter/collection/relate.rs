use geo::{
    BoundingRect, Coord, Geometry, Line, LineString, Polygon, Relate,
    coordinate_position::CoordPos, dimensions::Dimensions, line_intersection::LineIntersection,
    relate::IntersectionMatrix,
};
use rstar::AABB;
use std::{cmp::Ordering, collections::HashSet, ops::Range};

use crate::spatial_filter::edge_index::{self, EdgeIndex};

pub(in crate::spatial_filter) struct PreparedTopology {
    topology: Topology,
    index: EdgeIndex,
}

impl PreparedTopology {
    pub(in crate::spatial_filter) fn new(geometry: &Geometry) -> Self {
        let topology = Topology::from_geometry(geometry);
        let index = edge_index::build(topology.edges.iter().copied().map(Edge::line));
        Self { topology, index }
    }

    pub(in crate::spatial_filter) fn locate_point(&self, point: Coord) -> CoordPos {
        let vertex = Vertex::from(point);
        let sample = Sample::from(vertex);
        for polygon in &self.topology.polygons {
            if sample.in_box(polygon.min, polygon.max) {
                let area = polygon.locate_vertex(&self.index, &self.topology.edges, vertex);
                if area != CoordPos::Outside {
                    return area;
                }
            }
        }
        if self.topology.line_boundary.contains(&vertex) {
            return CoordPos::OnBoundary;
        }
        let lines = self.topology.area_edges..self.topology.edges.len();
        let on_line = !lines.is_empty()
            && self
                .index
                .locate_in_envelope_intersecting(AABB::from_point([point.x, point.y]))
                .any(|entry| {
                    lines.contains(&entry.index) && sample.on_edge(self.topology.edges[entry.index])
                });
        if on_line || self.topology.points.contains(&vertex) {
            CoordPos::Inside
        } else {
            CoordPos::Outside
        }
    }
}

/// Geo's topology graph labels overlapping dimensions of a collection
/// independently. Build a matrix from the actual union instead. Polygon
/// interiors supply the area cells. Vertices, intersections and the open
/// segments between them supply the remaining cells. The union's location is
/// constant along each open segment, except at isolated point members.
pub(in crate::spatial_filter) fn collection_matrix(
    left: &Geometry,
    right: &Geometry,
    prepared_right: Option<&PreparedTopology>,
) -> IntersectionMatrix {
    let left_area = area_part(left);
    let right_area = area_part(right);
    if let Some(prepared) = prepared_right {
        return collection_matrix_prepared(left, left_area, right_area, prepared);
    }
    let left = Topology::from_geometry(left);
    let right = Topology::from_geometry(right);
    let mut matrix = Matrix::default();
    matrix.add_area(left_area, right_area);

    let edges: Vec<_> = left.edges.iter().chain(&right.edges).copied().collect();
    let mut cuts = vec![Vec::new(); edges.len()];
    let mut cut_edge = |index: usize, cut: Fraction| {
        if cut.numerator != 0 && cut.numerator != cut.denominator {
            cuts[index].push(cut);
        }
    };
    let index = edge_index::build(edges.iter().copied().map(Edge::line));
    for (a, b, hit) in edge_index::intersections(&index) {
        record_intersection(edges[a], edges[b], hit, |ca, cb| {
            cut_edge(a, ca);
            cut_edge(b, cb);
        });
    }

    for (edge, cuts) in edges.into_iter().zip(&mut cuts) {
        cuts.sort_unstable_by(Fraction::compare);
        cuts.dedup_by(|a, b| a.compare(b) == Ordering::Equal);
        let mut previous = Fraction::START;
        matrix.add_sample(
            &left,
            &right,
            &index,
            &index,
            0,
            left.edges.len(),
            edge.start.into(),
            false,
        );
        for next in cuts.iter().copied().chain([Fraction::END]) {
            matrix.add_sample(
                &left,
                &right,
                &index,
                &index,
                0,
                left.edges.len(),
                edge.at(previous.between(next)),
                true,
            );
            matrix.add_sample(
                &left,
                &right,
                &index,
                &index,
                0,
                left.edges.len(),
                edge.at(next),
                false,
            );
            previous = next;
        }
    }
    for point in left.points.iter().chain(&right.points) {
        matrix.add_sample(
            &left,
            &right,
            &index,
            &index,
            0,
            left.edges.len(),
            Sample::from(*point),
            false,
        );
    }
    matrix.finish()
}

fn collection_matrix_prepared(
    left_geom: &Geometry,
    left_area: Option<&Geometry>,
    right_area: Option<&Geometry>,
    prepared: &PreparedTopology,
) -> IntersectionMatrix {
    let left = Topology::from_geometry(left_geom);
    let right = &prepared.topology;
    let right_index = &prepared.index;
    let left_index = edge_index::build(left.edges.iter().copied().map(Edge::line));

    let mut matrix = Matrix::default();
    matrix.add_area(left_area, right_area);

    let mut left_cuts = vec![Vec::new(); left.edges.len()];
    let mut right_cuts: std::collections::HashMap<usize, Vec<Fraction>> =
        std::collections::HashMap::new();

    // Left self-intersections (only when left has >1 edges).
    if left.edges.len() > 1 {
        for (a, b, hit) in edge_index::intersections(&left_index) {
            record_intersection(left.edges[a], left.edges[b], hit, |ca, cb| {
                push_cut(&mut left_cuts[a], ca);
                push_cut(&mut left_cuts[b], cb);
            });
        }
    }

    // Cross intersections: query each left edge against the pre-built right R-tree.
    for (a, &a_edge) in left.edges.iter().enumerate() {
        let a_line = a_edge.line();
        let env = AABB::from_corners(
            [
                a_line.start.x.min(a_line.end.x),
                a_line.start.y.min(a_line.end.y),
            ],
            [
                a_line.start.x.max(a_line.end.x),
                a_line.start.y.max(a_line.end.y),
            ],
        );
        for entry in right_index.locate_in_envelope_intersecting(env) {
            if let Some(hit) = geo::line_intersection::line_intersection(a_line, entry.line) {
                let b = entry.index;
                record_intersection(a_edge, right.edges[b], hit, |ca, cb| {
                    push_cut(&mut left_cuts[a], ca);
                    push_cut(right_cuts.entry(b).or_default(), cb);
                });
            }
        }
    }

    // Sample all left edges.
    for (edge, cuts) in left.edges.iter().copied().zip(&mut left_cuts) {
        cuts.sort_unstable_by(Fraction::compare);
        cuts.dedup_by(|a, b| a.compare(b) == Ordering::Equal);
        let mut previous = Fraction::START;
        matrix.add_sample(
            &left,
            right,
            &left_index,
            right_index,
            0,
            0,
            edge.start.into(),
            false,
        );
        for next in cuts.iter().copied().chain([Fraction::END]) {
            matrix.add_sample(
                &left,
                right,
                &left_index,
                right_index,
                0,
                0,
                edge.at(previous.between(next)),
                true,
            );
            matrix.add_sample(
                &left,
                right,
                &left_index,
                right_index,
                0,
                0,
                edge.at(next),
                false,
            );
            previous = next;
        }
    }

    // Sample right edges: edges intersecting left's bounding box must be evaluated against left;
    // edges strictly outside left's bounding box always have left == Outside.
    let left_bounds = left_geom.bounding_rect();
    let mut visited_right = HashSet::new();
    if let Some(lb) = left_bounds {
        let env = AABB::from_corners([lb.min().x, lb.min().y], [lb.max().x, lb.max().y]);
        for entry in right_index.locate_in_envelope_intersecting(env) {
            let b = entry.index;
            visited_right.insert(b);
            let edge = right.edges[b];
            let mut cuts = right_cuts.remove(&b).unwrap_or_default();
            cuts.sort_unstable_by(Fraction::compare);
            cuts.dedup_by(|a, b| a.compare(b) == Ordering::Equal);
            let mut previous = Fraction::START;
            matrix.add_sample(
                &left,
                right,
                &left_index,
                right_index,
                0,
                0,
                edge.start.into(),
                false,
            );
            for next in cuts.iter().copied().chain([Fraction::END]) {
                matrix.add_sample(
                    &left,
                    right,
                    &left_index,
                    right_index,
                    0,
                    0,
                    edge.at(previous.between(next)),
                    true,
                );
                matrix.add_sample(
                    &left,
                    right,
                    &left_index,
                    right_index,
                    0,
                    0,
                    edge.at(next),
                    false,
                );
                previous = next;
            }
        }
    }

    // For right edges strictly outside left's bounding box, left is guaranteed Outside.
    // We only need to sample until both (Outside, Inside) and (Outside, OnBoundary)
    // reach 1D (or all disjoint edges are checked).
    let right_outside_saturated = |m: &Matrix| {
        if right.polygons.is_empty() {
            m.0[2 * 3] >= 2
        } else {
            m.0[2 * 3] >= 2 && m.0[2 * 3 + 1] >= 2
        }
    };
    if visited_right.len() < right.edges.len() {
        for (b, &edge) in right.edges.iter().enumerate() {
            if visited_right.contains(&b) {
                continue;
            }
            if right_outside_saturated(&matrix) {
                break;
            }
            let mid = edge.at(Fraction::START.between(Fraction::END));
            let r_mid = right.locate(right_index, 0, mid, true);
            matrix.set(CoordPos::Outside, r_mid, 1);
            let r_start = right.locate(right_index, 0, edge.start.into(), false);
            matrix.set(CoordPos::Outside, r_start, 0);
            let r_end = right.locate(right_index, 0, edge.end.into(), false);
            matrix.set(CoordPos::Outside, r_end, 0);
        }
        // Always include right's line_boundary endpoints (at most 2 per open line).
        for &vertex in &right.line_boundary {
            let sample = Sample::from(vertex);
            matrix.add_sample(&left, right, &left_index, right_index, 0, 0, sample, false);
        }
    }

    for point in left.points.iter().chain(&right.points) {
        matrix.add_sample(
            &left,
            right,
            &left_index,
            right_index,
            0,
            0,
            Sample::from(*point),
            false,
        );
    }
    matrix.finish()
}

fn push_cut(cuts: &mut Vec<Fraction>, cut: Fraction) {
    if cut.numerator != 0 && cut.numerator != cut.denominator {
        cuts.push(cut);
    }
}

fn record_intersection(
    a_edge: Edge,
    b_edge: Edge,
    hit: LineIntersection<f64>,
    mut emit: impl FnMut(Fraction, Fraction),
) {
    match hit {
        LineIntersection::SinglePoint {
            is_proper: true, ..
        } => {
            let offset = b_edge.start.minus(a_edge.start);
            let a_dir = a_edge.direction();
            let b_dir = b_edge.direction();
            let denominator = a_dir.cross(b_dir);
            emit(
                Fraction::new(offset.cross(b_dir), denominator),
                Fraction::new(offset.cross(a_dir), denominator),
            );
        }
        LineIntersection::SinglePoint { intersection, .. } => {
            let point = Vertex::from(intersection);
            emit(a_edge.parameter(point), b_edge.parameter(point));
        }
        LineIntersection::Collinear { intersection } => {
            for point in [intersection.start, intersection.end].map(Vertex::from) {
                emit(a_edge.parameter(point), b_edge.parameter(point));
            }
        }
    }
}

fn area_part(geometry: &Geometry) -> Option<&Geometry> {
    let parts = match geometry {
        Geometry::GeometryCollection(parts) => &parts.0,
        _ => std::slice::from_ref(geometry),
    };
    parts
        .iter()
        .find(|part| matches!(part, Geometry::Polygon(_) | Geometry::MultiPolygon(_)))
}

#[derive(Default)]
struct Matrix([u8; 9]);

impl Matrix {
    fn set(&mut self, a: CoordPos, b: CoordPos, dimension: u8) {
        let index = |position| match position {
            CoordPos::Inside => 0,
            CoordPos::OnBoundary => 1,
            CoordPos::Outside => 2,
        };
        let cell = &mut self.0[index(a) * 3 + index(b)];
        *cell = (*cell).max(dimension + 1);
    }

    fn add_area(&mut self, left: Option<&Geometry>, right: Option<&Geometry>) {
        use CoordPos::{Inside, Outside};
        self.set(Outside, Outside, 2);
        match (left, right) {
            (Some(left), Some(right)) => {
                let areas = left.relate(right);
                for (a, b) in [(Inside, Inside), (Inside, Outside), (Outside, Inside)] {
                    if areas.get(a, b) == Dimensions::TwoDimensional {
                        self.set(a, b, 2);
                    }
                }
            }
            (Some(_), None) => self.set(Inside, Outside, 2),
            (None, Some(_)) => self.set(Outside, Inside, 2),
            (None, None) => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add_sample(
        &mut self,
        left: &Topology,
        right: &Topology,
        left_index: &EdgeIndex,
        right_index: &EdgeIndex,
        left_offset: usize,
        right_offset: usize,
        sample: Sample,
        open: bool,
    ) {
        self.set(
            left.locate(left_index, left_offset, sample, open),
            right.locate(right_index, right_offset, sample, open),
            u8::from(open),
        );
    }

    fn finish(self) -> IntersectionMatrix {
        let spec = self.0.map(|d| if d == 0 { b'F' } else { b'0' + d - 1 });
        std::str::from_utf8(&spec)
            .expect("matrix is ASCII")
            .parse()
            .expect("matrix entries are dimensions")
    }
}

#[derive(Default)]
struct Topology {
    polygons: Vec<PolygonEdges>,
    edges: Vec<Edge>,
    area_edges: usize,
    line_boundary: HashSet<Vertex>,
    points: HashSet<Vertex>,
}

impl Topology {
    fn from_geometry(geometry: &Geometry) -> Self {
        let mut result = Self::default();
        let parts = match geometry {
            Geometry::GeometryCollection(parts) => &parts.0,
            _ => std::slice::from_ref(geometry),
        };
        // Prepared collections store area before lines and points.
        for part in parts {
            match part {
                Geometry::Polygon(polygon) => {
                    result.add_polygon(polygon);
                }
                Geometry::MultiPolygon(polygons) => {
                    for polygon in &polygons.0 {
                        result.add_polygon(polygon);
                    }
                }
                Geometry::LineString(line) => result.add_line(line),
                Geometry::MultiLineString(lines) => {
                    for line in &lines.0 {
                        result.add_line(line);
                    }
                }
                Geometry::Point(point) => {
                    result.points.insert(point.0.into());
                }
                Geometry::MultiPoint(points) => result
                    .points
                    .extend(points.0.iter().map(|p| Vertex::from(p.0))),
                _ => unreachable!("unexpected normalized geometry type"),
            }
        }
        result
    }
}

impl Topology {
    fn add_edges(&mut self, line: &LineString) -> Range<usize> {
        let begin = self.edges.len();
        self.edges.extend(
            line.lines()
                .filter(|line| line.start != line.end)
                .map(|line| Edge {
                    start: line.start.into(),
                    end: line.end.into(),
                }),
        );
        begin..self.edges.len()
    }

    fn add_polygon(&mut self, polygon: &Polygon) {
        let bounds = polygon
            .bounding_rect()
            .expect("nonempty polygon has bounds");
        let rings = std::iter::once(polygon.exterior())
            .chain(polygon.interiors())
            .map(|ring| self.add_edges(ring))
            .collect();
        self.polygons.push(PolygonEdges {
            min: bounds.min().into(),
            max: bounds.max().into(),
            rings,
        });
        self.area_edges = self.edges.len();
    }

    fn add_line(&mut self, line: &LineString) {
        self.add_edges(line);
        if !line.is_closed() {
            for endpoint in [line.0[0], line.0[line.0.len() - 1]].map(Vertex::from) {
                if !self.line_boundary.insert(endpoint) {
                    self.line_boundary.remove(&endpoint);
                }
            }
        }
    }

    fn locate(&self, index: &EdgeIndex, offset: usize, sample: Sample, open: bool) -> CoordPos {
        for polygon in &self.polygons {
            if sample.in_box(polygon.min, polygon.max) {
                let area = polygon.locate(index, &self.edges, offset, sample);
                if area != CoordPos::Outside {
                    return area;
                }
            }
        }
        let vertex = if open { None } else { sample.vertex() };
        if vertex.is_some_and(|point| self.line_boundary.contains(&point)) {
            return CoordPos::OnBoundary;
        }
        let lines = self.area_edges + offset..self.edges.len() + offset;
        let on_line = !lines.is_empty()
            && index
                .locate_in_envelope_intersecting(sample.envelope())
                .any(|entry| {
                    lines.contains(&entry.index) && sample.on_edge(self.edges[entry.index - offset])
                });
        if on_line || vertex.is_some_and(|point| self.points.contains(&point)) {
            CoordPos::Inside
        } else {
            CoordPos::Outside
        }
    }
}

struct PolygonEdges {
    min: Vertex,
    max: Vertex,
    rings: Vec<Range<usize>>,
}

impl PolygonEdges {
    fn locate_vertex(&self, index: &EdgeIndex, edges: &[Edge], point: Vertex) -> CoordPos {
        let ray = AABB::from_corners(
            [point.x as f64, point.y as f64],
            [f64::from(i32::MAX), point.y as f64],
        );
        let ring_location = |ring: &Range<usize>| {
            let mut inside = false;
            for entry in index.locate_in_envelope_intersecting(ray) {
                if !ring.contains(&entry.index) {
                    continue;
                }
                let edge = edges[entry.index];
                let crossing = (edge.start.y > point.y) != (edge.end.y > point.y);
                let in_box = point.x >= edge.start.x.min(edge.end.x)
                    && point.x <= edge.start.x.max(edge.end.x)
                    && point.y >= edge.start.y.min(edge.end.y)
                    && point.y <= edge.start.y.max(edge.end.y);
                if !crossing && !in_box {
                    continue;
                }
                let dx = i128::from(edge.end.x - edge.start.x);
                let dy = i128::from(edge.end.y - edge.start.y);
                let cross = dx * i128::from(point.y - edge.start.y)
                    - dy * i128::from(point.x - edge.start.x);
                if cross == 0 && in_box {
                    return CoordPos::OnBoundary;
                }
                if crossing && cross.cmp(&0) == edge.end.y.cmp(&edge.start.y) {
                    inside = !inside;
                }
            }
            if inside {
                CoordPos::Inside
            } else {
                CoordPos::Outside
            }
        };
        let shell = ring_location(&self.rings[0]);
        if shell != CoordPos::Inside {
            return shell;
        }
        for hole in &self.rings[1..] {
            match ring_location(hole) {
                CoordPos::Inside => return CoordPos::Outside,
                CoordPos::OnBoundary => return CoordPos::OnBoundary,
                CoordPos::Outside => {}
            }
        }
        CoordPos::Inside
    }

    fn locate(&self, index: &EdgeIndex, edges: &[Edge], offset: usize, sample: Sample) -> CoordPos {
        let bounds = sample.envelope();
        let ray = AABB::from_corners(bounds.lower(), [f64::from(i32::MAX), bounds.upper()[1]]);
        let ring_location = |ring: &Range<usize>| {
            let range = ring.start + offset..ring.end + offset;
            ring_position(
                index
                    .locate_in_envelope_intersecting(ray)
                    .filter(|entry| range.contains(&entry.index))
                    .map(|entry| edges[entry.index - offset]),
                sample,
            )
        };
        let shell = ring_location(&self.rings[0]);
        if shell != CoordPos::Inside {
            return shell;
        }
        for hole in &self.rings[1..] {
            match ring_location(hole) {
                CoordPos::Inside => return CoordPos::Outside,
                CoordPos::OnBoundary => return CoordPos::OnBoundary,
                CoordPos::Outside => {}
            }
        }
        CoordPos::Inside
    }
}

fn ring_position(edges: impl Iterator<Item = Edge>, sample: Sample) -> CoordPos {
    let mut inside = false;
    for edge in edges {
        let crossing = (i128::from(edge.start.y) * sample.denominator > sample.y)
            != (i128::from(edge.end.y) * sample.denominator > sample.y);
        if !crossing && !sample.in_box(edge.start, edge.end) {
            continue;
        }
        let orientation = sample.orientation(edge);
        if orientation == Ordering::Equal && sample.in_box(edge.start, edge.end) {
            return CoordPos::OnBoundary;
        }
        if crossing && orientation == edge.end.y.cmp(&edge.start.y) {
            inside = !inside;
        }
    }
    if inside {
        CoordPos::Inside
    } else {
        CoordPos::Outside
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Vertex {
    x: i64,
    y: i64,
}

impl From<Coord> for Vertex {
    fn from(point: Coord) -> Self {
        Self {
            x: point.x as i64,
            y: point.y as i64,
        }
    }
}

impl Vertex {
    fn minus(self, other: Self) -> Self {
        Self {
            x: self.x - other.x,
            y: self.y - other.y,
        }
    }

    fn cross(self, other: Self) -> i128 {
        i128::from(self.x) * i128::from(other.y) - i128::from(self.y) * i128::from(other.x)
    }
}

#[derive(Clone, Copy)]
struct Edge {
    start: Vertex,
    end: Vertex,
}

impl Edge {
    fn direction(self) -> Vertex {
        self.end.minus(self.start)
    }

    fn parameter(self, point: Vertex) -> Fraction {
        let delta = point.minus(self.start);
        let dir = self.direction();
        if dir.x != 0 {
            Fraction::new(delta.x.into(), dir.x.into())
        } else {
            Fraction::new(delta.y.into(), dir.y.into())
        }
    }

    fn at(self, parameter: Fraction) -> Sample {
        let dir = self.direction();
        let denominator = parameter.denominator;
        Sample {
            x: i128::from(self.start.x) * denominator + i128::from(dir.x) * parameter.numerator,
            y: i128::from(self.start.y) * denominator + i128::from(dir.y) * parameter.numerator,
            denominator,
        }
    }

    fn line(self) -> Line {
        let coord = |vertex: Vertex| Coord {
            x: vertex.x as f64,
            y: vertex.y as f64,
        };
        Line::new(coord(self.start), coord(self.end))
    }
}

#[derive(Clone, Copy)]
struct Fraction {
    numerator: i128,
    denominator: i128,
}

impl Fraction {
    const START: Self = Self {
        numerator: 0,
        denominator: 1,
    };
    const END: Self = Self {
        numerator: 1,
        denominator: 1,
    };

    fn new(numerator: i128, denominator: i128) -> Self {
        let sign = denominator.signum();
        assert!(sign != 0, "intersection has a nonzero denominator");
        Self {
            numerator: numerator * sign,
            denominator: denominator * sign,
        }
    }

    fn compare(&self, other: &Self) -> Ordering {
        compare_products(
            self.numerator,
            other.denominator,
            other.numerator,
            self.denominator,
        )
    }

    fn between(self, other: Self) -> Self {
        // The mediant lies strictly between unequal fractions with positive
        // denominators while keeping values the size of the original determinants.
        Self {
            numerator: self.numerator + other.numerator,
            denominator: self.denominator + other.denominator,
        }
    }
}

#[derive(Clone, Copy)]
struct Sample {
    x: i128,
    y: i128,
    denominator: i128,
}

impl From<Vertex> for Sample {
    fn from(point: Vertex) -> Self {
        Self {
            x: point.x.into(),
            y: point.y.into(),
            denominator: 1,
        }
    }
}

impl Sample {
    fn in_box(self, a: Vertex, b: Vertex) -> bool {
        let within = |p, a: i64, b: i64| {
            p >= i128::from(a.min(b)) * self.denominator
                && p <= i128::from(a.max(b)) * self.denominator
        };
        within(self.x, a.x, b.x) && within(self.y, a.y, b.y)
    }

    fn orientation(self, edge: Edge) -> Ordering {
        let direction = edge.direction();
        compare_products(
            direction.x.into(),
            self.y - i128::from(edge.start.y) * self.denominator,
            direction.y.into(),
            self.x - i128::from(edge.start.x) * self.denominator,
        )
    }

    fn on_edge(self, edge: Edge) -> bool {
        self.in_box(edge.start, edge.end) && self.orientation(edge) == Ordering::Equal
    }

    fn envelope(self) -> AABB<[f64; 2]> {
        // Round rationals outwards before converting to f64. All rounded
        // coordinates fit within the integer Mercator range and convert exactly.
        let bounds = |numerator: i128| {
            let lower = numerator.div_euclid(self.denominator);
            let upper = lower + i128::from(numerator.rem_euclid(self.denominator) != 0);
            (lower as f64, upper as f64)
        };
        let (xmin, xmax) = bounds(self.x);
        let (ymin, ymax) = bounds(self.y);
        AABB::from_corners([xmin, ymin], [xmax, ymax])
    }

    fn vertex(self) -> Option<Vertex> {
        (self.x % self.denominator == 0 && self.y % self.denominator == 0).then(|| Vertex {
            x: (self.x / self.denominator) as i64,
            y: (self.y / self.denominator) as i64,
        })
    }
}

fn compare_products(a: i128, b: i128, c: i128, d: i128) -> Ordering {
    if let (Some(left), Some(right)) = (a.checked_mul(b), c.checked_mul(d)) {
        return left.cmp(&right);
    }
    let left_sign = a.signum() * b.signum();
    let right_sign = c.signum() * d.signum();
    match left_sign.cmp(&right_sign) {
        Ordering::Equal => {
            let left = wide_product(a.unsigned_abs(), b.unsigned_abs());
            let right = wide_product(c.unsigned_abs(), d.unsigned_abs());
            if left_sign < 0 {
                right.cmp(&left)
            } else {
                left.cmp(&right)
            }
        }
        ordering => ordering,
    }
}

// Coordinate differences have at most 32 magnitude bits. Determinants have
// at most 65, and a rational sample's scaled coordinates have at most 99.
// Comparing products of these terms can exceed i128. Their high/low partial
// products fit u128, which lets us compare the full two-limb results exactly.
fn wide_product(a: u128, b: u128) -> [u128; 2] {
    let low_a = u128::from(a as u64);
    let low_b = u128::from(b as u64);
    let high_a = a >> 64;
    let high_b = b >> 64;
    let cross = low_a * high_b + high_a * low_b;
    let (lo, carry) = (low_a * low_b).overflowing_add(cross << 64);
    [high_a * high_b + (cross >> 64) + u128::from(carry), lo]
}
