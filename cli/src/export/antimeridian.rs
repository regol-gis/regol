use geo::algorithm::bool_ops::unary_union;
use geo::{
    BooleanOps, BoundingRect, Coord, CoordsIter, Intersects, LineString, MapCoords,
    MapCoordsInPlace, MultiPolygon, Polygon, Rect,
};
use regol::units::angle::degree;
use regol::{Geometry, Limits, Point};
use rustc_hash::FxHashMap;

const SEAM: f64 = 180.0;
const TURN: f64 = 360.0;
const OVERLAY_SCALE: f64 = (1 << 30) as f64;

pub(super) enum CutGeometry<'a> {
    Unchanged(&'a Geometry),
    Lines(Vec<LineString>),
    Polygons { pieces: Vec<Polygon>, multi: bool },
    Collection(Vec<Self>),
}

impl<'a> CutGeometry<'a> {
    pub(super) fn new(geometry: &'a Geometry) -> Result<Self, &'static str> {
        Ok(match geometry {
            Geometry::LineString(points) if crosses(points) => Self::Lines(cut_line(points)),
            Geometry::Polygon(polygon) if polygon_crosses(polygon) => {
                let pieces = cut_polygons(std::slice::from_ref(polygon))?;
                Self::Polygons {
                    multi: pieces.len() != 1,
                    pieces,
                }
            }
            Geometry::MultiPolygon(polygons) if polygons.iter().any(polygon_crosses) => {
                Self::Polygons {
                    pieces: cut_polygons(polygons)?,
                    multi: true,
                }
            }
            Geometry::Collection(children) if children.iter().any(geometry_crosses) => {
                Self::Collection(children.iter().map(Self::new).collect::<Result<_, _>>()?)
            }
            _ => Self::Unchanged(geometry),
        })
    }

    pub(super) fn bounds(&self) -> Option<[f64; 4]> {
        if matches!(self, Self::Unchanged(_)) {
            return None;
        }
        let mut bounds = ComponentBounds::default();
        self.add_bounds(&mut bounds);
        bounds.circular_bounds()
    }

    fn add_bounds(&self, bounds: &mut ComponentBounds) {
        match self {
            Self::Unchanged(geometry) => bounds.add_original(geometry),
            Self::Lines(lines) => {
                for line in lines {
                    bounds.add(line.coords_iter());
                }
            }
            Self::Polygons { pieces, .. } => {
                for polygon in pieces {
                    bounds.add(polygon.coords_iter());
                }
            }
            Self::Collection(children) => {
                for child in children {
                    child.add_bounds(bounds);
                }
            }
        }
    }
}

pub(super) fn degrees(point: Point) -> Coord {
    let position = point.lon_lat();
    Coord {
        x: position.longitude.get::<degree>(),
        y: position.latitude.get::<degree>(),
    }
}

fn crosses(points: &[Point]) -> bool {
    points
        .windows(2)
        .any(|edge| edge[0].x.abs_diff(edge[1].x) > i32::MAX as u32)
}

fn polygon_crosses(polygon: &regol::Polygon) -> bool {
    crosses(&polygon.exterior) || polygon.interiors.iter().any(|ring| crosses(ring))
}

fn geometry_crosses(geometry: &Geometry) -> bool {
    match geometry {
        Geometry::Point(_) => false,
        Geometry::LineString(points) => crosses(points),
        Geometry::Polygon(polygon) => polygon_crosses(polygon),
        Geometry::MultiPolygon(polygons) => polygons.iter().any(polygon_crosses),
        Geometry::Collection(children) => children.iter().any(geometry_crosses),
    }
}

fn longitude_shift(delta: f64) -> f64 {
    if delta > SEAM {
        -TURN
    } else if delta < -SEAM {
        TURN
    } else {
        0.0
    }
}

fn cut_line(points: &[Point]) -> Vec<LineString> {
    let mut points = points.iter().copied().map(cut_degrees);
    let mut previous = points
        .next()
        .expect("crossing line has at least two points");
    let mut current = vec![previous];
    let mut parts = Vec::new();
    for mut next in points {
        let original = next.x;
        next.x += longitude_shift(next.x - previous.x);
        if next.x.abs() > SEAM {
            let seam = SEAM.copysign(next.x);
            let cut = Coord {
                x: seam,
                y: (next.y - previous.y)
                    .mul_add((seam - previous.x) / (next.x - previous.x), previous.y),
            };
            if cut != previous {
                current.push(cut);
            }
            if has_length(&current) {
                parts.push(LineString(std::mem::take(&mut current)));
            } else {
                current.clear();
            }
            current.push(Coord { x: -seam, y: cut.y });
            next.x = original;
        }
        current.push(next);
        previous = next;
    }
    if parts.is_empty() || has_length(&current) {
        parts.push(LineString(current));
    }
    parts
}

fn has_length(points: &[Coord]) -> bool {
    points[1..].iter().any(|point| *point != points[0])
}

fn cut_degrees(point: Point) -> Coord {
    let mut coord = degrees(point);
    // Exactly ±180 degrees in OSM projects to these endpoint cells as well.
    if point.x.unsigned_abs() >= i32::MAX as u32 {
        coord.x = SEAM.copysign(coord.x);
    }
    coord
}

fn unwrap_ring(points: &[Point]) -> Result<(LineString, bool), &'static str> {
    let mut previous = 0.0;
    let mut shift = 0.0;
    let path = points
        .iter()
        .copied()
        .map(cut_degrees)
        .enumerate()
        .map(|(index, coord)| {
            if index != 0 {
                shift += longitude_shift(coord.x - previous);
            }
            previous = coord.x;
            Coord {
                x: coord.x + shift,
                y: coord.y,
            }
        })
        .collect::<Vec<_>>();
    if shift == 0.0 {
        Ok((LineString(path), false))
    } else if shift.abs() == TURN {
        Ok((polar_cap(&path, shift), true))
    } else {
        Err("polygon ring winds around the world more than once")
    }
}

