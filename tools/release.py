#!/usr/bin/env python3
"""Validate a release or build its distributable files from the locked workspace."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[1]
TARGET = "x86_64-unknown-linux-gnu"


def cargo(*args, cwd=ROOT):
    subprocess.run(["cargo", *args], cwd=cwd, check=True)


def add_dependency_notices(archive, prefix, workspace):
    metadata = json.loads(subprocess.check_output([
        "cargo", "metadata", "--locked", "--format-version", "1", "--features", "build,mmap,http",
        "--filter-platform", TARGET,
    ], cwd=workspace))
    for package in metadata["packages"]:
        if package["source"] is None:
            continue
        directory = Path(package["manifest_path"]).parent
        notices = sorted(p for p in directory.iterdir() if p.is_file() and
                         p.name.upper().startswith(("LICENSE", "COPYING", "COPYRIGHT", "NOTICE", "UNLICENSE")))
        if not notices:
            # Some upstream workspaces keep their notices outside the crate tarball.
            fallback = ROOT / "tools/licenses" / f"{package['name']}-{package['version']}"
            notices = sorted(p for p in fallback.glob("LICENSE*") if p.is_file())
        if not notices:
            raise ValueError(f"no license files found for {package['name']} {package['version']}")
        for notice in notices:
            relative = f"{package['name']}-{package['version']}/{notice.name}"
            archive.add(notice, arcname=f"{prefix}/licenses/{relative}")
    sysroot = Path(subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip())
    archive.add(sysroot / "share/doc/rust/COPYRIGHT-library.html",
                arcname=f"{prefix}/licenses/rust-standard-library.html")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="print the validated version and exit")
    parser.add_argument("--tag", help="require this exact release tag")
    args = parser.parse_args()
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"], cwd=ROOT))
    packages = {p["name"]: p for p in metadata["packages"]}
    version = packages["regol"]["version"]
    cli = packages["regol-cli"]
    dependency = next(d for d in cli["dependencies"] if d["name"] == "regol")
    if cli["version"] != version or dependency["req"] not in (f"^{version}", f"={version}"):
        parser.error("regol, regol-cli and the CLI's regol dependency must agree on the version")
    number = r"(?:0|[1-9][0-9]*)"
    if not re.fullmatch(rf"{number}\.{number}\.{number}(?:-rc\.{number})?", version):
        parser.error("use X.Y.Z or X.Y.Z-rc.N for a release version")
    if args.tag is not None and args.tag != f"v{version}":
        parser.error(f"tag {args.tag!r} does not match workspace version v{version}")
    notes = ROOT / "docs" / "releases" / f"{version}.md"
    if not notes.is_file() or not notes.read_text().strip():
        parser.error(f"missing release notes in {notes.relative_to(ROOT)}")
    if args.check:
        print(version)
        return

    target_dir = Path(metadata["target_directory"])
    # Cargo can package unpublished workspace dependencies together. Verify
    # the resulting archives below against each other before the first upload.
    cargo("package", "--locked", "--workspace", "--all-features", "--no-verify")
    binaries = target_dir / TARGET / "release"
    name = f"regol-{version}-{TARGET}"
    out = target_dir / "dist" / f"v{version}"
    out.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=out.parent) as temp:
        staging = Path(temp)
        packages = [target_dir / "package" / f"{pkg}-{version}.crate" for pkg in ("regol", "regol-cli")]
        workspace = staging / "sources"
        workspace.mkdir()
        for package in packages:
            with tarfile.open(package) as source:
                source.extractall(workspace, filter="data")
        (workspace / "Cargo.toml").write_text(
            f'[workspace]\nresolver = "3"\nmembers = ["regol-{version}", "regol-cli-{version}"]\n'
            f'[patch.crates-io]\nregol = {{ path = "regol-{version}" }}\n')
        shutil.copyfile(ROOT / "Cargo.lock", workspace / "Cargo.lock")
        cargo("test", "--locked", "-p", "regol-cli", "--target-dir", target_dir, cwd=workspace)
        cargo("build", "--release", "--locked", "--target", TARGET, "--target-dir", target_dir,
              "-p", "regol-cli", "--bin", "regol", cwd=workspace)
        reported = subprocess.check_output([binaries / "regol", "--version"], text=True).strip()
        if reported != f"regol {version}":
            parser.exit(1, f"unexpected binary version: {reported}\n")
        sample = staging / "sample.gol"
        subprocess.run([binaries / "regol", "build", ROOT / "tests/fixtures/basic.osm.pbf", sample], check=True)
        ids = subprocess.check_output([binaries / "regol", "query", sample, "na[amenity=cafe]",
                                       "-f", "ids"], text=True)
        if ids.splitlines() != ["N1"]:
            parser.exit(1, f"unexpected smoke test result: {ids!r}\n")
        # Only complete, smoke-tested assets are installed in the upload directory.
        archive = staging / f"{name}.tar.gz"
        with tarfile.open(archive, "w:gz") as tar:
            for path in (binaries / "regol", ROOT / "LICENSE", notes):
                filename = "RELEASE.md" if path == notes else path.name
                tar.add(path, arcname=f"{name}/{filename}")
            add_dependency_notices(tar, name, workspace)
        assets = [archive]
        for package in packages:
            assets.append(staging / package.name)
            shutil.copyfile(package, assets[-1])
        checksums = staging / "SHA256SUMS"
        with checksums.open("w") as output:
            for asset in assets:
                with asset.open("rb") as source:
                    digest = hashlib.file_digest(source, "sha256").hexdigest()
                output.write(f"{digest}  {asset.name}\n")
        out.mkdir(exist_ok=True)
        for asset in [*assets, checksums]:
            asset.replace(out / asset.name)
    print(out)


if __name__ == "__main__":
    main()
