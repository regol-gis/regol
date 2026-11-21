#!/usr/bin/env python3
"""Record encoding coverage for the original wide-reference fixtures.

This inspects word widths, not feature decoding; expected semantics come from
original PBF data and the separate SDK differential checks.
"""
import argparse
import collections
import hashlib
import json
import pathlib
import struct


def inspect(path, relation_id, way_id):
    data = path.read_bytes()
    def u16(p): return struct.unpack_from('<H', data, p)[0]
    def i16(p): return struct.unpack_from('<h', data, p)[0]
    def u32(p): return struct.unpack_from('<I', data, p)[0]
    def i32(p): return struct.unpack_from('<i', data, p)[0]
    def signed(n): return n if n < 2**31 else n - 2**32
    snapshot = 128 + data[16] * 64
    index = u32(snapshot + 24) * 4096
    entry = index + 4
    if u32(entry) & 1: entry += i32(entry) & ~3
    tile = (u32(entry) >> 2) * 4096
    end = tile + 4 + (u32(tile) & 0x3fffffff)
    anchors = {}
    for p in (range(0, len(data)-15, 4) if len(data) < 1048576 else range(tile+16, end-15, 4)):
        word = struct.unpack_from('<Q', data, p)[0]
        if (word >> 12, (word >> 3) & 3) in [(relation_id, 2), (way_id, 1)]:
            anchors[word >> 12] = p
    counts = collections.Counter()
    ranges = collections.defaultdict(list)
    a = anchors[relation_id]
    p = a + 12 + i32(a+12)
    while True:
        word = u16(p)
        width = 2 if word & 2 and not word & 16 else 4
        word = i16(p) if width == 2 else i32(p)
        p += width
        if word & 2:
            counts['member_foreign'] += 1
            counts['member_wide_export'] += bool(word & 16)
            ranges['member_export_delta'].append(word >> 5)
            if word & 8:
                width = 4 if u16(p) & 1 else 2
                counts['member_wide_tile'] += width == 4
                ranges['member_tile_delta'].append((i32(p) if width == 4 else i16(p)) >> 1)
                p += width
        if word & 4: p += 2 if u16(p) & 1 else 4
        if word & 1: break
    a = anchors[way_id]
    p = a + 12 + i32(a+12) - 4 - (u16(a) & 4)
    while True:
        raw = u32(p)
        narrow = raw & (2 << 16) and not raw & (8 << 16)
        word = signed(raw) >> 16 if narrow else signed(((raw << 16) | (raw >> 16)) & 0xffffffff)
        cursor = p + 2 if narrow else p
        if word & 2:
            counts['way_node_foreign'] += 1
            counts['way_node_wide_export'] += bool(word & 8)
            ranges['way_node_export_delta'].append(word >> 4)
            if word & 4:
                cursor -= 2
                low = i16(cursor)
                if low & 1:
                    cursor -= 2
                    delta = (i16(cursor) << 15) | ((low >> 1) & 0x7fff)
                    counts['way_node_wide_tile'] += 1
                else:
                    delta = low >> 1
                ranges['way_node_tile_delta'].append(delta)
        if word & 1: break
        p = cursor - 4
    return {"file":str(path),"sha256":hashlib.sha256(data).hexdigest(), "bytes":len(data),
            "tile_count":u32(snapshot+28), "counts":dict(counts),
            "delta_ranges":{k:[min(v),max(v)] for k,v in ranges.items()}}


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('file', type=pathlib.Path)
    parser.add_argument('relation_id', type=int)
    parser.add_argument('way_id', type=int)
    parser.add_argument('output', type=pathlib.Path)
    args = parser.parse_args()
    result = inspect(args.file,args.relation_id,args.way_id)
    args.output.write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps(result))
