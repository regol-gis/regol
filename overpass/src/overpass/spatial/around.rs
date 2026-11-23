use super::{MAX_SEARCH_WINDOWS, reference_vertices, visit_paths};
use crate::overpass::scan::WORLD_WIDTH;
use crate::{geometry, usable_geometry, with_geometry_reads};
use regol::{
    Bounds, Feature, FeatureId, Geometry, LonLat, Point,
    source::Source,
    units::{angle::radian, length::meter},
};
use rstar::{
    AABB, RTree,
    primitives::{GeomWithData, Rectangle},
};
use std::{
    collections::{BTreeMap, HashSet},
    f64::consts::{FRAC_PI_2, PI, TAU},
    ops::ControlFlow,
};

const EARTH_RADIUS: f64 = 6_371_008.8;
const ANGULAR_MARGIN: f64 = 1e-10;
// Match Geometry::distance for arcs whose endpoints cannot be projected.
const ANGULAR_TOLERANCE: f64 = 1e-12;
const CELL_DEGREES: f64 = 0.25;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Segment(Point, Point);

impl Segment {
    fn geometry(self) -> Geometry {
        if self.0 == self.1 {
            Geometry::Point(self.0)
        } else {
            Geometry::LineString(vec![self.0, self.1])
        }
    }

    fn endpoints(self) -> [[f64; 3]; 2] {
        [vector(self.0), vector(self.1)]
    }
}

fn visit_segments<B>(
    shape: Geometry,
    mut visit: impl FnMut(Segment) -> regol::Result<ControlFlow<B>>,
) -> regol::Result<ControlFlow<B>> {
    match shape {
        Geometry::Point(p) => visit(Segment(p, p)),
        Geometry::LineString(points) if points.len() == 1 => visit(Segment(points[0], points[0])),
        Geometry::LineString(points) => {
            for pair in points.windows(2) {
                if let ControlFlow::Break(value) = visit(Segment(pair[0], pair[1]))? {
                    return Ok(ControlFlow::Break(value));
                }
            }
            Ok(ControlFlow::Continue(()))
        }
        _ => unreachable!("an outline only includes points and lines"),
    }
}

type IndexEntry<T> = GeomWithData<Rectangle<[f64; 3]>, T>;

enum Reference {
    Projected(RTree<IndexEntry<Segment>>),
    Geographic(RTree<IndexEntry<Arc>>),
    PolarPoint([f64; 3]),
}

pub(crate) struct Around {
    meters: f64,
    reference: Reference,
}

impl Around {
    fn new(meters: f64, segments: impl IntoIterator<Item = Segment>) -> Self {
        Self {
            meters,
            reference: Reference::Projected(segment_index(meters, segments, Segment::endpoints)),
        }
    }

    pub(in crate::overpass) fn point(meters: f64, point: LonLat) -> Self {
        if let Some(p) = project_grid_point(point) {
            return Self::new(meters, [Segment(p, p)]);
        }
        // Candidate arcs can reach latitudes beyond the Mercator grid.
        Self {
            meters,
            reference: Reference::PolarPoint(geographic_vector(point)),
        }
    }

    pub(in crate::overpass) fn polyline(
        meters: f64,
        points: &[[f64; 2]],
        vertices: &mut usize,
        check: &impl Fn() -> Result<(), String>,
    ) -> Result<Self, String> {
        reference_vertices(vertices, points.len())?;
        let mut positions = Vec::with_capacity(points.len());
        for &point in points {
            check()?;
            let point = geometry::geographic_point(point)?;
            let geographic = geographic_vector(point);
            if let Some(&(_, previous)) = positions.last()
                && Cap::for_arc(previous, geographic).is_none()
            {
                return Err("An around polyline contains an antipodal segment.".into());
            }
            positions.push((project_grid_point(point), geographic));
        }
        if positions.iter().all(|(grid, _)| grid.is_some()) {
            let segments = positions
                .windows(2)
                .map(|pair| Segment(pair[0].0.unwrap(), pair[1].0.unwrap()));
            let around = Self::new(meters, segments);
            check()?;
            return Ok(around);
        }
        // Keep endpoints beyond the grid at their geographic latitude, where
        // shorter great-circle arcs can still meet a stored way's outline.
        let endpoint = |(grid, geographic): (Option<Point>, _)| grid.map_or(geographic, vector);
        let arcs = positions
            .windows(2)
            .map(|pair| Arc::new(endpoint(pair[0]), endpoint(pair[1])));
        let index = segment_index(meters, arcs, Arc::endpoints);
        check()?;
        Ok(Self {
            meters,
            reference: Reference::Geographic(index),
        })
    }

