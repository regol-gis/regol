use crate::units::{Length, length::meter};
use crate::{
    Bounds, Error, Feature, Geometry, Point, Polygon, PreparedQuery, Result,
    geometry::{signed_area, validate_ring},
    measurements::Surface,
    source::Source,
};
use geo::{
    BoundingRect, Contains, Intersects, Relate, coordinate_position::CoordPos,
    dimensions::Dimensions, line_intersection::LineIntersection,
};
use std::fmt;

mod collection;
mod edge_index;

use collection::{PreparedTopology, collection_matrix, normalize_collection, point_position};

/// The relationship required of each candidate relative to the filter operand.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum SpatialPredicate {
    /// The candidate lies inside or on the boundary of the operand, and their
    /// interiors meet. A point on the boundary of a polygon alone cannot match.
    Within,
    /// The operand lies within the candidate. This is the inverse of [`Self::Within`].
    Contains,
    /// Every point of the candidate lies inside or on the boundary of the operand.
    CoveredBy,
    /// The candidate and operand have a point in common, including boundary contacts.
    Intersects,
    /// The candidate and operand have no point in common. An empty geometry
    /// is disjoint from every geometry, including another empty geometry.
    Disjoint,
    /// The geometries meet, but their interiors have no point in common.
    Touches,
    /// The geometries have the same dimension and share a region of that
    /// dimension. Each also has a part outside the other.
    Overlaps,
    /// The interiors cross as defined by the Simple Features DE-9IM predicates.
    /// For example, a line that enters a polygon crosses it, and two lines
    /// sharing only an endpoint do not cross.
    Crosses,
    /// The shortest distance is at most this length, using
    /// [`Geometry::distance`]. The threshold must be finite and nonnegative.
    WithinDistance(Length),
}

/// A reusable exact spatial predicate against one geometry or feature.
///
/// Topological comparisons use the integer Mercator plane. They account for
/// holes, all polygon components and the geometries in collections, with no
/// antimeridian wrapping. Collections use the union of their members. A point
/// member on a line endpoint has the same location as that endpoint. Supply
/// valid, closed polygon rings and valid polygon topology. The DE-9IM
/// predicates return a geometry error for collections
/// whose polygon components overlap or share edges. Empty geometries match
/// only [`SpatialPredicate::Disjoint`]. Distance comparisons use the model
/// documented on [`Geometry::distance`].
pub struct SpatialFilter {
    predicate: SpatialPredicate,
    operand: Operand,
}

enum Operand {
    Empty,
    Topology {
        geometry: geo::Geometry,
        bounds: Bounds,
        prepared: Option<Box<PreparedTopology>>,
    },
    Distance {
        surface: Surface,
        meters: f64,
    },
}

fn should_prepare(geometry: &geo::Geometry) -> bool {
    fn edge_count(geometry: &geo::Geometry) -> usize {
        match geometry {
            geo::Geometry::Polygon(p) => {
                p.exterior().0.len().saturating_sub(1)
                    + p.interiors()
                        .iter()
                        .map(|r| r.0.len().saturating_sub(1))
                        .sum::<usize>()
            }
            geo::Geometry::MultiPolygon(mp) => {
                mp.0.iter()
                    .map(|p| {
                        p.exterior().0.len().saturating_sub(1)
                            + p.interiors()
                                .iter()
                                .map(|r| r.0.len().saturating_sub(1))
                                .sum::<usize>()
                    })
                    .sum()
            }
            geo::Geometry::LineString(l) => l.0.len().saturating_sub(1),
            geo::Geometry::MultiLineString(ml) => {
                ml.0.iter().map(|l| l.0.len().saturating_sub(1)).sum()
            }
            geo::Geometry::GeometryCollection(gc) => gc.0.iter().map(edge_count).sum(),
            _ => 0,
        }
    }
    matches!(
        geometry,
        geo::Geometry::GeometryCollection(_)
            | geo::Geometry::MultiLineString(_)
            | geo::Geometry::MultiPoint(_)
    ) || edge_count(geometry) >= 16
}