fn polar_cap(path: &[Coord], turn: f64) -> LineString {
    let pole = smaller_pole(path);
    if let Some(aligned) = aligned_polar_cap(path, turn, pole) {
        return aligned;
    }

    // A seam that crosses the shoreline repeatedly cannot serve as the
    // closing meridian. Use a poleward-most vertex, so the added edges
    // leading to the pole do not cross the boundary.
    let body = &path[..path.len() - 1];
    let (index, first) = body
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| (a.y * pole).total_cmp(&(b.y * pole)))
        .expect("polar ring has vertices");
    let mut ring = body[index..].to_vec();
    ring.extend(body[..=index].iter().map(|coord| Coord {
        x: coord.x + turn,
        y: coord.y,
    }));
    ring.extend([
        Coord {
            x: first.x + turn,
            y: pole,
        },
        Coord {
            x: first.x,
            y: pole,
        },
        *first,
    ]);
    LineString(ring)
}

fn aligned_polar_cap(path: &[Coord], turn: f64, pole: f64) -> Option<LineString> {
    let end = SEAM.copysign(turn);
    let start = -end;
    let split = path.windows(2).position(|edge| {
        (end - edge[0].x) * turn >= 0.0
            && (edge[1].x - end) * turn >= 0.0
            && (edge[1].x - edge[0].x) * turn > 0.0
    })?;
    let (a, b) = (path[split], path[split + 1]);
    let lat = if end == a.x {
        a.y
    } else if end == b.x {
        b.y
    } else {
        (b.y - a.y).mul_add((end - a.x) / (b.x - a.x), a.y)
    };
    let first = Coord { x: start, y: lat };
    let mut ring = vec![first];
    for (points, shift) in [
        (&path[split + 1..path.len() - 1], start - end),
        (&path[..=split], 0.0),
    ] {
        for point in points {
            let x = point.x + shift;
            if !(-SEAM..=SEAM).contains(&x) {
                return None;
            }
            let point = Coord { x, y: point.y };
            if Some(&point) != ring.last() {
                ring.push(point);
            }
        }
    }
    let last = Coord { x: end, y: lat };
    if Some(&last) != ring.last() {
        ring.push(last);
    }
    ring.extend([
        Coord { x: end, y: pole },
        Coord { x: start, y: pole },
        first,
    ]);
    Some(LineString(ring))
}

