#!/usr/bin/env python3
"""Write original synthetic OSM data as a minimal PBF; stdlib only.

Not a GOL writer. The external, pinned GOL tool is the fixture producer.
The JSON description is also an independent source of semantic expectations.
"""
import argparse
import json
import pathlib
import struct
import zlib


def varint(n):
    assert n >= 0
    result = bytearray()
    while n > 127:
        result.append((n & 127) | 128)
        n >>= 7
    result.append(n)
    return bytes(result)


def zigzag(n):
    return n * 2 if n >= 0 else -n * 2 - 1


def number(field, n):
    return varint(field << 3) + varint(n)


def blob(field, data):
    return varint((field << 3) | 2) + varint(len(data)) + data


def packed(field, values):
    return blob(field, b"".join(varint(v) for v in values))


def deltas(values):
    prev = 0
    for value in values:
        yield zigzag(value - prev)
        prev = value


def block(kind, data):
    body = number(2, len(data)) + blob(3, zlib.compress(data))
    header = blob(1, kind.encode()) + number(3, len(body))
    return struct.pack(">I", len(header)) + header + body


def dataset():
    nodes = [
        {"id": 1, "lon": 0.001, "lat": 0.001,
         "tags": {"amenity": "cafe", "name": "Café 東京", "capacity": "12", "height": "3.25", "empty": "", "negative": "-256", "large": "70000", "odd:key": "0012"}},
        {"id": 2, "lon": 0.002, "lat": 0.001, "tags": {}},
        {"id": 3, "lon": 0.002, "lat": 0.002, "tags": {}},
        {"id": 4, "lon": 0.001, "lat": 0.002, "tags": {}},
        {"id": 5, "lon": -0.001, "lat": -0.001, "tags": {"name": "West", "amenity": "bench"}},
        {"id": 6, "lon": 0.0012, "lat": 0.0012, "tags": {}},
        {"id": 7, "lon": 0.0018, "lat": 0.0012, "tags": {}},
        {"id": 8, "lon": 0.0018, "lat": 0.0018, "tags": {}},
        {"id": 9, "lon": 0.0012, "lat": 0.0018, "tags": {}},
    ]
    ways = [
        {"id": 10, "nodes": [5, 1, 2, 3], "tags": {"highway": "residential", "name": "Test Road"}},
        {"id": 11, "nodes": [1, 2, 3, 4, 1], "tags": {"building": "yes", "name": "Square"}},
        {"id": 12, "nodes": [6, 7, 8, 9, 6], "tags": {}},
    ]
    relations = [
        {"id": 20, "members": [["way", 11, "outer"], ["way", 12, "inner"]],
         "tags": {"type": "multipolygon", "landuse": "grass", "name": "Courtyard"}},
        {"id": 21, "members": [["node", 5, "stop"], ["way", 10, ""], ["node", 1, "custom rôle"], ["node", 5, "stop"]],
         "tags": {"type": "route", "route": "bus", "name": "Test Route"}},
        {"id": 22, "members": [["relation", 21, "part"], ["node", 1, "label"]],
         "tags": {"type": "collection", "name": "Nested"}},
    ]
    return {"nodes": nodes, "ways": ways, "relations": relations}


