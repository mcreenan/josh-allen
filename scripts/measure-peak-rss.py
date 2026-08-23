#!/usr/bin/env python3
"""Run one command and report its Darwin peak resident set size in bytes."""

from __future__ import annotations

import resource
import subprocess
import sys


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: measure-peak-rss.py COMMAND [ARG ...]", file=sys.stderr)
        return 2

    completed = subprocess.run(sys.argv[1:], check=False)
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    print(f"{usage.ru_maxrss} maximum resident set size", file=sys.stderr)
    return completed.returncode


if __name__ == "__main__":
    raise SystemExit(main())