fn smaller_pole(path: &[Coord]) -> f64 {
    // Sum the signed solid angles of triangles from the north pole to each
    // edge. Their unsigned total distinguishes the smaller spherical side.
    let area: f64 = path
        .windows(2)
        .map(|edge| {
            let (a_sin, a_cos) = edge[0].y.to_radians().sin_cos();
            let (b_sin, b_cos) = edge[1].y.to_radians().sin_cos();
            let (sin, cos) = (edge[1].x - edge[0].x).to_radians().sin_cos();
            let scale = a_cos * b_cos;
            2.0 * (scale * sin).atan2((1.0 + a_sin) * (1.0 + b_sin) + scale * cos)
        })
        .sum();
    if area.abs() > std::f64::consts::TAU {
        -90.0
    } else {
        90.0
    }
}

enum Shell {
    Ordinary {
        polygon: Polygon,
        bounds: Rect,
    },
    Polar {
        region: MultiPolygon,
        seam_holes: Vec<Polygon>,
        interior_holes: Vec<LineString>,
    },
}

impl Shell {
    fn fallback_shift(&self, hole_bounds: Rect) -> f64 {
        match self {
            Self::Ordinary { bounds, .. } => {
                ((bounds.center().x - hole_bounds.center().x) / TURN).round() * TURN
            }
            Self::Polar { .. } => 0.0,
        }
    }

    fn hole_shift(&self, first: Coord, hole: Rect) -> Option<f64> {
        match self {
            Self::Ordinary { polygon, bounds } => {
                if bounds.min().y > hole.min().y || bounds.max().y < hole.max().y {
                    return None;
                }
                // A ribbon can pass the same longitude in more than one
                // world copy. Only one has the matching latitude and shell.
                let start = ((bounds.min().x - hole.min().x) / TURN).ceil() as i32;
                let end = ((bounds.max().x - hole.max().x) / TURN).floor() as i32;
                (start..=end)
                    .map(|turn| f64::from(turn) * TURN)
                    .find(|shift| polygon.intersects(&geo::Point::new(first.x + shift, first.y)))
            }
            Self::Polar { region, .. } => region.intersects(&geo::Point(first)).then_some(0.0),
        }
    }
}

fn cut_polygons(polygons: &[regol::Polygon]) -> Result<Vec<Polygon>, &'static str> {
    let mut shells = polygons
        .iter()
        .map(|polygon| {
            let (exterior, polar) = unwrap_ring(&polygon.exterior)?;
            let bounds = exterior.bounding_rect().ok_or("empty exterior ring")?;
            let polygon = Polygon::new(exterior, Vec::new());
            if polar {
                Ok(Shell::Polar {
                    region: wrap_polar(polygon, bounds)?,
                    seam_holes: Vec::new(),
                    interior_holes: Vec::new(),
                })
            } else {
                Ok(Shell::Ordinary { polygon, bounds })
            }
        })
        .collect::<Result<Vec<_>, &str>>()?;

    for (original, polygon) in polygons.iter().enumerate() {
        for ring in &polygon.interiors {
            let (mut hole, polar) = unwrap_ring(ring)?;
            let hole_bounds = hole.bounding_rect().ok_or("empty interior ring")?;
            // Stored Mercator shells can appear to contain a hole on the
            // opposite side of the world. Check ownership after unwrapping.
            let first = cut_degrees(ring[0]);
            let (index, shift) = shells[original]
                .hole_shift(first, hole_bounds)
                .map(|shift| (original, shift))
                .or_else(|| {
                    shells
                        .iter()
                        .enumerate()
                        .filter(|&(index, _)| index != original)
                        .find_map(|(index, shell)| {
                            shell
                                .hole_shift(first, hole_bounds)
                                .map(|shift| (index, shift))
                        })
                })
                .unwrap_or_else(|| (original, shells[original].fallback_shift(hole_bounds)));
            let shell = &mut shells[index];
            match shell {
                Shell::Ordinary { polygon, .. } => {
                    if shift != 0.0 {
                        hole.map_coords_in_place(|coord| Coord {
                            x: coord.x + shift,
                            y: coord.y,
                        });
                    }
                    polygon.interiors_push(hole);
                }
                Shell::Polar {
                    seam_holes,
                    interior_holes,
                    ..
                } => {
                    if polar || hole_bounds.min().x <= -SEAM || hole_bounds.max().x >= SEAM {
                        let polygon = Polygon::new(hole, Vec::new());
                        if polar {
                            seam_holes.extend(wrap_polar(polygon, hole_bounds)?.0);
                        } else {
                            cut_polygon(polygon, hole_bounds, seam_holes)?;
                        }
                    } else {
                        interior_holes.push(hole);
                    }
                }
            }
        }
    }

    let mut pieces = Vec::new();
    for shell in shells {
        match shell {
            Shell::Ordinary { polygon, bounds } => cut_polygon(polygon, bounds, &mut pieces)?,
            Shell::Polar {
                region,
                seam_holes,
                interior_holes,
            } => {
                let mut cap = if seam_holes.is_empty() {
                    region
                } else {
                    region.difference(&MultiPolygon(seam_holes))
                };
                for hole in interior_holes {
                    add_hole(&mut cap.0, hole)?;
                }
                for polygon in &mut cap.0 {
                    polygon.exterior_mut(subdivide_long_edges);
                    polygon.interiors_mut(|holes| holes.iter_mut().for_each(subdivide_long_edges));
                }
                pieces.extend(cap.0);
            }
        }
    }
    Ok(pieces)
}

