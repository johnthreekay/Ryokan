#!/bin/sh
# Ryokan Docker entrypoint.
#
# Creates (or updates) a `ryokan` user whose UID/GID match the PUID/PGID
# environment variables, fixes ownership on Ryokan's data directory
# (RYOKAN_DATA_DIR, /data by default), then drops privileges via gosu
# before execing the Ryokan binary.
#
# This matches the linuxserver.io PUID/PGID convention used by the
# Sonarr/Radarr/Jellyfin ecosystem so files written by Ryokan's
# post-processor share ownership with the rest of a typical *arr stack.

set -e

PUID="${PUID:-1000}"
PGID="${PGID:-1000}"
# Ryokan's own state. Only this directory is chowned: with
# RYOKAN_DATA_DIR=/config, a /data mount is someone else's media
# filesystem and must be left alone.
DATA_DIR="${RYOKAN_DATA_DIR:-/data}"

# --- Group ---
if ! getent group ryokan >/dev/null 2>&1; then
    groupadd -o -g "$PGID" ryokan
else
    current_gid=$(getent group ryokan | cut -d: -f3)
    if [ "$current_gid" != "$PGID" ]; then
        groupmod -o -g "$PGID" ryokan
    fi
fi

# --- User ---
if ! id -u ryokan >/dev/null 2>&1; then
    useradd -o -u "$PUID" -g "$PGID" -d "$DATA_DIR" -s /usr/sbin/nologin ryokan
else
    current_uid=$(id -u ryokan)
    if [ "$current_uid" != "$PUID" ]; then
        usermod -o -u "$PUID" ryokan
    fi
fi

# --- Data directory ownership ---
# Only touch files that aren't already correctly owned. On a warm start
# this is a no-op scan; on a PUID change it quietly re-chowns everything.
# The user-mounted /downloads and /media/* paths are intentionally left
# alone — those belong to the host. A fresh named volume at a path the
# image doesn't ship (e.g. /config) arrives root-owned, hence the mkdir
# + chown rather than relying on the image's own /data. "/" is refused
# outright: a recursive chown of the whole container is never meant.
if [ "$DATA_DIR" = "/" ]; then
    echo "RYOKAN_DATA_DIR=/ is not allowed; point it at a dedicated directory." >&2
    exit 1
fi
mkdir -p "$DATA_DIR"
find "$DATA_DIR" \! -user ryokan -exec chown ryokan:ryokan {} + 2>/dev/null || true

exec gosu ryokan "$@"
