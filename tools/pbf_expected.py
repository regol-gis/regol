"""Minimal independent OSM-PBF way-reference reader for conformance checks.

Only standard-library code; unsupported compression is an error. This does not
read GOL and is never included in the Rust runtime.
"""
import struct
import zlib


def varint(data, pos):
    value = 0
    for shift in range(0, 70, 7):
        byte = data[pos]
        pos += 1
        value |= (byte & 127) << shift
        if byte < 128:
            return value, pos
    raise ValueError("invalid varint")


def fields(data):
    pos = 0
    while pos < len(data):
        tag, pos = varint(data, pos)
        field, wire = tag >> 3, tag & 7
        if wire == 0:
            value, pos = varint(data, pos)
        elif wire == 2:
            length, pos = varint(data, pos)
            value = data[pos:pos+length]
            if len(value) != length:
                raise ValueError("truncated protobuf field")
            pos += length
        elif wire in (1, 5):
            length = 8 if wire == 1 else 4
            value = data[pos:pos+length]
            pos += length
        else:
            raise ValueError(f"unsupported wire type {wire}")
        yield field, wire, value


def packed(data):
    pos = 0
    while pos < len(data):
        value, pos = varint(data, pos)
        yield value


def read_ways(path):
    result = {}
    with open(path, "rb") as source:
        while prefix := source.read(4):
            length, = struct.unpack(">I", prefix)
            header = dict((k, v) for k, _, v in fields(source.read(length)))
            body = dict((k, v) for k, _, v in fields(source.read(header[3])))
            if header[1] != b"OSMData":
                continue
            if 1 in body:
                data = body[1]
            elif 3 in body:
                data = zlib.decompress(body[3])
            else:
                raise ValueError("unsupported PBF compression")
            for field, _, group in fields(data):
                if field != 2:
                    continue
                for kind, _, obj in fields(group):
                    if kind != 3:
                        continue
                    ident = None
                    refs = []
                    previous = 0
                    for key, wire, value in fields(obj):
                        if key == 1:
                            ident = value
                        elif key == 8:
                            for delta in packed(value) if wire == 2 else [value]:
                                previous += (delta >> 1) ^ -(delta & 1)
                                refs.append(previous)
                    result[ident] = refs
    return result