fn wrap_polar(polygon: Polygon, bounds: Rect) -> Result<MultiPolygon, &'static str> {
    let mut pieces = Vec::new();
    cut_polygon(polygon, bounds, &mut pieces)?;
    if pieces.len() > 1 {
        // The closing meridian is only an artifact of opening the ring.
        // Dissolve its shared edges after restoring geographic longitudes.
        Ok(unary_union(&pieces))
    } else {
        Ok(MultiPolygon(pieces))
    }
}

fn add_hole(pieces: &mut [Polygon], hole: LineString) -> Result<(), &'static str> {
    let first = geo::Point(hole.0[0]);
    let owner = pieces
        .iter_mut()
        .find(|piece| piece.intersects(&first))
        .ok_or("cut polygon does not contain its interior ring")?;
    owner.interiors_push(hole);
    Ok(())
}

fn subdivide_long_edges(ring: &mut LineString) {
    if ring
        .lines()
        .all(|line| (line.end.x - line.start.x).abs() <= SEAM)
    {
        return;
    }
    let mut points = vec![ring.0[0]];
    for line in ring.lines() {
        if (line.end.x - line.start.x).abs() > SEAM {
            points.push(Coord {
                x: (line.start.x + line.end.x) / 2.0,
                y: (line.start.y + line.end.y) / 2.0,
            });
        }
        points.push(line.end);
    }
    *ring = LineString(points);
}

