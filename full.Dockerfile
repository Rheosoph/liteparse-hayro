# Stage 1: Build from source
FROM rust:1-bookworm AS builder

RUN apt-get update && apt-get install -y \
    libclang-dev \
    libtesseract-dev \
    libleptonica-dev \
    cmake \
    g++ \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/

RUN cargo build --release

# Stage 2: Runtime image with conversion tools
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    libtesseract5 \
    liblept5  \
    tesseract-ocr-eng \
    ca-certificates \
    libreoffice \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/lit /usr/local/bin/lit
# pdfium shared library
COPY --from=builder /root/.cache/pdfium-rs/ /usr/local/lib/pdfium-rs/

# Ensure pdfium is discoverable at runtime
ENV LD_LIBRARY_PATH="/usr/local/lib/pdfium-rs"

RUN ln -s /usr/local/bin/lit /usr/local/bin/liteparse

CMD ["/bin/sh"]
