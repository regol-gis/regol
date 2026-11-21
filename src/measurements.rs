use crate::{Feature, Geometry, Point, Polygon, Result, source::Source};
use crate::{
    coordinates::WORLD_WIDTH,
    units::{Area, Length, area::square_meter, length::meter},
};
use geo::{
    Area as _, Centroid, Coord, GeometryCollection, HasDimensions, LineString, Translate,
    dimensions::Dimensions,
};
use std::{f64::consts::TAU, ops::Range};

const EARTH_RADIUS_METERS: f64 = 6_371_008.8;
// A few micrometres on the sphere, below the resolution of a GOL coordinate.
const ANGULAR_TOLERANCE: f64 = 1e-12;

impl Geometry {
    /// Total line length and polygon perimeter, in metres.
    ///
    /// Segments follow the shorter great-circle arc on a sphere of radius
    /// 6,371,008.8 metres. Polygon perimeters include hole boundaries.
    /// Collection members are added individually, including repeated edges.
    /// Points and empty geometries contribute zero.
    pub fn length(&self) -> Length {
        Length::new::<meter>(self.angular_length(true) * EARTH_RADIUS_METERS)
    }

    /// Total length of polygon boundaries, in metres, including hole boundaries.
    ///
    /// Only polygonal components contribute. Uses the same metric as
    /// [`Geometry::length`].
    pub fn perimeter(&self) -> Length {
        Length::new::<meter>(self.angular_length(false) * EARTH_RADIUS_METERS)
    }

    /// Total polygonal area in square metres on a sphere of radius 6,371,008.8 metres.
    ///
    /// Ring edges follow the shorter great-circle arcs. Each ring bounds the
    /// smaller of the two regions of the sphere, independent of winding. Hole
    /// areas are subtracted and polygon areas are added without dissolving overlaps.
    /// Assumes simple rings with holes inside their exterior. Non-polygonal
    /// components and empty geometries have zero area.
    pub fn area(&self) -> Area {
        Area::new::<square_meter>(self.solid_angle() * EARTH_RADIUS_METERS.powi(2))
    }

    /// Centroid in integer Mercator coordinates, rounded to the nearest point.
    ///
    /// Weights the components of highest nonzero dimension by projected area
    /// or length. Points are averaged if no area or length is present. Longitudes
    /// are unwrapped along paths and each component starts in the world copy
    /// closest to the first component of the contributing dimension. Empty geometries return `None`.
    /// A centroid can lie outside a polygon or inside a hole.
    pub fn centroid(&self) -> Option<Point> {
        if let Self::Point(point) = self {
            return Some(*point);
        }
        let mut parts = Vec::new();
        centroid_components(self, &mut parts);
        let center = match parts.as_slice() {
            [] => return None,
            [(_, geometry)] => geometry.centroid()?,
            _ => {
                // A point or a zero-area ring must not choose the world copy
                // in which independent, higher-dimensional members are averaged.
                let mut highest = Dimensions::Empty;
                let mut reference = 0.0;
                for (start, part) in &parts {
                    let dimension = match part {
                        geo::Geometry::Polygon(p) if p.unsigned_area() == 0.0 => {
                            p.exterior().dimensions()
                        }
                        geo::Geometry::Polygon(_) => Dimensions::TwoDimensional,
                        part => part.dimensions(),
                    };
                    if dimension > highest {
                        highest = dimension;
                        reference = *start;
                        if highest == Dimensions::TwoDimensional {
                            break;
                        }
                    }
                }
                let geometries = parts.into_iter().map(|(start, mut part)| {
                    let shift = nearest_world_copy(start, reference) - start;
                    if shift != 0.0 {
                        part.translate_mut(shift, 0.0);
                    }
                    part
                });
                GeometryCollection::from_iter(geometries).centroid()?
            }
        };
        let x = if (f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&center.x().round()) {
            center.x()
        } else {
            let half_width = WORLD_WIDTH / 2.0;
            (center.x() + half_width).rem_euclid(WORLD_WIDTH) - half_width
        };
        Some(Point {
            x: x.round() as i32,
            y: center.y().round() as i32,
        })
    }

