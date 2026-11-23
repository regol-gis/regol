use super::ring_topology::{contains_ring, edge_envelope, edge_index, validate_rings};
use super::{Area, Bounds, Point, Ring, WORLD_WIDTH, world_copies};
use crate::MAX_VERTICES;
use geo::{
    Area as _, BoundingRect,
    line_intersection::{LineIntersection, line_intersection},
};
use regol::{Error, Result};
use std::f64::consts::TAU;

const MIN_REPEATS: i64 = 5;

pub(super) fn prepare(mut shell: Ring, mut holes: Vec<Ring>) -> Result<Area> {
    if shell.winding() < 0 {
        shell.positions.reverse();
    }
    let north = smaller_side(&shell.positions)?;
    let limit = if north { i32::MAX } else { i32::MIN };
    let shell_cut = polar_cut(&shell.positions, north);
    let mut south = shell.positions.iter().map(|p| p.1).min().unwrap();
    let mut top = shell.positions.iter().map(|p| p.1).max().unwrap();
    let (outer, inner) = if let Some(index) = holes.iter().position(|hole| hole.winding() != 0) {
        let mut hole = holes.remove(index);
        if holes.iter().any(|hole| hole.winding() != 0) {
            return Err(Error::Geometry("polar shell has overlapping winding holes"));
        }
        if hole.winding() < 0 {
            hole.positions.reverse();
        }
        if smaller_side(&hole.positions)? != north {
            return Err(Error::Geometry("winding hole encloses the opposite pole"));
        }
        for &(_, y) in &hole.positions {
            south = south.min(y);
            top = top.max(y);
        }
        let (outer, inner) = band_cuts(
            &shell.positions,
            &hole.positions,
            shell_cut.x.floor() as i64,
            north,
        )?;
        if let Some(pinch) = pinched_band(&shell, &hole)? {
            let contact = pinch.positions[0];
            return Area::from_wide_ring(pinch, holes, Some(contact));
        }
        (outer, Some(inner))
    } else {
        if north {
            top = limit;
        } else {
            south = limit;
        }
        (shell_cut, None)
    };
    let extra = std::iter::once(&outer)
        .chain(inner.iter())
        .map(CutRing::extra_width)
        .max()
        .unwrap();
    let repeats = MIN_REPEATS + 2 * (extra + WORLD_WIDTH - 1).div_euclid(WORLD_WIDTH);
    let vertex_count = outer.vertices.len()
        + inner.as_ref().map_or(0, |ring| ring.vertices.len())
        + holes.iter().map(|hole| hole.positions.len()).sum::<usize>();
    if vertex_count > MAX_VERTICES / repeats as usize {
        return Err(Error::Geometry(
            "polar polygon exceeds the reference vertex limit",
        ));
    }
    let start = outer.x.floor() as i64;
    let center = start + repeats * WORLD_WIDTH / 2;
    let mut exterior = outer.repeated(repeats, center);
    if let Some(inner) = inner {
        exterior.extend(inner.repeated(repeats, center).into_iter().rev());
    } else {
        // Close outside the latitude and longitude range any candidate uses.
        let pole = f64::from(limit) * 2.0;
        exterior.push(geo::Coord {
            x: exterior.last().unwrap().x,
            y: pole,
        });
        exterior.push(geo::Coord {
            x: exterior[0].x,
            y: pole,
        });
    }
    exterior.dedup();

    let shell = geo::Polygon::new(geo::LineString(exterior), Vec::new());
    let left = start + 1;
    let right = start + repeats * WORLD_WIDTH - 1;
    let mut interiors = Vec::new();
    for hole in holes {
        let copies = world_copies(hole.max, hole.min, left, right);
        for copy in copies {
            let line = hole.geo_line(copy * WORLD_WIDTH - center);
            if !contains_ring(&shell, &line) {
                return Err(Error::Geometry("hole lies outside its polar shell"));
            }
            interiors.push(line);
        }
    }
    validate_rings(shell.exterior(), &interiors)?;
    let polygon = geo::Polygon::new(shell.into_inner().0, interiors);
    Ok(Area {
        center,
        bounds: Bounds {
            min: Point {
                x: i32::MIN,
                y: south,
            },
            max: Point {
                x: i32::MAX,
                y: top,
            },
        },
        longitude: None,
        polygon,
        filters: None,
    })
}