def stress_dataset():
    """Dense grid plus boundary/encoding cases; all data is original/synthetic."""
    nodes = []
    for row in range(32):
        for col in range(32):
            ident = row * 32 + col + 1
            tags = {"name": f"Grid {ident}", "amenity": ["bench", "cafe", "no", "school"][ident % 4],
                    "capacity": str(ident), "test:group": str(ident % 17)}
            nodes.append({"id": ident, "lon": (col - 16) * 0.0001, "lat": 51 + row * 0.0001, "tags": tags})
    for lon, lat in [(179.9999,0),(-179.9999,0),(0,85.0511287),(0,-85.0511287),(0,0),(-0.0000001,0)]:
        ident = len(nodes) + 1
        nodes.append({"id": ident, "lon": lon, "lat": lat, "tags": {"name": f"Boundary {ident}"}})
    nodes[0]["tags"].update({
        "bench": "rare-global-value-as-local-key",
        "long:key:" + "é" * 70: "東京" * 80,
        "empty": "", "lower": "-256", "below_lower": "-257", "upper_narrow": "65279",
        "above_narrow": "65280", "upper_wide": "1073741567", "above_wide": "1073741568",
        "decimal1": "-25.6", "decimal2": "-2.56", "decimal3": "-0.256", "trailing": "3.250",
        "noncanonical": "+12", "scientific": "1e3", "quoted": "a\"b\\c", "no": "no",
    })
    ways = []
    for i in range(96):
        start = 1 + (i // 16) * 32 + i % 16
        refs = [start, start + 1, start + 33, start + 32]
        area = i % 3 == 0
        ways.append({"id": 2000 + i, "nodes": refs + ([start] if area else []),
                     "tags": {"building": "yes", "name": f"Area {i}"} if area else {"highway": "path", "name": f"Path {i}"}})
    relations = []
    for i in range(8):
        members = [["way", 2000 + i * 3 + j, ["", "forward", "custom rôle"][j]] for j in range(3)]
        members += [["node", i + 1, "stop"], ["node", i + 1, "stop"]]
        relations.append({"id": 3000+i, "members": members, "tags": {"type": "route", "route": "bus", "name": f"Route {i}"}})
    return {"nodes": nodes, "ways": ways, "relations": relations}


def builder_dataset():
    data = dataset()
    for ident, lon, tags in [
        (30, 1, {}), (31, 1, {}), (32, 2, {}), (33, 2, {"name": "Tagged"}),
        (34, 3, {}), (35, 4, {}), (36, 5, {}), (37, 6, {}),
    ]:
        data["nodes"].append(dict(id=ident, lon=lon, lat=51, tags=tags))
    for i in range(300):
        tags = {"width": "4.7", "lanes": "2"}
        if i == 0:
            tags.update(height="4.8", ref="004")
        data["nodes"].append(dict(id=100+i, lon=i/10000, lat=50, tags=tags))
    data["ways"] += [
        dict(id=30, nodes=[32, 33, 34], tags={}),
        dict(id=31, nodes=[35, 36, 35], tags={"highway": "path"}),
        dict(id=32, nodes=[1, 2, 3, 1], tags={"type": "boundary"}),
        *[dict(id=40+i, nodes=[1, 2, 3, 1], tags={"building": value})
          for i, value in enumerate(["2", "01", "4.7", "4.8"])],
    ]
    for ident, tags, members in [
        (30, {"type": "multipolygon"}, [("way", 11, "inner"), ("way", 999, "outer")]),
        (31, {"type": "boundary", "geodesk:missing_members": "stale"},
         [("way", 11, "outer"), ("way", 999, "outer"), ("way", 999, "outer")]),
        (32, {"amenity": "park"}, [("way", 11, "outer")]),
        (33, {"type": "route", "area": "yes"}, [("way", 11, "outer")]),
        (34, {}, [("node", 37, "")]),
    ]:
        data["relations"].append(dict(id=ident, members=members, tags=tags))
    return data


def encode(data):
    strings = {"": 0}

    def sid(s):
        return strings.setdefault(s, len(strings))

    def tags(obj):
        return packed(2, (sid(k) for k in obj["tags"])) + packed(3, (sid(v) for v in obj["tags"].values()))

    groups = []
    chunks = []
    for n in data["nodes"]:
        if not chunks or len(chunks[-1]) >= 512 or any(abs(n[k] - chunks[-1][-1][k]) > 1 for k in ["lon", "lat"]):
            chunks.append([])
        chunks[-1].append(n)
    # Keep dense groups geographically local, as ordinary PBF producers do.
    # GOL Tool 2.0.0 misdecodes some very large inter-node coordinate deltas.
    for chunk in chunks:
        key_values = []
        for n in chunk:
            for k, v in n["tags"].items():
                key_values.extend([sid(k), sid(v)])
            key_values.append(0)
        dense = packed(1, deltas(n["id"] for n in chunk))
        dense += packed(8, deltas(round(n["lat"] * 1e7) for n in chunk))
        dense += packed(9, deltas(round(n["lon"] * 1e7) for n in chunk))
        dense += packed(10, key_values)
        groups.append(blob(2, dense))
    group = bytearray()
    for w in data["ways"]:
        group += blob(3, number(1, w["id"]) + tags(w) + packed(8, deltas(w["nodes"])))
    for r in data["relations"]:
        msg = number(1, r["id"]) + tags(r)
        msg += packed(8, (sid(m[2]) for m in r["members"]))
        msg += packed(9, deltas(m[1] for m in r["members"]))
        msg += packed(10, ({"node": 0, "way": 1, "relation": 2}[m[0]] for m in r["members"]))
        group += blob(4, msg)
    table = b"".join(blob(1, s.encode()) for s in strings)
    header = blob(4, b"OsmSchema-V0.6") + blob(4, b"DenseNodes") + blob(16, b"gol-reader synthetic fixtures")
    groups.append(bytes(group))
    return block("OSMHeader", header) + b"".join(block("OSMData", blob(1, table) + blob(2, g)) for g in groups)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("output", type=pathlib.Path)
    parser.add_argument("--builder", action="store_true")
    args = parser.parse_args()
    data = builder_dataset() if args.builder else dataset()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.with_suffix(".json").write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n")
    args.output.write_bytes(encode(data))
