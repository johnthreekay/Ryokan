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

*Last updated: 2026-08-29.*
