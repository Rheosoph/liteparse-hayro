#!/bin/sh
set -eux

# tesseract-rs's build.rs hard-codes -DCMAKE_CXX_COMPILER=clang++ and -stdlib=libc++,
# so we need real clang + libc++ in the image (gcc/g++ from build-base is not enough).
# Alpine's libc++ links against llvm-libunwind (NOT the GNU libunwind, which conflicts).
# The libc++ runtime depends on libc++abi.so.1 too; that .so is shipped in the
# `libc++` package itself (no separate apk needed at build time, but we DO need
# to bundle the .so next to the .node for runtime — see the ldd loop below).
# Static libs (openssl-libs-static, zlib-static) are required because musl rust defaults
# to crt-static for build scripts.
apk add --no-cache \
  build-base cmake git curl pkgconf perl \
  clang libc++-dev llvm-libunwind-dev \
  tesseract-ocr-dev leptonica-dev \
  openssl-dev openssl-libs-static zlib-static

curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable -t x86_64-unknown-linux-musl
. /root/.cargo/env

# napi produces a cdylib (.node) which MUST be dynamically linked against libc.
export RUSTFLAGS="-C target-feature=-crt-static"
npx napi build --cargo-cwd ../../crates/liteparse-napi --platform --release --js false --dts native.d.ts --target x86_64-unknown-linux-musl .

# The resulting .node has DT_NEEDED entries for the clang+libc++ runtime
# (libc++.so.1, libc++abi.so.1, libunwind.so.1) which are not present on stock
# node:20-alpine. Bundle them next to the .node so the $ORIGIN rpath
# (set by liteparse-napi/build.rs) finds them at dlopen time. Same pattern we
# use for libpdfium.so.
#
# We use `ldd` to discover the exact DT_NEEDED list rather than hard-coding it,
# so future toolchain changes (e.g. clang adding a new runtime dep) don't
# silently break the smoke test. Anything resolved under /usr/lib that isn't a
# core musl/libc/loader file gets copied.
NODE_FILE=$(ls liteparse.*.node | head -n1)
echo "DT_NEEDED for $NODE_FILE:"
ldd "$NODE_FILE" || true
# ldd on musl exits 0 even for unresolved deps and prints them; parse names.
for needed in $(ldd "$NODE_FILE" 2>/dev/null | awk '{print $1}' | grep -E '^lib'); do
  case "$needed" in
    # Stock musl / loader libs that are always present in any Alpine image.
    libc.musl-*.so.* | ld-musl-*.so.* | libc.so | libdl.so* | libm.so* | libpthread.so* | librt.so*)
      continue
      ;;
    # Already bundled.
    libpdfium.so)
      continue
      ;;
  esac
  src=$(find /usr/lib /usr/lib64 -maxdepth 3 -name "$needed" 2>/dev/null | head -n1)
  if [ -z "$src" ]; then
    echo "WARN: could not locate $needed under /usr/lib, skipping"
    continue
  fi
  src=$(readlink -f "$src")
  [ -f "$src" ] || { echo "ERROR: $src does not resolve to a file"; exit 1; }
  cp -v "$src" "./$needed"
done

# NOTE: the prebuilt libpdfium.so from run-llama/pdfium-binaries is a glibc
# binary. To run it on musl (Alpine), the user MUST have the gcompat package
# installed (`apk add gcompat`). gcompat provides:
#   - the glibc dynamic loader at /lib/ld-linux-x86-64.so.2
#   - the FORTIFY_SOURCE wrapper symbols (__memcpy_chk, __vsnprintf_chk, ...)
# This is a runtime requirement of the musl @llamaindex/liteparse-linux-x64-musl
# package and is documented in the npm package README. Until the pdfium-binaries
# fork ships a musl-native build, this is the practical path forward.

echo "Bundled libs in $(pwd):"
ls -la *.so* *.node 2>/dev/null
