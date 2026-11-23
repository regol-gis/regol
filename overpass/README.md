# regol-overpass

Compile and execute bounded Overpass QL queries against a `regol::Library`. A query can select features by tag, box, polygon, named area, proximity or direct membership. It can combine sets, filter candidates, loop over node neighbourhoods and emit multiple outputs. The library works with local and range-backed GOL sources, including browser WebAssembly.

```rust
use regol::Library;
use regol::source::Source;
use regol_overpass::{compile_request, execute_groups, GroupData};

fn cafes<S: Source>(library: &Library<S>) -> Result<Vec<String>, String> {
    let request = compile_request(
        "node[amenity=cafe](43.733,7.414,43.742,7.426); out body asc;",
        None,
    )?;
    let mut ids = Vec::new();
    for group in execute_groups(library, request, &|| Ok(()))? {
        if let GroupData::Features(features) = group.data {
            ids.extend(features.iter().map(|feature| feature.id().to_string()));
        }
    }
    Ok(ids)
}
```

Each output group carries its name, mode, detail and sort order. Feature groups contain handles borrowed from the open library, so consumers can read just the IDs, tags or geometries they need. An explicit `out count` returns counts by OSM type. Intermediate set and geometry traversal limits apply during evaluation.

Use `execute_stream` with an `OutputSink` to consume results incrementally. The sink can stop after a group or a feature. On native targets, `execute_stream_parallel` can prepare features on a caller-supplied Rayon pool and deliver them in query order.

Pass west/south/east/north windows to `compile_request` when the query contains `{{bbox}}`. An explicit box or named-area lookup can run without a viewport. `compiler::compile` returns the serialized query plan. Native clients can populate a request's named lookups with `area_index::AreaIndex::hydrate` and generate a matching index with `area_index::build`.

The CLI exposes the same compiler and executor through `regol overpass`. [Command-line examples](https://github.com/regol-gis/regol/tree/main/cli#overpass-ql)
