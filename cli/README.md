# regol-cli

`regol-cli` installs the `regol` command for building, inspecting, and querying
GeoDesk GOL 2.0 files on disk or over HTTP(S). It compiles OpenStreetMap PBF snapshots into GOL
files, verifies integrity, prints feature counts or IDs, and exports matching
features to XML, PBF, GeoJSON, GeoJSONL, WKT, or CSV. It requires Rust 1.90 or
later to build.

For a crates.io release, run `cargo install --locked regol-cli`. From the
repository root, run `cargo install --locked --path cli`.

To open a local GOL in an interactive browser map, install the separate server
with `cargo install --locked regol-live`, then run `regol live region.gol`.
Pass `--help` after `live` for the server options.

```sh
regol build region.osm.pbf region.gol
regol query region.gol 'na[amenity=cafe]'
regol query region.gol 'na[amenity=cafe]' -f ids
regol query region.gol 'w[highway]' --bbox=-0.14,51.49,-0.10,51.52 -f pbf > roads.osm.pbf
regol query region.gol '*' --area boundary.geojson -f xml > area.osm
regol query region.gol 'a[building]' --area boundary.geojson -f geojson > buildings.geojson
regol query region.gol 'n[amenity=cafe]' -f csv --keys id,name,lat,lon > cafes.csv
```

Quote the GOQL selector so the shell passes it unchanged. The default format
is a count. Results go to stdout. `--bbox` takes west, south, east and north in
degrees and tests stored feature bounds. `--area` tests geometry against a
GeoJSON polygon. For spatial PBF exports, `--complete-relations` controls which
relation types are completed. The default is `multipolygon`.

Pass an HTTP(S) URL in place of the GOL filename to `query`, `overpass`, `info`,
or `check`.
Direct HTTPS tries HTTP/3 on the URL’s port and falls back to pooled HTTP/1.1.
Plain HTTP and connections routed through a configured proxy use the pool.
The server must serve the GOL with identity encoding and return HTTP 206 with
a valid `Content-Range`. Each invocation retains the pages it reads.
By default a 128 KiB opening read selects the range size based on latency and
transfer time. `--http-page-kib N` selects a fixed power of two from 4 to 16384. Parallel bounding-box counts can overlap reads from distinct
tiles. Integrity checking and unbounded scans can read and retain the full object.
Linux source builds of the CLI require OpenSSL development headers and pkg-config.

By default, `regol query` memory-maps a local GOL file (requiring it to remain
immutable for the duration of the command), loads the string dictionary on
demand, and uses the available CPUs for counts and large geometry exports.
`--threads N` (or `-j N`) sets the worker budget. `--threads 1` requests serial
encoding and compression. Pass `--no-mmap` for safe positioned file reads or
`--eager-dictionary` to validate the full string table on open (`--mmap` and
`--lazy-dictionary` remain accepted for compatibility). Run `regol query --help`
for the full syntax or `regol --version` to check the installed version.

By default, multi-threaded GeoJSON, GeoJSON Lines, WKT, CSV, and ID exports
scan and encode spatial partitions in parallel tile order (`--unordered`). Pass
`--ordered` (or `--threads 1`) to encode consecutive feature groups in parallel
while preserving serial query order (buffering up to 4 MiB of encoded text and
streaming oversized records directly). Large XML exports format sorted chunks in
parallel, and large PBF exports overlap compression of one block with
preparation of the next.

### Overpass QL

`regol overpass` evaluates the Overpass QL subset shared with Regol Live.
Supply a UTF-8 query file with `--file` or pass `--file -` to read stdin.
Use `--bbox WEST,SOUTH,EAST,NORTH` to fill a `{{bbox}}` placeholder. West
greater than east crosses the antimeridian. Literal QL boxes use the QL order
south, west, north, east.

```sh
printf '%s\n' 'node[amenity=cafe]({{bbox}}); out count;' |
  regol overpass region.gol --file - --bbox=-0.14,51.49,-0.10,51.52

printf '%s\n' 'node[amenity=cafe](51.49,-0.14,51.52,-0.10)->.cafes; .cafes out asc;' > cafes.ql
regol overpass region.gol --file cafes.ql -f ids
regol overpass region.gol --file cafes.ql -f geojson > cafes.geojson

regol overpass uk.gol --file cli/examples/midlands.ql -f ids
```

Each `out` produces a JSON object on its own line, in statement order.
Records carry the output set `name` (`_` for the default set), QL `mode`,
and any explicit `detail` or `order` modifiers. The default `ids` format
uses type-qualified IDs such as `N123`. An `out count` record has `counts`
in every format. Use `-f count` to summarize feature groups with the same
per-type and total counts. For example, the Midlands query writes one
`groups` record with IDs and then one `groups` record with counts.
The UK snapshot used to check CLI and Live parity produced 15 IDs.

