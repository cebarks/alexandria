#!/bin/sh
# Python 3.12+ with httpx installed; no Hermes installation is required.
set -eu
ROOT="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
exec "${PYTHON:-python3}" -m unittest discover -s "$ROOT/tests" -v "$@"