impl fmt::Debug for SpatialFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpatialFilter")
            .field("predicate", &self.predicate)
            .field("candidate_bounds", &self.candidate_bounds())
            .finish_non_exhaustive()
    }
}

impl SpatialFilter {
    /// Create a filter against an owned-geometry value. The filter can be used
    /// after the original operand has been dropped.
    pub fn new(predicate: SpatialPredicate, operand: &Geometry) -> Result<Self> {
        let operand = match predicate {
            SpatialPredicate::WithinDistance(distance) => {
                let meters = distance.get::<meter>();
                if !meters.is_finite() || meters < 0.0 {
                    return Err(Error::Geometry(
                        "distance threshold must be finite and nonnegative",
                    ));
                }
                let surface = Surface::new(operand);
                if surface.is_empty() {
                    Operand::Empty
                } else {
                    Operand::Distance { surface, meters }
                }
            }
            _ => match prepare_topology(predicate, operand)? {
                Some(geometry) => {
                    let bounds = geometry
                        .bounding_rect()
                        .expect("nonempty geometry has bounds");
                    let point = |coord: geo::Coord| Point {
                        x: coord.x as i32,
                        y: coord.y as i32,
                    };
                    let prepared = should_prepare(&geometry)
                        .then(|| Box::new(PreparedTopology::new(&geometry)));
                    Operand::Topology {
                        geometry,
                        bounds: Bounds {
                            min: point(bounds.min()),
                            max: point(bounds.max()),
                        },
                        prepared,
                    }
                }
                None => Operand::Empty,
            },
        };
        Ok(Self { predicate, operand })
    }

    /// Assemble a feature's geometry once and use it as the filter operand.
    /// The feature can come from any library.
    pub fn from_feature<S: Source>(
        predicate: SpatialPredicate,
        operand: Feature<'_, S>,
    ) -> Result<Self> {
        Self::new(predicate, &operand.geometry()?)
    }

    /// The inclusive bounding box used to prune candidates for this filter.
    /// Returns `None` for an empty operand, a disjointness filter or a distance
    /// filter. Great-circle arcs may extend beyond the stored Mercator bounds
    /// of their endpoints.
    pub fn candidate_bounds(&self) -> Option<Bounds> {
        match &self.operand {
            Operand::Topology { bounds, .. } if self.predicate != SpatialPredicate::Disjoint => {
                Some(*bounds)
            }
            _ => None,
        }
    }

    /// Test a feature. Nonoverlapping stored bounds settle topological
    /// predicates before geometry assembly. Geometry and source errors from
    /// the remaining candidates are returned to the caller.
    pub fn matches<S: Source>(&self, candidate: Feature<'_, S>) -> Result<bool> {
        if let Some(bounds) = self.candidate_bounds() {
            if !bounds.intersects(candidate.bounds()?) {
                return Ok(false);
            }
        }
        self.matches_candidate(candidate)
    }

    /// Test already assembled geometry against the same operand.
    pub fn matches_geometry(&self, candidate: &Geometry) -> Result<bool> {
        match &self.operand {
            Operand::Empty => Ok(self.predicate == SpatialPredicate::Disjoint),
            Operand::Topology {
                geometry, prepared, ..
            } => match prepare_topology(self.predicate, candidate)? {
                Some(candidate) => {
                    topology_matches(self.predicate, &candidate, geometry, prepared.as_deref())
                }
                None => Ok(self.predicate == SpatialPredicate::Disjoint),
            },
            Operand::Distance { surface, meters } => Ok(Surface::new(candidate)
                .distance_meters(surface)
                .is_some_and(|distance| distance <= *meters)),
        }
    }

    pub(crate) fn has_no_matches(&self) -> bool {
        matches!(self.operand, Operand::Empty) && self.predicate != SpatialPredicate::Disjoint
    }

