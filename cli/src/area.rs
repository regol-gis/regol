use regol::{
    Bounds, Error as GolError, Feature, FeatureId, FeatureType, Latitude, Limits, LonLat,
    Longitude, Point, source::Source, units::angle::degree,
};
use serde_json::Value;
use std::{error::Error, fs, path::Path};

fn position(longitude: f64, latitude: f64) -> Result<LonLat, &'static str> {
    Ok(LonLat {
        longitude: Longitude::new::<degree>(longitude).map_err(|_| "longitude is out of range")?,
        latitude: Latitude::new::<degree>(latitude).map_err(|_| "latitude is out of range")?,
    })
}

pub(super) fn parse_bbox(argument: &str) -> Result<Bounds, Box<dyn Error>> {
    let values: Vec<_> = argument
        .split(',')
        .map(|value| value.trim().parse::<f64>())
        .collect::<Result<_, _>>()?;
    let [west, south, east, north] = values[..] else {
        return Err("bbox must have four values in west,south,east,north order".into());
    };
    Ok(Bounds::from_lon_lat(
        position(west, south)?,
        position(east, north)?,
    )?)
}

fn array(value: &Value) -> Result<&[Value], &'static str> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or("expected a GeoJSON array")
}

fn read_ring(value: &Value) -> Result<Ring, &'static str> {
    let positions = array(value)?;
    if positions.len() < 4 {
        return Err("a GeoJSON ring needs at least four positions and must be closed");
    }
    let lon_lat = |position| -> Result<(f64, f64), &'static str> {
        let position = array(position)?;
        let longitude = position
            .first()
            .and_then(Value::as_f64)
            .ok_or("invalid longitude")?;
        let latitude = position
            .get(1)
            .and_then(Value::as_f64)
            .ok_or("invalid latitude")?;
        Ok((longitude, latitude))
    };
    if lon_lat(&positions[0])? != lon_lat(&positions[positions.len() - 1])? {
        return Err("a GeoJSON ring needs at least four positions and must be closed");
    }
    let points = positions
        .iter()
        .map(|vertex| {
            let (longitude, latitude) = lon_lat(vertex)?;
            position(longitude, latitude).map(LonLat::project)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Ring::new(&points))
}

fn read_polygon(value: &Value) -> Result<AreaPolygon, &'static str> {
    let (exterior, interiors) = array(value)?.split_first().ok_or("polygon has no rings")?;
    Ok(AreaPolygon {
        exterior: read_ring(exterior)?,
        interiors: interiors.iter().map(read_ring).collect::<Result<_, _>>()?,
    })
}

fn read_geojson(value: &Value, polygons: &mut Vec<AreaPolygon>) -> Result<(), &'static str> {
    match value["type"].as_str().ok_or("missing GeoJSON type")? {
        "Polygon" => polygons.push(read_polygon(&value["coordinates"])?),
        "MultiPolygon" => {
            for polygon in array(&value["coordinates"])? {
                polygons.push(read_polygon(polygon)?);
            }
        }
        "Feature" => read_geojson(&value["geometry"], polygons)?,
        "FeatureCollection" => {
            for feature in array(&value["features"])? {
                read_geojson(feature, polygons)?;
            }
        }
        "GeometryCollection" => {
            for geometry in array(&value["geometries"])? {
                read_geojson(geometry, polygons)?;
            }
        }
        _ => return Err("GeoJSON area must contain polygon geometry"),
    }
    Ok(())
}

pub(super) struct Area {
    bounds: Bounds,
    polygons: Vec<AreaPolygon>,
}

impl Area {
    pub(super) fn open(path: &Path) -> Result<Self, Box<dyn Error>> {
        let read = || -> Result<Self, Box<dyn Error>> {
            let value = serde_json::from_slice(&fs::read(path)?)?;
            Ok(Self::from_geojson(&value)?)
        };
        read().map_err(|error| format!("{}: {error}", path.display()).into())
    }

