<h1><img src="docs/regol-wordmark.png" width="330" height="110" alt="regol"></h1>

**regol** is an independent, pure-Rust GeoDesk GOL 2.0 reader, spatial query
engine, and OpenStreetMap PBF compiler.

A GOL (Geo-Object Library) reorganizes an OpenStreetMap `.osm.pbf` snapshot
into a single indexed binary file grouped by zoom-pyramid tiles, tag categories,
and Hilbert-ordered R-trees. Queries read only the tiles and R-tree branches
that overlap the requested region and tags—making country- and continent-scale
OSM lookups fast on ordinary disks, memory-mapped files, or custom range-read
backends (such as object storage).

regol ships as two separate workspace crates with no GeoDesk, C, or SQLite
dependencies:

- **`regol`** (library crate) — embed GOL reading, GOQL tag queries, spatial
  predicates, geometry assembly, spherical measurements, relation/way traversal,
  and optional multi-threaded PBF compilation directly in Rust applications.
- **`regol-cli`** (CLI crate, `regol` binary) — a single command-line tool to
  `build`, `info`, `check`, and `query` GOL files and export results to
  GeoJSON, GeoJSONL, WKT, CSV, OSM XML, or PBF.

---

## Command-line usage (`regol`)

### Quick try from a checkout

```sh
cargo run --locked -p regol-cli -- query tests/fixtures/basic.gol 'na[amenity=cafe]' -f ids
```

The sample contains one matching café and prints `N1`. In GeoDesk Query
Language (GOQL), `na` selects nodes and areas and `[amenity=cafe]` filters by
tag. Omit `-f ids` to print a count.

### Install

```sh
# From this checkout
cargo install --locked --path cli

# Or from crates.io
cargo install --locked regol-cli
```

### Build a GOL from OpenStreetMap PBF

`regol build` compiles raw or zlib-compressed `.osm.pbf` snapshots in parallel
in memory (100% safe Rust, no C compiler or temporary SQLite database) and
atomically publishes the finished `.gol` file:

```sh
# Default layout (16 MiB target tiles, consecutive zooms, single placement)
regol build region.osm.pbf region.gol

# Tuned for object-storage / S3 range-read vector tile generation
regol build \
  --target-tile-kib 4096 \
  --max-duplicate-tiles 16 \
  --even-zooms \
  --rtree-fanout 16 \
  --rtree-leaf-size 64 \
  region.osm.pbf region-s3.gol
```

Key `regol build` flags (`regol build --help` for full details):

- `--target-tile-kib N` — target tile payload size in KiB (default `16384`).
- `--max-duplicate-tiles K` — duplicate border-straddling features into up to
  `K` covering leaf tiles using GOL North/West deduplication bits (default `1`
  for single placement).
- `--even-zooms` — use 4×4 even zoom steps (`0/2/4/6/8/10/12`) to halve tile
  pyramid depth.
- `--rtree-fanout F` selects 2–255 children per R-tree branch (default `8`).
  For example, `--rtree-fanout 32` builds wider branches while retaining the
  default 32 features per leaf. Set `--rtree-leaf-size L` to tune leaves separately.
- `--compact-dictionary` — keep only required keys and frequent strings (≥300
  uses) in the global string table for smaller metadata headers.

### Inspect, verify, and query

```sh
# Print GOL metadata (revision, tile count, indexed keys, way-node IDs)
regol info region.gol

# Verify header/metadata/tile CRC32C checksums, R-tree bounds, tags, and links
regol check region.gol

# Count or list feature IDs matching a GOQL query
regol query region.gol 'na[amenity=cafe]'
regol query region.gol 'na[amenity=cafe]' -f ids

# Filter by GeoJSON polygon or lon/lat bounding box and export to GIS/OSM formats
regol query region.gol 'na[amenity=cafe]' --area district.geojson -f geojsonl > cafes.geojsonl
regol query region.gol 'w[highway]' --bbox=-0.14,51.49,-0.10,51.52 -f csv --keys id,name,highway
regol query region.gol 'w[highway]' --bbox=-0.14,51.49,-0.10,51.52 -f pbf > roads.osm.pbf

# By default, query uses immutable mmap, on-demand dictionary loading, and
# available CPUs for counts and large geometry exports; -j 1 requests serial output
regol query region.gol 'w[highway]' --no-mmap --eager-dictionary --threads 1
```