fn cut_polygon(
    mut polygon: Polygon,
    bounds: Rect,
    pieces: &mut Vec<Polygon>,
) -> Result<(), &'static str> {
    let min = bounds.min();
    let max = bounds.max();
    let width = max.x - min.x;
    let height = max.y - min.y;
    let first = ((min.x + SEAM) / TURN).floor() as i32;
    let last = ((max.x + SEAM) / TURN).ceil() as i32 - 1;
    if first >= last {
        let strip = f64::from(first) * TURN;
        polygon.map_coords_in_place(|coord| Coord {
            x: (coord.x - strip).clamp(-SEAM, SEAM),
            y: coord.y,
        });
        pieces.push(polygon);
        return Ok(());
    }
    if height == 0.0 {
        return Ok(());
    }
    let strips = u64::from(first.abs_diff(last) + 1);
    if strips.saturating_mul(polygon.coords_count() as u64) > Limits::default().max_traversal_steps
    {
        return Err("antimeridian clipping exceeds the geometry traversal budget");
    }

    let (exterior, interiors) = polygon.into_inner();
    let mut cut_holes = Vec::new();
    let mut intact_holes = Vec::new();
    for hole in interiors {
        let bounds = hole.bounding_rect().ok_or("empty interior ring")?;
        let strip = ((bounds.min().x + SEAM) / TURN).floor() * TURN;
        if bounds.min().x > strip - SEAM && bounds.max().x < strip + SEAM {
            intact_holes.push((hole, strip));
        } else {
            cut_holes.push(hole);
        }
    }
    let polygon = Polygon::new(exterior, cut_holes);
    let first_piece = pieces.len();
    // Scale both axes into the overlay's integer grid so narrow polygons
    // retain longitude precision even when the latitude span is large.
    let unit = polygon.map_coords(|coord| Coord {
        x: (coord.x - min.x) / width,
        y: (coord.y - min.y) / height,
    });
    let grid_key = |coord: Coord| {
        let index = |value: f64| ((value - 0.5) * OVERLAY_SCALE).round() as i32;
        (index(coord.x), index(coord.y))
    };
    let originals: FxHashMap<_, _> = unit
        .coords_iter()
        .map(grid_key)
        .zip(polygon.coords_iter())
        .collect();
    for index in first..=last {
        let strip = f64::from(index) * TURN;
        let left = (strip - SEAM - min.x) / width;
        let right = (strip + SEAM - min.x) / width;
        let limit = Rect::new((left.max(0.0), 0.0), (right.min(1.0), 1.0)).to_polygon();
        let cut = unit.intersection(&limit);
        for mut piece in cut.0 {
            piece.map_coords_in_place(|coord| {
                // The overlay rounds to a grid of 2^-30 on the unit square.
                // Restore an exact ±180 at vertices on the cut.
                let epsilon = 2.0 / OVERLAY_SCALE;
                let original = originals.get(&grid_key(coord));
                let lon = if (coord.x - left).abs() < epsilon {
                    -SEAM
                } else if (coord.x - right).abs() < epsilon {
                    SEAM
                } else if let Some(original) = original {
                    (original.x - strip).clamp(-SEAM, SEAM)
                } else {
                    coord.x.mul_add(width, min.x - strip).clamp(-SEAM, SEAM)
                };
                let lat = original.map_or_else(
                    || coord.y.clamp(0.0, 1.0).mul_add(height, min.y),
                    |original| original.y,
                );
                Coord { x: lon, y: lat }
            });
            pieces.push(piece);
        }
    }
    // Interior rings on one side need no overlay. This also retains holes
    // that would fit between the grid cells of a much larger shell.
    for (mut hole, strip) in intact_holes {
        if strip != 0.0 {
            hole.map_coords_in_place(|coord| Coord {
                x: coord.x - strip,
                y: coord.y,
            });
        }
        add_hole(&mut pieces[first_piece..], hole)?;
    }
    Ok(())
}

#[derive(Default)]
struct ComponentBounds {
    longitudes: Vec<(f64, f64)>,
    latitude: Option<(f64, f64)>,
}

impl ComponentBounds {
    fn add(&mut self, coords: impl IntoIterator<Item = Coord>) {
        let mut coords = coords.into_iter();
        let Some(first) = coords.next() else {
            return;
        };
        let (mut west, mut east) = (first.x, first.x);
        let (mut south, mut north) = self.latitude.unwrap_or((first.y, first.y));
        south = south.min(first.y);
        north = north.max(first.y);
        for coord in coords {
            west = west.min(coord.x);
            east = east.max(coord.x);
            south = south.min(coord.y);
            north = north.max(coord.y);
        }
        self.longitudes.push((west, east));
        self.latitude = Some((south, north));
    }

    fn add_original(&mut self, geometry: &Geometry) {
        match geometry {
            Geometry::Point(point) => self.add([degrees(*point)]),
            Geometry::LineString(points) => self.add(points.iter().copied().map(degrees)),
            Geometry::Polygon(polygon) => self.add(polygon_coords(polygon)),
            Geometry::MultiPolygon(polygons) => {
                for polygon in polygons {
                    self.add(polygon_coords(polygon));
                }
            }
            Geometry::Collection(children) => {
                for child in children {
                    self.add_original(child);
                }
            }
        }
    }

    fn circular_bounds(mut self) -> Option<[f64; 4]> {
        let (south, north) = self.latitude?;
        self.longitudes.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        let (&(first, mut east), remaining) = self.longitudes.split_first()?;
        let mut largest_gap = (0.0, 0.0, 0.0);
        for &(west, next_east) in remaining {
            if west - east > largest_gap.0 {
                largest_gap = (west - east, west, east);
            }
            east = east.max(next_east);
        }
        // Cover entire connected components, including their edges.
        if first + TURN - east >= largest_gap.0 {
            Some([first, south, east, north])
        } else {
            Some([largest_gap.1, south, largest_gap.2, north])
        }
    }
}

fn polygon_coords(polygon: &regol::Polygon) -> impl Iterator<Item = Coord> + '_ {
    polygon
        .exterior
        .iter()
        .chain(polygon.interiors.iter().flatten())
        .copied()
        .map(degrees)
}
