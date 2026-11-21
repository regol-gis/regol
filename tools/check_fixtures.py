#!/usr/bin/env python3
"""Verify pinned fixture inputs, binary stores and retained oracle outputs."""
import hashlib
import json
import pathlib

root = pathlib.Path(__file__).resolve().parents[1] / 'tests/fixtures'
manifest = json.loads((root / 'manifest.json').read_text())

def check(name, expected):
    actual = hashlib.sha256((root / name).read_bytes()).hexdigest()
    if actual != expected:
        raise SystemExit(f'{name}: hash mismatch; regenerate the fixture manifest intentionally')

check(manifest['input'], manifest['input_sha256'])
for name, entry in manifest['variants'].items():
    check(entry['input'], entry['input_sha256'])
    check(name + '.gol', entry['sha256'])
    check(name + '.oracle.geojson', entry['oracle_sha256'])
    check(name + '.queries.json', entry['queries_sha256'])
    check(str(pathlib.Path(entry['input']).with_suffix('.json')), entry['expected_input_sha256'])
# The CLI is a separate crate. Its packaged tests carry these two fixtures.
for variant in ('basic', 'ids'):
    cli_fixture = root.parents[1] / 'cli' / 'tests' / 'fixtures' / f'{variant}.gol'
    if hashlib.sha256(cli_fixture.read_bytes()).hexdigest() != manifest['variants'][variant]['sha256']:
        raise SystemExit(f'{cli_fixture}: expected the library fixture for {variant}')
# The writer conformance test compares these two files directly.
check('builder.osm.pbf', '9f0bdcd1e230a93f6720e90f1c13786bd3e41b60d880408ee1f990097a35c035')
check('builder.gol', '08529d904e7b188236fb5cd587b4a9a326879918a4b5f907dc4527c91ccdc0f5')
print(f"Verified {len(manifest['variants'])} fixture variants with their independent inputs/oracles and the builder conformance pair")