    /// Shortest surface distance, in metres, between these geometries.
    ///
    /// Segments follow the shorter great-circle arcs on a sphere of radius
    /// 6,371,008.8 metres. Polygons include their interiors and all boundaries,
    /// with hole interiors excluded. Rings are interpreted as in
    /// [`Geometry::area`]. Intersections and separations below
    /// the numerical tolerance of about six micrometres return zero. Returns
    /// `None` if either geometry is empty.
    ///
    /// Boundary comparisons take time proportional to the product of the two
    /// segment counts in the worst case.
    pub fn distance(&self, other: &Self) -> Option<Length> {
        Surface::new(self)
            .distance_meters(&Surface::new(other))
            .map(Length::new::<meter>)
    }

    fn angular_length(&self, include_lines: bool) -> f64 {
        fn perimeter(polygon: &Polygon) -> f64 {
            path_angle(&polygon.exterior, true)
                + polygon
                    .interiors
                    .iter()
                    .map(|ring| path_angle(ring, true))
                    .sum::<f64>()
        }

        match self {
            Self::Point(_) => 0.0,
            Self::LineString(points) if include_lines => path_angle(points, false),
            Self::LineString(_) => 0.0,
            Self::Polygon(polygon) => perimeter(polygon),
            Self::MultiPolygon(polygons) => polygons.iter().map(perimeter).sum(),
            Self::Collection(items) => items
                .iter()
                .map(|item| item.angular_length(include_lines))
                .sum(),
        }
    }

    fn solid_angle(&self) -> f64 {
        fn polygon_area(polygon: &Polygon) -> f64 {
            let area = signed_ring_area(polygon.exterior.iter().copied().map(Vector::from)).abs();
            let holes: f64 = polygon
                .interiors
                .iter()
                .map(|ring| signed_ring_area(ring.iter().copied().map(Vector::from)).abs())
                .sum();
            (area - holes).max(0.0)
        }

        match self {
            Self::Point(_) | Self::LineString(_) => 0.0,
            Self::Polygon(polygon) => polygon_area(polygon),
            Self::MultiPolygon(polygons) => polygons.iter().map(polygon_area).sum(),
            Self::Collection(items) => items.iter().map(Self::solid_angle).sum(),
        }
    }
}

