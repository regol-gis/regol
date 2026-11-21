#!/usr/bin/env python3
"""Regenerate the checked-in synthetic corpus with GOL Tool 2.0.0.

Usage: python3 tools/regenerate.py /path/to/gol
The executable is a development tool, not bundled or linked with regol.
"""
import argparse
import hashlib
import json
import pathlib
import re
import subprocess
import sys

from make_fixture import dataset, encode, stress_dataset
from geometry_fixture import geometry_dataset
from query_fixture import query_dataset, QUERIES as SEMANTIC_QUERIES
from wide_fixture import export_dataset
from coincident_fixture import coincident_dataset

ROOT = pathlib.Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "tests" / "fixtures"
VARIANTS = {
    "basic": [],
    "global": ["--min-string-usage", "1"],
    "tiled": ["--min-tile-density", "1", "--rtree-branch-size", "4"],
    "ids": ["-w"],
    "tiled_global": ["--min-tile-density", "1", "--min-string-usage", "1", "--levels", "0,3,6,9,12", "-w"],
    "stress": ["--min-tile-density", "10", "--rtree-branch-size", "4", "--min-string-usage", "2", "-w"],
    "geometry": ["--min-tile-density", "1", "-w"],
    "query_semantics": ["--min-string-usage", "1"],
    "wide_exports": ["--min-tile-density", "1", "-w"],
    "coincident": [],
    "coincident_ids": ["-w"],
}
QUERIES = [
    "*", "n", "w", "a", "r", "na", "wa", "ra", "nwr",
    "*[name]", "*[!name]", "*[empty]", "*[!empty]",
    "*[building]", "*[!building]", "*[amenity=cafe,bench]",
    "*[amenity!=cafe]", "*[name=Test*]", "*[name=*Road]", "*[name=*est*]",
    "*[capacity=12]", "*[capacity!=12]", "*[height>=3.2]", "*[height<4]",
    "*[negative=-256]", "*[large>65535]", "*[\"odd:key\"=0012]",
    "n[amenity=cafe],n[amenity=bench]", "n,n", "*[name~\"Test.*\"]",
    "*[name!~\"Test.*\"]",
]


def run(gol, *args):
    result = subprocess.run([str(gol), *map(str, args)], check=True, capture_output=True, timeout=60)
    return result.stdout


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("gol", type=pathlib.Path)
    parser.add_argument("--only", nargs="+", choices=list(VARIANTS))
    args = parser.parse_args()
    gol = args.gol.resolve()
    version = run(gol, "--version").decode().strip()
    if "2.0.0" not in version:
        raise SystemExit(f"Expected GOL Tool 2.0.0, got {version!r}")
    FIXTURES.mkdir(parents=True, exist_ok=True)
    source = FIXTURES / "basic.osm.pbf"
    source.write_bytes(encode(dataset()))
    source.with_suffix(".json").write_text(json.dumps(dataset(), indent=2, ensure_ascii=False) + "\n")
    manifest = {"producer": version, "executable_sha256": sha(gol),
                "release": "https://github.com/clarisma/geodesk-gol/releases/tag/v2.0.0",
                "input": source.name, "input_sha256": sha(source),
                "data_license": "Apache-2.0; original synthetic data, no OSM extracts", "variants": {}}
    if args.only:
        existing = json.loads((FIXTURES / "manifest.json").read_text())
        if existing["executable_sha256"] != sha(gol):
            raise SystemExit("Partial regeneration requires the same pinned producer executable")
        manifest = existing
    for name, options in VARIANTS.items():
        if args.only and name not in args.only:
            continue
        input_path = source
        if name in ("stress", "geometry", "query_semantics", "wide_exports", "coincident", "coincident_ids"):
            input_path = FIXTURES / f"{name}.osm.pbf"
            data = {"stress":stress_dataset, "geometry":geometry_dataset, "query_semantics":query_dataset, "wide_exports":export_dataset, "coincident":coincident_dataset, "coincident_ids":coincident_dataset}[name]()
            input_path.write_bytes(encode(data))
            input_path.with_suffix(".json").write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n")
        path = FIXTURES / f"{name}.gol"
        # The tool refuses to overwrite existing GOLs. Only generated targets
        # in this fixed manifest are removed.
        path.unlink(missing_ok=True)
        command = ["build", path, input_path, *options]
        run(gol, *command)
        run(gol, "check", path)
        oracle = path.with_suffix(".oracle.geojson")
        oracle.write_bytes(run(gol, "query", path, "*", "-f", "geojson", "-p", "12"))
        queries = {}
        failures = {}
        for query in (["*", "n", "w", "r"] if name == "wide_exports" else QUERIES + (SEMANTIC_QUERIES if name == "query_semantics" else [])):
            try:
                ids = run(gol, "query", path, query, "-f", "list").decode().splitlines()
                if any(not re.fullmatch(r"[NWR][0-9]+", ident) for ident in ids):
                    raise ValueError("oracle returned malformed ID output")
                if len(set(ids)) != len(ids):
                    raise ValueError("oracle returned duplicate IDs for a set query")
                queries[query] = sorted(ids)
            except subprocess.CalledProcessError as error:
                failures[query] = {"returncode": error.returncode, "stderr": error.stderr.decode(errors="replace")}
            except ValueError as error:
                failures[query] = {"error": str(error)}
        path.with_suffix(".queries.json").write_text(json.dumps(queries, indent=2, ensure_ascii=False) + "\n")
        manifest["variants"][name] = {"input": input_path.name, "input_sha256": sha(input_path), "options": options, "sha256": sha(path), "oracle_sha256": sha(oracle), "queries_sha256": sha(path.with_suffix(".queries.json")), "expected_input_sha256": sha(input_path.with_suffix(".json")), "oracle_failures": failures}
    # GOL headers contain time/random IDs, so regeneration is semantically
    # reproducible rather than byte-identical. Hashes pin this particular corpus.
    (FIXTURES / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


if __name__ == "__main__":
    main()
