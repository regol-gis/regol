#!/usr/bin/env python3
"""Download pinned test-only Java artifacts into a caller-selected directory."""
import hashlib
import json
import pathlib
import sys
import urllib.request

ARTIFACTS = [
    ("com/geodesk", "geodesk", "2.0.0"),
    ("org/eclipse/collections", "eclipse-collections", "11.1.0"),
    ("org/eclipse/collections", "eclipse-collections-api", "11.1.0"),
    ("org/locationtech/jts", "jts-core", "1.20.0"),
    ("org/ow2/asm", "asm", "9.7.1"),
]

destination = pathlib.Path(sys.argv[1])
destination.mkdir(parents=True, exist_ok=True)
manifest = []
for group, name, version in ARTIFACTS:
    filename = f"{name}-{version}.jar"
    url = f"https://repo.maven.apache.org/maven2/{group}/{name}/{version}/{filename}"
    data = urllib.request.urlopen(url).read()
    (destination / filename).write_bytes(data)
    manifest.append({"name": filename, "url": url, "sha256": hashlib.sha256(data).hexdigest()})
(destination / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
