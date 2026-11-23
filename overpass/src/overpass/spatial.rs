mod around;
mod polar;
mod ring_topology;
#[cfg(test)]
mod tests;

use crate::{MAX_VERTICES, geometry, usable_geometry, with_geometry_reads};
use geo::{
    Area as _, BoundingRect, Contains, Intersects, Relate, Translate,
    coordinate_position::CoordPos, dimensions::Dimensions,
};
use regol::{
    Bounds, Feature, FeatureId, FeatureType, Geometry, Point, Polygon, SpatialFilter,
    SpatialPredicate, source::Source,
};
use rstar::{
    AABB, RTree,
    primitives::{GeomWithData, Rectangle},
};
use std::{
    collections::{BTreeMap, HashSet},
    ops::{ControlFlow, RangeInclusive},
};

pub(super) use around::Around;
pub(crate) use around::{NodePosition, NodeRadius};

const MAX_SEARCH_WINDOWS: usize = 1024;

pub(crate) enum Filter {
    Area(Areas),
    Around(Around),
    MapToArea,
}

impl Filter {
    pub(crate) fn matches<S: Source>(
        &self,
        f: Feature<'_, S>,
        check: &impl Fn() -> Result<(), String>,
    ) -> regol::Result<bool> {
        match self {
            Self::Area(area) => area.matches(f, check),
            Self::Around(around) => around.matches(f, check),
            Self::MapToArea => Ok(match f.kind() {
                FeatureType::Node => false,
                FeatureType::Relation => {
                    f.is_area()
                        && matches!(
                            f.geometry()?,
                            Geometry::Polygon(_) | Geometry::MultiPolygon(_)
                        )
                }
                FeatureType::Way => closed_way(f, check)?.is_some(),
            }),
        }
    }

    pub(crate) fn windows(
        &self,
        check: &impl Fn() -> Result<(), String>,
    ) -> Result<Vec<Bounds>, String> {
        match self {
            Self::Area(areas) => areas.windows(check),
            Self::Around(around) => around.windows(check),
            Self::MapToArea => unreachable!("map_to_area consumes a bounded input set"),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        match self {
            Self::Area(areas) => areas.index.size() == 0,
            Self::Around(around) => around.is_empty(),
            Self::MapToArea => false,
        }
    }
}

fn reference_vertices(vertices: &mut usize, count: usize) -> Result<(), String> {
    *vertices += count;
    if *vertices > MAX_VERTICES {
        return Err("Reference vertex limit reached. Narrow the set or its bounds.".into());
    }
    Ok(())
}

fn closed_way<S: Source>(
    way: Feature<'_, S>,
    check: &impl Fn() -> Result<(), String>,
) -> regol::Result<Option<Geometry>> {
    let coordinates = way.coordinates()?.collect::<regol::Result<Vec<_>>>()?;
    if coordinates.len() < 4 || coordinates.first() != coordinates.last() {
        return Ok(None);
    }
    if !way.is_area() {
        let mut nodes = way.way_nodes()?;
        let first = nodes.next().transpose()?.and_then(|node| node.id);
        if let Some(first) = first {
            let mut last = None;
            for (i, node) in nodes.enumerate() {
                if i % 256 == 0 {
                    check().map_err(std::io::Error::other)?;
                }
                last = node?.id;
            }
            if last.is_some_and(|last| last != first) {
                return Ok(None);
            }
        }
    }
    Ok(Some(Geometry::Polygon(Polygon {
        exterior: coordinates,
        interiors: Vec::new(),
    })))
}

// Overpass considers only direct node and way members for these spatial tests.
// A way is always an outline, even when the library renders it as a polygon.
fn visit_paths<S: Source, B>(
    feature: Feature<'_, S>,
    mut visit: impl FnMut(Geometry) -> regol::Result<ControlFlow<B>>,
) -> regol::Result<ControlFlow<B>> {
    let path = |feature: Feature<'_, S>| -> regol::Result<Option<Geometry>> {
        match feature.kind() {
            FeatureType::Node => Ok(feature.point()?.map(Geometry::Point)),
            FeatureType::Way => Ok(Some(Geometry::LineString(
                feature.coordinates()?.collect::<regol::Result<_>>()?,
            ))),
            FeatureType::Relation => Ok(None),
        }
    };
    if feature.kind() == FeatureType::Relation {
        for member in feature.members()? {
            if let Some(geometry) = path(member?.feature)?
                && let ControlFlow::Break(value) = visit(geometry)?
            {
                return Ok(ControlFlow::Break(value));
            }
        }
        return Ok(ControlFlow::Continue(()));
    }
    match path(feature)? {
        Some(geometry) => visit(geometry),
        None => Ok(ControlFlow::Continue(())),
    }
}

