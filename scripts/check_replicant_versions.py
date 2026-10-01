#!/usr/bin/env python3
"""Release gate: every binary that links replicant-client must link the same version.

Usage: check_replicant_versions.py PATH [PATH ...]
Each PATH is a binary or a directory (an app or plugin bundle), searched recursively.
Exits 1 when a PATH has no version marker, carries two versions, or the PATHs disagree.
"""

import os
import re
import sys

MARKER = re.compile(rb"replicant-client-version=([0-9A-Za-z.+-]+)\x00")


def files_under(path):
    if os.path.isfile(path):
        yield path
        return
    for root, _dirs, names in os.walk(path):
        for name in names:
            candidate = os.path.join(root, name)
            if os.path.isfile(candidate) and not os.path.islink(candidate):
                yield candidate


def versions_in(path):
    found = set()
    for file in files_under(path):
        with open(file, "rb") as handle:
            found.update(match.decode() for match in MARKER.findall(handle.read()))
    return found


def main(paths):
    if not paths:
        print(__doc__)
        return 1
    failed = False
    versions = {}
    for path in paths:
        found = versions_in(path)
        if not found:
            print(f"FAIL {path}: no replicant-client version marker")
            failed = True
        elif len(found) > 1:
            print(f"FAIL {path}: several replicant-client versions {sorted(found)}")
            failed = True
        else:
            versions[path] = found.pop()
            print(f"ok   {path}: {versions[path]}")
    if len(set(versions.values())) > 1:
        print(f"FAIL the binaries link different versions: {sorted(set(versions.values()))}")
        failed = True
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
