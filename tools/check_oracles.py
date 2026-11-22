#!/usr/bin/env python3
"""Run pinned Python and independent Java SDKs against stored query expectations.

Run using the Python environment containing geodesk==2.0.4. Java artifacts are
downloaded separately by fetch_java_oracle.py. Results include failures rather
than treating a broken oracle as proof of reader incompatibility.
"""
import argparse
import importlib.metadata
import json
import os
import pathlib
import subprocess
import tempfile
from geodesk import Features
from fetch_java_oracle import stage_jars

ROOT = pathlib.Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument("java_directory", type=pathlib.Path)
parser.add_argument("output", type=pathlib.Path)
args = parser.parse_args()
version = importlib.metadata.version("geodesk")
if version != "2.0.4":
    raise SystemExit(f"Expected GeoDesk Python 2.0.4, got {version}")
manifest = json.loads((ROOT / "tests/fixtures/manifest.json").read_text())
with tempfile.TemporaryDirectory(prefix="gol-oracle-") as temporary:
    staging = pathlib.Path(temporary)
    artifacts = stage_jars(staging, args.java_directory)
    classpath = os.pathsep.join([temporary, *(str(staging / jar["name"]) for jar in artifacts)])
    subprocess.run(["javac", "-cp", classpath, "-d", temporary, str(ROOT / "tools/java_oracle.java")], check=True)
    report = {"python_version": version, "java_artifacts": artifacts, "results": {}}
    for name in manifest["variants"]:
        path = ROOT / "tests/fixtures" / f"{name}.gol"
        library = Features(str(path))
        expected = json.loads(path.with_suffix(".queries.json").read_text())
        # Java 2.0.0 reads an eight-byte child mask even when only four bytes
        # exist at EOF. Zero padding only the test copy avoids that oracle bug.
        padded = pathlib.Path(temporary) / path.name
        data = path.read_bytes()
        padded.write_bytes(data + bytes((-len(data)) % 4096))
        result = {"python_passed": 0, "java_passed": 0, "failures": {}}
        for text, want in expected.items():
            actual = sorted(("N" if f.is_node else "W" if f.is_way else "R") + str(f.id) for f in library(text))
            if actual == want:
                result["python_passed"] += 1
            else:
                result["failures"]["python:" + text] = {"actual": actual, "expected": want}
            proc = subprocess.run(["java", "-cp", classpath, "java_oracle", str(padded), text], capture_output=True, text=True, timeout=30)
            actual = sorted(proc.stdout.splitlines())
            if proc.returncode == 0 and actual == want:
                result["java_passed"] += 1
            else:
                result["failures"]["java:" + text] = {"returncode": proc.returncode, "stderr": proc.stderr, "actual": actual, "expected": want}
        report["results"][name] = result
args.output.parent.mkdir(parents=True, exist_ok=True)
args.output.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n")
print(json.dumps({n: {k: v for k, v in r.items() if k != "failures"} | {"failures": len(r["failures"])} for n, r in report["results"].items()}, indent=2))