Supported output formats (`-f`): `count` (default), `ids`, `geojson`,
`geojsonl`, `wkt`, `csv`, `xml`, and `pbf`. GeoJSON and GeoJSONL export OSM tag
values as strings by default. Use `--typed-tags` for GeoDesk-style numbers. See the
[command-line guide](cli/README.md) for CSV columns, precision rounding,
antimeridian splitting, and `--complete-relations`.

---

## Rust library usage (`regol`)

Add the library to your crate:

```toml
[dependencies]
regol = "0.2"
# Optional features:
# regol = { version = "0.2", features = ["mmap", "build", "way-checkpoints"] }
```

When `mmap` is not enabled, `regol` enforces `#![forbid(unsafe_code)]`.

### 1. Prepare and reuse GOQL queries

Preparing a `Query` resolves dictionary key/value codes and category masks once
for a given `Library`, so you can reuse the `PreparedQuery` across repeated
bounding-box searches, counts, or streaming iterations with zero per-field
allocations on borrowed traversal:

```rust
use regol::{Bounds, Library, Query, Result};

fn main() -> Result<()> {
    let library = Library::open("tests/fixtures/basic.gol")?;
    let query = Query::parse("na[amenity=cafe]")?;
    let cafes = query.prepare(&library)?;

    println!("Total cafés: {}", cafes.count()?);
    for feature in cafes.iter() {
        let feature = feature?;
        let name = feature.tag("name")?.map(|v| v.to_string()).unwrap_or_default();
        println!("{} ({:?}): {}", feature.id(), feature.bounds()?, name);
    }

    // Reuse the same prepared query for a bounding-box window (integer Mercator)
    let window = Bounds::new(-100_000, -100_000, 100_000, 100_000)?;
    println!("In window: {}", cafes.count_in_bounds(window)?);
    Ok(())
}
```

### 2. Spatial filters, feature sets, relationships, and measurements

Use `FeatureSet` for set algebra (`and`, `or`, `without`), typed-ID lookup,
relation/way graph traversal (`member_set`, `parent_set`, `connected_to`),
exact topological predicates (`Intersects`, `Within`, `Contains`, `CoveredBy`,
`Touches`, `Overlaps`, `Crosses`, `Disjoint`, `WithinDistance`), and spherical
measurements:

```rust
use regol::{FeatureId, FeatureType, Library, Query, Result, SpatialFilter, SpatialPredicate};
use regol::units::area::hectare;

fn main() -> Result<()> {
    let library = Library::open("tests/fixtures/ids.gol")?;
    let all = library.feature_set();

    let area_id = FeatureId { kind: FeatureType::Way, id: 11 };
    if let Some(boundary) = all.get(area_id)? {
        let filter = SpatialFilter::from_feature(SpatialPredicate::Intersects, boundary)?;
        let cafes = all.select(Query::parse("na[amenity=cafe]")?).spatial(filter);
        for cafe in cafes.iter() {
            println!("{}", cafe?.id());
        }
        println!("Area: {:.1} ha", boundary.area()?.get::<hectare>());
    }
    Ok(())
}
```

Lengths and areas use standard `uom` quantities. A `Geometry::distance` result can
feed directly into `SpatialPredicate::WithinDistance`. Use `regol::units` to select
a display unit. For a geographic query window, `Bounds::from_lon_lat` takes
southwest and northeast corners with distinct `Longitude` and `Latitude` types.

For multi-threaded scans across large libraries (enabled by default via the
`parallel` feature), create a `QueryExecutor` to run parallel counts,
reductions, or ordered/unordered iteration.

### 3. Choosing a storage backend & dictionary loading mode

`Library<S>` is generic over `regol::source::Source` (positioned random byte
reads), so you can match memory and I/O behavior to your environment:

| Backend | Constructor / Type | Best for |
|---|---|---|
| Retained pages | `Library::open(path)` (`source::PagedFile`) | Default safe file access; retains touched pages for repeated reads |
| Bounded page cache | `source::CachedFile::open(path, bytes)` | Long-running services with a strict RAM budget |
| Reusable read window | `source::WindowedFile::open(path)` | Low-memory streaming scans |
| Direct positioned I/O | `source::FileSource::open(path)` | Minimal wrapper around OS `pread` |
| Memory-mapped file | `source::MappedFile::open(path)` (`mmap` feature) | Maximum warm throughput on immutable local files (`unsafe` contract) |
| HTTP(S) range reads | `source::HttpSource::open(url)` (`http` feature) | Querying an immutable remote GOL and reusing fetched pages |
| In-memory / custom | `&[u8]`, `Vec<u8>`, or custom `impl Source` | Embedded fixtures and application-specific storage |

