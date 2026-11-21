//! Conformance adapter, not part of the library API. JSON allocation is explicit.
use regol::{FeatureType, Library, Query, source::FileSource};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{self, Write},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("usage: dump FILE.gol [GOQL]")?;
    let query = Query::parse(&args.next().unwrap_or_else(|| "*".to_owned()))?;
    let lib = Library::new(FileSource::open(path)?)?;
    let prepared = query.prepare(&lib)?;
    let mut out = io::BufWriter::new(io::stdout().lock());
    for feature in prepared.iter() {
        let f = feature?;
        let tags: BTreeMap<_, _> = f
            .tags()?
            .map(|t| t.map(|t| (t.key.into_owned(), t.value.to_string())))
            .collect::<regol::Result<_>>()?;
        let parents: Vec<_> = f
            .parent_relations()?
            .map(|p| p.map(|p| p.id().to_string()))
            .collect::<regol::Result<_>>()?;
        let mut result = json!({"id": f.id().to_string(), "area": f.is_area(), "tags": tags, "parents": parents});
        if let Some(point) = f.point()? {
            result["point"] = json!([point.x, point.y]);
        }
        if f.kind() == FeatureType::Way {
            let nodes: Vec<Value> = f
                .way_nodes()?
                .map(|n| n.map(|n| json!([n.id, n.point.x, n.point.y])))
                .collect::<regol::Result<_>>()?;
            result["nodes"] = json!(nodes);
        }
        if f.kind() == FeatureType::Relation {
            let members: Vec<Value> = f
                .members()?
                .map(|m| m.map(|m| json!([m.feature.id().to_string(), m.role])))
                .collect::<regol::Result<_>>()?;
            result["members"] = json!(members);
        }
        serde_json::to_writer(&mut out, &result)?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}
