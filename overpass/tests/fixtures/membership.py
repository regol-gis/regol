#!/usr/bin/env python3
"""Build both membership fixtures with the GeoDesk GOL Tool and the Regol PBF encoder.

PYTHONPATH=/path/to/regol/tools python3 tests/fixtures/membership.py /path/to/gol
"""

from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory

from make_fixture import encode


def dataset():
    points = [
        (0.0002, 0.0002, "First"),
        (0.0002, 0.0002, "Coincident"),
        (0.0004, 0.0004, "Shared"),
        (0.0120, 0.0006, "East"),
        (0.0003, 0.0010, "Corner 1"),
        (0.0005, 0.0010, "Corner 2"),
        (0.00035, 0.0003, None),
    ]
    nodes = [
        {"id": ident, "lon": lon, "lat": lat, "tags": {"name": name} if name else {}}
        for ident, (lon, lat, name) in enumerate(points, 1)
    ]
    ways = [
        {"id": 10, "nodes": [1, 3, 1, 4], "tags": {"name": "Main", "highway": "path"}},
        {"id": 11, "nodes": [2, 3], "tags": {"name": "Decoy", "highway": "path"}},
        {"id": 12, "nodes": [1, 5, 6, 1], "tags": {"name": "Ring", "building": "yes"}},
        {"id": 13, "nodes": [1, 7, 3], "tags": {"name": "Geometry-only vertex", "highway": "path"}},
    ]
    relations = [
        {"id": 20, "members": [["node", 1, "stop"], ["node", 1, "stop"]],
         "tags": {"type": "route", "name": "Route"}},
    ]
    return {"nodes": nodes, "ways": ways, "relations": relations}


def main():
    gol = Path(sys.argv[1]).resolve()
    destination = Path(__file__).parent
    with TemporaryDirectory() as tmp:
        source = Path(tmp) / "membership.osm.pbf"
        source.write_bytes(encode(dataset()))
        for name, options in [("membership", []), ("membership-ids", ["-w"])]:
            output = source.with_name(f"{name}.gol")
            subprocess.run([gol, "build", output, source, *options], check=True)
            subprocess.run([gol, "check", output], check=True)
            (destination / output.name).write_bytes(output.read_bytes())


if __name__ == "__main__":
    main()
