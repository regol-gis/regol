# Compatibility contract

Target: on-disk GOL format word **2**, with pinned `gol 2.0.0` production fixtures.
This is an independently designed Rust API. It does not require Python, Java,
GeoDesk native libraries, subprocesses, or network services at runtime.
Unsupported versions, unavailable tiles, corrupt data and exhausted resource
limits produce errors. Iterators are lazy, so errors can arise after opening.

The native PBF writer chooses global strings across tag keys, text values and
relation roles. It reserves codes 1–8190 for globally encoded keys and
keeps a key spelling local if it cannot fit in that range. Narrow integers use numeric storage. A wide numeric spelling used at least 300
times can receive a global text code. Area classification uses the same value
representation as the tag encoder. The builder's `--compact-dictionary` option
starts from the standard catalog, then keeps the required keys and strings,
numeric spellings already represented as text, and optional strings with at
least 300 source uses. Both layouts preserve the same tag types and area rules.
Additional layout options (`--target-tile-kib`, `--max-duplicate-tiles`,
`--even-zooms`, `--rtree-fanout`, and `--rtree-leaf-size`) tune tile sizing,
GOL North/West border-feature duplication, zoom steps, and R-tree shape while
preserving exact feature identities and query semantics.

Different builders can store the same OSM decimal spelling as different GOL
types. In the UK corpus, GeoDesk Python reads `height=3.4` on way 1298592364
as a number on the GeoDesk-built file and as text on the native file. This
affects clients that inspect tag types or distinguish numeric and text predicates.
Some GOL Tool 2.0.0 files contain mixed spellings for which exact tag queries
and tag iteration disagree.

## File access and feature semantics

The reader supports active snapshots, global/local strings and typed numbers,
indexed keys, tile directories and R-trees, all feature types, geometry and
optional original node IDs, ordered relation members and roles, cross-tile
exports, direct parent relations and parent-way lookup. Narrow and wide foreign
tile/export deltas are exercised in both forward and backward tables.
Coincident nodes keep distinct IDs when identities are retained in the file.
Geometry-only vertices without retained identity expose `None`.

Sources may be borrowed bytes, owned bytes, safe positioned file reads or an
optional immutable mmap. mmap construction is unsafe: the backing file must not
change for the mapping's entire lifetime. All sources require a stable file for
consistent results; concurrent updates, writing, updating and downloading tiles
are outside this read-only contract. Completed snapshots with distinct tile
and directory generations are tested synthetically. GOL Tool 2.0.0 exposes no
update command, so no production updater lifecycle is claimed.

By default, opening a library via the Rust API validates every global spelling.
With `Library::with_dictionary_loading(source, limits, DictionaryLoading::OnDemand)`
(or by default in `regol query`, which uses immutable mmap + on-demand
dictionary loading unless `--no-mmap` or `--eager-dictionary` is passed), the
reader validates those spellings on first use. A count or ID scan without tag filters can complete without reading
them. A tag predicate pays that one-time cost when it is prepared. Concurrent
queries share the decoded dictionary. Failed initialization can be retried
when the underlying source supports retrying the read. `Library::open_with_dictionary_loading(path, DictionaryLoading::OnDemand)`
also allocates the checked file reader’s page slots in groups as it touches
regions of the file. Header, index and traversal checks apply on both paths.

Bbox selection uses stored bounding boxes, includes boundary contact and prunes
spatial indexes. It is not exact intersection. `Geometry::intersects_bounds`
provides exact geometry/rectangle intersection in integer coordinates. Bounds
cannot wrap the antimeridian; split such windows and combine by feature identity.

Spatial filters compare actual geometries in the integer Mercator plane.
`Intersects` includes contact at an edge or vertex. `Within` and `Contains`
also require the interiors to intersect. Polygon holes exclude their interiors.
`CoveredBy` includes boundary-only cases. `Touches`, `Overlaps`, `Crosses`
and `Disjoint` distinguish the other common spatial relationships. For
collections with polygon components that overlap or share an edge, intersection
and disjointness are supported. Other topological predicates return a geometry
error for those collections.

A geometry collection represents the union of its members. A stop coinciding
with the endpoint of an open route line remains on the route boundary. A
polygon is covered by a collection containing that polygon and a detached
point. GEOS 3.15.0 reports false for the latter case.

Set intersection applies both conditions to each feature. A way can therefore
match two disjoint bounding-box filters when it extends across both.
Unions of filtered member or parent sets use the handles retained by those
sets. Iterating a spatial selection known to be empty reads no feature tiles.
`FeatureSet::connected_to` matches other features sharing vertex coordinates,
including nested relation members. Direct parent-way lookup uses node identity
where available.

`WithinDistance` and the metric measurements use the shorter great-circle
arc on a sphere of radius 6,371,008.8 metres. This accounts for latitude and
wraps longitude at the antimeridian. Polygon area subtracts holes, and perimeter
includes their boundaries. Collection measurements sum their components. The
projected centroid uses the highest-dimensional contributing components to
choose the longitude reference and calculate their weighted average. These
choices can produce different values from GeoDesk's SDK measurements.

`regol check` verifies the active snapshot's header, metadata, normalized tile
directory and feature-tile CRC32C checksums. It checks tile extents, tile counts
and the R-tree bounds enclosing each branch and feature record. It also
traverses reachable tags, way vertices, relation members and parent links.
It requires complete coverage, as do unbounded queries. Inactive snapshots,
updater-only structures and the geometric validity of OSM objects are outside
the check. `regol info` reads metadata without the full scan.