// Both the GOL builder and the query projection round ±180° to ±i32::MAX.
// Their difference is the period for exact planar topology, so the two
// encodings coincide at the seam.
const WORLD_WIDTH: i64 = i32::MAX as i64 * 2;

fn longitude_delta(a: Point, b: Point) -> i64 {
    let delta = i64::from(b.x) - i64::from(a.x);
    if delta.abs() > WORLD_WIDTH / 2 {
        delta - delta.signum() * WORLD_WIDTH
    } else {
        delta
    }
}

fn crosses_dateline(a: Point, b: Point) -> bool {
    longitude_delta(a, b) != i64::from(b.x) - i64::from(a.x)
}

// Integer world shifts whose translated interval intersects [low, high].
fn world_copies(min: i64, max: i64, low: i64, high: i64) -> RangeInclusive<i64> {
    -(max - low).div_euclid(WORLD_WIDTH)..=(high - min).div_euclid(WORLD_WIDTH)
}

fn longitude_windows(min: i64, max: i64, south: i32, north: i32) -> impl Iterator<Item = Bounds> {
    let low = i64::from(i32::MIN);
    let high = i64::from(i32::MAX);
    world_copies(min, max, low, high).map(move |copy| {
        let shift = copy * WORLD_WIDTH;
        Bounds {
            min: Point {
                x: (min + shift).max(low) as i32,
                y: south,
            },
            max: Point {
                x: (max + shift).min(high) as i32,
                y: north,
            },
        }
    })
}

struct Ring {
    positions: Vec<(i64, i32)>,
    min: i64,
    max: i64,
}

impl Ring {
    fn new(points: &[Point]) -> regol::Result<Self> {
        let [first, ..] = points else {
            return Err(regol::Error::Geometry("empty area ring"));
        };
        let mut x = i64::from(first.x);
        let mut positions = Vec::with_capacity(points.len());
        positions.push((x, first.y));
        for pair in points.windows(2) {
            x += longitude_delta(pair[0], pair[1]);
            positions.push((x, pair[1].y));
        }
        if positions.len() < 4 || positions[0].1 != positions[positions.len() - 1].1 {
            return Err(regol::Error::Geometry("invalid polygon ring"));
        }
        if (positions[positions.len() - 1].0 - positions[0].0).abs() > WORLD_WIDTH {
            return Err(regol::Error::Geometry(
                "polygon ring does not close in one longitude frame",
            ));
        }
        let min = positions.iter().map(|p| p.0).min().unwrap();
        let max = positions.iter().map(|p| p.0).max().unwrap();
        Ok(Self {
            positions,
            min,
            max,
        })
    }

    fn winding(&self) -> i64 {
        (self.positions.last().unwrap().0 - self.positions[0].0) / WORLD_WIDTH
    }

    fn center(&self) -> i64 {
        self.min + (self.max - self.min) / 2
    }

    fn aligned_offset(&self, center: i64) -> i64 {
        (center - self.center() + WORLD_WIDTH / 2).div_euclid(WORLD_WIDTH) * WORLD_WIDTH - center
    }

