# Validating regol

Run these checks from the repository root. The workspace tests use synthetic
fixtures committed with the source. After fetching dependencies they can run
offline. Python 3.11 or later is needed for the maintainer tools.

```sh
cargo fetch --locked
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo test --locked -p regol --no-default-features
RUSTDOCFLAGS='-D warnings -D missing-docs' cargo doc --locked -p regol --lib --all-features --no-deps
python3 tools/check_fixtures.py
python3 tools/check_licenses.py
python3 tools/release.py --check
```

CI runs these checks on Linux. It also checks the minimum supported Rust
version and cross-compiles the reader and CLI with mmap support for
macOS ARM64 and Windows x86-64. The manual **Fuzz** workflow exercises the
reader and query parser with the sanitizer harness in `fuzz/`.

The **Security** workflow checks both Cargo lockfiles against RustSec on pull
requests, pushes to `main`, and weekly. To run the same checks locally:

```sh
cargo install cargo-audit --version 0.23.0 --locked
cargo audit --file Cargo.lock
cargo audit --no-fetch --file fuzz/Cargo.lock
```

Both lockfiles currently resolve `heapless 0.8.0` through `rstar`. The
library uses `rstar 0.13.1` directly, and `geo` uses `rstar 0.12.2`.
Both versions require the 0.8 series. RustSec reports nine unsoundness
warnings for this version. The audit displays them while we await a compatible
upstream release. [RustSec advisories for heapless](https://rustsec.org/packages/heapless.html)

## Targeted tests

The `http` feature tests start loopback TCP and UDP servers with a test certificate.
Run `cargo test --locked -p regol --features http --test http_source` for range
validation, caching and worker scheduling, and
`cargo test --locked -p regol --features http --lib source::http` for QUIC
multiplexing, cancellation and TLS fallback.

The GOB tests run offline. They compare queries and relationships with
independently produced GOL fixtures, follow a relation beyond the initial
viewport, and reopen partial and complete serialized stores.

```sh
cargo test --locked -p regol --no-default-features --features gob --test gob
cargo test --locked -p regol --no-default-features --features gob --lib gob::
rustup target add wasm32-unknown-unknown
cargo check --locked -p regol --no-default-features --features gob --target wasm32-unknown-unknown
```

The allocation test checks that borrowed field traversal on the fixtures
performs zero allocations:

```sh
cargo test --locked --test allocations
```

Run `cargo run --locked --release --example spatial_filters_bench -- 100` for
the fixture-scale spatial benchmark. It reuses a prepared GOQL query and filter
and prints timing and feature counts for each operation, including boundary
contact between route relations with polygon, line and stop members. The
geometric fixtures and feature-set tests cover holes, boundaries, collections
and shared vertices.

## Fixtures and external references

`tests/fixtures/manifest.json` pins the producer executable, inputs, build
options and expected outputs. To regenerate one variant intentionally, use
the pinned GOL Tool 2.0.0 executable. Close any readers mapping the files first.

```sh
python3 tools/regenerate.py /path/to/gol --only geometry
python3 tools/check_fixtures.py
```

The optional oracle checks compare synthetic inputs against independent SDKs.
Build the Rust dump examples first. The geometry check uses GeoDesk Python
2.0.4 and Shapely 2.1.2. The query check also needs a JDK and the pinned Java
artifacts fetched by the helper.

```sh
cargo build --locked --release --example dump --example geometry_dump
python3 tools/fetch_java_oracle.py /tmp/regol-java-oracle
python3 tools/check_oracles.py /tmp/regol-java-oracle /tmp/regol-queries.json
python3 tools/check_geometry.py tests/fixtures/geometry.gol /tmp/regol-geometry.json
```

The fixture provenance and documented differences between readers are recorded
in the [provenance notes](PROVENANCE.md) and [compatibility guide](COMPATIBILITY.md).
