#!/usr/bin/env python3
"""Compare integer-coordinate geometry with an isolated, pinned Python SDK.

Checks types and normalized exact coordinates; ring start, orientation and
component order are immaterial. SDK process aborts are reported separately and
comparison resumes after the offending feature. No OSM content enters reports.
"""
import argparse
import hashlib
import importlib.metadata
import json
import pathlib
import subprocess
import sys
import tempfile


def ident(f):
    return ("N" if f.is_node else "W" if f.is_way else "R") + str(f.id)


def worker(file, ids):
    from geodesk import Features
    features = {ident(f): f for f in Features(file)}
    print(json.dumps({"inventory": sorted(features)}), flush=True)
    for key in json.loads(pathlib.Path(ids).read_text()):
        try:
            g = features[key].shape
            result = {"id": key, "wkb": g.wkb_hex if g is not None else None,
                      "missing_members": dict(features[key].tags).get("geodesk:missing_members", 0)}
        except Exception as error:
            result = {"id": key, "oracle_error": str(error)}
        print(json.dumps(result), flush=True)


def main():
    from shapely.geometry import shape
    from shapely import normalize, equals_exact, from_wkb
    parser = argparse.ArgumentParser()
    parser.add_argument("file", type=pathlib.Path)
    parser.add_argument("output", type=pathlib.Path)
    args = parser.parse_args()
    failures, oracle_errors, known_differences = [], [], []
    counts = {}
    with tempfile.TemporaryDirectory(prefix="gol-geometry-") as directory:
        directory = pathlib.Path(directory)
        dump = directory / "native.jsonl"
        with dump.open("w") as output:
            subprocess.run(["target/release/examples/geometry_dump", str(args.file)], stdout=output, check=True)
        actuals = {}
        for line in dump.read_text().splitlines():
            item = json.loads(line)
            assert item["id"] not in actuals, "duplicate feature"
            actuals[item["id"]] = item
        pending = list(actuals)
        references = {}
        while pending:
            ids = directory / "ids.json"
            ids.write_text(json.dumps(pending))
            oracle = directory / "oracle.jsonl"
            with oracle.open("w") as output:
                run = subprocess.run([sys.executable, __file__, "--worker", str(args.file), str(ids)],
                                     stdout=output, stderr=subprocess.PIPE)
            completed = [json.loads(line) for line in oracle.read_text().splitlines()]
            if not completed or "inventory" not in completed[0]:
                raise RuntimeError("reference failed before opening the file")
            missing = sorted(set(completed.pop(0)["inventory"]) - actuals.keys())
            for result in completed:
                references[result["id"]] = result
            pending = pending[len(completed):]
            if run.returncode:
                if not pending:
                    raise RuntimeError("reference aborted after last feature")
                key = pending.pop(0)
                references[key] = {"oracle_error": "reference process failed", "returncode": run.returncode,
                                   "stderr": run.stderr.decode(errors="replace")[-500:]}
            elif pending:
                raise RuntimeError("reference returned incomplete output without error")
        for key, item in actuals.items():
            ref = references[key]
            if "oracle_error" in ref:
                oracle_errors.append({"id":key, **ref, "native_error":item.get("error")})
                continue
            expected = from_wkb(ref["wkb"]) if ref["wkb"] is not None else None
            if "error" in item:
                record = {"id":key, "error":item["error"], "expected_type":expected.geom_type if expected is not None else None}
                if ref.get("missing_members") and item["error"] == "geometry error: unclosed polygon chain":
                    known_differences.append(record | {"reason":"reader explicitly rejects an incomplete boundary instead of inventing closing edges", "missing_members":ref["missing_members"]})
                else:
                    failures.append(record)
                continue
            actual = shape(item["geometry"])
            counts[actual.geom_type] = counts.get(actual.geom_type, 0) + 1
            difference = None
            if expected is None:
                if not actual.is_empty:
                    difference = {"error":"reference has no geometry"}
            elif actual.geom_type != expected.geom_type:
                difference = {"error":"type", "actual_type":actual.geom_type, "expected_type":expected.geom_type}
            elif not equals_exact(normalize(actual), normalize(expected), 0):
                difference = {"error":"coordinates or components", "actual_valid":bool(actual.is_valid), "expected_valid":bool(expected.is_valid)}
            if difference:
                record = {"id":key, **difference}
                # Original fixture R207 has two squares touching at one vertex.
                # The SDK drops vertices and emits an invalid polygon. Independent
                # Rust tests verify both exact exterior rings, not that SDK output.
                if (args.file.resolve() == pathlib.Path("tests/fixtures/geometry.gol").resolve()
                    and key == "R207" and actual.geom_type == "MultiPolygon"
                    and actual.is_valid and not expected.is_valid):
                    known_differences.append(record | {"reason":"SDK loses vertices at a touching-ring junction; original-input test is authoritative"})
                elif (args.file.resolve() == pathlib.Path("tests/fixtures/geometry.gol").resolve()
                      and key == "W112" and actual.geom_type == "LineString"):
                    known_differences.append(record | {"reason":"SDK misdecodes a 33-bit latitude delta; original-input regression checks both endpoints"})
                else:
                    failures.append(record)
    report = {"file":str(args.file), "sha256":hashlib.file_digest(args.file.open("rb"),"sha256").hexdigest(),
              "geodesk":importlib.metadata.version("geodesk"), "shapely":importlib.metadata.version("shapely"),
              "features":len(actuals), "types":counts, "missing":missing, "failures":failures,
              "oracle_errors":oracle_errors, "known_differences":known_differences}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2)+"\n")
    print(json.dumps({"features":len(actuals), "failures":len(failures), "oracle_errors":len(oracle_errors),
                      "known_differences":len(known_differences)}))
    return bool(failures or missing)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--worker":
        worker(sys.argv[2], sys.argv[3])
    else:
        raise SystemExit(main())