impl<S: Source> Feature<'_, S> {
    /// Assemble the feature geometry and measure its length in metres.
    /// See [`Geometry::length`] for metric and component semantics.
    pub fn length(self) -> Result<Length> {
        Ok(self.geometry()?.length())
    }

    /// Assemble the feature geometry and measure its polygon perimeter in metres.
    /// See [`Geometry::perimeter`] for metric and component semantics.
    pub fn perimeter(self) -> Result<Length> {
        Ok(self.geometry()?.perimeter())
    }

    /// Assemble the feature geometry and measure its area in square metres.
    /// See [`Geometry::area`] for metric and component semantics.
    pub fn area(self) -> Result<Area> {
        Ok(self.geometry()?.area())
    }

    /// Assemble the feature geometry and find its centroid, if it has coordinates.
    /// See [`Geometry::centroid`] for the weighting and coordinate system.
    pub fn centroid(self) -> Result<Option<Point>> {
        Ok(self.geometry()?.centroid())
    }

    /// Assemble the feature geometry and measure its distance from `other` in metres.
    /// See [`Geometry::distance`] for metric and empty geometry semantics.
    pub fn distance(self, other: &Geometry) -> Result<Option<Length>> {
        Ok(self.geometry()?.distance(other))
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Vector {
    x: f64,
    y: f64,
    z: f64,
}

impl From<Point> for Vector {
    fn from(point: Point) -> Self {
        let (longitude, latitude) = point.radians();
        let (sin_lon, cos_lon) = longitude.sin_cos();
        let (sin_lat, cos_lat) = latitude.sin_cos();
        Self {
            x: cos_lat * cos_lon,
            y: cos_lat * sin_lon,
            z: sin_lat,
        }
    }
}

impl Vector {
    fn dot(self, other: Self) -> f64 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }

    fn cross(self, other: Self) -> Self {
        Self {
            x: self.y * other.z - self.z * other.y,
            y: self.z * other.x - self.x * other.z,
            z: self.x * other.y - self.y * other.x,
        }
    }

    fn minus(self, other: Self) -> Self {
        Self {
            x: self.x - other.x,
            y: self.y - other.y,
            z: self.z - other.z,
        }
    }

    fn scaled(self, scale: f64) -> Self {
        Self {
            x: self.x * scale,
            y: self.y * scale,
            z: self.z * scale,
        }
    }

    fn norm(self) -> f64 {
        self.dot(self).sqrt()
    }

    fn normalized(self) -> Option<Self> {
        let norm = self.norm();
        (norm > f64::EPSILON).then(|| self.scaled(norm.recip()))
    }

    fn stable_cross(self, other: Self, dot: f64) -> Self {
        // Equivalent to self × other. Subtract nearby vectors before the
        // products to reduce cancellation for short or almost antipodal edges.
        self.cross(other.minus(self.scaled(1.0f64.copysign(dot))))
    }

    fn angle(self, other: Self) -> f64 {
        let dot = self.dot(other);
        self.stable_cross(other, dot).norm().atan2(dot)
    }
}

fn path_angle(points: &[Point], close: bool) -> f64 {
    let mut points = points.iter().copied().map(Vector::from);
    let Some(first) = points.next() else {
        return 0.0;
    };
    let mut previous = first;
    let mut total = 0.0;
    for point in points {
        total += previous.angle(point);
        previous = point;
    }
    if close {
        total += previous.angle(first);
    }
    total
}

fn signed_ring_area(mut points: impl Iterator<Item = Vector>) -> f64 {
    let (Some(first), Some(mut previous)) = (points.next(), points.next()) else {
        return 0.0;
    };
    let mut total = 0.0;
    let mut correction = 0.0;
    for point in points {
        if point == first || previous == first || point == previous {
            previous = point;
            continue;
        }
        let adjusted = triangle_area(first, previous, point) - correction;
        let next = total + adjusted;
        correction = (next - total) - adjusted;
        total = next;
        previous = point;
    }
    // A ring defines two sides. Use the smaller side and retain orientation.
    let total = total % (2.0 * TAU);
    if total.abs() > TAU {
        total - (2.0 * TAU).copysign(total)
    } else {
        total
    }
}

fn triangle_area(reference: Vector, a: Vector, b: Vector) -> f64 {
    // Signed solid angle (Van Oosterom and Strackee, 1983,
    // doi:10.1109/TBME.1983.325207). Translated vectors avoid cancellation
    // in the determinant and in 1 + reference·a + a·b + b·reference.
    let a_sum = a.minus(reference.scaled(-1.0));
    let b_sum = b.minus(reference.scaled(-1.0));
    let a_delta = if reference.dot(a) > 0.0 {
        a.minus(reference)
    } else {
        a_sum
    };
    let b_delta = if reference.dot(b) > 0.0 {
        b.minus(reference)
    } else {
        b_sum
    };
    let determinant = reference.dot(a_delta.cross(b_delta));
    2.0 * determinant.atan2(a_sum.dot(b_sum))
}

#[derive(Clone, Copy)]
struct Arc {
    start: Vector,
    end: Vector,
    normal: Option<Vector>,
}

impl Arc {
    fn new(start: Vector, end: Vector) -> Self {
        Self {
            start,
            end,
            normal: start.stable_cross(end, start.dot(end)).normalized(),
        }
    }

