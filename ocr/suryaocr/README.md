# Surya OCR Service

A FastAPI server wrapping [Surya OCR 2](https://github.com/datalab-to/surya) to
conform to the LiteParse OCR API specification (see `../../OCR_API_SPEC.md`).

Surya 2 is a multilingual OCR foundation model with strong accuracy across many
languages — a single model handles all languages with no per-language setup.

## Build and Run

```bash
# install and run (in one command)
uv run server.py
```

The first run downloads Surya model weights from Hugging Face and may take a few
minutes; weights are cached afterward.

## Inference backend (required)

Surya 2 is a VLM-backed model: text recognition runs through a separate
inference backend, which you must provide. Surya does **not** bundle it.

- **CPU / local (llama.cpp):** install the `llama-server` binary so Surya's
  `llamacpp` backend can spawn it:
  - macOS: `brew install llama.cpp`
  - Linux: `brew install llama.cpp`, or download a release from
    https://github.com/ggml-org/llama.cpp/releases and put `llama-server` on
    your `PATH` (or set `LLAMA_CPP_BINARY=/path/to/llama-server`).
- **GPU (vllm):** set `SURYA_INFERENCE_BACKEND=vllm` (requires a CUDA GPU).
- **External server:** point `SURYA_INFERENCE_URL` at an already-running Surya
  inference server to attach without spawning one locally.

Without a backend, startup succeeds and `GET /health` works, but `POST /ocr`
returns a 500 with "llama-server binary not found".

## Docker (GPU, single image)

The provided `Dockerfile` builds **one image that runs both the API and the
model server on a GPU**. It is based on the official CUDA llama.cpp image
(`ghcr.io/ggml-org/llama.cpp:server-cuda`), which ships a `qwen35`-capable
`llama-server` (Surya 2's custom GGUF architecture) with `libggml-cuda` and the
CUDA runtime. Surya spawns `llama-server` from `PATH` inside the same container,
so no sidecar or separate inference service is needed.

```bash
# Build
docker build -t suryaocr-liteparse:cuda .

# Run (requires the NVIDIA container runtime)
docker run --rm --gpus all -p 8830:8830 \
  -v "$HOME/.cache/suryaocr-models:/models" \
  suryaocr-liteparse:cuda
```

- `--gpus all` exposes the host GPUs; all model layers are offloaded
  (`LLAMA_CPP_NGL=99`).
- The `-v …:/models` mount caches weights (`HF_HOME=/models`). The **first**
  `POST /ocr` downloads the GGUF weights from Hugging Face and spawns
  `llama-server` (a few minutes); subsequent requests reuse the cache and the
  running server.

The image bakes in the backend configuration: `SURYA_INFERENCE_BACKEND=llamacpp`,
`LLAMA_CPP_NGL=99`, and `SURYA_GUIDED_LAYOUT=false`. The last one is required on
the upstream `llama-server` build: Surya's layout step is the only request that
sends a grammar, and its regex `pattern` breaks llama.cpp's
json-schema→GBNF converter (`failed to parse grammar`), which otherwise yields
empty results. With guided layout off, layout runs as free generation (Surya
parses the output itself) and the rest of the pipeline never uses a grammar.

To use a CUDA GPU via vLLM instead of bundled llama.cpp, set
`SURYA_INFERENCE_BACKEND=vllm` (see the backend section above).

Verify it is up:

```bash
curl http://localhost:8830/health
curl -X POST -F "file=@image.png" http://localhost:8830/ocr
```

## Usage

The service exposes:

- `POST /ocr` — Perform OCR on an uploaded image
- `GET /health` — Health check

### Parameters

- `file` — Image file (multipart/form-data)
- `language` — Language code (accepted for API compatibility; **ignored**, since
  Surya 2 is multilingual)

### Example

```bash
curl -X POST -F "file=@image.png" http://localhost:8830/ocr
```

### Response Format

```json
{
  "results": [
    {
      "text": "recognized text",
      "bbox": [x1, y1, x2, y2],
      "confidence": 0.95,
      "polygon": [[x1, y1], [x2, y2], [x3, y3], [x4, y4]]
    }
  ]
}
```

Results are **block-level** (one entry per detected layout block), with each
block's HTML stripped to plain text. This conforms to the LiteParse OCR API spec.

## Supported Languages

Surya 2 is a single multilingual model — no `language` parameter is required
(the `language` field is accepted but ignored).

Per Surya's own benchmark, it scores an **87.2% overall pass rate across 91
languages**, with 38 of the 91 languages scoring ≥ 90% and 76 scoring ≥ 80%,
covering text accuracy, layout, tables, math, and reading order. It has strong
performance across both Latin and non-Latin scripts.

- Full 91-language results: https://github.com/datalab-to/surya/blob/master/static/docs/multilingual.md
- Project overview: https://github.com/datalab-to/surya

## Device / GPU

There are two device knobs, for two different parts of the pipeline:

- `LLAMA_CPP_NGL` controls how many layers of the **VLM** (the model that reads
  text) the `llamacpp` backend offloads to the GPU. `99` offloads everything;
  `0` keeps it on CPU. The Docker image sets `99`.
- `TORCH_DEVICE` controls the device for Surya's **helper torch models**
  (e.g. layout). Surya auto-detects, or you can force it:

  ```bash
  TORCH_DEVICE=cuda uv run server.py   # GPU
  TORCH_DEVICE=cpu  uv run server.py   # CPU
  ```

For a turnkey GPU deployment, prefer the Docker image above — it wires both up.

## Use with LiteParse

```bash
lit parse document.pdf --ocr-server-url http://localhost:8830/ocr
```

Or in code:

```typescript
import { LiteParse } from 'liteparse';

const parser = new LiteParse({
  ocrServerUrl: 'http://localhost:8830/ocr',
});

const result = await parser.parse('document.pdf');
```

## Testing

```bash
uv run pytest test_server.py
```

Tests mock the Surya predictor, so they run without downloading any models.

## Notes

- First request/startup may be slow while models download.
- Default port is 8830 (easyocr 8828, paddleocr 8829).
- Output is block-granular; Surya 2 has no per-line text API.