    // Positive topological selection already checked bounds during traversal.
    // A disjointness filter has to keep candidates outside the operand bounds.
    pub(crate) fn matches_candidate<S: Source>(&self, candidate: Feature<'_, S>) -> Result<bool> {
        match &self.operand {
            Operand::Empty => return Ok(self.predicate == SpatialPredicate::Disjoint),
            Operand::Topology { bounds, .. }
                if self.predicate == SpatialPredicate::Disjoint
                    && !bounds.intersects(candidate.bounds()?) =>
            {
                return Ok(true);
            }
            _ => {}
        }
        self.matches_geometry(&candidate.geometry()?)
    }
}

impl<'a, S: Source> PreparedQuery<'a, '_, S> {
    /// Stream matches of this query and an exact spatial filter in file
    /// traversal order. Stops after the first traversal or geometry error.
    pub fn spatial<'f>(
        &'f self,
        filter: &'f SpatialFilter,
    ) -> impl std::iter::FusedIterator<Item = Result<Feature<'a, S>>> + 'f {
        let features = self.features(filter.candidate_bounds());
        #[cfg(all(feature = "mmap", target_os = "linux"))]
        let mut features = features.enable_mapped_ahead();
        #[cfg(not(all(feature = "mmap", target_os = "linux")))]
        let mut features = features;
        std::iter::from_fn(move || {
            if filter.has_no_matches() {
                return None;
            }
            self.scan_unbounded_erased(&mut features, &mut |feature| {
                filter.matches_candidate(feature)
            })
        })
        .fuse()
    }
}

fn operand_point_position(
    operand: &geo::Geometry,
    prepared: Option<&PreparedTopology>,
    point: geo::Coord,
) -> CoordPos {
    if let Some(prepared) = prepared {
        prepared.locate_point(point)
    } else {
        point_position(operand, point)
    }
}

fn topology_matches(
    predicate: SpatialPredicate,
    candidate: &geo::Geometry,
    operand: &geo::Geometry,
    prepared_operand: Option<&PreparedTopology>,
) -> Result<bool> {
    match predicate {
        SpatialPredicate::Intersects => {
            if let geo::Geometry::Point(point) = candidate {
                if let Some(prepared) = prepared_operand {
                    return Ok(prepared.locate_point(point.0) != CoordPos::Outside);
                }
            }
            return Ok(candidate.intersects(operand));
        }
        SpatialPredicate::Disjoint => {
            if let geo::Geometry::Point(point) = candidate {
                if let Some(prepared) = prepared_operand {
                    return Ok(prepared.locate_point(point.0) == CoordPos::Outside);
                }
            }
            return Ok(!candidate.intersects(operand));
        }
        // A direct point location test avoids building a topology graph for the
        // common point-in-polygon query.
        SpatialPredicate::Within => {
            if let geo::Geometry::Point(point) = candidate {
                return Ok(
                    operand_point_position(operand, prepared_operand, point.0) == CoordPos::Inside
                );
            }
            if !bounds_cover(operand, candidate) {
                return Ok(false);
            }
        }
        SpatialPredicate::Contains => {
            if let geo::Geometry::Point(point) = operand {
                return Ok(point_position(candidate, point.0) == CoordPos::Inside);
            }
            if !bounds_cover(candidate, operand) {
                return Ok(false);
            }
        }
        SpatialPredicate::CoveredBy => {
            if let geo::Geometry::Point(point) = candidate {
                if let Some(prepared) = prepared_operand {
                    return Ok(prepared.locate_point(point.0) != CoordPos::Outside);
                }
                return Ok(operand.intersects(&point.0));
            }
            if !bounds_cover(operand, candidate) {
                return Ok(false);
            }
        }
        SpatialPredicate::Touches => {
            if let geo::Geometry::Point(point) = candidate {
                return Ok(operand_point_position(operand, prepared_operand, point.0)
                    == CoordPos::OnBoundary);
            }
            if let geo::Geometry::Point(point) = operand {
                return Ok(point_position(candidate, point.0) == CoordPos::OnBoundary);
            }
        }
        SpatialPredicate::Crosses | SpatialPredicate::Overlaps
            if matches!(candidate, geo::Geometry::Point(_))
                || matches!(operand, geo::Geometry::Point(_)) =>
        {
            return Ok(false);
        }
        _ => {}
    }
    let matrix = if [candidate, operand].into_iter().any(|geometry| {
        matches!(
            geometry,
            geo::Geometry::GeometryCollection(_)
                | geo::Geometry::MultiLineString(_)
                | geo::Geometry::MultiPoint(_)
        )
    }) {
        collection_matrix(candidate, operand, prepared_operand)
    } else {
        candidate.relate(operand)
    };
    Ok(match predicate {
        SpatialPredicate::Within => matrix.is_within(),
        SpatialPredicate::Contains => matrix.is_contains(),
        SpatialPredicate::CoveredBy => matrix.is_coveredby(),
        SpatialPredicate::Crosses => matrix.is_crosses(),
        SpatialPredicate::Touches => matrix.is_touches(),
        SpatialPredicate::Overlaps => matrix.is_overlaps(),
        _ => unreachable!("predicate does not require DE-9IM"),
    })
}

