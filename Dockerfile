# Base images are pinned by digest (multi-arch index, so amd64 and
# arm64 alike); the tag only says what the digest is. Dependabot's
# docker updates bump the digest. A tag alone is whatever the registry
# serves at build time.

# Build stage
FROM rust:1-trixie@sha256:5d05167b28cef0fa3a6c781cd77949386848191f3382e82cf53bd1277a47a98f AS builder

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

# ffprobe stage. Source classification shells out to `ffprobe`
# (services::source_ffprobe), and Debian's ffmpeg package would add
# ~470 MB of codec libraries for it. This builds a ~4 MB static ffprobe
# cut down to the fields that layer reads: demuxers for every extension
# post-processing imports (mkv/webm, mp4/m4v, avi, wmv, flv, ts), the
# codec parsers, and the video decoders, which fill in `pix_fmt` (the
# bit-depth signal). Audio and subtitle codec names come from the
# container, so no audio or subtitle decoders. zlib covers mkv tracks
# compressed with it (older mkvmerge did that to subtitles by default).
# -static so the binary needs nothing from the runtime image; it runs
# once here so each native CI runner (amd64 and arm64) executes it.
#
# To update: bump FFMPEG_VERSION and set FFMPEG_SHA256 to the sha256 of
# that release tarball, after checking its .asc signature against
# FFmpeg's release key (https://ffmpeg.org/download.html#releases).
# A new importable extension needs its demuxer added to the list.
FROM debian:trixie-slim@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a AS ffprobe

ARG FFMPEG_VERSION=9.0.2
ARG FFMPEG_SHA256=8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    gcc \
    libc6-dev \
    make \
    xz-utils \
    zlib1g-dev

WORKDIR /src
RUN curl -fsSLo ffmpeg.tar.xz "https://ffmpeg.org/releases/ffmpeg-${FFMPEG_VERSION}.tar.xz" \
    && echo "${FFMPEG_SHA256}  ffmpeg.tar.xz" | sha256sum -c - \
    && tar xf ffmpeg.tar.xz --strip-components=1
RUN ./configure \
        --disable-everything --disable-autodetect --disable-doc --disable-debug \
        --disable-network --disable-programs --enable-ffprobe \
        --disable-avdevice --disable-avfilter --disable-swscale --disable-swresample \
        --disable-x86asm --enable-small --enable-zlib \
        --enable-protocol=file \
        --enable-demuxer=matroska,mov,avi,asf,flv,mpegts \
        --enable-parser=h264,hevc,av1,mpegvideo,mpeg4video,vc1,aac,aac_latm,ac3,flac,dca,mlp,opus,mpegaudio,vp8,vp9 \
        --enable-decoder=h264,hevc,av1,mpeg1video,mpeg2video,mpeg4,msmpeg4v3,wmv1,wmv2,wmv3,vc1,vp8,vp9 \
        --extra-ldflags=-static \
    && make -j"$(nproc)" ffprobe \
    && strip ffprobe \
    && ./ffprobe -hide_banner -version > /dev/null

# Runtime stage
FROM debian:trixie-slim@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a

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
COPY --from=ffprobe /src/ffprobe /usr/local/bin/ffprobe
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
