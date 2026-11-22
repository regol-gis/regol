#!/usr/bin/env python3
"""Download pinned test-only Java artifacts into a caller-selected directory."""
import hashlib
import json
import pathlib
import sys
import tempfile
import urllib.request

ARTIFACTS = [
    ("com/geodesk", "geodesk", "2.0.0", "87fcddcc16d22c81fe45c02eb9ecc9e72c25de962df9e166a2a9a97dfff34ea3"),
    ("org/eclipse/collections", "eclipse-collections", "11.1.0", "8881749de791c28c6ccea621314a3a154cfe76ef77c30054b6e714c3dc76969d"),
    ("org/eclipse/collections", "eclipse-collections-api", "11.1.0", "1b8608ecf4e681f031c29a28c403f71476fe5df3a8f0c0dd8c7ad33b91984212"),
    ("org/locationtech/jts", "jts-core", "1.20.0", "6a783d8f9dba3d3cf7265435f134402f63c05838aa6cbcc4297ad3a5b2842baf"),
    ("org/ow2/asm", "asm", "9.7.1", "8cadd43ac5eb6d09de05faecca38b917a040bb9139c7edeb4cc81c740b713281"),
]


def stage_jars(destination, cache=None):
    manifest = []
    for group, name, version, digest in ARTIFACTS:
        filename = f"{name}-{version}.jar"
        url = f"https://repo.maven.apache.org/maven2/{group}/{name}/{version}/{filename}"
        source = (cache / filename).open("rb") if cache is not None else urllib.request.urlopen(url, timeout=30)
        with source:
            data = source.read(16 * 1024 * 1024 + 1)
        if hashlib.sha256(data).hexdigest() != digest:
            raise ValueError(f"SHA-256 mismatch for {filename}")
        (destination / filename).write_bytes(data)
        manifest.append({"name": filename, "url": url, "sha256": digest})
    return manifest


if __name__ == "__main__":
    destination = pathlib.Path(sys.argv[1])
    destination.mkdir(mode=0o700, parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=destination) as temp:
        staging = pathlib.Path(temp)
        manifest = stage_jars(staging)
        (staging / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        for path in staging.iterdir():
            path.replace(destination / path.name)
