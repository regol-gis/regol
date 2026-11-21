#!/usr/bin/env python3
"""Require a permissive licensing choice in every dependency's SPDX expression."""
import argparse
import json
import re
import subprocess

ALLOWED = {"Apache-2.0", "MIT", "BSD-2-Clause", "BSD-3-Clause", "ISC", "Zlib", "Unicode-3.0", "Unlicense", "0BSD", "NCSA"}


def permitted(expression):
    # Cargo also accepts this legacy spelling of the dual MIT/Apache license.
    if expression == "MIT/Apache-2.0":
        expression = "MIT OR Apache-2.0"
    # AND requires every term; OR permits selecting a permissive alternative.
    # A WITH-qualified branch is not implicitly approved. A separate permitted
    # OR alternative may still be selected, as required by the SPDX grammar.
    tokens = re.findall(r'[A-Za-z0-9][A-Za-z0-9.+-]*|[()]', expression)
    if ''.join(tokens) != re.sub(r'\s+', '', expression): return False
    position = 0
    def factor():
        nonlocal position
        token = tokens[position]
        position += 1
        if token == '(':
            result = disjunction()
            if tokens[position] != ')': raise ValueError('unclosed expression')
            position += 1
            return result
        if token in {'AND','OR',')','WITH'}: raise ValueError('invalid SPDX term')
        if position < len(tokens) and tokens[position] == 'WITH':
            position += 1
            if position == len(tokens) or tokens[position] in {'AND','OR','(',')','WITH'}:
                raise ValueError('invalid SPDX exception')
            position += 1
            return False
        return token in ALLOWED
    def conjunction():
        nonlocal position
        result = factor()
        while position < len(tokens) and tokens[position] == 'AND':
            position += 1
            right = factor()
            result = result and right
        return result
    def disjunction():
        nonlocal position
        result = conjunction()
        while position < len(tokens) and tokens[position] == 'OR':
            position += 1
            right = conjunction()
            result = result or right
        return result
    try:
        result = disjunction()
        return result and position == len(tokens)
    except (IndexError, ValueError):
        return False


def main():
    for expression, expected in [('MIT/Apache-2.0', True), ('MIT/GPL-3.0', False), ('MIT OR LGPL-2.1-or-later', True), ('MIT AND GPL-3.0', False),
                                 ('(MIT OR Apache-2.0) AND NCSA', True), ('MIT WITH Unknown', False),
                                 ('MIT AND (BSD-2-Clause OR Unknown)', True), ('MIT OR',False),
                                 ('MIT garbage',False), ('',False),
                                 ('Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT',True),
                                 ('MIT WITH Unknown OR GPL-3.0',False),
                                 ('MIT AND Unknown WITH Exception',False),
                                 ('MIT OR Unknown WITH Exception',True), ('MIT WITH',False)]:
        assert permitted(expression) == expected
    parser = argparse.ArgumentParser()
    parser.add_argument('--manifest-path', default='Cargo.toml')
    args = parser.parse_args()
    metadata = json.loads(subprocess.check_output(['cargo','metadata','--manifest-path',args.manifest_path,'--locked','--offline','--all-features','--format-version','1']))
    failed = False
    for package in sorted(metadata['packages'], key=lambda p:p['name']):
        expression = package.get('license') or 'UNKNOWN'
        okay = permitted(expression)
        print(f"{'OK' if okay else 'REJECT'} {package['name']} {package['version']}: {expression}")
        failed |= not okay
    return failed


if __name__ == '__main__':
    raise SystemExit(main())