pub(super) fn comparison_area(cap: &Area) -> f64 {
    let y = cap.polygon.bounding_rect().unwrap();
    let artificial_height = if cap.bounds.max.y == i32::MAX {
        y.max().y - f64::from(i32::MAX)
    } else {
        f64::from(i32::MIN) - y.min().y
    };
    let ring = &cap.polygon.exterior().0;
    let closure = &ring[ring.len() - 3..ring.len() - 1];
    let repeats = (closure[0].x - closure[1].x) / WORLD_WIDTH as f64;
    cap.polygon.unsigned_area() / repeats - artificial_height * WORLD_WIDTH as f64
}

fn coord(x: i64, y: i32) -> geo::Coord {
    geo::Coord {
        x: x as f64,
        y: y.into(),
    }
}

fn coords(points: &[(i64, i32)], shift: i64) -> impl Iterator<Item = geo::Coord> {
    points.iter().map(move |&(x, y)| coord(x + shift, y))
}

fn smaller_side(points: &[(i64, i32)]) -> Result<bool> {
    let angle = |y: i32| f64::from(y) * TAU / WORLD_WIDTH as f64;
    // The signed mean of sin(latitude) around one eastward circuit determines
    // which polar region has the smaller spherical area.
    let mean: f64 = points
        .windows(2)
        .map(|pair| {
            let ((x1, y1), (x2, y2)) = (pair[0], pair[1]);
            let (a, b) = (angle(y1), angle(y2));
            let sin_latitude = if (b - a).abs() < 1e-6 {
                ((a + b) / 2.0).tanh()
            } else {
                (b.cosh().ln() - a.cosh().ln()) / (b - a)
            };
            (x2 - x1) as f64 / WORLD_WIDTH as f64 * sin_latitude
        })
        .sum();
    if mean.abs() <= 1e-12 {
        return Err(Error::Geometry("polar ring encloses two equal sides"));
    }
    Ok(mean > 0.0)
}

struct CutRing {
    x: f64,
    y: f64,
    vertices: Vec<(i64, i32)>,
}

impl CutRing {
    fn extra_width(&self) -> i64 {
        let start = self.x.floor() as i64;
        self.vertices
            .iter()
            .map(|&(x, _)| (start - x).max(x - start - WORLD_WIDTH))
            .max()
            .unwrap_or_default()
    }

    fn repeated(&self, copies: i64, center: i64) -> Vec<geo::Coord> {
        let mut line = Vec::with_capacity(self.vertices.len() * copies as usize + 2);
        line.push(geo::Coord {
            x: self.x - center as f64,
            y: self.y,
        });
        for copy in 0..copies {
            line.extend(coords(&self.vertices, copy * WORLD_WIDTH - center));
        }
        line.push(geo::Coord {
            x: self.x + (copies * WORLD_WIDTH - center) as f64,
            y: self.y,
        });
        line
    }
}

fn polar_cut(points: &[(i64, i32)], north: bool) -> CutRing {
    let open = &points[..points.len() - 1];
    // From this vertex the meridian toward the pole cannot cross another
    // true edge higher (or lower) than the ring's latitude extremum.
    let (start, &(x, y)) = open
        .iter()
        .enumerate()
        .max_by_key(|(_, point)| {
            if north {
                i64::from(point.1)
            } else {
                -i64::from(point.1)
            }
        })
        .unwrap();
    let mut vertices = open[start..].to_vec();
    vertices.extend(open[..start].iter().map(|&(x, y)| (x + WORLD_WIDTH, y)));
    CutRing {
        x: x as f64,
        y: y.into(),
        vertices,
    }
}

struct Pinch {
    point: (i64, i32),
    outer_edge: usize,
    inner_edge: usize,
    inner_offset: i64,
}

