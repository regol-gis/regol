#!/usr/bin/env python3
"""Optional all-feature differential check; does not download or redistribute data.

Run with the pinned Python oracle environment after building examples/dump.rs.
Input is an existing, immutable GOL 2.0 file. Dumps go to a temporary file, not
one unbounded stdout capture. Python's own feature index is test-only memory.
"""
import argparse
import hashlib
import importlib.metadata
import json
import pathlib
import subprocess
import tempfile
from geodesk import Features
from pbf_expected import read_ways

parser = argparse.ArgumentParser()
parser.add_argument("gol", type=pathlib.Path)
parser.add_argument("--reader", type=pathlib.Path, default=pathlib.Path("target/release/examples/dump"))
parser.add_argument("--output", type=pathlib.Path, required=True)
parser.add_argument("--source-pbf", type=pathlib.Path, help="Independently verify stored way-node IDs against original PBF")
args = parser.parse_args()
original_ways = read_ways(args.source_pbf) if args.source_pbf else None

def ident(f):
    return ("N" if f.is_node else "W" if f.is_way else "R") + str(f.id)

library = Features(str(args.gol))
reference = {ident(f): f for f in library}
seen = set()
failures = []
oracle_corrections = []
with tempfile.TemporaryFile(mode="w+") as dump:
    subprocess.run([str(args.reader.resolve()), str(args.gol)], stdout=dump, check=True)
    dump.seek(0)
    for line in dump:
        record = json.loads(line)
        key = record["id"]
        if key in seen:
            raise AssertionError(f"duplicate feature {key}")
        seen.add(key)
        f = reference[key]
        tags = {key: value if isinstance(value, str) else f.str(key) for key, value in dict(f.tags).items() if key != ""}
        expected = {"id": key, "area": f.is_area, "tags": tags,
                    "parents": sorted(ident(p) for p in f.parents.relations)}
        if f.is_node:
            expected["point"] = [f.x, f.y]
        if f.is_way:
            expected["nodes"] = [[n.id or None, n.x, n.y] for n in f.nodes]
            if original_ways is not None:
                ids = original_ways[f.id]
                if len(ids) != len(expected["nodes"]):
                    raise AssertionError(f"PBF/GOL vertex count differs for {key}")
                for node, original_id in zip(expected["nodes"], ids):
                    node[0] = original_id
                    # The SDK's way iterator mishandles 33-bit coordinate deltas.
                    # A separately stored node is an independent point record.
                    point_feature = reference.get("N" + str(original_id))
                    if point_feature is not None:
                        point = [point_feature.x, point_feature.y]
                        if node[1:] != point:
                            oracle_corrections.append({"id":key,"node_id":original_id,
                                "reason":"SDK way coordinate differs from its separately stored node record"})
                        node[1:] = point
        if f.is_relation:
            expected["members"] = [[ident(m), m.role] for m in f.members]
        record["parents"].sort()
        if record != expected:
            failures.append({"id": key, "fields": [field for field in expected if record.get(field) != expected[field]]})
missing = sorted(set(reference) - seen)
report = {"file": str(args.gol), "sha256": hashlib.file_digest(args.gol.open("rb"), "sha256").hexdigest(),
          "python_geodesk": importlib.metadata.version("geodesk"), "features": len(seen), "missing": missing, "failures": failures}
report["oracle_corrections"] = oracle_corrections
report["way_coordinates_oracle"] = "Separate node records when retained; SDK way iterator otherwise"
report["way_ids_oracle"] = "original PBF" if original_ways is not None else "Python (may omit retained IDs in 2.0.4)"
args.output.parent.mkdir(parents=True, exist_ok=True)
args.output.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n")
print(json.dumps({"features": len(seen), "missing": len(missing), "failures": len(failures)}))
raise SystemExit(bool(failures or missing))
