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

# Numeric ids only: anything else would reach useradd / groupmod as an
# option or a name. 0 runs Ryokan as root, which works but drops the
# privilege separation this entrypoint exists for, so say so.
case "$PUID$PGID" in
    *[!0-9]*|'')
        echo "Error: PUID and PGID must be numeric (got PUID=$PUID PGID=$PGID)." >&2
        exit 1
        ;;
esac
if [ "$PUID" = "0" ] || [ "$PGID" = "0" ]; then
    echo "Warning: PUID or PGID is 0, so Ryokan runs as root. Set them to the owner of your media and download folders instead." >&2
fi

is_blank() {
    [ -z "$(printf '%s' "$1" | tr -d '[:space:]')" ]
}

# --- Data directory ---
# Ryokan's own state. Only this directory is chowned: with
# RYOKAN_DATA_DIR=/config, a /data mount is someone else's media
# filesystem and must be left alone.
#
# Resolved here exactly as the binary would resolve it, then exported,
# so the folder that gets chowned is the folder Ryokan uses:
#   - blank or whitespace-only counts as unset (`paths::env_nonblank`),
#     so `RYOKAN_DATA_DIR=` in compose means /data, not CWD-relative
#     `data` (/app/data, which the ryokan user can't write);
#   - a relative path resolves against the working directory (/app);
#     made absolute so useradd accepts it as a home directory.
DATA_DIR="${RYOKAN_DATA_DIR:-}"
if is_blank "$DATA_DIR"; then
    DATA_DIR=/data
fi
case "$DATA_DIR" in
    /*) ;;
    *) DATA_DIR="$(pwd)/$DATA_DIR" ;;
esac
mkdir -p "$DATA_DIR"
# Logical pwd normalizes trailing slashes, `.` and `..`.
DATA_DIR="$(cd "$DATA_DIR" && pwd)"
# A recursive chown of the container root (which would include every
# media mount) or of the app itself is never meant. Compared after
# resolving symlinks so `//`, `/data/..` or a link to / can't slip past.
case "$(cd "$DATA_DIR" && pwd -P)" in
    / | /app | /app/static)
        echo "RYOKAN_DATA_DIR=${RYOKAN_DATA_DIR} resolves to $(cd "$DATA_DIR" && pwd -P), which is not allowed. Point it at a dedicated directory such as /config." >&2
        exit 1
        ;;
esac
export RYOKAN_DATA_DIR="$DATA_DIR"

# The per-path variables still override single paths, but only the data
# directory is chowned. Pointing them elsewhere without moving
# RYOKAN_DATA_DIR (the pre-#259 workaround) leaves their folders with
# whatever owner they have and still chowns the data directory, which
# may be a shared media mount.
warn_outside_data_dir() {
    # $1: variable name, $2: the path it names
    is_blank "$2" && return 0
    case "$2" in
        "$DATA_DIR" | "$DATA_DIR"/*) return 0 ;;
    esac
    echo "Warning: $1 points outside RYOKAN_DATA_DIR ($DATA_DIR). Only $DATA_DIR is chowned, and it still is. To keep Ryokan's state somewhere else, set RYOKAN_DATA_DIR to that folder and remove $1." >&2
}
db_path="${DATABASE_URL:-}"
db_path="${db_path#sqlite://}"
db_path="${db_path#sqlite:}"
db_path="${db_path%%\?*}"
warn_outside_data_dir DATABASE_URL "$db_path"
warn_outside_data_dir RYOKAN_KEY_FILE_PATH "${RYOKAN_KEY_FILE_PATH:-}"
warn_outside_data_dir RYOKAN_MEDIA_CACHE_DIR "${RYOKAN_MEDIA_CACHE_DIR:-}"
warn_outside_data_dir RYOKAN_ANIBRIDGE_CACHE_DIR "${RYOKAN_ANIBRIDGE_CACHE_DIR:-}"

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
# The home directory is only a passwd field (nologin, nothing reads it);
# a `:` would corrupt the entry, so such a data dir gets a placeholder.
home_dir="$DATA_DIR"
case "$home_dir" in
    *:*) home_dir=/nonexistent ;;
esac
if ! id -u ryokan >/dev/null 2>&1; then
    useradd -o -u "$PUID" -g "$PGID" -d "$home_dir" -s /usr/sbin/nologin ryokan
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
# above + chown rather than relying on the image's own /data.
# `chown -h`: a symlink is re-owned itself, never its target. Plain
# chown followed links, so a link planted in the data dir (a restored
# archive, another container sharing the volume) had its target, say
# /etc/passwd, chowned to ryokan by this root process on a PUID change.
find "$DATA_DIR" \! -user ryokan -exec chown -h ryokan:ryokan {} + 2>/dev/null || true

exec gosu ryokan "$@"