fn pinched_band(shell: &Ring, hole: &Ring) -> Result<Option<Ring>> {
    let width = (shell.max - shell.min + hole.max - hole.min) / WORLD_WIDTH + 2;
    if shell.positions.len() + hole.positions.len() > MAX_VERTICES / width as usize {
        return Err(Error::Geometry(
            "polar band exceeds the reference vertex limit",
        ));
    }
    let edges = edge_index(
        shell
            .positions
            .windows(2)
            .map(|pair| geo::Line::new(coord(pair[0].0, pair[0].1), coord(pair[1].0, pair[1].1))),
    );
    let mut touch: Option<Pinch> = None;
    for (inner, pair) in hole.positions.windows(2).enumerate() {
        let ((x1, y1), (x2, y2)) = (pair[0], pair[1]);
        for copy in world_copies(x1.min(x2), x1.max(x2), shell.min, shell.max) {
            let offset = copy * WORLD_WIDTH;
            let line = geo::Line::new(coord(x1 + offset, y1), coord(x2 + offset, y2));
            for candidate in edges.locate_in_envelope_intersecting(&edge_envelope(line)) {
                let (outer, edge) = candidate.data;
                let Some(hit) = line_intersection(edge, line) else {
                    continue;
                };
                let LineIntersection::SinglePoint {
                    intersection,
                    is_proper: false,
                } = hit
                else {
                    return Err(Error::Geometry(
                        "polar shell and winding hole cross or share an edge",
                    ));
                };
                // A nonproper single intersection uses an original endpoint.
                let point = (intersection.x as i64, intersection.y as i32);
                if let Some(pinch) = &touch {
                    if (point.0 - pinch.point.0).rem_euclid(WORLD_WIDTH) != 0
                        || point.1 != pinch.point.1
                    {
                        return Err(Error::Geometry("polar band has more than one pinch point"));
                    }
                } else {
                    touch = Some(Pinch {
                        point,
                        outer_edge: outer,
                        inner_edge: inner,
                        inner_offset: offset,
                    });
                }
            }
        }
    }
    let Some(pinch) = touch else {
        return Ok(None);
    };
    let (x, y) = pinch.point;
    // Cut both exact rings at the one physical touch and join the two
    // lifted copies there. The resulting interior is contractible.
    let mut positions = vec![pinch.point];
    positions.extend(cut_vertices(&shell.positions, pinch.outer_edge, 0));
    positions.push((x + WORLD_WIDTH, y));
    positions.extend(
        cut_vertices(&hole.positions, pinch.inner_edge, pinch.inner_offset)
            .into_iter()
            .rev(),
    );
    positions.push(pinch.point);
    positions.dedup();
    let min = positions.iter().map(|p| p.0).min().unwrap();
    let max = positions.iter().map(|p| p.0).max().unwrap();
    Ok(Some(Ring {
        positions,
        min,
        max,
    }))
}

fn cut_vertices(points: &[(i64, i32)], edge: usize, offset: i64) -> Vec<(i64, i32)> {
    let open = &points[..points.len() - 1];
    let next = edge + 1;
    let mut vertices = Vec::with_capacity(open.len());
    vertices.extend(open[next..].iter().map(|&(x, y)| (x + offset, y)));
    vertices.extend(
        open[..next]
            .iter()
            .map(|&(x, y)| (x + offset + WORLD_WIDTH, y)),
    );
    vertices
}

struct Crossing {
    edge: usize,
    world: i64,
    y: f64,
    inner: bool,
}

fn band_cuts(
    shell: &[(i64, i32)],
    hole: &[(i64, i32)],
    meridian: i64,
    north: bool,
) -> Result<(CutRing, CutRing)> {
    // A half-grid meridian avoids all vertices and vertical boundary segments.
    let mut hits = Vec::new();
    for (points, inner) in [(shell, false), (hole, true)] {
        for (edge, pair) in points.windows(2).enumerate() {
            let ((x1, y1), (x2, y2)) = (pair[0], pair[1]);
            let world = (x1.min(x2) - meridian + WORLD_WIDTH - 1).div_euclid(WORLD_WIDTH);
            let cut = meridian + world * WORLD_WIDTH;
            if cut < x1.max(x2) {
                let t = ((cut - x1) as f64 + 0.5) / (x2 - x1) as f64;
                let y = f64::from(y1) + t * (f64::from(y2) - f64::from(y1));
                hits.push(Crossing {
                    edge,
                    world,
                    y,
                    inner,
                });
            }
        }
    }
    hits.sort_unstable_by(|a, b| {
        if north {
            b.y.total_cmp(&a.y)
        } else {
            a.y.total_cmp(&b.y)
        }
    });
    let mut inside_shell = true;
    let mut inside_hole = true;
    let mut cut = None;
    for pair in hits.windows(2) {
        let first = &pair[0];
        let second = &pair[1];
        if first.inner {
            inside_hole = !inside_hole;
        } else {
            inside_shell = !inside_shell;
        }
        if inside_hole && !inside_shell {
            return Err(Error::Geometry("winding hole lies outside its polar shell"));
        }
        if cut.is_none()
            && inside_shell
            && !inside_hole
            && first.inner != second.inner
            && first.y != second.y
        {
            cut = Some(if first.inner {
                (second, first)
            } else {
                (first, second)
            });
        }
    }
    let (outer, inner) = cut.ok_or(Error::Geometry("polar band has no interior cut"))?;
    let rotate = |points: &[(i64, i32)], hit: &Crossing| {
        let vertices = cut_vertices(points, hit.edge, -hit.world * WORLD_WIDTH);
        CutRing {
            x: meridian as f64 + 0.5,
            y: hit.y,
            vertices,
        }
    };
    Ok((rotate(shell, outer), rotate(hole, inner)))
}
