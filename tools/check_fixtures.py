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
# The CLI is a separate crate. Its packaged tests carry these three fixtures.
for variant in ('basic', 'ids', 'tiled'):
    cli_fixture = root.parents[1] / 'cli' / 'tests' / 'fixtures' / f'{variant}.gol'
    if hashlib.sha256(cli_fixture.read_bytes()).hexdigest() != manifest['variants'][variant]['sha256']:
        raise SystemExit(f'{cli_fixture}: expected the library fixture for {variant}')
# The writer conformance test compares these two files directly.
check('builder.osm.pbf', '9f0bdcd1e230a93f6720e90f1c13786bd3e41b60d880408ee1f990097a35c035')
check('builder.gol', '08529d904e7b188236fb5cd587b4a9a326879918a4b5f907dc4527c91ccdc0f5')
# Reference GOB snapshots exported from the pinned GOL fixtures.
check('basic.gob', '0cec72174ca0809d595aeaeb40231379ba19c2f54e611333a4c366ff0f79efb7')
check('geometry.gob', 'fef0f27eeb5a5bd8e6c49a81fb5c2666c9435f5180c39d90d003fd29ff2f2a7b')
check('tiled_global.gob', 'a1111306e184a6b45439de24cc9af11a4037e2286be29ad7db8fcfee27e892ed')
print(f"Verified {len(manifest['variants'])} fixture variants with their independent inputs/oracles, the builder conformance pair and three GOB snapshots")