    fn geo_line(&self, offset: i64) -> geo::LineString {
        let mut coords: Vec<_> = self
            .positions
            .iter()
            .map(|&(x, y)| geo::Coord {
                x: (x + offset) as f64,
                y: y.into(),
            })
            .collect();
        coords.dedup();
        geo::LineString(coords)
    }

    fn project(&self, center: i64) -> regol::Result<Vec<Point>> {
        if self.winding() != 0 {
            return Err(regol::Error::Geometry(
                "a polar ring cannot bound a nonpolar hole",
            ));
        }
        // A contained hole must have its midpoint within half a world of the shell.
        let offset = self.aligned_offset(center);
        self.positions
            .iter()
            .map(|&(x, y)| {
                Ok(Point {
                    x: i32::try_from(x + offset)
                        .map_err(|_| regol::Error::Geometry("hole lies outside its shell"))?,
                    y,
                })
            })
            .collect()
    }
}

fn geo_polygon(polygon: &Polygon) -> geo::Polygon {
    let ring = |points: &[Point]| {
        let mut positions = points
            .iter()
            .map(|point| geo::Coord {
                x: point.x.into(),
                y: point.y.into(),
            })
            .collect::<Vec<_>>();
        positions.dedup();
        geo::LineString(positions)
    };
    geo::Polygon::new(
        ring(&polygon.exterior),
        polygon.interiors.iter().map(|p| ring(p)).collect(),
    )
}

struct Area {
    center: i64,
    bounds: Bounds,
    longitude: Option<(i64, i64)>,
    polygon: geo::Polygon,
    filters: Option<(SpatialFilter, SpatialFilter)>,
}

impl Area {
    fn new(polygon: Polygon) -> regol::Result<Self> {
        let exterior = Ring::new(&polygon.exterior)?;
        let holes = polygon
            .interiors
            .iter()
            .map(|ring| Ring::new(ring))
            .collect::<regol::Result<Vec<_>>>()?;
        Self::from_rings(exterior, holes)
    }

    fn from_rings(exterior: Ring, holes: Vec<Ring>) -> regol::Result<Self> {
        if exterior.winding() != 0 {
            return polar::prepare(exterior, holes);
        }
        if exterior.max - exterior.min >= WORLD_WIDTH {
            return Self::from_wide_ring(exterior, holes, None);
        }
        let center = exterior.center();
        let longitude = Some((exterior.min, exterior.max));
        let projected = Polygon {
            exterior: exterior.project(center)?,
            interiors: holes
                .iter()
                .map(|ring| ring.project(center))
                .collect::<regol::Result<_>>()?,
        };
        let polygon = geo_polygon(&projected);
        let geometry = Geometry::Polygon(projected);
        let touches = SpatialFilter::new(SpatialPredicate::Touches, &geometry)?;
        let intersects = SpatialFilter::new(SpatialPredicate::Intersects, &geometry)?;
        let bounds = intersects
            .candidate_bounds()
            .ok_or(regol::Error::Geometry("empty area"))?;
        Ok(Self {
            center,
            bounds,
            longitude,
            polygon,
            filters: Some((intersects, touches)),
        })
    }