fn bounds_cover(outer: &geo::Geometry, inner: &geo::Geometry) -> bool {
    outer
        .bounding_rect()
        .zip(inner.bounding_rect())
        .is_some_and(|(outer, inner)| outer.contains(&inner))
}

fn prepare_topology(
    predicate: SpatialPredicate,
    geometry: &Geometry,
) -> Result<Option<geo::Geometry>> {
    let Some(geometry) = topology(geometry)? else {
        return Ok(None);
    };
    if matches!(
        predicate,
        SpatialPredicate::Intersects | SpatialPredicate::Disjoint
    ) {
        return Ok(Some(geometry));
    }
    validate_topology(&geometry)?;
    Ok(Some(normalize_collection(geometry)))
}

fn validate_topology(geometry: &geo::Geometry) -> Result<()> {
    fn polygons_in<'a>(geometry: &'a geo::Geometry, polygons: &mut Vec<&'a geo::Polygon>) {
        match geometry {
            geo::Geometry::Polygon(polygon) => polygons.push(polygon),
            geo::Geometry::MultiPolygon(parts) => polygons.extend(&parts.0),
            geo::Geometry::GeometryCollection(parts) => {
                for part in &parts.0 {
                    polygons_in(part, polygons);
                }
            }
            _ => {}
        }
    }

    if let geo::Geometry::Polygon(polygon) = geometry {
        return validate_polygon(polygon);
    }
    let mut polygons = Vec::new();
    polygons_in(geometry, &mut polygons);
    for polygon in &polygons {
        validate_polygon(polygon)?;
    }
    check_distinct_polygons(polygons)
}

fn check_distinct_polygons<'a>(polygons: impl IntoIterator<Item = &'a geo::Polygon>) -> Result<()> {
    let mut polygons: Vec<_> = polygons
        .into_iter()
        .map(|polygon| {
            (
                polygon
                    .bounding_rect()
                    .expect("nonempty polygon has bounds"),
                polygon,
            )
        })
        .collect();
    polygons.sort_unstable_by(|(a, _), (b, _)| a.min().x.total_cmp(&b.min().x));
    for (index, (bounds, polygon)) in polygons.iter().enumerate() {
        for (other_bounds, other) in polygons[index + 1..]
            .iter()
            .take_while(|(other, _)| other.min().x <= bounds.max().x)
        {
            if bounds.intersects(other_bounds) {
                let relation = polygon.relate(*other);
                if relation.get(CoordPos::Inside, CoordPos::Inside) == Dimensions::TwoDimensional
                    || relation.get(CoordPos::OnBoundary, CoordPos::OnBoundary)
                        == Dimensions::OneDimensional
                {
                    return Err(Error::Geometry(
                        "polygon components overlap or share an edge",
                    ));
                }
            }
        }
    }
    Ok(())
}