On Linux, eligible memory-mapped scans can overlap filtering with reads of
upcoming tiles. The reader samples major page faults, input bytes and time
spent off CPU. When Linux is already fetching large ranges and each fault
takes milliseconds, the scan leaves read-ahead to the kernel.

Use `DictionaryLoading::OnDemand` via `Library::open_with_dictionary_loading` or
`Library::with_dictionary_loading` when startup latency matters (e.g. cold
serverless opens or ID/count scans that do not inspect tag spellings).

The CLI accepts an HTTP(S) URL in `query`, `info` and `check`. A bounded query
can read a small part of a planet GOL. Direct HTTPS prefers HTTP/3 and shares
one QUIC connection across workers. HTTP/1.1 fallback reuses a connection pool.
Each handle retains its fetched ranges until drop. By default the first 128 KiB
transfer determines the range size
(32, 128 or 512 KiB). Use `--http-page-kib` on a query or
`HttpSource::with_page_size` in Rust to select a fixed size.

```sh
regol query 'https://download.openplanetdata.com/osm/planet/gol/v2/planet-latest.osm.gol' \
  'w[highway]' --bbox=-0.212,5.541,-0.190,5.563 --threads 8
```

### 4. Building GOL files from Rust (`build` feature)

```rust
use regol::build::{BuildOptions, TileZooms, build_pbf_with_layout};

fn compile_region() -> anyhow::Result<()> {
    let options = BuildOptions {
        target_tile_bytes: 4 * 1024 * 1024,
        max_duplicate_tiles: 16,
        rtree_branch_fanout: 16,
        rtree_leaf_size: 64,
        ..BuildOptions::default()
    };
    let report = build_pbf_with_layout("region.osm.pbf", "region.gol", options, TileZooms::Even)?;
    println!("Wrote {} features across {} tiles", report.features, report.tiles);
    Ok(())
}
```

### 5. Importing a GOB snapshot (`gob` feature)

A Geo-Object Bundle (GOB) stores individually compressed tiles. Enable the
`gob` Cargo feature to import the tiles covering a viewport and query them
through the usual `Library` API. The caller supplies bytes from a local file
or HTTP range requests. When a relation needs another tile, the library
returns its ID so the caller can fetch it and retry.

The example writes a new GOL containing the selected tiles. Coordinates are
west, south, east and north in degrees.

```sh
cargo run --locked --release --no-default-features --features gob --example import_gob -- \
  planet.gob london.gol -0.178,51.482,-0.078,51.532
```

### Cargo features (`regol` library)

| Feature | Default | Purpose |
|---|---|---|
| `parallel` | Yes | Multi-threaded `QueryExecutor` pool for counts, reductions, and iteration |
| `http` | No | Read immutable GOLs over HTTP/3 or pooled HTTP/1.1 with `source::HttpSource`. The CLI enables this feature. |
| `mmap` | No | `source::MappedFile` zero-copy memory-mapped reader for immutable files |
| `build` | No | Pure-Rust multi-threaded OSM PBF → GOL 2.0 compiler (`regol::build`) |
| `gob` | No | Import GOB 2.0 snapshot tiles, query them in memory and write the loaded tiles as a GOL. |
| `way-checkpoints` | No | Index long ways for repeated vertex and original-node-ID lookups |
| `experimental-tag-sidecars` | No | Auxiliary Unix tag sidecar indexes (requires `mmap`) |

---

## Documentation

- **Library guide:** [src/README.md](src/README.md) — API walkthrough, `FeatureSet`, spatial queries, and Cargo features.
- **Command-line guide:** [cli/README.md](cli/README.md) — `regol build`, `query`, `info`, `check`, GIS/CSV export options, and antimeridian handling.
- **Compatibility & format:** [docs/COMPATIBILITY.md](docs/COMPATIBILITY.md) · [docs/FORMAT.md](docs/FORMAT.md) — GOQL semantics, UTF-8/error contract, geometry rules, and GOL 2.0 binary layout notes.
- **Validation & releasing:** [docs/VALIDATION.md](docs/VALIDATION.md) · [docs/RELEASING.md](docs/RELEASING.md) — test reproduction, packaging, and release workflow.

regol and its original synthetic fixtures are licensed under Apache-2.0.
[License](LICENSE) · [Implementation and fixture provenance](docs/PROVENANCE.md)
