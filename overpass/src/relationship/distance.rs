use regol::{
    Geometry, Point, Polygon, SpatialFilter, SpatialPredicate,
    units::{Length, angle::radian, length::meter},
};
use rstar::{
    AABB, Envelope, RTree,
    primitives::{GeomWithData, Rectangle},
};

#[cfg(test)]
mod tests;

type Box3 = AABB<[f64; 3]>;

// Regol's spherical distance model. Allow a few centimetres in the broad phase:
// public lon_lat() clamps the westernmost grid cell, and exact distances have
// a small numerical tolerance at zero. Regol makes the final threshold decision.
const EARTH_RADIUS: f64 = 6_371_008.8;
const PADDING: f64 = 4e-9;

fn unit_vector(p: Point) -> [f64; 3] {
    let pos = p.lon_lat();
    let (sin_lon, cos_lon) = pos.longitude.get::<radian>().sin_cos();
    let (sin_lat, cos_lat) = pos.latitude.get::<radian>().sin_cos();
    [cos_lat * cos_lon, cos_lat * sin_lon, sin_lat]
}

fn path_bounds(points: &[Point]) -> Option<Box3> {
    let (&first, rest) = points.split_first()?;
    let center = unit_vector(first);
    let mut max_squared = 0.0_f64;
    for &point in rest {
        let [x, y, z] = unit_vector(point);
        max_squared = max_squared
            .max((x - center[0]).powi(2) + (y - center[1]).powi(2) + (z - center[2]).powi(2));
    }
    let radius = max_squared.sqrt() + PADDING;
    // A spherical cap smaller than a hemisphere is geodesically convex. If
    // it contains the vertices it contains their shorter arcs and the smaller
    // polygon interior. For larger spans, leave pruning to the exact filter.
    if radius >= std::f64::consts::SQRT_2 {
        return Some(Box3::from_corners([-1.0; 3], [1.0; 3]));
    }
    Some(Box3::from_corners(
        center.map(|v| v - radius),
        center.map(|v| v + radius),
    ))
}

fn polygon_bounds(p: &Polygon) -> Option<Box3> {
    let mut bounds = path_bounds(&p.exterior)?;
    for hole in &p.interiors {
        if let Some(inner) = path_bounds(hole) {
            bounds.merge(&inner);
        }
    }
    Some(bounds)
}

fn bounds(g: &Geometry) -> Option<Box3> {
    match g {
        Geometry::Point(p) => Some(Box3::from_point(unit_vector(*p))),
        Geometry::LineString(line) => path_bounds(line),
        Geometry::Polygon(p) => polygon_bounds(p),
        Geometry::MultiPolygon(parts) => parts
            .iter()
            .filter_map(polygon_bounds)
            .reduce(|a, b| a.merged(&b)),
        Geometry::Collection(parts) => parts.iter().filter_map(bounds).reduce(|a, b| a.merged(&b)),
    }
}

enum Reference {
    // Avoid allocating a prepared surface for every bus stop in a large set.
    Point(Point),
    Shape(Box<SpatialFilter>),
}

type Entry = GeomWithData<Rectangle<[f64; 3]>, Reference>;

pub(crate) struct DistanceIndex {
    references: RTree<Entry>,
    threshold: Length,
    padding: f64,
}

impl DistanceIndex {
    pub(super) fn new(
        geometries: Vec<Geometry>,
        threshold: Length,
        check: &impl Fn() -> Result<(), String>,
    ) -> Result<Self, String> {
        let mut references = Vec::with_capacity(geometries.len());
        for geometry in geometries {
            check()?;
            if let Some(envelope) = bounds(&geometry) {
                let reference = match geometry {
                    Geometry::Point(p) => Reference::Point(p),
                    _ => Reference::Shape(Box::new(
                        SpatialFilter::new(SpatialPredicate::WithinDistance(threshold), &geometry)
                            .map_err(|e| e.to_string())?,
                    )),
                };
                references.push(Entry::new(envelope.into(), reference));
            }
        }
        // On the unit sphere, chord length is at most arc length in radians.
        let padding = (threshold.get::<meter>() / EARTH_RADIUS).min(2.0) + PADDING;
        Ok(Self {
            references: RTree::bulk_load(references),
            threshold,
            padding,
        })
    }

    fn nearby(&self, b: Box3) -> impl Iterator<Item = &Entry> {
        let search = Box3::from_corners(
            b.lower().map(|v| v - self.padding),
            b.upper().map(|v| v + self.padding),
        );
        self.references.locate_in_envelope_intersecting(&search)
    }

    pub(super) fn matches(&self, candidate: &Geometry) -> regol::Result<bool> {
        let Some(b) = bounds(candidate) else {
            return Ok(false);
        };
        for reference in self.nearby(b) {
            let matched = match &reference.data {
                Reference::Point(p) => candidate
                    .distance(&Geometry::Point(*p))
                    .is_some_and(|d| d <= self.threshold),
                Reference::Shape(filter) => filter.matches_geometry(candidate)?,
            };
            if matched {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