    fn from_wide_ring(
        exterior: Ring,
        holes: Vec<Ring>,
        pinch: Option<(i64, i32)>,
    ) -> regol::Result<Self> {
        let center = exterior.center();
        let longitude = Some((exterior.min, exterior.max));
        let copies = (exterior.max - exterior.min) / WORLD_WIDTH;
        let vertices =
            exterior.positions.len() + holes.iter().map(|hole| hole.positions.len()).sum::<usize>();
        if vertices > MAX_VERTICES / (copies + 1) as usize {
            return Err(regol::Error::Geometry(
                "wide polygon exceeds the reference vertex limit",
            ));
        }
        let shell = geo::Polygon::new(exterior.geo_line(-center), Vec::new());
        let interiors = holes
            .iter()
            .map(|hole| {
                if hole.winding() != 0 {
                    return Err(regol::Error::Geometry(
                        "a winding hole requires a polar shell",
                    ));
                }
                let lines = contained_hole(hole, center, exterior.min, exterior.max, &shell);
                lines.ok_or(regol::Error::Geometry(
                    "polygon hole lies outside its shell",
                ))
            })
            .collect::<regol::Result<Vec<_>>>()?;
        ring_topology::validate_rings(shell.exterior(), &interiors)?;
        if shell.unsigned_area() == 0.0 {
            return Err(regol::Error::Geometry("polygon ring has zero signed area"));
        }
        // Overlapping world copies would make the cylindrical shell ambiguous.
        let copies_valid = (1..=copies).all(|copy| {
            let translated = shell.translate((copy * WORLD_WIDTH) as f64, 0.0);
            let relation = shell.relate(&translated);
            relation.is_disjoint()
                || (copy == 1
                    && relation.is_touches()
                    && relation.get(CoordPos::OnBoundary, CoordPos::OnBoundary)
                        == Dimensions::ZeroDimensional
                    && pinch.is_some_and(|(x, y)| {
                        let contact = geo::Coord {
                            x: (x + WORLD_WIDTH - center) as f64,
                            y: y.into(),
                        };
                        ring_topology::touches_only_at(
                            shell.exterior(),
                            translated.exterior(),
                            contact,
                        )
                    }))
        });
        if !copies_valid {
            return Err(regol::Error::Geometry(
                "polygon ring intersects its wrapped copy",
            ));
        }
        let extent = shell.bounding_rect().unwrap();
        let bounds = Bounds {
            min: Point {
                x: i32::MIN,
                y: extent.min().y as i32,
            },
            max: Point {
                x: i32::MAX,
                y: extent.max().y as i32,
            },
        };
        Ok(Self {
            center,
            bounds,
            longitude,
            polygon: geo::Polygon::new(shell.into_inner().0, interiors),
            filters: None,
        })
    }

    fn windows(&self) -> impl Iterator<Item = Bounds> {
        let narrow = self.longitude.filter(|&(min, max)| max - min < WORLD_WIDTH);
        narrow
            .is_none()
            .then_some(self.bounds)
            .into_iter()
            .chain(narrow.into_iter().flat_map(|(min, max)| {
                longitude_windows(min, max, self.bounds.min.y, self.bounds.max.y)
            }))
    }

