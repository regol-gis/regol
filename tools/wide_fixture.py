"""Original large-reference fixtures; the tile case is an optional stress input."""
import argparse
import json
import pathlib
from make_fixture import encode


def export_dataset():
    nodes = [{"id":i+1,"lon":(10 if i<5000 else 20)+(i%5000)*.000001,
              "lat":50+(i%5000)*.000001,"tags":{"name":f"Vertex {i+1}"}} for i in range(10000)]
    order = [n for pair in zip(range(1,5001),range(10000,5000,-1)) for n in pair]
    return {"nodes":nodes,"ways":[{"id":10001,"nodes":order,"tags":{"highway":"path"}}],
            "relations":[{"id":20001,"members":[["node",i,""] for i in order]+[["way",10001,""]],"tags":{"type":"route"}}]}


def tile_dataset():
    nodes = [{"id":r*120+c+1,"lon":-89.5+c*179/119,"lat":-79.5+r*159/119,
              "tags":{"name":"Vertex"}} for r in range(120) for c in range(120)]
    order = [n for pair in zip(range(1,7201),range(14400,7200,-1)) for n in pair]
    return {"nodes":nodes,"ways":[{"id":40001,"nodes":[1,14400],"tags":{"highway":"path"}}],
            "relations":[{"id":50001,"members":[["node",i,""] for i in order]+[["way",40001,""]],"tags":{"type":"route"}}]}


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("kind", choices=["exports","tiles"])
    parser.add_argument("output", type=pathlib.Path)
    args = parser.parse_args()
    data = export_dataset() if args.kind == "exports" else tile_dataset()
    args.output.write_bytes(encode(data))
    args.output.with_suffix(".json").write_text(json.dumps(data, indent=2)+"\n")
