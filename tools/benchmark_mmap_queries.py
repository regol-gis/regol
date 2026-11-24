"""Compare unconfigured 'regol query' binaries on a disposable, immutable UK GOL (Linux)."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import subprocess
import time


CASES = {
    "boundary": ["a[boundary=administrative][admin_level=8][name=Cambridge]", "-f", "ids"],
    "london-cafes": ["na[amenity=cafe]", "--bbox=-0.5104,51.2868,0.334,51.6919", "-f", "ids"],
    "uk-cafes": ["na[amenity=cafe]", "-f", "ids"],
    "highway-count": ["w[highway]"],
    "stations-count": ["n[railway=station]"],
    "small-window": ["na[amenity=cafe]", "--bbox=-0.135,51.503,-0.12,51.515", "-f", "ids"],
}


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def pages(path):
    data = subprocess.check_output(["fincore", "--json", "--bytes", "-o", "PAGES", path])
    return int(json.loads(data)["fincore"][0]["pages"])


def disk(stat):
    fields = stat.read_text().split()
    return int(fields[0]), int(fields[2]) * 512


def fingerprint(output):
    return hashlib.sha256(b"\n".join(sorted(output.splitlines()))).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("gol", type=Path, help="private copy; the harness evicts this file repeatedly")
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("--device", required=True, help="backing whole disk, e.g. sda")
    parser.add_argument("--rounds", type=int, default=2)
    parser.add_argument("--seed", type=int, default=20261124)
    parser.add_argument("--cases", nargs="+", choices=CASES, default=list(CASES))
    args = parser.parse_args()
    if args.rounds < 1:
        parser.error("--rounds must be positive")
    paths = {"before": args.before.resolve(), "after": args.after.resolve()}
    gol = args.gol.resolve()
    stat = Path("/sys/block") / args.device / "stat"
    expected = {}
    print(json.dumps(dict(kernel=platform.release(), cpus=sorted(os.sched_getaffinity(0)),
                          gol_sha256=digest(gol), binaries={label: digest(path) for label, path in paths.items()},
                          seed=args.seed, rounds=args.rounds)), flush=True)
    for case in args.cases:
        ref = subprocess.check_output([paths["before"], "query", gol, *CASES[case], "--threads", "1"])
        expected[case] = fingerprint(ref)
    rng = random.Random(args.seed)
    with gol.open("rb") as file:
        for lap in range(1, args.rounds + 1):
            schedule = [(case, label) for case in args.cases for label in paths]
            rng.shuffle(schedule)
            for case, label in schedule:
                for _ in range(8):
                    os.posix_fadvise(file.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
                    if pages(gol) == 0:
                        break
                    time.sleep(0.25)
                else:
                    raise RuntimeError("could not evict benchmark file")
                for cache in ("cold", "repeat"):
                    resident = pages(gol)
                    if cache == "cold" and resident:
                        raise RuntimeError("input warmed before starting process")
                    reads, size = disk(stat)
                    start = time.monotonic_ns()
                    out = subprocess.check_output([paths[label], "query", gol, *CASES[case]])
                    ms = (time.monotonic_ns() - start) / 1e6
                    end_reads, end_size = disk(stat)
                    signature = fingerprint(out)
                    if signature != expected[case]:
                        raise RuntimeError(f"wrong result: {case} {label}")
                    count = int(out) if case.endswith("-count") else len(out.splitlines())
                    print(json.dumps(dict(lap=lap, case=case, binary=label, cache=cache,
                                          resident_pages=resident, ms=round(ms, 3), count=count,
                                          result_sha256=signature, reads=end_reads - reads,
                                          read_bytes=end_size - size)), flush=True)


if __name__ == "__main__":
    main()