Prepared scalar counts use the same query semantics as iteration, including
union deduplication, stored-bbox candidates and resource limits. `count()` and
`count_in_bounds()` return `Result<u64>`; `QueryExecutor::count` applies the
shared traversal budget across worker reductions. Errors do not return a partial
count. Unused feature fields are not decoded, and answers are not cached.

## Geometry

Owned geometry includes points, lines, polygons, multipolygons and collections.
Non-area nested relations flatten into collections, matching the Python geometry
API; homogeneous collections also remain collections. CLI exports and Java can
choose different collection structures. Polygon assembly joins reversed/open
segments and splits cycles at shared endpoints. Exterior components retain shared
edges. Repeated inner edges within the holes of a shell cancel regardless of winding.
A hole attaches to the smallest containing exterior, or the first exterior in assembly
order if none contains it. Area relations use
direct way members with outer/empty or inner roles; nested area relations do not
contribute boundaries, consistent with the pinned Python reference.

Incomplete rings and cyclic recursive expansion fail explicitly. Direct cyclic
member references can still be iterated. Geometry is not a general topology
repair service: invalid OSM topology can remain invalid or produce an assembly
error. For example, GeoDesk preserves an explicitly assigned hole outside its only
shell. Applications that require valid polygons should validate assembled geometry.

## GOQL matrix

| Syntax | Behavior |
| --- | --- |
| `n`, `w`, `a`, `r`, `*`, combinations | `w`/`r` exclude areas |
| `[key]`, `[!key]` | Presence, with `no` treated as false; empty values are present |
| `=`, `!=`, comma-separated values | Typed literals; missing tags match negation |
| Unquoted numeric literals | Numeric equality across decimal scales |
| Quoted strings / nonnumeric literals | Text matching; numeric storage values do not match text predicates |
| Leading/trailing `*` | Prefix, suffix, substring and any-text matching |
| `<`, `<=`, `>`, `>=` | Numeric comparisons; query constants must be finite |
| `~`, `!~` | Whole-string Rust regex matching against text values |
| Single/double quotes | Escaped quotes, backslashes and common whitespace escapes |
| Comma-separated selectors | Set union, one result per feature |

GOQL resolves each key to one representation in the open library. A key with a
common-key dictionary code (1 through 8190) is looked up in the common tag
table. Other keys use the local table. An exact text operand with a nonzero
global dictionary code matches a value stored with that spelling's code.
Wildcards and regexes can still match local text under the selected key.
Presence tests compare against the dictionary code for `no` when available.
These rules apply to simple, prepared, indexed and parallel queries.

GOL Tool 2.0.0 can write a frequent value such as `bench` as local text when
it occurs once as a key. A GOQL selector can then miss a tag visible through
`Feature::tag` and `Feature::tags`. Similarly, a locally encoded value can be
visible through raw tags and fail an exact dictionary comparison. During search,
selectors validate structural record and string-length bounds and compare raw
local-string bytes directly without running UTF-8 validation on rejected
candidates; reading tag strings (`Feature::tag`, `Feature::tags`), global
dictionary decoding, and `regol check` still validate UTF-8 strictly.

Numeric comparisons convert text tags using an ASCII decimal prefix: whitespace
and a minus sign are accepted, while a plus sign or exponent is not part of the
tag number. Thus tag `12 m` compares as 12, tag `1e3` as 1, and tag `+12` is not a
number. Query literals are parsed separately and can use signs and exponents.
`TagValue::as_number()` instead parses a complete Rust floating-point string;
it is intentionally a general value conversion, not the GOQL prefix conversion.

`Predicate::equal`/`not_equal` build text predicates. `equal_number` builds numeric
equality. Prepared and simple queries have the same semantics. Regex syntax is
Rust's, not Java's or C++'s: lookaround and backreferences are rejected. Interior
wildcards, additional operators and trailing unsupported syntax also fail.

Parsing is limited to 1 MiB, 256 selectors, 1,024 predicates and 32 regexes. Each
regex has 256 KiB compiled-size and DFA-cache budgets and nesting limit 64.
File traversal, recursion, members, tags and coordinates have adjustable `Limits`.
These are bounded-work controls, not a guarantee about the operating system's
available memory. Owned geometry and regex caches allocate; borrowed field
iteration does not allocate per field.

## Deliberate differences and oracle limitations

- Empty-string equality and escaped exact-string equality follow the original
  tag values. The pinned CLI/Python matcher disagrees on two retained test cases.
- Touching-ring geometry preserves both original exteriors. Python 2.0.4 drops
  vertices in the synthetic junction case; the expected rings come from input.
- Full 33-bit coordinate deltas are decoded. The SDK's way iterator/geometry can
  misdecode them; original coordinates and independent node records establish
  the expected result.
- Incomplete boundaries return errors. Some reference readers repair incomplete boundaries or abort during assembly.
- Repeated selectors are duplicate-free. GOL Tool 2.0.0 can crash or duplicate
  results for `n,n`. Its empty-tag exporter can also invent an empty-key tag.
- Java rejects some accepted C++ queries and differs on numeric/string/regex
  coercion. The optional oracle checks record disagreements per query.
- The dated full-UK benchmark exposes a reference inconsistency for
  `way/1502890331`, tagged `building=3`: Python 2.0.4 includes it for
  `a[building=3]` and `a[building!=no]`, but excludes it for `a[building]`.
  Rust includes the numeric, present value in the truth test. An isolated
  synthetic numeric-tag case does not reproduce the reference behavior, so its
  internal cause remains unresolved.

The producer also exposed failures on some tiny/empty-category inputs and very
large tile counts. Only successfully checked production fixtures enter the
normal corpus. The independent input JSON/PBF remains authoritative when an
oracle itself is defective. See the [validation guide](VALIDATION.md) for reproduction instructions.