    fn contains_direction(self, normal: Vector, direction: Vector) -> bool {
        normal.cross(self.start).dot(direction) >= -ANGULAR_TOLERANCE
            && self.end.cross(normal).dot(direction) >= -ANGULAR_TOLERANCE
            // Near-coincident endpoints can satisfy both tangent tests at
            // the opposite side of the sphere within the tolerance.
            && self.start.dot(direction) + self.end.dot(direction) >= 0.0
    }

    fn distance_to_point(self, point: Vector) -> f64 {
        if let Some(normal) = self.normal {
            // These tangent tests also apply to the projection of point onto
            // the arc's great circle because both tangents lie in that plane.
            if self.contains_direction(normal, point) {
                return normal.dot(point).abs().atan2(normal.cross(point).norm());
            }
            self.start.angle(point).min(self.end.angle(point))
        } else {
            self.start.angle(point)
        }
    }

    fn distance(self, other: Self) -> f64 {
        let (Some(left), Some(right)) = (self.normal, other.normal) else {
            return match self.normal {
                Some(_) => self.distance_to_point(other.start),
                None => other.distance_to_point(self.start),
            };
        };
        if let Some(direction) = left.stable_cross(right, left.dot(right)).normalized() {
            // The planes cross along an axis. Either end of the axis can be
            // the common point, depending on where the two arcs lie.
            if [direction, direction.scaled(-1.0)].iter().any(|&point| {
                self.contains_direction(left, point) && other.contains_direction(right, point)
            }) {
                return 0.0;
            }
        }
        // An interior minimum for both arcs occurs only at an intersection
        // or on the same great circle. In the latter case an endpoint
        // attains the minimum too.
        self.distance_to_point(other.start)
            .min(self.distance_to_point(other.end))
            .min(other.distance_to_point(self.start))
            .min(other.distance_to_point(self.end))
    }
}

struct Ring {
    edges: Range<usize>,
    area: f64,
}

impl Ring {
    fn contains(&self, edges: &[Arc], point: Vector) -> bool {
        if self.area == 0.0 {
            return false;
        }
        let antipode = point.scaled(-1.0);
        let area: f64 = edges[self.edges.clone()]
            .iter()
            .map(|edge| triangle_area(antipode, edge.start, edge.end))
            .sum();
        // A triangle fan based at the antipode differs from the signed ring
        // area by an entire sphere when the query point is inside the ring.
        (area - self.area).abs() > TAU
    }
}

struct SurfacePolygon {
    exterior: Ring,
    interiors: Vec<Ring>,
}

#[derive(Default)]
pub(crate) struct Surface {
    edges: Vec<Arc>,
    samples: Vec<Vector>,
    polygons: Vec<SurfacePolygon>,
}

