#!/bin/sh
# Run the alexandria-provider test suite with Hermes' PM venv interpreter (has httpx).
# Picks the newest venv that can import httpx, so it survives `hermes update` rebuilds.
ROOT="$(cd "$(dirname "$0")" && pwd)"
PY=""
for c in $(ls -t "$HOME"/.hermes/installs/*/environments/*/venv/bin/python 2>/dev/null); do
    if "$c" -c "import httpx" 2>/dev/null; then PY="$c"; break; fi
done
[ -n "$PY" ] || { echo "no Hermes venv python with httpx found" >&2; exit 1; }
exec "$PY" -m unittest discover -s "$ROOT/tests" -v "$@"
