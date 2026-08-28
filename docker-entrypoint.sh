#!/bin/sh
set -eu

# When the container is started with an explicit non-root --user, there is
# nothing to remap — just run the server directly.
if [ "$(id -u)" -ne 0 ]; then
    exec /usr/local/bin/alexandria "$@"
fi

# Unraid-style user remapping: the container starts as root (Unraid's
# Tailscale integration injects tailscaled into the container and needs root),
# then drops to the alexandria user remapped to PUID/PGID before exec'ing the
# server.
PUID="${PUID:-10001}"
PGID="${PGID:-10001}"

groupmod -o -g "$PGID" alexandria
usermod -o -u "$PUID" alexandria

umask "${UMASK:-022}"

chown -R alexandria:alexandria /data /home/alexandria

exec su-exec alexandria:alexandria /usr/local/bin/alexandria "$@"
