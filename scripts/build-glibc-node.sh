#!/usr/bin/env bash
set -eux

# Build the linux-gnu napi binding inside an OLD-glibc Debian container so the
# resulting .node loads on hosts with an older glibc than the CI runner.
#
# The GitHub `ubuntu-22.04` runner ships glibc 2.35, which bakes a
# `GLIBC_2.35` symbol requirement into the binary. That binary then fails to
# dlopen on AWS Lambda / Amazon Linux 2023 (glibc 2.34) and other older hosts.
# Debian bullseye ships glibc 2.31 — comfortably below the 2.34 Lambda floor —
# and glibc is backward-compatible, so a 2.31-baseline binary still runs on the
# newer runners/hosts. This only WIDENS compatibility; it is not a breaking
# change for existing users.
#
# tesseract-rs (build-tesseract) compiles Tesseract + Leptonica from source via
# cmake. On linux-gnu it selects g++/libstdc++ (see the crate's build.rs), so —
# unlike the musl path — we do NOT need clang/libc++ and do not have to bundle a
# C++ runtime next to the .node.

TARGET="${1:?usage: build-glibc-node.sh <rust-target>}"

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
# build-essential/pkg-config: toolchain to compile Tesseract + Leptonica.
# lib*-dev: image-format deps Leptonica links against (same set apt pulls in for
# libleptonica-dev on the ubuntu runner today).
# cmake is installed separately below — bullseye's apt cmake (3.18) is too old
# for Leptonica 1.84.1's C17 requirement (needs CMake >=3.21).
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

# Pin the same toolchain the non-container matrix builds use (dtolnay@1.95.0).
curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --default-toolchain 1.95.0 -t "$TARGET"
. "$HOME/.cargo/env"

# node/npm/npx come from the node:20-bullseye base image; @napi-rs/cli is already
# present in the host-installed node_modules (mounted via the workspace).
npx napi build --cargo-cwd ../../crates/liteparse-napi --platform --release \
  --js false --dts native.d.ts --target "$TARGET" .

NODE_FILE=$(ls liteparse.*.node | head -n1)
echo "Built native module: $NODE_FILE"
ls -la ./*.node
# Fail loudly if the build somehow still references a too-new glibc, so a broken
# baseline can't silently ship again.
echo "glibc symbol versions required by $NODE_FILE:"
objdump -T "$NODE_FILE" 2>/dev/null \
  | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sort -u -V || true
