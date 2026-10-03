# Build stage
FROM rust:1-trixie AS builder

WORKDIR /app

# Cache dependency builds: copy manifests first, build deps, then copy source.
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs
RUN cargo build --release

# Now copy the real source and build.
# static/ is needed at compile time: src/handlers/settings.rs uses
# include_str!("../../static/default_custom_formats.json") to embed the
# bundled CF defaults into the binary.
COPY src/ src/
COPY templates/ templates/
COPY static/ static/
RUN touch src/main.rs && cargo build --release

# Runtime stage
FROM debian:trixie-slim

# ca-certificates: outbound HTTPS to AniList / Jikan / Kitsu / Nyaa.
# curl:            used by the compose healthcheck.
# gosu:            drops privileges from root to the ryokan user in the entrypoint.
# passwd:          provides useradd/groupadd/usermod/groupmod for the entrypoint.
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    gosu \
    passwd \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/ryokan /app/ryokan
COPY static/ /app/static/
COPY LICENSE /app/LICENSE
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

RUN mkdir -p /data

# Ryokan's own state (SQLite database, encryption key, artwork and
# anibridge caches, default backup folder) lives under this one
# directory (#259). Set it to e.g. /config and mount a volume there to
# free /data for a shared media / downloads mount; the entrypoint
# creates and chowns whichever directory this names. The per-path
# variables (DATABASE_URL, RYOKAN_KEY_FILE_PATH, RYOKAN_MEDIA_CACHE_DIR,
# RYOKAN_ANIBRIDGE_CACHE_DIR) still override single paths, but the
# image no longer sets them: an image-level value would pin that path
# to /data no matter what RYOKAN_DATA_DIR says. The derived defaults
# are the paths those variables used to name, so existing installs
# resolve to the same files.
ENV RYOKAN_DATA_DIR=/data
ENV LISTEN_ADDR=0.0.0.0:8978
ENV RUST_LOG=ryokan=info
# Default UID/GID for the ryokan user. Override via -e PUID=... / PGID=...
# to match the ownership of host-mounted media and download directories.
ENV PUID=1000
ENV PGID=1000

EXPOSE 8978

# start-period covers cold first-boot work before axum::serve binds:
# `models::migrate` (idempotent ALTER TABLEs across the schema),
# `bcrypt::warm_timing_equalizer` spawn_blocking, `rebuild_clients_cache`,
# optional Jellyfin client init. 10s was tight on a cold-data ARM64
# first-run; 30s matches the CI smoke-test poll budget.
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD curl -fsS http://localhost:8978/login || exit 1

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["/app/ryokan"]
