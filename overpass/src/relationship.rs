mod distance;
use crate::{MAX_VERTICES, geometry, usable_geometry, with_geometry_reads};
use distance::DistanceIndex;
use regol::units::{Length, length::meter};
use regol::{
    Bounds, Feature, FeatureId, Geometry, SpatialFilter, SpatialPredicate, source::Source,
};
use rstar::{
    AABB, RTree,
    primitives::{GeomWithData, Rectangle},
};
use std::collections::HashSet;

type IndexedFilter = GeomWithData<Rectangle<[f64; 2]>, SpatialFilter>;

pub(crate) enum Matcher {
    Never,
    Single(SpatialFilter),
    Distance(DistanceIndex),
    Overpass(crate::overpass::SpatialMatcher),
    Any {
        filters: RTree<IndexedFilter>,
        disjoint: bool,
    },
}

fn corners(b: Bounds) -> ([f64; 2], [f64; 2]) {
    (
        [b.min.x.into(), b.min.y.into()],
        [b.max.x.into(), b.max.y.into()],
    )
}

pub(crate) fn predicate(name: &str, meters: Option<f64>) -> Result<SpatialPredicate, String> {
    use SpatialPredicate::*;
    Ok(match name {
        "intersects" => Intersects,
        "within" => Within,
        "contains" => Contains,
        "covered_by" => CoveredBy,
        "touches" => Touches,
        "overlaps" => Overlaps,
        "crosses" => Crosses,
        "disjoint" => Disjoint,
        "within_distance" => WithinDistance(Length::new::<meter>(
            meters
                .filter(|m| m.is_finite() && *m >= 0.0)
                .ok_or("Enter a finite, nonnegative distance in metres.")?,
        )),
        _ => return Err(format!("Unknown spatial relationship {name}.")),
    })
}

pub(crate) fn assembled<'g, S: Source>(
    f: Feature<'_, S>,
    geometry: &'g mut Option<Geometry>,
) -> regol::Result<&'g Geometry> {
    if geometry.is_none() {
        *geometry = Some(f.geometry()?);
    }
    Ok(geometry.as_ref().unwrap())
}

impl Matcher {
    pub(crate) fn from_set<S: Source>(
        predicate: SpatialPredicate,
        members: &[Feature<'_, S>],
        skipped: &mut HashSet<FeatureId>,
        mut observe: impl FnMut(&Geometry),
        vertices: &mut usize,
        check: &impl Fn() -> Result<(), String>,
    ) -> Result<Self, String> {
        let mut distance = Vec::new();
        let mut filters = Vec::new();
        let disjoint = predicate == SpatialPredicate::Disjoint;
        let indexed = if disjoint {
            SpatialPredicate::Intersects
        } else {
            predicate
        };
        for &feature in members {
            check()?;
            let Some(g) = usable_geometry(
                feature.id(),
                with_geometry_reads(feature, |f| f.geometry()),
                skipped,
            )?
            else {
                continue;
            };
            *vertices += geometry::vertices(&g);
            if *vertices > MAX_VERTICES {
                return Err(
                    "Reference vertex limit reached. Narrow the set or use a smaller bounding box."
                        .into(),
                );
            }
            if !matches!(predicate, SpatialPredicate::WithinDistance(_)) {
                let Some(filter) =
                    usable_geometry(feature.id(), SpatialFilter::new(indexed, &g), skipped)?
                else {
                    continue;
                };
                if let Some(bounds) = filter.candidate_bounds() {
                    let (min, max) = corners(bounds);
                    filters.push(GeomWithData::new(Rectangle::from_corners(min, max), filter));
                }
            }
            observe(&g);
            if matches!(predicate, SpatialPredicate::WithinDistance(_)) {
                distance.push(g);
            }
        }
        if let SpatialPredicate::WithinDistance(threshold) = predicate {
            return match distance.as_slice() {
                [] => Ok(Self::Never),
                [only] => SpatialFilter::new(predicate, only)
                    .map(Self::Single)
                    .map_err(|e| e.to_string()),
                _ => DistanceIndex::new(distance, threshold, check).map(Self::Distance),
            };
        }
        if filters.is_empty() && !disjoint {
            return Ok(Self::Never);
        }
        Ok(Self::Any {
            filters: RTree::bulk_load(filters),
            disjoint,
        })
    }

    pub(crate) fn matches<S: Source>(
        &self,
        feature: Feature<'_, S>,
        geometry: &mut Option<Geometry>,
        check: &impl Fn() -> Result<(), String>,
    ) -> regol::Result<bool> {
        match self {
            Self::Never => Ok(false),
            Self::Distance(index) => index.matches(assembled(feature, geometry)?),
            Self::Overpass(filter) => filter.matches(feature, check),
            Self::Single(filter) => {
                if let Some(b) = filter.candidate_bounds()
                    && !b.intersects(feature.bounds()?)
                {
                    return Ok(false);
                }
                filter.matches_geometry(assembled(feature, geometry)?)
            }
            Self::Any { filters, disjoint } => {
                let (min, max) = corners(feature.bounds()?);
                for reference in
                    filters.locate_in_envelope_intersecting(&AABB::from_corners(min, max))
                {
                    if reference
                        .data
                        .matches_geometry(assembled(feature, geometry)?)?
                    {
                        return Ok(!disjoint);
                    }
                }
                Ok(*disjoint)
            }
        }
    }
}
