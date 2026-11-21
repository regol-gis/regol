//! Geometry conformance adapter; coordinates stay in integer Mercator units.
use regol::{Geometry, Library, Point, Polygon, source::FileSource};
use serde_json::{Value, json};
use std::io::{self, Write};

fn points(line: &[Point]) -> Vec<[i32; 2]> {
    line.iter().map(|p| [p.x, p.y]).collect()
}
fn polygon(p: &Polygon) -> Vec<Vec<[i32; 2]>> {
    std::iter::once(&p.exterior)
        .chain(&p.interiors)
        .map(|r| points(r))
        .collect()
}
fn geometry(g: &Geometry) -> Value {
    match g {
        Geometry::Point(p) => json!({"type":"Point","coordinates":[p.x,p.y]}),
        Geometry::LineString(line) => json!({"type":"LineString","coordinates":points(line)}),
        Geometry::Polygon(p) => json!({"type":"Polygon","coordinates":polygon(p)}),
        Geometry::MultiPolygon(ps) => {
            json!({"type":"MultiPolygon","coordinates":ps.iter().map(polygon).collect::<Vec<_>>()})
        }
        Geometry::Collection(gs) => {
            json!({"type":"GeometryCollection","geometries":gs.iter().map(geometry).collect::<Vec<_>>()})
        }
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: geometry_dump FILE.gol")?;
    let library = Library::new(FileSource::open(path)?)?;
    let mut out = io::BufWriter::new(io::stdout().lock());
    for f in library.features() {
        let f = f?;
        let value = match f.geometry() {
            Ok(g) => json!({"id":f.id().to_string(),"geometry":geometry(&g)}),
            Err(e) => json!({"id":f.id().to_string(),"error":e.to_string()}),
        };
        serde_json::to_writer(&mut out, &value)?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}
