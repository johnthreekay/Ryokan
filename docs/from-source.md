# Build from source

You don't need this if you run the Docker image. This page is for contributing to Ryokan or running pre-release commits from the `dev` branch.

## What you need

- **Rust 1.95 or later**.
- **A C/C++ compiler and `cmake`**. Two of Ryokan's dependencies build native code (the anime title parser and the TLS library).
- **`mold` and `clang`** on Linux. Ryokan links with them on Linux because it makes rebuilds much faster. Without them the first build stops with `linker 'clang' not found` or `ld.mold not found`.
- **`cargo-nextest`** (optional) for the `cargo t` test shortcut. Plain `cargo test` works without it.

## Install the toolchain

Debian or Ubuntu:

```sh
sudo apt install mold clang cmake
```

Fedora:

```sh
sudo dnf install mold clang cmake
```

Arch:

```sh
sudo pacman -S mold clang cmake
```

macOS:

```sh
xcode-select --install     # Apple's compiler, once
brew install cmake
```

The mold linker setting only applies to Linux, so macOS builds use Apple's linker and need nothing else.

!!! note "macOS builds are untested"
    Ryokan's automated builds and tests run on Linux only, so a native macOS build may work but nothing checks it. On a Mac, including Apple Silicon, the supported way to run Ryokan is the Docker image. Docker Desktop, OrbStack, and Colima run its arm64 build natively, without emulation.

Then, on any of them:

```sh
cargo install cargo-nextest --locked
```

## Clone and run

```sh
git clone https://github.com/johnthreekay/Ryokan.git
cd Ryokan
cargo run                # http://localhost:8978; creates data/ryokan.db on first run
```

The first build takes a while. Rebuilds after that are quick.

## Run it as a service

Build a release binary with `cargo build --release`. A service needs two folders:

- **The install folder, as the working directory.** Ryokan loads its stylesheets and scripts from the `static` folder in the directory it starts in. Started anywhere else, pages load without styling. Either run it from the repository checkout, or copy `target/release/ryokan` and the `static` folder side by side, for example to `/opt/ryokan/ryokan` and `/opt/ryokan/static`.
- **The data folder**, for the database, the encryption key, the caches, and backups. By default it is `data` inside the working directory. Set `RYOKAN_DATA_DIR` to keep it apart from the install, and create it owned by the user Ryokan runs as:

```sh
sudo useradd --system --home-dir /var/lib/ryokan --shell /usr/sbin/nologin ryokan
sudo install -d -o ryokan -g ryokan /var/lib/ryokan
```

Any init system works, since all it needs is a working directory and one environment variable. The examples below are for Linux. On macOS, use the Docker image instead.

systemd, as `/etc/systemd/system/ryokan.service`:

```ini
[Unit]
Description=Ryokan
After=network-online.target
Wants=network-online.target

[Service]
User=ryokan
Group=ryokan
WorkingDirectory=/opt/ryokan
Environment=RYOKAN_DATA_DIR=/var/lib/ryokan
ExecStart=/opt/ryokan/ryokan
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

OpenRC, as `/etc/init.d/ryokan`:

```sh
#!/sbin/openrc-run
description="Ryokan"
command="/opt/ryokan/ryokan"
command_user="ryokan:ryokan"
command_background=true
pidfile="/run/${RC_SVCNAME}.pid"
directory="/opt/ryokan"
export RYOKAN_DATA_DIR=/var/lib/ryokan

depend() {
    need net
}
```

runit, as an executable `/etc/sv/ryokan/run`:

```sh
#!/bin/sh
exec 2>&1
cd /opt/ryokan || exit 1
export RYOKAN_DATA_DIR=/var/lib/ryokan
exec chpst -u ryokan:ryokan ./ryokan
```

To move an existing install's data, stop Ryokan and copy everything from the old `data` folder into the new one, including the hidden `.ryokan-key` file. Then set `RYOKAN_DATA_DIR` and start it. Without the copy, Ryokan starts with an empty library. Without the key file, linked AniList and MyAnimeList accounts have to be linked again.

## Tests and lints

```sh
cargo t                                                                  # the test suite
cargo fmt --all -- --check                                               # formatting (CI runs this first)
cargo clippy --workspace --all-targets --features test-support -- -D warnings   # lints, the way CI runs them
```

## Where next

- **[Configuration](configuration.md)**: the Settings tabs.
- **[Download clients](download-clients.md)**: per-client setup notes.
- **[Docker reference](docker.md)**: the environment variable table; the `RYOKAN_*` variables work the same way when running from source.

---

*Last updated: 2026-10-03.*