impl Surface {
    pub(crate) fn new(geometry: &Geometry) -> Self {
        let mut surface = Self::default();
        surface.add_geometry(geometry);
        surface
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    pub(crate) fn distance_meters(&self, other: &Self) -> Option<f64> {
        if self.is_empty() || other.is_empty() {
            return None;
        }
        if self.contains_sample_of(other) || other.contains_sample_of(self) {
            return Some(0.0);
        }
        let mut angle = f64::INFINITY;
        for left in &self.edges {
            for right in &other.edges {
                angle = angle.min(left.distance(*right));
                if angle <= ANGULAR_TOLERANCE {
                    return Some(0.0);
                }
            }
        }
        Some(angle * EARTH_RADIUS_METERS)
    }

    fn add_geometry(&mut self, geometry: &Geometry) {
        match geometry {
            Geometry::Point(point) => self.add_sample_path(std::slice::from_ref(point), false),
            Geometry::LineString(points) => self.add_sample_path(points, false),
            Geometry::Polygon(polygon) => self.add_polygon(polygon),
            Geometry::MultiPolygon(polygons) => {
                for polygon in polygons {
                    self.add_polygon(polygon);
                }
            }
            Geometry::Collection(items) => {
                for item in items {
                    self.add_geometry(item);
                }
            }
        }
    }

    fn add_sample_path(&mut self, points: &[Point], close: bool) {
        let start = self.edges.len();
        self.add_path(points, close);
        if let Some(first) = self.edges.get(start) {
            self.samples.push(first.start);
        }
    }

    fn add_path(&mut self, points: &[Point], close: bool) {
        let Some((&first, rest)) = points.split_first() else {
            return;
        };
        let start = Vector::from(first);
        let mut previous = start;
        for &point in rest {
            let current = Vector::from(point);
            self.edges.push(Arc::new(previous, current));
            previous = current;
        }
        if rest.is_empty() || (close && points.last() != Some(&first)) {
            self.edges.push(Arc::new(previous, start));
        }
    }

    fn add_ring(&mut self, points: &[Point]) -> Ring {
        let start = self.edges.len();
        self.add_path(points, true);
        self.ring_from(start)
    }

    fn ring_from(&self, start: usize) -> Ring {
        Ring {
            edges: start..self.edges.len(),
            area: signed_ring_area(self.edges[start..].iter().map(|edge| edge.start)),
        }
    }

    fn add_polygon(&mut self, polygon: &Polygon) {
        if polygon.exterior.is_empty() {
            return;
        }
        let start = self.edges.len();
        self.add_sample_path(&polygon.exterior, true);
        let exterior = self.ring_from(start);
        let interiors = polygon
            .interiors
            .iter()
            .map(|ring| self.add_ring(ring))
            .collect();
        self.polygons.push(SurfacePolygon {
            exterior,
            interiors,
        });
    }

    fn contains_sample_of(&self, other: &Self) -> bool {
        // One point per connected component detects full containment.
        // Boundary comparisons catch the remaining intersections.
        self.polygons.iter().any(|polygon| {
            other.samples.iter().any(|&point| {
                polygon.exterior.contains(&self.edges, point)
                    && !polygon
                        .interiors
                        .iter()
                        .any(|ring| ring.contains(&self.edges, point))
            })
        })
    }
}

fn nearest_world_copy(x: f64, reference: f64) -> f64 {
    x + ((reference - x) / WORLD_WIDTH).round() * WORLD_WIDTH
}

fn centroid_path(points: &[Point], mut reference: f64) -> LineString<f64> {
    LineString(
        points
            .iter()
            .map(|point| {
                let x = nearest_world_copy(f64::from(point.x), reference);
                reference = x;
                Coord {
                    x,
                    y: f64::from(point.y),
                }
            })
            .collect(),
    )
}

fn centroid_polygon(polygon: &Polygon, reference: f64) -> geo::Polygon<f64> {
    let exterior = centroid_path(&polygon.exterior, reference);
    let reference = exterior.0.first().map_or(reference, |point| point.x);
    let interiors = polygon
        .interiors
        .iter()
        .map(|ring| centroid_path(ring, reference))
        .collect();
    geo::Polygon::new(exterior, interiors)
}

fn centroid_components(geometry: &Geometry, parts: &mut Vec<(f64, geo::Geometry)>) {
    let mut polygon = |p: &Polygon| {
        if let Some(first) = p.exterior.first() {
            let x = f64::from(first.x);
            parts.push((x, centroid_polygon(p, x).into()));
        }
    };
    match geometry {
        Geometry::Point(point) => {
            let x = f64::from(point.x);
            parts.push((x, geo::Point::new(x, f64::from(point.y)).into()));
        }
        Geometry::LineString(points) => {
            if let Some(first) = points.first() {
                let x = f64::from(first.x);
                parts.push((x, centroid_path(points, x).into()));
            }
        }
        Geometry::Polygon(p) => polygon(p),
        Geometry::MultiPolygon(polygons) => polygons.iter().for_each(polygon),
        Geometry::Collection(items) => {
            for item in items {
                centroid_components(item, parts);
            }
        }
    }
}