fn validate_polygon(polygon: &geo::Polygon) -> Result<()> {
    if !std::iter::once(polygon.exterior())
        .chain(polygon.interiors())
        .all(simple_ring)
    {
        return Err(Error::Geometry("polygon ring intersects itself"));
    }
    if polygon.interiors().is_empty() {
        return Ok(());
    }

    let shell = geo::Polygon::new(polygon.exterior().clone(), Vec::new());
    let mut holes = Vec::with_capacity(polygon.interiors().len());
    for ring in polygon.interiors() {
        let relation = shell.relate(ring);
        if !relation.is_contains()
            || relation.get(CoordPos::OnBoundary, CoordPos::Inside) == Dimensions::OneDimensional
        {
            return Err(Error::Geometry(
                "polygon hole crosses, shares an edge with, or lies outside the exterior",
            ));
        }
        holes.push(geo::Polygon::new(ring.clone(), Vec::new()));
    }
    check_distinct_polygons(&holes)
        .map_err(|_| Error::Geometry("polygon holes overlap or share an edge"))
}

fn simple_ring(ring: &geo::LineString) -> bool {
    let final_edge = ring.0.len() - 2;
    let edges = edge_index::build(ring.lines());
    edge_index::intersections(&edges).all(|(a, b, hit)| {
        let gap = a.abs_diff(b);
        (gap == 1 || gap == final_edge) && matches!(hit, LineIntersection::SinglePoint { .. })
    })
}

fn topology(geometry: &Geometry) -> Result<Option<geo::Geometry>> {
    Ok(match geometry {
        Geometry::Point(point) => Some(geo::Geometry::Point(geo::Point(coord(*point)))),
        Geometry::LineString(line) => line.first().map(|first| {
            if line.iter().all(|point| point == first) {
                geo::Geometry::Point(geo::Point(coord(*first)))
            } else {
                geo::Geometry::LineString(line_string(line))
            }
        }),
        Geometry::Polygon(polygon) => topology_polygon(polygon)?.map(geo::Geometry::Polygon),
        Geometry::MultiPolygon(polygons) => {
            let mut parts = Vec::new();
            for polygon in polygons {
                if let Some(polygon) = topology_polygon(polygon)? {
                    parts.push(polygon);
                }
            }
            match parts.len() {
                0 => None,
                1 => parts.pop().map(geo::Geometry::Polygon),
                _ => Some(geo::Geometry::MultiPolygon(geo::MultiPolygon(parts))),
            }
        }
        Geometry::Collection(collection) => {
            let mut parts = Vec::new();
            for part in collection {
                if let Some(part) = topology(part)? {
                    parts.push(part);
                }
            }
            match parts.len() {
                0 => None,
                1 => parts.pop(),
                _ => Some(geo::Geometry::GeometryCollection(geo::GeometryCollection(
                    parts,
                ))),
            }
        }
    })
}

fn topology_polygon(polygon: &Polygon) -> Result<Option<geo::Polygon>> {
    if polygon.exterior.is_empty() && polygon.interiors.is_empty() {
        return Ok(None);
    }
    let ring = |ring: &[Point]| {
        validate_ring(ring)?;
        if signed_area(ring) == 0 {
            return Err(Error::Geometry("ring has zero signed area"));
        }
        let mut ring = line_string(ring);
        ring.0.dedup();
        Ok(ring)
    };
    Ok(Some(geo::Polygon::new(
        ring(&polygon.exterior)?,
        polygon
            .interiors
            .iter()
            .map(|r| ring(r))
            .collect::<Result<_>>()?,
    )))
}

fn line_string(points: &[Point]) -> geo::LineString {
    geo::LineString(points.iter().copied().map(coord).collect())
}

fn coord(point: Point) -> geo::Coord {
    geo::Coord {
        x: f64::from(point.x),
        y: f64::from(point.y),
    }
}
