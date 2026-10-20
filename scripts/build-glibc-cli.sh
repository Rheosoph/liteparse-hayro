#!/usr/bin/env bash
set -eux

# Build the `lit` CLI inside an OLD-glibc Debian container so the resulting
# binary loads on hosts with an older glibc than the CI runner.
#
# The GitHub `ubuntu-22.04` runners ship glibc 2.35, which bakes a `GLIBC_2.35`
# symbol requirement into the binary and breaks it on AWS Lambda / Amazon Linux
# 2023 (glibc 2.34) and other older hosts. Debian bullseye ships glibc 2.31 —
# below the 2.34 Lambda floor — and glibc is backward-compatible, so a
# 2.31-baseline binary still runs on newer hosts. This only WIDENS
# compatibility; it is not a breaking change.
#
# Mirrors scripts/build-glibc-node.sh but produces the standalone CLI via
# `cargo build` instead of the napi module. tesseract-rs (build-tesseract)
# compiles Tesseract + Leptonica from source; on linux-gnu it uses
# g++/libstdc++, so no clang/libc++ bundling is required.

TARGET="${1:?usage: build-glibc-cli.sh <rust-target>}"

export DEBIAN_FRONTEND=noninteractive
# Debian 11 (bullseye) is past end-of-life, and deb.debian.org has started
# pruning the bullseye-security pool while still serving its package index — apt
# therefore resolves to .deb files that 404 ("Failed to fetch ... 404 Not Found"
# for libpng-dev / libarchive-dev, exit code 100). Repoint at
# archive.debian.org, which carries bullseye + bullseye-updates permanently, and
# drop bullseye-security entirely (its pool is gone from deb.debian.org and has
# not landed on archive.debian.org). Archived Release files are past their
# Valid-Until, hence Check-Valid-Until=false.
cat > /etc/apt/sources.list <<'EOF'
deb http://archive.debian.org/debian bullseye main
deb http://archive.debian.org/debian bullseye-updates main
EOF
rm -f /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/*.sources || true
echo 'Acquire::Check-Valid-Until "false";' > /etc/apt/apt.conf.d/99no-check-valid-until

apt-get update
# NOTE: cmake is deliberately NOT installed from apt here. Debian bullseye ships
# cmake 3.18, but Leptonica 1.84.1 (compiled from source by tesseract-rs)
# requires the C17 language dialect, which CMake only learned to map to compiler
# flags in 3.21. We install a newer CMake below instead.
apt-get install -y --no-install-recommends \
  build-essential git curl pkg-config ca-certificates \
  libtesseract-dev libleptonica-dev \
  libpng-dev libjpeg-dev libtiff-dev zlib1g-dev

# Install a modern CMake (>=3.21) from Kitware's official static binaries so the
# tesseract-rs Leptonica build can enable the C17 dialect. bullseye's apt cmake
# (3.18) fails with "does not know the compile flags to use to enable C17".
CMAKE_VERSION=3.31.7
case "$TARGET" in
  x86_64-*)  CMAKE_ARCH=x86_64 ;;
  aarch64-*) CMAKE_ARCH=aarch64 ;;
  *) echo "unsupported target for cmake install: $TARGET" >&2; exit 1 ;;
esac
curl --proto "=https" --tlsv1.2 -sSfL \
  "https://github.com/Kitware/CMake/releases/download/v${CMAKE_VERSION}/cmake-${CMAKE_VERSION}-linux-${CMAKE_ARCH}.tar.gz" \
  | tar -xz -C /opt
export PATH="/opt/cmake-${CMAKE_VERSION}-linux-${CMAKE_ARCH}/bin:$PATH"
cmake --version

curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --default-toolchain 1.95.0 -t "$TARGET"
. "$HOME/.cargo/env"

cargo build --release --target "$TARGET" -p liteparse

BIN="target/$TARGET/release/lit"
echo "Built CLI: $BIN"
ls -la "$BIN"
echo "glibc symbol versions required by $BIN:"
objdump -T "$BIN" 2>/dev/null \
  | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sort -u -V || true
