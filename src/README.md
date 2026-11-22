# regol

Read GeoDesk GOL 2.0 files in Rust. Select OpenStreetMap features by tag and
location, read their geometry, and traverse way nodes and relation members.
The optional builder compiles OSM PBF snapshots into new GOL files.

## Query a library

For the crates.io release, add `regol = "0.2"` to `[dependencies]`. From a
source checkout, use `cargo add --path /path/to/regol` in your application.

```rust
use regol::{Library, Query, Result};

fn main() -> Result<()> {
    let path = std::env::args_os().nth(1)
        .unwrap_or_else(|| "tests/fixtures/basic.gol".into());
    let library = Library::open(path)?;
    let cafes = Query::parse("na[amenity=cafe]")?;
    let prepared = cafes.prepare(&library)?;
    for feature in prepared.iter() {
        let feature = feature?;
        println!("{}", feature.id());
    }
    Ok(())
}
```

Pass a GOL path as the first argument. The fallback uses the packaged sample,
which prints `N1`. Preparing a query resolves tags against a specific library.
Reuse the prepared query for iteration, bounded queries or counts. Iteration
is lazy, so handle errors both when opening the file and when reading results.
`Feature::tags` and `Feature::coordinates` stream values, while
`Feature::geometry` constructs an owned geometry.

`Library::open` uses positioned reads. `Library::new` accepts borrowed bytes,
owned bytes or a custom `source::Source`. Bounding-box queries use stored feature
bounds and include boundary contact. For exact rectangle intersection use
`Geometry::intersects_bounds`. `Library::with_limits` bounds work such as
traversal, recursion and coordinate decoding.

With the `http` feature, `source::HttpSource::open(url)` opens an immutable
GOL on an HTTP(S) server that supports byte ranges. Pass it to
`Library::with_dictionary_loading(source, Limits::default(), DictionaryLoading::OnDemand)`
for a lazy dictionary. Reuse the library across bounded queries to share connections
and cached ranges. Direct HTTPS prefers HTTP/3 and falls back
to pooled HTTP/1.1. `HttpSource::protocol()` reports the current selection.
For bounded queries with uncached data, `QueryExecutor` can overlap range reads
by running each tile through feature matching on the same worker. Use
`with_query` for unordered results or `try_fold_reduce` to accumulate on the workers.
`PreparedQuery::in_bounds` provides ordered traversal. A full scan can retain the entire object in
memory. The opening 128 KiB read selects the range size for this handle. Use
`HttpSource::with_page_size(url, bytes)` for a fixed size. To interrupt pending
network requests from another thread, call `library.source().cancel_requests()`.
This closes active QUIC streams and pooled TCP connections for every query sharing that source.

With the `gob` feature, `gob::GobCatalog` maps a Geo-Object Bundle 2.0
snapshot's tile IDs to byte ranges. Read the first 64 bytes and use
`GobCatalog::prefix_len` to find the catalog and metadata extent. Open that
prefix and call `tiles_in_bounds` with a projected viewport to plan the
initial tile reads. Use `GobStore::import_tile` to load each checked chunk
and query the resulting `Library`. A feature link can return
`Error::MissingTile(tip)`, which lets the caller load that dependency and
retry. The caller supplies bytes from the same archive snapshot. Native
or browser callers can stream the loaded snapshot to a GOL file with
`GobStore::write_gol`.

## Spatial queries and feature sets

This example reads a building by its typed OSM ID and finds cafés that intersect
its footprint, including cafés on the boundary.

```rust
use regol::{FeatureId, FeatureType, Library, Query, Result, SpatialFilter, SpatialPredicate};
use regol::units::area::hectare;

fn main() -> Result<()> {
    let library = Library::open("tests/fixtures/ids.gol")?;
    let all = library.feature_set();
    let boundary_id = FeatureId { kind: FeatureType::Way, id: 11 };
    if let Some(boundary) = all.get(boundary_id)? {
        let filter = SpatialFilter::from_feature(SpatialPredicate::Intersects, boundary)?;
        let cafes = all.select(Query::parse("na[amenity=cafe]")?).spatial(filter);
        for cafe in cafes.iter() {
            println!("{}", cafe?.id());
        }
        println!("Boundary area: {:.1} ha", boundary.area()?.get::<hectare>());
    }
    Ok(())
}
```

Use `Query::prepare` and `PreparedQuery::spatial` to reuse a tag query or a
spatial filter across repeated searches. `FeatureSet` provides intersection,
union, difference, membership tests and typed-ID lookup. A global ID lookup can
scan the whole library. Use `in_bounds` when you know the approximate location.
`member_set`, `members_with_role`, `parent_set` and `connected_to` support
queries over OSM relationships. Use `Feature::members` for ordered relation
members, including repetitions.

`length`, `perimeter` and `distance` return spherical lengths, and `area` returns
spherical area. `regol::units` re-exports the standard `uom` quantities and unit
modules. `WithinDistance` takes a `Length`, so a measured distance can be used
directly as a threshold. `centroid` returns an integer Mercator `Point`.
`Point::lon_lat` yields a `LonLat` with distinct longitude and latitude angles.
Use `Bounds::from_lon_lat` to build a query window from two geographic corners.
To verify stored checksums and traverse the active snapshot's feature data,
call `Library::check_integrity`.

## Cargo features

| Feature | Use |
| --- | --- |
| `parallel` (default) | Use an explicit `QueryExecutor` pool for counts, iteration and reductions. Set `default-features = false` for a serial reader. |
| `http` | Open URLs with HTTP/3 or pooled HTTP/1.1 range GETs using system trust roots. Linux builds use the system OpenSSL development package. |
| `mmap` | Read from a memory-mapped file. The backing file must stay unmodified and untruncated for the lifetime of the mapping. |
| `build` | Compile a raw or zlib-compressed OSM PBF snapshot with `build::build_pbf` using the pure-Rust multi-threaded pipeline (no C compiler or SQLite required). |
| `gob` | Import complete tiles from a GOB 2.0 snapshot into a queryable store and write them as a GOL. |
| `way-checkpoints` | Index long ways for repeated vertex and original-node-ID lookups. Original IDs must be present in the GOL. |
| `experimental-tag-sidecars` | Build and query auxiliary tag indexes on Unix. Requires `mmap` and immutable backing files. |

The reader and builder require Rust 1.90 or later. The `regol` CLI (covering
`build`, `query`, `info`, and `check`) is distributed separately as `regol-cli`
(`cargo install --locked regol-cli`). The builder stages and encodes objects in
parallel in memory and atomically publishes the finished GOL file beside the
destination.

Licensed under Apache-2.0. [API reference](https://docs.rs/regol) · [Project homepage](https://regol.org)
