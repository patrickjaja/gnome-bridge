#!/usr/bin/env bash
# Reproduce the CI cross-build pipeline against the local source tree.
#
# Base image is ubuntu:noble (glibc 2.39, PipeWire 1.0.5) - the oldest base
# whose PipeWire satisfies what lamco-pipewire 0.4.x actually calls:
# pw_stream_get_nsec, pw_stream_get_time_n, and the extended pw_time fields
# (PipeWire >= 1.0.5), plus the spa_video_info_raw layout libspa 0.9.x needs
# (>= 0.3.65). Note lamco-pipewire DECLARES a 0.3.33 pkg-config floor, but
# that metadata is wrong - jammy (0.3.48) and Debian 12 (0.3.65) can neither
# compile nor load the binary (Rust links BIND_NOW, missing .so symbols fail
# at startup). Runtime floor: Ubuntu 24.04+, Fedora 40+, Debian 13+.
#
# Unlike x11-bridge (pure Rust, static musl), this bridge links libpipewire
# dynamically via lamco-pipewire, so it is a glibc-dynamic build. The pipewire
# sys crates use bindgen, so clang + libclang are required in the image.
#
# Output binaries land in ./dist/ (override with OUTPUT_DIR=...):
#   dist/gnome-portal-bridge          (x86_64, dynamic glibc)
#   dist/gnome-portal-bridge-aarch64  (aarch64, dynamic glibc)
# A persistent build cache lives in ./.pipeline-cache/ so re-runs are fast.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="${OUTPUT_DIR:-${REPO_ROOT}/dist}"
CACHE_DIR="${CACHE_DIR:-${REPO_ROOT}/.pipeline-cache}"
IMAGE="${IMAGE:-ubuntu:noble}"
RUST_VERSION="${RUST_VERSION:-stable}"

mkdir -p "${OUTPUT_DIR}"
mkdir -p "${CACHE_DIR}/target" "${CACHE_DIR}/cargo-registry" "${CACHE_DIR}/cargo-git" \
         "${CACHE_DIR}/rustup" "${CACHE_DIR}/apt-archives" "${CACHE_DIR}/apt-lists"

echo ">>> Building gnome-portal-bridge from ${REPO_ROOT}"
echo ">>> Base:    ${IMAGE}"
echo ">>> Output:  ${OUTPUT_DIR}"
echo ">>> Cache:   ${CACHE_DIR}"

docker run --rm \
  -v "${REPO_ROOT}:/src:ro" \
  -v "${OUTPUT_DIR}:/output" \
  -v "${CACHE_DIR}/target:/build/target" \
  -v "${CACHE_DIR}/cargo-registry:/root/.cargo/registry" \
  -v "${CACHE_DIR}/cargo-git:/root/.cargo/git" \
  -v "${CACHE_DIR}/rustup:/root/.rustup" \
  -v "${CACHE_DIR}/apt-archives:/var/cache/apt/archives" \
  -v "${CACHE_DIR}/apt-lists:/var/lib/apt/lists" \
  -e CARGO_TARGET_DIR=/build/target \
  -e DEBIAN_FRONTEND=noninteractive \
  -e RUST_VERSION="${RUST_VERSION}" \
  "${IMAGE}" \
  bash -c '
    set -eo pipefail

    # --- multiarch apt: split host (amd64) and ports (arm64) ---
    # Noble uses deb822 sources. Rewrite ubuntu.sources so the amd64 entries
    # point at archive.ubuntu.com and the arm64 entries at ports.ubuntu.com
    # (archive 404s for non-x86 arches).
    dpkg --add-architecture arm64
    cat >/etc/apt/sources.list.d/ubuntu.sources <<SOURCES
Types: deb
URIs: http://archive.ubuntu.com/ubuntu
Suites: noble noble-updates noble-backports
Components: main restricted universe multiverse
Architectures: amd64
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg

Types: deb
URIs: http://security.ubuntu.com/ubuntu
Suites: noble-security
Components: main restricted universe multiverse
Architectures: amd64
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg

Types: deb
URIs: http://ports.ubuntu.com/ubuntu-ports
Suites: noble noble-updates noble-security
Components: main restricted universe multiverse
Architectures: arm64
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
SOURCES

    apt-get update
    apt-get install -y --no-install-recommends \
      build-essential pkg-config clang libclang-dev cmake curl ca-certificates \
      gcc-aarch64-linux-gnu g++-aarch64-linux-gnu \
      libpipewire-0.3-dev \
      libpipewire-0.3-dev:arm64

    # --- Rust toolchain (cached on host) ---
    export CARGO_HOME=/root/.cargo
    export RUSTUP_HOME=/root/.rustup
    export PATH="$CARGO_HOME/bin:$PATH"
    if [ ! -x "$CARGO_HOME/bin/cargo" ]; then
      curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | \
        sh -s -- -y --profile minimal --default-toolchain "$RUST_VERSION" \
                 --target x86_64-unknown-linux-gnu \
                 --target aarch64-unknown-linux-gnu
    else
      rustup default "$RUST_VERSION" >/dev/null
      rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu
    fi

    # --- writable copy of source so cargo can touch Cargo.lock if needed ---
    mkdir -p /build/src
    cp -a /src/. /build/src/
    cd /build/src

    # --- x86_64 build (native) ---
    cargo build --release --locked --target x86_64-unknown-linux-gnu
    cp /build/target/x86_64-unknown-linux-gnu/release/gnome-portal-bridge /output/gnome-portal-bridge
    echo "gnome-portal-bridge x86_64 built successfully"

    # --- aarch64 cross build (gcc-aarch64-linux-gnu as linker) ---
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
    export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
    export CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++
    export AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar
    export PKG_CONFIG_ALLOW_CROSS=1
    export PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig
    export PKG_CONFIG_SYSROOT_DIR=/
    # bindgen (pipewire-sys) needs clang pointed at the arm64 sysroot headers.
    export BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_gnu="--target=aarch64-linux-gnu -I/usr/include/aarch64-linux-gnu -I/usr/include -I/usr/include/pipewire-0.3 -I/usr/include/spa-0.2"

    cargo build --release --locked --target aarch64-unknown-linux-gnu
    cp /build/target/aarch64-unknown-linux-gnu/release/gnome-portal-bridge /output/gnome-portal-bridge-aarch64
    echo "gnome-portal-bridge aarch64 built successfully"

    # --- glibc floor verification (must be <= 2.39 for noble) ---
    for f in /output/gnome-portal-bridge /output/gnome-portal-bridge-aarch64; do
      echo ">>> GLIBC symbol versions in $f:"
      MAXVER=$(objdump -T "$f" 2>/dev/null | grep -oE "GLIBC_[0-9]+\.[0-9]+" | sort -V | tail -1)
      echo "    highest: ${MAXVER:-none}"
      # 2.39 is the noble floor; fail if anything above it slips in.
      case "$MAXVER" in
        ""|GLIBC_2.1[0-9]|GLIBC_2.2[0-9]|GLIBC_2.3[0-9]) : ;;
        *) echo "FAIL: $f requires ${MAXVER} > GLIBC_2.39 (noble floor)" >&2; exit 1 ;;
      esac
    done
    echo "both binaries respect the GLIBC 2.39 floor"
  '

echo
echo ">>> Artifacts:"
ls -lh "${OUTPUT_DIR}"