    pub(in crate::overpass) fn from_set<S: Source>(
        meters: f64,
        features: &[Feature<'_, S>],
        bad: &mut HashSet<FeatureId>,
        vertices: &mut usize,
        check: &impl Fn() -> Result<(), String>,
    ) -> Result<Self, String> {
        let mut unique = HashSet::new();
        let mut segments = Vec::new();
        let mut check_periodically = periodic_check(check);
        for &feature in features {
            check()?;
            let start = segments.len();
            let paths = with_geometry_reads(feature, |feature| {
                visit_paths(feature, |shape| {
                    check().map_err(std::io::Error::other)?;
                    reference_vertices(vertices, geometry::vertices(&shape))
                        .map_err(std::io::Error::other)?;
                    visit_segments(shape, |segment| {
                        check_periodically()?;
                        if unique.insert(segment) {
                            segments.push(segment);
                        }
                        Ok(ControlFlow::<()>::Continue(()))
                    })
                })
            });
            if usable_geometry(feature.id(), paths, bad)?.is_none() {
                for segment in segments.drain(start..) {
                    unique.remove(&segment);
                }
            }
        }
        drop(unique);
        Ok(Self::new(meters, segments))
    }

    pub(super) fn is_empty(&self) -> bool {
        match &self.reference {
            Reference::Projected(index) => index.size() == 0,
            Reference::Geographic(index) => index.size() == 0,
            Reference::PolarPoint(_) => false,
        }
    }

    pub(super) fn matches<S: Source>(
        &self,
        feature: Feature<'_, S>,
        check: &impl Fn() -> Result<(), String>,
    ) -> regol::Result<bool> {
        let mut check_periodically = periodic_check(check);
        let matched = visit_paths(feature, |shape| {
            visit_segments(shape, |segment| {
                check_periodically()?;
                let [start, end] = segment.endpoints();
                match &self.reference {
                    Reference::PolarPoint(center) => {
                        let arc = Arc::new(start, end);
                        if arc.distance_to_point(*center, 0.0) * EARTH_RADIUS <= self.meters {
                            return Ok(ControlFlow::Break(()));
                        }
                    }
                    Reference::Projected(index) => {
                        let mut candidate = None;
                        for item in
                            index.locate_in_envelope_intersecting(&envelope(start, end, 0.0))
                        {
                            check_periodically()?;
                            if candidate
                                .get_or_insert_with(|| segment.geometry())
                                .distance(&item.data.geometry())
                                .is_some_and(|d| d.get::<meter>() <= self.meters)
                            {
                                return Ok(ControlFlow::Break(()));
                            }
                        }
                    }
                    Reference::Geographic(index) => {
                        let mut candidate = None;
                        for item in
                            index.locate_in_envelope_intersecting(&envelope(start, end, 0.0))
                        {
                            check_periodically()?;
                            let distance = candidate
                                .get_or_insert_with(|| Arc::new(start, end))
                                .distance(item.data);
                            if distance <= ANGULAR_TOLERANCE
                                || distance * EARTH_RADIUS <= self.meters
                            {
                                return Ok(ControlFlow::Break(()));
                            }
                        }
                    }
                }
                Ok(ControlFlow::Continue(()))
            })
        })?;
        Ok(matched.is_break())
    }

    pub(super) fn windows(
        &self,
        check: &impl Fn() -> Result<(), String>,
    ) -> Result<Vec<Bounds>, String> {
        let radians = self.meters / EARTH_RADIUS;
        if radians >= PI {
            return Ok(vec![Bounds::WORLD]);
        }
        let mut windows = BTreeMap::new();
        let mut add =
            |[start, end]: [[f64; 3]; 2]| add_arc_windows(start, end, radians, &mut windows, check);
        match &self.reference {
            Reference::Projected(index) => {
                for item in index {
                    add(item.data.endpoints())?;
                }
            }
            Reference::Geographic(index) => {
                for item in index {
                    add(item.data.endpoints())?;
                }
            }
            Reference::PolarPoint(center) => add([*center, *center])?,
        }
        windows.into_values().map(geometry::project_box).collect()
    }
}

fn segment_index<T: Copy>(
    meters: f64,
    segments: impl IntoIterator<Item = T>,
    endpoints: impl Fn(T) -> [[f64; 3]; 2],
) -> RTree<IndexEntry<T>> {
    let radians = meters / EARTH_RADIUS;
    RTree::bulk_load(
        segments
            .into_iter()
            .map(|segment| {
                let [start, end] = endpoints(segment);
                GeomWithData::new(Rectangle::from_aabb(envelope(start, end, radians)), segment)
            })
            .collect(),
    )
}

fn periodic_check(check: &impl Fn() -> Result<(), String>) -> impl FnMut() -> regol::Result<()> {
    let mut steps = 0usize;
    move || {
        if steps.is_multiple_of(256) {
            check().map_err(std::io::Error::other)?;
        }
        steps += 1;
        Ok(())
    }
}

fn vector(point: Point) -> [f64; 3] {
    // Preserve the westernmost cell's actual longitude, as distance() does.
    let scale = TAU / WORLD_WIDTH as f64;
    let lon = f64::from(point.x) * scale;
    let lat = (f64::from(point.y) * scale).sinh().atan();
    unit_vector(lon, lat)
}

fn project_grid_point(point: LonLat) -> Option<Point> {
    let latitude = point.latitude;
    let south = Bounds::WORLD.min.lon_lat().latitude;
    let north = Bounds::WORLD.max.lon_lat().latitude;
    (south..=north).contains(&latitude).then(|| point.project())
}

fn geographic_vector(point: LonLat) -> [f64; 3] {
    let latitude = point.latitude.get::<radian>();
    if latitude.abs() == FRAC_PI_2 {
        [0.0, 0.0, latitude.signum()]
    } else {
        unit_vector(point.longitude.get::<radian>(), latitude)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct NodePosition {
    point: Point,
    pub(crate) vector: [f64; 3],
}

impl NodePosition {
    pub(crate) fn new(point: Point) -> Self {
        Self {
            point,
            vector: vector(point),
        }
    }
}

pub(crate) struct NodeRadius {
    meters: f64,
    chord: f64,
}

impl NodeRadius {
    pub(crate) fn new(meters: f64) -> Self {
        let radians = (meters / EARTH_RADIUS).min(PI);
        Self {
            meters,
            chord: 2.0 * (radians / 2.0).sin(),
        }
    }

    pub(crate) fn envelope(&self, center: [f64; 3]) -> AABB<[f64; 3]> {
        let margin = self.chord + ANGULAR_MARGIN;
        AABB::from_corners(center.map(|n| n - margin), center.map(|n| n + margin))
    }

    pub(crate) fn includes(&self, a: NodePosition, b: NodePosition) -> bool {
        if a.point == b.point {
            return true;
        }
        let chord = norm(std::array::from_fn(|i| a.vector[i] - b.vector[i]));
        // Use the engine's exact threshold near the boundary, including the
        // coincident grid cells on the antimeridian.
        if (chord - self.chord).abs() <= ANGULAR_MARGIN {
            return Geometry::Point(a.point)
                .distance(&Geometry::Point(b.point))
                .is_some_and(|d| d.get::<meter>() <= self.meters);
        }
        chord < self.chord
    }
}

fn unit_vector(lon: f64, lat: f64) -> [f64; 3] {
    let (sin_lon, cos_lon) = lon.sin_cos();
    let (sin_lat, cos_lat) = lat.sin_cos();
    [cos_lon * cos_lat, cos_lat * sin_lon, sin_lat]
}

struct Cap {
    center: [f64; 3],
    radians: f64,
}

impl Cap {
    fn for_arc(start: [f64; 3], end: [f64; 3]) -> Option<Self> {
        let sum = std::array::from_fn(|i| start[i] + end[i]);
        let length = norm(sum);
        if length < 1e-12 {
            return None;
        }
        let difference = std::array::from_fn(|i| start[i] - end[i]);
        Some(Self {
            center: sum.map(|value| value / length),
            radians: norm(difference).atan2(length),
        })
    }
}

fn norm(v: [f64; 3]) -> f64 {
    v.iter().map(|n| n * n).sum::<f64>().sqrt()
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a.into_iter().zip(b).map(|(a, b)| a * b).sum()
}

fn cross([ax, ay, az]: [f64; 3], [bx, by, bz]: [f64; 3]) -> [f64; 3] {
    [ay * bz - az * by, az * bx - ax * bz, ax * by - ay * bx]
}

fn stable_cross(start: [f64; 3], end: [f64; 3]) -> [f64; 3] {
    // Subtract the nearby or opposite vector before crossing to preserve
    // precision for almost coincident or antipodal endpoints.
    let sign = 1.0_f64.copysign(dot(start, end));
    cross(start, std::array::from_fn(|i| end[i] - sign * start[i]))
}

fn normalized(v: [f64; 3]) -> Option<[f64; 3]> {
    let length = norm(v);
    (length > f64::EPSILON).then(|| v.map(|component| component / length))
}

#[derive(Clone, Copy)]
struct Arc {
    start: [f64; 3],
    end: [f64; 3],
    normal: Option<[f64; 3]>,
}

impl Arc {
    fn new(start: [f64; 3], end: [f64; 3]) -> Self {
        Self {
            start,
            end,
            normal: normalized(stable_cross(start, end)),
        }
    }

    fn endpoints(self) -> [[f64; 3]; 2] {
        [self.start, self.end]
    }

    fn within_tangents(self, normal: [f64; 3], point: [f64; 3], tolerance: f64) -> bool {
        dot(cross(normal, self.start), point) >= -tolerance
            && dot(cross(self.end, normal), point) >= -tolerance
            // Very short arcs can pass both tests at the opposite direction
            // when the dot products are inside the numerical tolerance.
            && (tolerance == 0.0 || dot(self.start, point) + dot(self.end, point) >= 0.0)
    }

    fn distance_to_point(self, point: [f64; 3], tolerance: f64) -> f64 {
        if let Some(normal) = self.normal
            && self.within_tangents(normal, point, tolerance)
        {
            // Projection onto the great-circle plane preserves the tangent
            // tests, and the angle to that plane is the shortest distance.
            return dot(normal, point).abs().atan2(norm(cross(normal, point)));
        }
        let angle = |endpoint| norm(cross(endpoint, point)).atan2(dot(endpoint, point));
        angle(self.start).min(angle(self.end))
    }

    fn distance(self, other: Self) -> f64 {
        if let (Some(left), Some(right)) = (self.normal, other.normal)
            && let Some(direction) = normalized(stable_cross(left, right))
            && [direction, direction.map(|component| -component)]
                .into_iter()
                .any(|point| {
                    self.within_tangents(left, point, ANGULAR_TOLERANCE)
                        && other.within_tangents(right, point, ANGULAR_TOLERANCE)
                })
        {
            return 0.0;
        }
        // In the absence of an intersection, at least one endpoint attains
        // the minimum separation. This also covers degenerate point segments.
        self.distance_to_point(other.start, ANGULAR_TOLERANCE)
            .min(self.distance_to_point(other.end, ANGULAR_TOLERANCE))
            .min(other.distance_to_point(self.start, ANGULAR_TOLERANCE))
            .min(other.distance_to_point(self.end, ANGULAR_TOLERANCE))
    }
}

fn envelope(start: [f64; 3], end: [f64; 3], margin: f64) -> AABB<[f64; 3]> {
    let Some(cap) = Cap::for_arc(start, end) else {
        return AABB::from_corners([-1.0; 3], [1.0; 3]);
    };
    let chord = 2.0 * ((cap.radians + margin + ANGULAR_MARGIN).min(PI) / 2.0).sin();
    AABB::from_corners(
        cap.center.map(|n| (n - chord).max(-1.0)),
        cap.center.map(|n| (n + chord).min(1.0)),
    )
}

type Windows = BTreeMap<(i32, i32), [f64; 4]>;

fn add_arc_windows(
    start: [f64; 3],
    end: [f64; 3],
    radius: f64,
    windows: &mut Windows,
    check: &impl Fn() -> Result<(), String>,
) -> Result<(), String> {
    check()?;
    let Some(cap) = Cap::for_arc(start, end) else {
        return Err(
            "An around reference contains an antipodal segment. Add a bounding box.".into(),
        );
    };
    // Subdivide long edges for scan planning only. Keep testing the original
    // arc so draw resolution and GOL quantization cannot change the match.
    if cap.radians > (CELL_DEGREES / 4.0).to_radians().max(radius) {
        add_arc_windows(start, cap.center, radius, windows, check)?;
        return add_arc_windows(cap.center, end, radius, windows, check);
    }
    let [x, y, z] = cap.center;
    let lon = y.atan2(x);
    let lat = z.atan2(x.hypot(y));
    let angle = cap.radians + radius + ANGULAR_MARGIN;
    let south = (lat - angle).max(-FRAC_PI_2);
    let north = (lat + angle).min(FRAC_PI_2);
    if south <= -FRAC_PI_2 || north >= FRAC_PI_2 {
        add_window(windows, [-PI, south, PI, north]);
    } else {
        // The two extreme projected X cells coincide on the sphere, just
        // inside the eastern seam. Cover the alias even for radius zero.
        let dx = (angle.sin() / lat.cos()).clamp(0.0, 1.0).asin() + TAU / WORLD_WIDTH as f64;
        let west = lon - dx;
        let east = lon + dx;
        if west < -PI {
            add_window(windows, [west + TAU, south, PI, north]);
            add_window(windows, [-PI, south, east, north]);
        } else if east > PI {
            add_window(windows, [west, south, PI, north]);
            add_window(windows, [-PI, south, east - TAU, north]);
        } else {
            add_window(windows, [west, south, east, north]);
        }
    }
    if windows.len() > MAX_SEARCH_WINDOWS {
        return Err("Around references cover too many search windows. Add a bounding box.".into());
    }
    Ok(())
}

fn add_window(windows: &mut Windows, coords: [f64; 4]) {
    let [w, s, e, n] = coords.map(f64::to_degrees);
    let col = ((w + e) / (2.0 * CELL_DEGREES)).floor() as i32;
    let row = ((s + n) / (2.0 * CELL_DEGREES)).floor() as i32;
    windows
        .entry((col, row))
        .and_modify(|window| {
            *window = [
                w.min(window[0]),
                s.min(window[1]),
                e.max(window[2]),
                n.max(window[3]),
            ];
        })
        .or_insert([w, s, e, n]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_diagonal_reference_scans_a_corridor_and_can_cancel() {
        let project = |point| geometry::geographic_point(point).unwrap().project();
        let points = [[-30.0, -20.0], [30.0, 20.0]];
        let around = Around::polyline(100.0, &points, &mut 0, &|| Ok(())).unwrap();
        let windows = around.windows(&|| Ok(())).unwrap();
        let covers = |point| windows.iter().any(|window| window.contains(project(point)));
        assert!(windows.len() > 100);
        assert!(covers([-30.0, -20.0]) && covers([0.0, 0.0]) && covers([30.0, 20.0]));
        assert!(!covers([-29.0, 19.0]));
        assert_eq!(
            around.windows(&|| Err("cancelled".into())),
            Err("cancelled".into())
        );
        assert!(matches!(
            Around::polyline(100.0, &points, &mut 0, &|| Err("cancelled".into())),
            Err(message) if message == "cancelled"
        ));
        let mut vertices = crate::MAX_VERTICES;
        assert!(matches!(
            Around::polyline(100.0, &points, &mut vertices, &|| Ok(())),
            Err(message) if message.contains("Reference vertex limit")
        ));
    }

    #[test]
    fn long_polyline_honors_the_search_window_limit() {
        let points = [[-80.0, 0.0], [80.0, 0.0], [80.0, 50.0], [-80.0, 50.0]];
        let around = Around::polyline(25.0, &points, &mut 0, &|| Ok(())).unwrap();
        assert!(matches!(
            around.windows(&|| Ok(())),
            Err(message) if message.contains("too many search windows")
        ));
    }

    #[test]
    fn dateline_windows_cover_both_sides() {
        let project = |point| geometry::geographic_point(point).unwrap().project();
        let around = Around::point(500.0, geometry::geographic_point([179.999, 0.0]).unwrap());
        let windows = around.windows(&|| Ok(())).unwrap();
        assert_eq!(windows.len(), 2);
        assert!(
            windows
                .iter()
                .any(|window| window.contains(project([-179.999, 0.0])))
        );
        assert!(
            windows
                .iter()
                .any(|window| window.contains(project([179.999, 0.0])))
        );
    }

    #[test]
    fn zero_radius_windows_cover_the_aliased_seam_cell() {
        let east = Point { x: i32::MAX, y: 0 };
        let west = Point { x: i32::MIN, y: 0 };
        assert_eq!(
            Geometry::Point(east)
                .distance(&Geometry::Point(west))
                .unwrap()
                .get::<meter>(),
            0.0,
        );
        let windows = Around::point(0.0, east.lon_lat())
            .windows(&|| Ok(()))
            .unwrap();
        assert!(windows.iter().any(|window| window.contains(west)));
    }

    #[test]
    fn cached_node_distances_keep_exact_thresholds() {
        let east = Point {
            x: i32::MAX - 20,
            y: 0,
        };
        let west = Point {
            x: i32::MIN + 20,
            y: 0,
        };
        let meters = Geometry::Point(east)
            .distance(&Geometry::Point(west))
            .unwrap()
            .get::<meter>();
        assert!(NodeRadius::new(meters).includes(NodePosition::new(east), NodePosition::new(west)));
        assert!(
            !NodeRadius::new(meters.next_down())
                .includes(NodePosition::new(east), NodePosition::new(west))
        );
        let seam_east = NodePosition::new(Point { x: i32::MAX, y: 0 });
        let seam_west = NodePosition::new(Point { x: i32::MIN, y: 0 });
        assert!(NodeRadius::new(0.0).includes(seam_east, seam_west));
    }
}