```jsonl
{"name":"groups","mode":"geom","detail":"body","ids":["N5088910764","N5088910763","N5088910767","N5088910766","N5088910765","N5707833521","N5707831720","N5707831715","N5707831719","N5707831718","N4596374136","N4631922525","N4596374158","N4596374126","N3523485319"]}
{"name":"groups","mode":"count","counts":{"nodes":15,"ways":0,"relations":0,"total":15}}
```

Results depend on the snapshot. `-f geojson` writes feature groups as GeoJSON
FeatureCollections with the output metadata as foreign members. Use it
directly as a GeoJSON file for a query with a single feature output. For
multiple outputs, process the response as newline-delimited JSON and
select each FeatureCollection. `out center` emits center points and
`out skel` emits empty properties. OSM tags are strings.

Named-area queries on local GOLs use the sibling `FILE.gol.areas` directory
when present and can otherwise scan the file. Pass `--area-index DIRECTORY`
to select a local index explicitly. Named-area queries on an HTTP(S) GOL need
a matching index. Generate one with Regol Live's `regol-area-index` tool.
A query with `[timeout:SECONDS];` fails with a nonzero exit status if it
times out. Execution budget failures also return a nonzero status.

### GIS and CSV output

`geojson` writes a FeatureCollection, and `geojsonl` writes one Feature per
line. Each includes the same type-qualified ID as `ids`, such as `N123`.
OSM tag values are JSON strings. For example, `maxheight=4.1` exports as
`"maxheight":"4.1"` whether the GOL stores the value as text or as a number.
Decimal scale and leading zeroes in text are preserved. Pass `--typed-tags`
for GeoDesk-style properties, with JSON numbers for numerically stored tags.
Use `--keys name,height` to select properties and
`--keys ''` for an empty properties object. Include `bbox` for a top-level
feature bounding box and `*` for all tags. Use `tag:bbox` to select the OSM
tag named `bbox`.

GeoJSON and GeoJSONL cut lines and polygons at the antimeridian, including
geometries in collections. A cut can yield a MultiLineString or MultiPolygon.
For a cut feature, `bbox` covers the exported geometry and its west longitude
can exceed its east longitude. Polar caps close through the pole on the
smaller spherical side of their boundary. Export reports an error if a
ring has a net winding of more than one turn around the globe or clipping
exceeds the geometry traversal budget.

`wkt` writes one geometry per line. `csv` writes a header and one row per
feature. The default CSV columns are `id,type,geometry`. Use `--keys` or
`--columns` to choose columns in order.

| CSV column | Value |
| --- | --- |
| `id` | Type-qualified ID, such as `N123` |
| `type` | OSM object type, such as `node` |
| `osm_id` | Numeric OSM ID |
| `lon`, `lat` | Geometry centroid in degrees |
| `bbox` | Stored bounds in west, south, east, north order |
| `geometry`, `geom`, `wkt` | WKT geometry |
| `tags` | JSON object of all stored tags |
| Other names | Individual tag values, with an empty field for missing tags |

Prefix an OSM tag key with `tag:` to select it when the name is also a built-in
column. For example, `--keys id,type,tag:type` includes both the OSM object type
and the OSM `type` tag. Geometry positions use longitude followed by latitude
in degrees. `--precision` rounds positions, bounds, and centroids to the given
number of decimal places, from 0 to 15 with a default of 7. GeoJSON winds
exterior rings counterclockwise and holes clockwise using the rounded
coordinates. Coarse rounding can merge distinct vertices or collapse a ring,
which can make a polygon invalid in GIS tools. These formats export whole
selected features, including portions outside the requested area.

### Building and inspecting GOL files

Use `regol build [OPTIONS] INPUT OUTPUT` to compile a raw or zlib-compressed
`.osm.pbf` snapshot into a GOL 2.0 file using the pure-Rust multi-threaded
builder. Pass `--preset vector-tiles` (4 MiB tiles, even zooms, border
duplication up to 16 tiles, wider R-tree nodes) or `--preset compact` (compact
dictionary + even zooms), or tune individual knobs with `--even-zooms`,
`--target-tile-kib N`, `--max-duplicate-tiles K`, `--rtree-fanout F`,
`--rtree-leaf-size L`, and `--compact-dictionary` (run `regol build --help` for
full option details). When available system or cgroup RAM is too small to hold
the build in memory, `regol build` automatically spills staging and resolved
buffers to temporary files beside `OUTPUT` and cleans them up on completion;
pass `--memory-limit-mib N` to set an explicit budget (or `0` to force spilling).

Use `regol info FILE` to inspect GOL metadata and `regol check FILE` to
validate the file.

The Rust library is distributed separately in the `regol` package. Licensed
under Apache-2.0. [Library documentation](https://docs.rs/regol) · [Project homepage](https://regol.org)