    fn matches(
        &self,
        path: &[Point],
        bounds: Bounds,
        last_shift: i64,
        include_boundary: bool,
    ) -> regol::Result<bool> {
        let (min, max) = path_longitudes(path, bounds, last_shift);
        let copies = if let Some((left, right)) = self.longitude {
            world_copies(min, max, left, right)
        } else {
            let copy = (self.center - (min + max) / 2 + WORLD_WIDTH / 2).div_euclid(WORLD_WIDTH);
            copy..=copy
        };
        for copy in copies {
            let shift = copy * WORLD_WIDTH - self.center;
            let positions = || {
                path.iter().enumerate().map(|(index, point)| {
                    let extra = if index == path.len() - 1 {
                        last_shift
                    } else {
                        0
                    };
                    (i64::from(point.x) + shift + extra, point.y)
                })
            };
            let point = min == max && bounds.min.y == bounds.max.y;
            if let Some((intersects, touches)) = &self.filters
                && min + shift >= i64::from(i32::MIN)
                && max + shift <= i64::from(i32::MAX)
            {
                let mut points = positions().map(|(x, y)| Point { x: x as i32, y });
                let candidate = if point {
                    Geometry::Point(points.next().unwrap())
                } else {
                    Geometry::LineString(points.collect())
                };
                if intersects.matches_geometry(&candidate)?
                    && (include_boundary || !touches.matches_geometry(&candidate)?)
                {
                    return Ok(true);
                }
            } else {
                // A long path can reach outside the centered i32 frame. Keep the
                // original coordinates instead of clipping at a rounded seam.
                let mut points = positions().map(|(x, y)| geo::Coord {
                    x: x as f64,
                    y: y.into(),
                });
                let matched = if point {
                    let candidate = geo::Point(points.next().unwrap());
                    if include_boundary {
                        self.polygon.intersects(&candidate)
                    } else {
                        self.polygon.contains(&candidate)
                    }
                } else {
                    let candidate = geo::LineString(points.collect());
                    self.polygon.intersects(&candidate)
                        && (include_boundary || !self.polygon.relate(&candidate).is_touches())
                };
                if matched {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

fn path_longitudes(path: &[Point], bounds: Bounds, last_shift: i64) -> (i64, i64) {
    if last_shift == 0 {
        (i64::from(bounds.min.x), i64::from(bounds.max.x))
    } else {
        let start = i64::from(path[0].x);
        let end = i64::from(path[1].x) + last_shift;
        (start.min(end), start.max(end))
    }
}

fn contained_hole(
    hole: &Ring,
    frame: i64,
    left: i64,
    right: i64,
    shell: &geo::Polygon,
) -> Option<geo::LineString> {
    world_copies(hole.max, hole.min, left, right).find_map(|copy| {
        let line = hole.geo_line(copy * WORLD_WIDTH - frame);
        ring_topology::contains_ring(shell, &line).then_some(line)
    })
}

fn prepare_areas(polygons: Vec<Polygon>) -> regol::Result<Vec<Area>> {
    let ring_wraps = |ring: &[Point]| {
        ring.windows(2)
            .any(|pair| crosses_dateline(pair[0], pair[1]))
    };
    if polygons.len() <= 1
        || !polygons
            .iter()
            .any(|p| ring_wraps(&p.exterior) || p.interiors.iter().any(|ring| ring_wraps(ring)))
        || polygons.iter().all(|p| p.interiors.is_empty())
    {
        return polygons.into_iter().map(Area::new).collect();
    }

    // The engine assembles OSM ways into rings before assigning holes in the
    // flat world. Reattach those inner rings in the same frame as each shell.
    let mut shells = polygons
        .iter()
        .map(|polygon| {
            let ring = Ring::new(&polygon.exterior)?;
            let (frame, bounds, geometry, area) = if ring.winding() == 0 {
                let frame = ring.center();
                let geometry = geo::Polygon::new(ring.geo_line(-frame), Vec::new());
                let bounds = Some((ring.min, ring.max));
                let area = geometry.unsigned_area();
                (frame, bounds, geometry, area)
            } else {
                let cap = polar::prepare(Ring::new(&polygon.exterior)?, Vec::new())?;
                let area = polar::comparison_area(&cap);
                (cap.center, None, cap.polygon, area)
            };
            Ok((ring, frame, bounds, geometry, area, Vec::new()))
        })
        .collect::<regol::Result<Vec<_>>>()?;
    for interior in polygons.into_iter().flat_map(|p| p.interiors) {
        let hole = Ring::new(&interior)?;
        let mut best = None;
        for (index, (_, frame, bounds, polygon, area, _)) in shells.iter().enumerate() {
            if best.is_some_and(|(_, smallest)| *area >= smallest) {
                continue;
            }
            let offset = hole.aligned_offset(*frame);
            let contained = if hole.winding() != 0 {
                let (x, y) = hole.positions[0];
                let local = (x - frame + WORLD_WIDTH / 2).rem_euclid(WORLD_WIDTH) - WORLD_WIDTH / 2;
                // A winding hole only fits a polar shell. A boundary vertex
                // identifies the owning component; the assembled band is
                // validated against the entire inner ring.
                bounds.is_none() && polygon.intersects(&geo::Point::new(local as f64, f64::from(y)))
            } else if let Some((min, max)) = bounds {
                contained_hole(&hole, *frame, *min, *max, polygon).is_some()
            } else {
                ring_topology::contains_ring(polygon, &hole.geo_line(offset))
            };
            if contained {
                best = Some((index, *area));
            }
        }
        let (index, _) = best.ok_or(regol::Error::Geometry(
            "polygon hole lies outside all shells",
        ))?;
        shells[index].5.push(hole);
    }
    shells
        .into_iter()
        .map(|(shell, _, _, _, _, holes)| Area::from_rings(shell, holes))
        .collect()
}

type IndexedArea = GeomWithData<Rectangle<[f64; 2]>, (usize, Bounds)>;

pub(crate) struct Areas {
    areas: Vec<Area>,
    index: RTree<IndexedArea>,
    polygon_filter: bool,
}

fn area_envelope(bounds: Bounds) -> AABB<[f64; 2]> {
    AABB::from_corners(
        [bounds.min.x.into(), bounds.min.y.into()],
        [bounds.max.x.into(), bounds.max.y.into()],
    )
}

impl Areas {
    fn new(areas: Vec<Area>, polygon_filter: bool) -> Self {
        let index = RTree::bulk_load(
            areas
                .iter()
                .enumerate()
                .flat_map(|(index, area)| {
                    area.windows().map(move |bounds| {
                        GeomWithData::new(
                            Rectangle::from_aabb(area_envelope(bounds)),
                            (index, bounds),
                        )
                    })
                })
                .collect(),
        );
        Self {
            areas,
            index,
            polygon_filter,
        }
    }

    pub(super) fn polygon(vertices: &[[f64; 2]]) -> Result<Self, String> {
        if !(3..=10_000).contains(&vertices.len()) {
            return Err("A polygon needs 3 to 10,000 latitude and longitude pairs.".into());
        }
        let mut exterior = vertices
            .iter()
            .map(|&point| geometry::geographic_point(point).map(|point| point.project()))
            .collect::<Result<Vec<_>, _>>()?;
        if exterior.first() != exterior.last() {
            exterior.push(exterior[0]);
        }
        Area::new(Polygon {
            exterior,
            interiors: Vec::new(),
        })
        .map(|area| Self::new(vec![area], true))
        .map_err(|e| e.to_string())
    }

    pub(super) fn from_set<S: Source>(
        members: &[Feature<'_, S>],
        mapped: bool,
        skipped: &mut HashSet<FeatureId>,
        vertices: &mut usize,
        check: &impl Fn() -> Result<(), String>,
    ) -> Result<Self, String> {
        let mut areas = Vec::new();
        for &feature in members {
            check()?;
            let read = with_geometry_reads(feature, |feature| match feature.kind() {
                FeatureType::Way => closed_way(feature, check),
                FeatureType::Relation if mapped && feature.is_area() => {
                    feature.geometry().map(Some)
                }
                _ => Ok(None),
            });
            let Some(Some(geometry)) = usable_geometry(feature.id(), read, skipped)? else {
                continue;
            };
            reference_vertices(vertices, geometry::vertices(&geometry))?;
            let polygons = match geometry {
                Geometry::Polygon(polygon) => vec![polygon],
                Geometry::MultiPolygon(polygons) => polygons,
                _ => Vec::new(),
            };
            let prepared = prepare_areas(polygons);
            if let Some(prepared) = usable_geometry(feature.id(), prepared, skipped)? {
                areas.extend(prepared);
            }
        }
        Ok(Self::new(areas, false))
    }

    fn windows(&self, check: &impl Fn() -> Result<(), String>) -> Result<Vec<Bounds>, String> {
        let mut local = BTreeMap::<(i32, i32), Bounds>::new();
        let mut large = HashSet::new();
        let mut individual = Vec::new();
        for item in &self.index {
            check()?;
            let (_, bounds) = item.data;
            // Merge neighboring small areas into one index traversal. Keep a
            // large component separate so it cannot link unrelated regions.
            let cell = |p: regol::Point| (p.x >> 21, p.y >> 21);
            if cell(bounds.min) == cell(bounds.max) {
                local
                    .entry(cell(bounds.min))
                    .and_modify(|window| {
                        window.min.x = window.min.x.min(bounds.min.x);
                        window.min.y = window.min.y.min(bounds.min.y);
                        window.max.x = window.max.x.max(bounds.max.x);
                        window.max.y = window.max.y.max(bounds.max.y);
                    })
                    .or_insert(bounds);
            } else if large.insert((bounds.min, bounds.max)) {
                individual.push(bounds);
            }
            if local.len() + individual.len() > MAX_SEARCH_WINDOWS {
                return Err(
                    "Area references cover too many search windows. Add a bounding box.".into(),
                );
            }
        }
        individual.extend(local.into_values());
        Ok(individual)
    }

    fn matches<S: Source>(
        &self,
        feature: Feature<'_, S>,
        check: &impl Fn() -> Result<(), String>,
    ) -> regol::Result<bool> {
        let bounds = feature.bounds()?;
        let candidates = self
            .index
            .locate_in_envelope_intersecting(&area_envelope(bounds));
        if !crosses_dateline(bounds.min, bounds.max) && candidates.into_iter().next().is_none() {
            return Ok(false);
        }
        let include_boundary = feature.kind() == FeatureType::Node
            || (self.polygon_filter && feature.kind() == FeatureType::Way);
        let matched = visit_paths(feature, |geometry| {
            let points = match &geometry {
                Geometry::Point(point) => std::slice::from_ref(point),
                Geometry::LineString(points) => points.as_slice(),
                _ => unreachable!("a path has only points and lines"),
            };
            self.matches_path(points, include_boundary, check)
                .map(|yes| {
                    if yes {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                })
        })?;
        Ok(matched.is_break())
    }

    fn matches_path(
        &self,
        points: &[Point],
        include_boundary: bool,
        check: &impl Fn() -> Result<(), String>,
    ) -> regol::Result<bool> {
        let test = |path: &[Point], last_shift| {
            let Some(bounds) = path_bounds(path) else {
                return Ok(false);
            };
            let (min, max) = path_longitudes(path, bounds, last_shift);
            for window in longitude_windows(min, max, bounds.min.y, bounds.max.y) {
                for item in self
                    .index
                    .locate_in_envelope_intersecting(&area_envelope(window))
                {
                    check().map_err(std::io::Error::other)?;
                    if self.areas[item.data.0].matches(
                        path,
                        bounds,
                        last_shift,
                        include_boundary,
                    )? {
                        return Ok(true);
                    }
                }
            }
            Ok::<_, regol::Error>(false)
        };
        let mut start = 0;
        for (index, pair) in points.windows(2).enumerate() {
            if index % 256 == 0 {
                check().map_err(std::io::Error::other)?;
            }
            let shift =
                longitude_delta(pair[0], pair[1]) - (i64::from(pair[1].x) - i64::from(pair[0].x));
            if shift != 0 {
                if (index > start && test(&points[start..=index], 0)?) || test(pair, shift)? {
                    return Ok(true);
                }
                start = index + 1;
            }
        }
        if start == 0 || points.len() - start > 1 {
            return test(&points[start..], 0);
        }
        Ok(false)
    }
}

fn path_bounds(points: &[Point]) -> Option<Bounds> {
    let first = *points.first()?;
    Some(points[1..].iter().fold(
        Bounds {
            min: first,
            max: first,
        },
        |mut bounds, point| {
            bounds.min.x = bounds.min.x.min(point.x);
            bounds.min.y = bounds.min.y.min(point.y);
            bounds.max.x = bounds.max.x.max(point.x);
            bounds.max.y = bounds.max.y.max(point.y);
            bounds
        },
    ))
}