    fn from_geojson(value: &Value) -> Result<Self, &'static str> {
        let mut polygons = Vec::new();
        read_geojson(value, &mut polygons)?;
        let bounds = polygons
            .iter()
            .map(|polygon| polygon.exterior.bounds())
            .reduce(union)
            .ok_or("GeoJSON area contains no polygons")?;
        Ok(Self { bounds, polygons })
    }

    pub(super) fn bounds(&self) -> Bounds {
        self.bounds
    }

    pub(super) fn intersects_feature<S: Source>(
        &self,
        feature: Feature<'_, S>,
    ) -> regol::Result<bool> {
        match feature.kind() {
            FeatureType::Node => self.intersects_node(feature),
            FeatureType::Way => self.intersects_way(feature),
            FeatureType::Relation => {
                let mut limits = Limits::default();
                self.intersects_relation(
                    feature,
                    &mut Vec::new(),
                    &mut limits.max_traversal_steps,
                    limits.max_tree_depth,
                )
            }
        }
    }

    fn intersects_node<S: Source>(&self, feature: Feature<'_, S>) -> regol::Result<bool> {
        let point = feature.point()?.ok_or(GolError::WrongFeatureType)?;
        Ok(self.contains(point))
    }

    fn intersects_relation<S: Source>(
        &self,
        feature: Feature<'_, S>,
        path: &mut Vec<FeatureId>,
        steps: &mut u64,
        max_depth: usize,
    ) -> regol::Result<bool> {
        let bounds = feature.bounds()?;
        if !self.bounds.intersects(bounds) || path.contains(&feature.id()) {
            return Ok(false);
        }
        if feature.is_area() {
            return self.intersects_area_relation(feature, bounds, steps);
        }
        if path.len() >= max_depth {
            return Err(GolError::Limit("area relation depth"));
        }

        path.push(feature.id());
        for member in feature.members()? {
            spend(steps)?;
            let member = member?.feature;
            let intersects = match member.kind() {
                FeatureType::Node => self.intersects_node(member)?,
                FeatureType::Way => {
                    self.bounds.intersects(member.bounds()?) && self.intersects_way(member)?
                }
                FeatureType::Relation => {
                    self.intersects_relation(member, path, steps, max_depth)?
                }
            };
            if intersects {
                path.pop();
                return Ok(true);
            }
        }
        path.pop();
        Ok(false)
    }

    fn intersects_area_relation<S: Source>(
        &self,
        feature: Feature<'_, S>,
        bounds: Bounds,
        steps: &mut u64,
    ) -> regol::Result<bool> {
        // Test the direct features first. If none interact with the request,
        // cast one ray per request polygon across the member ways to detect
        // containment. Ray crossing uses the stored linework even when the
        // constituent ways cannot be assembled into closed rings.
        for member in feature.members()? {
            spend(steps)?;
            let member = member?.feature;
            let intersects = match member.kind() {
                FeatureType::Node => self.intersects_node(member)?,
                FeatureType::Way => {
                    self.bounds.intersects(member.bounds()?) && self.intersects_way(member)?
                }
                FeatureType::Relation => false,
            };
            if intersects {
                return Ok(true);
            }
        }

        for polygon in &self.polygons {
            let probe = polygon.exterior.first;
            if !bounds.contains(probe) {
                continue;
            }
            let mut inside = false;
            for member in feature.members()? {
                spend(steps)?;
                let member = member?.feature;
                if member.kind() != FeatureType::Way || !ray_overlaps(member.bounds()?, probe) {
                    continue;
                }
                match way_location(member, probe)? {
                    Location::Boundary => return Ok(true),
                    Location::Inside => inside = !inside,
                    Location::Outside => {}
                }
            }
            if inside {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn intersects_way<S: Source>(&self, feature: Feature<'_, S>) -> regol::Result<bool> {
        let mut points = feature.coordinates()?;
        let Some(mut previous) = points.next().transpose()? else {
            return Ok(false);
        };
        if self.contains(previous) {
            return Ok(true);
        }
        for point in points {
            let point = point?;
            if self.intersects_edge(Edge::new(previous, point)) {
                return Ok(true);
            }
            previous = point;
        }
        if !feature.is_area() {
            return Ok(false);
        }
        // The only remaining intersection is an area polygon contained
        // entirely inside the feature's exterior ring.
        let bounds = feature.bounds()?;
        for polygon in &self.polygons {
            let probe = polygon.exterior.first;
            if bounds.contains(probe) && way_location(feature, probe)? != Location::Outside {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn contains(&self, point: Point) -> bool {
        self.polygons
            .iter()
            .any(|polygon| polygon.locate(point) != Location::Outside)
    }

    fn intersects_edge(&self, edge: Edge) -> bool {
        self.polygons
            .iter()
            .any(|polygon| polygon.intersects_edge(edge))
    }
}

fn spend(steps: &mut u64) -> regol::Result<()> {
    *steps = steps
        .checked_sub(1)
        .ok_or(GolError::Limit("area member steps"))?;
    Ok(())
}

fn way_location<S: Source>(feature: Feature<'_, S>, probe: Point) -> regol::Result<Location> {
    let mut points = feature.coordinates()?;
    let Some(mut previous) = points.next().transpose()? else {
        return Ok(Location::Outside);
    };
    let mut inside = false;
    for point in points {
        let point = point?;
        match Edge::new(previous, point).ray_hit(probe) {
            Location::Boundary => return Ok(Location::Boundary),
            Location::Inside => inside = !inside,
            Location::Outside => {}
        }
        previous = point;
    }
    Ok(if inside {
        Location::Inside
    } else {
        Location::Outside
    })
}

struct AreaPolygon {
    exterior: Ring,
    interiors: Vec<Ring>,
}

impl AreaPolygon {
    fn locate(&self, point: Point) -> Location {
        let location = self.exterior.locate(point);
        if location != Location::Inside {
            return location;
        }
        for ring in &self.interiors {
            match ring.locate(point) {
                Location::Boundary => return Location::Boundary,
                Location::Inside => return Location::Outside,
                Location::Outside => {}
            }
        }
        location
    }

    fn intersects_edge(&self, edge: Edge) -> bool {
        self.exterior.bounds().intersects(edge.bounds)
            && (self.exterior.intersects_edge(edge)
                || self.interiors.iter().any(|ring| ring.intersects_edge(edge)))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Location {
    Outside,
    Inside,
    Boundary,
}

#[cfg(test)]
fn locate(point: Point, ring: &[Point]) -> Location {
    locate_hits(
        ring.windows(2)
            .map(|pair| Edge::new(pair[0], pair[1]).ray_hit(point)),
    )
}

fn locate_hits(hits: impl Iterator<Item = Location>) -> Location {
    let mut inside = false;
    for hit in hits {
        match hit {
            Location::Boundary => return Location::Boundary,
            Location::Inside => inside = !inside,
            Location::Outside => {}
        }
    }
    if inside {
        Location::Inside
    } else {
        Location::Outside
    }
}

#[derive(Clone, Copy)]
struct Edge {
    start: Point,
    end: Point,
    bounds: Bounds,
}

impl Edge {
    fn new(start: Point, end: Point) -> Self {
        Self {
            start,
            end,
            bounds: union(
                Bounds {
                    min: start,
                    max: start,
                },
                Bounds { min: end, max: end },
            ),
        }
    }

    fn ray_hit(self, point: Point) -> Location {
        if !ray_overlaps(self.bounds, point) {
            return Location::Outside;
        }
        let orientation = cross(self.start, self.end, point);
        if orientation == 0 && self.bounds.contains(point) {
            return Location::Boundary;
        }
        if self.bounds.min.y <= point.y
            && point.y < self.bounds.max.y
            && (orientation > 0) == (self.start.y < self.end.y)
        {
            Location::Inside
        } else {
            Location::Outside
        }
    }

    fn intersects(self, edge: Self) -> bool {
        self.bounds.intersects(edge.bounds) && straddles(self, edge) && straddles(edge, self)
    }
}

fn straddles(edge: Edge, other: Edge) -> bool {
    let first = cross(edge.start, edge.end, other.start).signum();
    let second = cross(edge.start, edge.end, other.end).signum();
    first != second || first == 0
}

fn cross(a: Point, b: Point, point: Point) -> i128 {
    let x = |p: Point| i128::from(i64::from(p.x) - i64::from(a.x));
    let y = |p: Point| i128::from(i64::from(p.y) - i64::from(a.y));
    x(b) * y(point) - y(b) * x(point)
}

fn ray_overlaps(bounds: Bounds, point: Point) -> bool {
    point.y >= bounds.min.y && point.y <= bounds.max.y && point.x <= bounds.max.x
}

fn union(a: Bounds, b: Bounds) -> Bounds {
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

struct Ring {
    first: Point,
    root: EdgeIndex,
    edges: Vec<Edge>,
}

impl Ring {
    fn new(points: &[Point]) -> Self {
        let mut edges: Vec<_> = points
            .windows(2)
            .map(|pair| Edge::new(pair[0], pair[1]))
            .collect();
        let root = EdgeIndex::build(&mut edges);
        Self {
            first: points[0],
            root,
            edges,
        }
    }

    fn bounds(&self) -> Bounds {
        self.root.bounds
    }

    fn locate(&self, point: Point) -> Location {
        if self.bounds().contains(point) {
            self.root.locate(&self.edges, point)
        } else {
            Location::Outside
        }
    }

    fn intersects_edge(&self, edge: Edge) -> bool {
        self.root.intersects_edge(&self.edges, edge)
    }
}

// Subdivide the longest axis so point rays and feature segments only visit
// nearby edges. Small request polygons stay in a single allocation.
struct EdgeIndex {
    bounds: Bounds,
    children: Option<Box<(Self, Self)>>,
}

impl EdgeIndex {
    fn build(edges: &mut [Edge]) -> Self {
        let bounds = edges
            .iter()
            .map(|edge| edge.bounds)
            .reduce(union)
            .expect("nonempty ring");
        let children = if edges.len() > 16 {
            let width = i64::from(bounds.max.x) - i64::from(bounds.min.x);
            let height = i64::from(bounds.max.y) - i64::from(bounds.min.y);
            let middle = edges.len() / 2;
            edges.select_nth_unstable_by_key(middle, |edge| {
                if width > height {
                    i64::from(edge.bounds.min.x) + i64::from(edge.bounds.max.x)
                } else {
                    i64::from(edge.bounds.min.y) + i64::from(edge.bounds.max.y)
                }
            });
            let (left, right) = edges.split_at_mut(middle);
            Some(Box::new((Self::build(left), Self::build(right))))
        } else {
            None
        };
        Self { bounds, children }
    }

    fn locate(&self, edges: &[Edge], point: Point) -> Location {
        if !ray_overlaps(self.bounds, point) {
            return Location::Outside;
        }
        match self.children.as_deref() {
            Some((left, right)) => {
                let (a, b) = edges.split_at(edges.len() / 2);
                locate_hits([left.locate(a, point), right.locate(b, point)].into_iter())
            }
            None => locate_hits(edges.iter().map(|edge| edge.ray_hit(point))),
        }
    }

    fn intersects_edge(&self, edges: &[Edge], edge: Edge) -> bool {
        if !self.bounds.intersects(edge.bounds) {
            return false;
        }
        match self.children.as_deref() {
            Some((left, right)) => {
                let (a, b) = edges.split_at(edges.len() / 2);
                left.intersects_edge(a, edge) || right.intersects_edge(b, edge)
            }
            None => edges.iter().any(|candidate| candidate.intersects(edge)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn point(lon: f64, lat: f64) -> Point {
        position(lon, lat).unwrap().project()
    }

    #[test]
    fn ring_closure_compares_numeric_longitude_and_latitude() {
        let coords = json!([[0, 0], [2, 0], [1, 2], [0.0, 0.0]]);
        assert!(read_ring(&coords).is_ok());
        let open = json!([[0, 0], [2, 0], [1, 2], [0.00000000001, 0]]);
        assert!(read_ring(&open).is_err());
    }

    #[test]
    fn polygon_holes_and_multipolygon_members_use_inclusive_boundaries() {
        let area = Area::from_geojson(&json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": {
                    "type": "MultiPolygon",
                    "coordinates": [
                        [[[0,0],[4,0],[4,4],[0,4],[0,0]], [[1,1],[3,1],[3,3],[1,3],[1,1]]],
                        [[[5,0],[6,0],[6,1],[5,1],[5,0]]]
                    ]
                }
            }]
        }))
        .unwrap();
        assert!(area.contains(point(0.5, 0.5)));
        assert!(area.contains(point(1.0, 2.0)));
        assert!(area.contains(point(0.0, 0.0)));
        assert!(area.contains(point(5.5, 0.5)));
        assert!(!area.contains(point(2.0, 2.0)));
        assert!(!area.contains(point(4.5, 0.5)));
        let intersects = |a, b| area.intersects_edge(Edge::new(a, b));
        assert!(!intersects(point(1.5, 1.5), point(2.5, 2.5)));
        assert!(intersects(point(2.0, 1.5), point(2.0, 3.5)));
        assert!(intersects(point(-1.0, 0.5), point(7.0, 0.5)));
    }

    #[test]
    fn indexed_ring_agrees_with_ray_crossings_at_vertices_and_edges() {
        let vertices = (0..32).map(|i| {
            let angle = f64::from(i) * std::f64::consts::TAU / 32.0;
            let scale = if i % 2 == 0 { 500.0 } else { 1000.0 };
            Point {
                x: (angle.cos() * scale) as i32,
                y: (angle.sin() * scale) as i32,
            }
        });
        let points = vertices
            .clone()
            .chain(vertices.clone().take(1))
            .collect::<Vec<_>>();
        let ring = Ring::new(&points);
        for y in (-1000..=1000).step_by(31) {
            for x in (-1000..=1000).step_by(29) {
                let point = Point { x, y };
                assert!(ring.locate(point) == locate(point, &points));
            }
        }
        for point in vertices {
            assert!(ring.locate(point) == Location::Boundary);
        }
    }

    #[test]
    fn area_excludes_way_when_only_its_stored_bounds_intersect() {
        let area = Area::from_geojson(&json!({
            "type": "Polygon",
            "coordinates": [[[0.00145,0.00165],[0.00155,0.00165],[0.00155,0.00175],[0.00145,0.00175],[0.00145,0.00165]]]
        }))
        .unwrap();
        let input = include_bytes!("../tests/fixtures/basic.gol");
        let library = regol::Library::new(input.as_slice()).unwrap();
        let query = regol::Query::parse("*").unwrap();
        let query = query.prepare(&library).unwrap();
        let mut selected: Vec<_> = query
            .in_bounds(area.bounds())
            .filter_map(|candidate| {
                let feature = candidate.unwrap();
                area.intersects_feature(feature)
                    .unwrap()
                    .then(|| feature.id().to_string())
            })
            .collect();
        selected.sort_unstable();
        assert_eq!(selected, ["R20", "W11"]);
    }
}
