import io
import logging
import os
import re
import traceback
from html import unescape
from html.parser import HTMLParser
from typing import Any

# Surya 2 is a VLM-backed model: text recognition runs through an inference
# backend. Default to the CPU-friendly llama.cpp backend, which spawns a
# `llama-server` binary (bundled in the Docker image; install locally with
# `brew install llama.cpp` or from github.com/ggml-org/llama.cpp/releases) and
# downloads the GGUF weights on first use. Override for GPU with
# SURYA_INFERENCE_BACKEND=vllm, or attach to an already-running inference
# server with SURYA_INFERENCE_URL. These must be set before importing surya.
os.environ.setdefault("SURYA_INFERENCE_BACKEND", "llamacpp")
os.environ.setdefault("LLAMA_CPP_NGL", "0")  # 0 = CPU; set 99 for full GPU offload
# Surya only sends a grammar for the layout step (LAYOUT_JSON_SCHEMA), whose
# regex `pattern` the upstream llama.cpp json-schema→GBNF converter cannot
# parse ("failed to parse grammar"). Disable guided layout so layout runs as
# free generation (Surya parses the output itself); block/full-page OCR never
# use a grammar. Required for the llama.cpp backend to return results.
os.environ.setdefault("SURYA_GUIDED_LAYOUT", "false")

import uvicorn
from fastapi import FastAPI, HTTPException
from fastapi.datastructures import UploadFile
from fastapi.param_functions import File, Form
from PIL import Image
from pydantic import BaseModel
from surya.inference import SuryaInferenceManager
from surya.recognition import RecognitionPredictor

_BLOCK_OR_BREAK = {
    "br", "p", "div", "li", "tr", "td", "th",
    "h1", "h2", "h3", "h4", "h5", "h6",
}


class _TextExtractor(HTMLParser):
    def __init__(self) -> None:
        super().__init__()
        self._parts: list[str] = []

    def handle_data(self, data: str) -> None:
        self._parts.append(data)

    def handle_starttag(self, tag: str, attrs: object) -> None:
        if tag in _BLOCK_OR_BREAK:
            self._parts.append(" ")

    def handle_endtag(self, tag: str) -> None:
        if tag in _BLOCK_OR_BREAK:
            self._parts.append(" ")

    def text(self) -> str:
        return "".join(self._parts)


def _html_to_text(html: str) -> str:
    """Strip block HTML to collapsed plain text (stdlib only)."""
    if not html:
        return ""
    parser = _TextExtractor()
    parser.feed(html)
    parser.close()
    return re.sub(r"\s+", " ", unescape(parser.text())).strip()


class OcrResponse(BaseModel):
    results: list[Any]


class StatusResponse(BaseModel):
    status: str


def _coerce_polygon(polygon: Any) -> list[list[float]] | None:
    """Return a 4x2 float polygon, or None if the shape is invalid."""
    if polygon is None:
        return None
    if hasattr(polygon, "tolist"):
        polygon = polygon.tolist()
    if len(polygon) == 4 and all(len(pt) == 2 for pt in polygon):
        return [[float(pt[0]), float(pt[1])] for pt in polygon]
    return None


def _block_to_result(block: Any) -> dict[str, Any] | None:
    """Map a Surya block to the LiteParse OCR shape, or None to skip it."""
    get = (
        block.get if isinstance(block, dict) else lambda k, d=None: getattr(block, k, d)
    )

    if get("skipped", False):
        return None

    text = _html_to_text(get("html", "") or "")
    if not text:
        return None

    polygon = _coerce_polygon(get("polygon"))

    bbox = get("bbox")
    if hasattr(bbox, "tolist"):
        bbox = bbox.tolist()
    if bbox is not None:
        bbox = [int(round(float(v))) for v in bbox]
    elif polygon is not None:
        xs = [pt[0] for pt in polygon]
        ys = [pt[1] for pt in polygon]
        bbox = [int(min(xs)), int(min(ys)), int(max(xs)), int(max(ys))]
    else:
        bbox = [0, 0, 0, 0]

    confidence = get("confidence")
    confidence = float(confidence) if confidence is not None else 1.0

    result: dict[str, Any] = {"text": text, "bbox": bbox, "confidence": confidence}
    if polygon is not None:
        result["polygon"] = polygon
    return result


class SuryaOCRServer:
    def __init__(self) -> None:
        # Surya 2 is multilingual; one model handles all languages. The
        # inference manager selects the device automatically (override with
        # the TORCH_DEVICE env var) and may download models on first run.
        self.manager = SuryaInferenceManager()
        self.recognition_predictor = RecognitionPredictor(self.manager)

    def _create_ocr_server(self) -> FastAPI:
        app = FastAPI()

        @app.post("/ocr")
        async def ocr_endpoint(
            file: UploadFile = File(...), language: str = Form(default="en")
        ) -> OcrResponse:
            # `language` is accepted for API compatibility but unused: Surya 2
            # is multilingual and needs no per-language model reload.
            try:
                image_data = await file.read()
                image = Image.open(io.BytesIO(image_data))
                if image.mode != "RGB":
                    image = image.convert("RGB")
            except Exception as e:
                raise HTTPException(status_code=400, detail=f"Invalid image: {e}")

            try:
                predictions = self.recognition_predictor([image])
            except Exception as e:
                logging.error("OCR failed:\n%s", traceback.format_exc())
                raise HTTPException(status_code=500, detail=str(e))

            formatted: list[dict[str, Any]] = []
            if predictions:
                page = predictions[0]
                blocks = (
                    page.get("blocks", [])
                    if isinstance(page, dict)
                    else getattr(page, "blocks", [])
                )
                for block in blocks:
                    mapped = _block_to_result(block)
                    if mapped is not None:
                        formatted.append(mapped)

            return OcrResponse(results=formatted)

        @app.get("/health")
        def health() -> StatusResponse:
            return StatusResponse(status="healthy")

        return app

    def serve(self) -> None:
        app = self._create_ocr_server()
        uvicorn.run(app, host="0.0.0.0", port=8830)


if __name__ == "__main__":
    logging.basicConfig(level=logging.DEBUG)
    logging.info("Starting server on port 8830")
    SuryaOCRServer().serve()
