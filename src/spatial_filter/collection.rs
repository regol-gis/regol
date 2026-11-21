use geo::{
    Coord, CoordinatePosition, Geometry, GeometryCollection, Intersects, LineString,
    MultiLineString, MultiPoint, MultiPolygon, Point, Polygon, coordinate_position::CoordPos,
};
use rstar::AABB;

use super::edge_index;

mod relate;

pub(super) use relate::{PreparedTopology, collection_matrix};

#[derive(Default)]
struct Components {
    polygons: Vec<Polygon>,
    lines: Vec<LineString>,
    points: Vec<Point>,
}

impl Components {
    fn add(&mut self, geometry: Geometry) {
        match geometry {
            Geometry::GeometryCollection(parts) => {
                for part in parts {
                    self.add(part);
                }
            }
            Geometry::Polygon(polygon) => self.polygons.push(polygon),
            Geometry::MultiPolygon(parts) => self.polygons.extend(parts.0),
            Geometry::LineString(line) => self.lines.push(line),
            Geometry::Point(point) => self.points.push(point),
            _ => unreachable!("unexpected converted geometry type"),
        }
    }
}

/// Group collection members by dimension. A single line with only redundant
/// points can use the ordinary line predicates directly, such as a route with
/// stops on the line.
pub(super) fn normalize_collection(geometry: Geometry) -> Geometry {
    let Geometry::GeometryCollection(_) = geometry else {
        return geometry;
    };
    let mut components = Components::default();
    components.add(geometry);

    let area = MultiPolygon(components.polygons);
    let lines = MultiLineString(components.lines);
    if area.0.is_empty() && !components.points.is_empty() {
        if let [line] = lines.0.as_slice() {
            let edges = edge_index::build(line.lines());
            components.points.retain(|point| {
                !edges
                    .locate_in_envelope_intersecting(&AABB::from_point([point.x(), point.y()]))
                    .any(|edge| edge.line.intersects(&point.0))
            });
        }
    }
    components
        .points
        .sort_unstable_by(|a, b| a.x().total_cmp(&b.x()).then(a.y().total_cmp(&b.y())));
    components.points.dedup();

    let mut normalized = Vec::with_capacity(3);
    match area.0.len() {
        0 => {}
        1 => normalized.extend(area.0.into_iter().map(Geometry::Polygon)),
        _ => normalized.push(Geometry::MultiPolygon(area)),
    }
    match lines.0.len() {
        0 => {}
        1 => normalized.extend(lines.0.into_iter().map(Geometry::LineString)),
        _ => normalized.push(Geometry::MultiLineString(lines)),
    }
    match components.points.len() {
        0 => {}
        1 => normalized.extend(components.points.into_iter().map(Geometry::Point)),
        _ => normalized.push(Geometry::MultiPoint(MultiPoint(components.points))),
    }
    if normalized.len() == 1 {
        normalized.pop().expect("one normalized component")
    } else {
        Geometry::GeometryCollection(GeometryCollection(normalized))
    }
}

/// Find the point in the normalized collection, giving area membership
/// precedence over line membership and applying the mod-2 rule at endpoints.
pub(super) fn point_position(geometry: &Geometry, point: Coord) -> CoordPos {
    match geometry {
        Geometry::GeometryCollection(parts) => {
            first_position(&parts.0, |part| point_position(part, point))
        }
        Geometry::MultiPolygon(parts) => {
            first_position(&parts.0, |polygon| polygon.coordinate_position(&point))
        }
        Geometry::MultiLineString(lines) => {
            let mut inside = false;
            let mut endpoints = 0;
            lines.calculate_coordinate_position(&point, &mut inside, &mut endpoints);
            if endpoints % 2 == 1 {
                CoordPos::OnBoundary
            } else if inside || endpoints != 0 {
                CoordPos::Inside
            } else {
                CoordPos::Outside
            }
        }
        geometry => geometry.coordinate_position(&point),
    }
}

fn first_position<T>(parts: &[T], position: impl FnMut(&T) -> CoordPos) -> CoordPos {
    parts
        .iter()
        .map(position)
        .find(|p| *p != CoordPos::Outside)
        .unwrap_or(CoordPos::Outside)
}
