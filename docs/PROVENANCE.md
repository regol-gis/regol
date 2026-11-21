# Implementation and fixture provenance

regol is an independent Rust implementation licensed under Apache-2.0.
The reader uses the GOL 2.0 on-disk format. Its format notes draw on the
Apache-2.0 GeoDesk Java library at tag `v2.0.0`, commit
`108f914ca1d9561d137e67d5ae70110f420f4fff`, and on files produced with the
matching GOL Tool. The reference source credits Clarisma and the GeoDesk
contributors. [Java reference](https://github.com/clarisma/geodesk/tree/108f914ca1d9561d137e67d5ae70110f420f4fff) · [Format notes](FORMAT.md)

The query implementation also drew on the architecture of the Apache-2.0
Java 2.1.0 matcher and the Apache-2.0 .NET port. The Java source JAR inspected
had SHA-256 `a24ab5e5dc15e1db8e935eab1f3d4e140f19b41e99a3606173f2d4f769a78c71`.
The Rust implementation uses its own checked traversal and shared work budgets.
[Java sources](https://repo.maven.apache.org/maven2/com/geodesk/geodesk/2.1.0/geodesk-2.1.0-sources.jar) · [.NET reference](https://github.com/sweepinc/geodesk-net/tree/ff60cc70e4e2b750ed96282b77e9e7ecf84bc200)

The native builder decodes OSM PBF using Prost and flate2 in pure Rust, staging
in memory with automatic disk spill for large inputs. Its closed-way area
classification draws on the public-domain [OSM polygon feature rules](https://github.com/tyrasd/osm-polygon-features).

The release archives include MIT notices copied from the upstream revisions for
three crates whose source packages omit the license file. The versioned copies
are in `tools/licenses`. [geo 0.31.0](https://github.com/georust/geo/blob/daf0515b0317b93480020a8cfb1dc7b46a4526ad/LICENSE-MIT) · [rstar 0.12.2](https://github.com/georust/rstar/blob/c8c5bf9e41468281d030869d409e3f67f1aaf5b7/LICENSE-MIT) · [i_overlay 4.0.7](https://github.com/iShape-Rust/iOverlay/blob/a071220dfc6095edb82426b880a4019db3ea67b5/LICENSE)

## Test data

The fixture generators create synthetic geographic objects under Apache-2.0.
The suite retains the source objects, encoded fixtures and expected query or
geometry results. The external AGPL-3.0 GOL Tool 2.0.0 generated the reference
GOLs and oracle output. It is distributed separately. The fixture manifest
records the executable hash, input hashes and producer options. Regeneration
can change header IDs and timestamps. [GOL Tool release](https://github.com/clarisma/geodesk-gol/releases/tag/v2.0.0) · [Fixture manifest](../tests/fixtures/manifest.json)

The CLI antimeridian fixture was built with regol from the synthetic OSM
objects in `cli/tests/fixtures/antimeridian.osm.json`. Its export tests check
coordinates and topology on both sides of the dateline.

Real OpenStreetMap data supplied to the tools retains its original licensing
and attribution requirements. [OpenStreetMap attribution](https://www.openstreetmap.org/copyright)
