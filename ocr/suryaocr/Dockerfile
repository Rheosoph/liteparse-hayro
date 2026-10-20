# Single image: GPU-enabled llama.cpp `llama-server` + the Surya OCR API.
#
# Based on the official CUDA llama.cpp server image, which already provides a
# `qwen35`-capable `llama-server` (with libggml-cuda), the CUDA runtime, and
# Python 3.12. Surya's llamacpp backend finds `llama-server` on PATH and spawns
# it INSIDE this same container, so one image runs both the API and the model
# server. Run with `--gpus all`.
#
# (Stock Ollama's bundled llama.cpp cannot load Surya 2's custom qwen35 GGUF;
#  this upstream build can.)
FROM ghcr.io/ggml-org/llama.cpp:server-cuda

# Bring uv in from its official image (no external install script needed).
COPY --from=ghcr.io/astral-sh/uv:0.11.21 /uv /usr/local/bin/uv

# Python/runtime libs for Surya, torch, and Pillow.
RUN apt-get update && apt-get install -y --no-install-recommends \
    python3-venv \
    libgl1 \
    libglib2.0-0 \
    libgomp1 \
    libcurl4 \
    && rm -rf /var/lib/apt/lists/*

# llama-server and its shared libraries live in /app.
ENV PATH="/app:${PATH}"
ENV LD_LIBRARY_PATH="/app:${LD_LIBRARY_PATH}"

# Surya: llama.cpp backend, all layers on GPU. Surya spawns llama-server and
# downloads the GGUF weights from Hugging Face on the first OCR request.
ENV SURYA_INFERENCE_BACKEND=llamacpp
ENV LLAMA_CPP_NGL=99
# Surya's layout step is the only request that sends a grammar; its regex
# `pattern` breaks the upstream llama.cpp grammar parser. Run layout
# unconstrained so the OCR pipeline returns results on this build.
ENV SURYA_GUIDED_LAYOUT=false
# Cache model weights (Surya torch models + the GGUF) under /models.
ENV HF_HOME=/models

WORKDIR /opt/app

# Copy project metadata, lockfile, and server code
COPY pyproject.toml uv.lock README.md server.py test_server.py ./

# Install dependencies (uses the committed lockfile)
RUN uv sync --frozen

# The base image's entrypoint is llama-server; clear it and run the API.
ENTRYPOINT []
EXPOSE 8830
CMD ["uv", "run", "python3", "server.py"]
