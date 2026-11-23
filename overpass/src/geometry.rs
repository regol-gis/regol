use regol::{Bounds, Geometry, Latitude, LonLat, Longitude, Point, Polygon, units::angle::degree};
use serde::{Deserialize, Serialize};

type Position = [f64; 2];
type Line = Vec<Position>;
type Rings = Vec<Line>;

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum GeoGeometry {
    Point { coordinates: Position },
    LineString { coordinates: Line },
    Polygon { coordinates: Rings },
    MultiPoint { coordinates: Line },
    MultiLineString { coordinates: Vec<Line> },
    MultiPolygon { coordinates: Vec<Rings> },
    GeometryCollection { geometries: Vec<GeoGeometry> },
}

pub(crate) fn geographic_point([x, y]: Position) -> Result<LonLat, String> {
    Ok(LonLat {
        longitude: Longitude::new::<degree>(x).map_err(|e| e.to_string())?,
        latitude: Latitude::new::<degree>(y).map_err(|e| e.to_string())?,
    })
}
pub fn project_box([w, s, e, n]: [f64; 4]) -> Result<Bounds, String> {
    Bounds::from_lon_lat(geographic_point([w, s])?, geographic_point([e, n])?)
        .map_err(|e| e.to_string())
}

fn projected(p: Position) -> Result<Point, String> {
    Ok(geographic_point(p)?.project())
}
fn project_line(l: &[Position]) -> Result<Vec<Point>, String> {
    if l.len() < 2 {
        return Err("A line needs at least two positions.".into());
    }
    l.iter().copied().map(projected).collect()
}
fn project_polygon(rings: &[Line]) -> Result<Polygon, String> {
    let ring = |line: &Line| {
        if line.len() < 4 || line.first() != line.last() {
            return Err("Polygon rings must be closed with at least four positions.".into());
        }
        project_line(line)
    };
    let (outer, holes) = rings.split_first().ok_or("Missing exterior ring.")?;
    Ok(Polygon {
        exterior: ring(outer)?,
        interiors: holes.iter().map(ring).collect::<Result<_, _>>()?,
    })
}

pub fn coordinates(p: &Point) -> Position {
    let pos = p.lon_lat();
    [pos.longitude.get::<degree>(), pos.latitude.get::<degree>()]
}
fn line(points: &[Point]) -> Line {
    points.iter().map(coordinates).collect()
}
fn rings(p: &Polygon) -> Rings {
    std::iter::once(&p.exterior)
        .chain(&p.interiors)
        .map(|l| line(l))
        .collect()
}

impl GeoGeometry {
    pub(crate) fn project(&self) -> Result<Geometry, String> {
        Ok(match self {
            Self::Point { coordinates } => Geometry::Point(projected(*coordinates)?),
            Self::LineString { coordinates } => Geometry::LineString(project_line(coordinates)?),
            Self::Polygon { coordinates } => Geometry::Polygon(project_polygon(coordinates)?),
            Self::MultiPolygon { coordinates } => Geometry::MultiPolygon(
                coordinates
                    .iter()
                    .map(|p| project_polygon(p))
                    .collect::<Result<_, _>>()?,
            ),
            Self::MultiPoint { coordinates } => Geometry::Collection(
                coordinates
                    .iter()
                    .map(|&p| projected(p).map(Geometry::Point))
                    .collect::<Result<_, _>>()?,
            ),
            Self::MultiLineString { coordinates } => Geometry::Collection(
                coordinates
                    .iter()
                    .map(|p| project_line(p).map(Geometry::LineString))
                    .collect::<Result<_, _>>()?,
            ),
            Self::GeometryCollection { geometries } => Geometry::Collection(
                geometries
                    .iter()
                    .map(Self::project)
                    .collect::<Result<_, _>>()?,
            ),
        })
    }
    pub fn from_regol(g: &Geometry) -> Self {
        match g {
            Geometry::Point(p) => Self::Point {
                coordinates: coordinates(p),
            },
            Geometry::LineString(l) => Self::LineString {
                coordinates: line(l),
            },
            Geometry::Polygon(p) => Self::Polygon {
                coordinates: rings(p),
            },
            Geometry::MultiPolygon(ps) => Self::MultiPolygon {
                coordinates: ps.iter().map(rings).collect(),
            },
            Geometry::Collection(gs) => Self::GeometryCollection {
                geometries: gs.iter().map(Self::from_regol).collect(),
            },
        }
    }
}
pub fn vertices(g: &Geometry) -> usize {
    let polygon = |p: &Polygon| p.exterior.len() + p.interiors.iter().map(Vec::len).sum::<usize>();
    match g {
        Geometry::Point(_) => 1,
        Geometry::LineString(l) => l.len(),
        Geometry::Polygon(p) => polygon(p),
        Geometry::MultiPolygon(ps) => ps.iter().map(polygon).sum(),
        Geometry::Collection(gs) => gs.iter().map(vertices).sum(),
    }
}
