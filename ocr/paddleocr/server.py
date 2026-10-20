import io
import os
import logging
import traceback
from typing import Any

import numpy as np
import uvicorn
from fastapi import FastAPI, HTTPException
from fastapi.datastructures import UploadFile
from fastapi.param_functions import File, Form
from paddleocr import PaddleOCR
from PIL import Image
from pydantic import BaseModel


class OcrResponse(BaseModel):
    results: list[Any]


class StatusResponse(BaseModel):
    status: str


class PaddleOCRServer:
    def __init__(self) -> None:
        self.ocr: PaddleOCR = self._make_ocr("en")
        self.current_language: str = "en"

    @staticmethod
    def _make_ocr(language: str) -> PaddleOCR:
        """Build a PaddleOCR 3.x pipeline.

        PADDLE_DET_LIMIT_SIDE_LEN (default 1600) caps the long edge fed to the
        text detector ("max" limit type). Large page renders (e.g. 3500x4500 px
        scans at 150 DPI) otherwise take minutes and several GB on CPU; the
        recognizer still crops from the full-resolution image.
        """
        kwargs: dict[str, Any] = dict(
            lang=language,
            use_doc_orientation_classify=False,
            use_doc_unwarping=False,
            use_textline_orientation=os.environ.get("PADDLE_TEXTLINE_ORIENTATION", "1") not in ("0", "false"),
        )
        limit = os.environ.get("PADDLE_DET_LIMIT_SIDE_LEN", "1600")
        if limit and limit != "0":
            kwargs["text_det_limit_side_len"] = int(limit)
            kwargs["text_det_limit_type"] = "max"
        threads = os.environ.get("PADDLE_CPU_THREADS")
        if threads:
            kwargs["cpu_threads"] = int(threads)
        return PaddleOCR(**kwargs)

    @staticmethod
    def normalize_language(language: str) -> str:
        normalized = language.lower()
        aliases = {
            "eng": "en",
            "zh": "ch",
            "zh-cn": "ch",
            "zh-hans": "ch",
            "zh-tw": "chinese_cht",
            "zh-hant": "chinese_cht",
            "ja": "japan",
            "ko": "korean",
        }
        return aliases.get(normalized, normalized)

    def _create_ocr_server(
        self,
    ) -> FastAPI:
        app = FastAPI()

        @app.post("/ocr")
        async def ocr_endpoint(
            file: UploadFile = File(...), language: str = Form(default="en")
        ) -> OcrResponse:
            # Get language from request
            language = self.normalize_language(language)

            try:
                # Initialize OCR if needed or language changed
                if self.current_language != language:
                    # PaddleOCR 3.x parameters
                    self.ocr = self._make_ocr(language)
                    self.current_language = language

                # Load image

                image_data = await file.read()
                image = Image.open(io.BytesIO(image_data))

                # Convert to numpy array (RGB)
                if image.mode != "RGB":
                    image = image.convert("RGB")
                image_array = np.array(image)

                # Run OCR
                # PaddleOCR 3.x returns: list of result dicts
                # Each result has: res['rec_texts'], res['rec_scores'], res['rec_boxes']
                results = self.ocr.predict(image_array)
            except ValueError as ve:
                if "No models are available for the language" in str(ve):
                    raise HTTPException(status_code=400, detail=str(ve))
                raise HTTPException(status_code=500, detail=str(ve))
            except Exception as e:
                logging.error("OCR failed:\n%s", traceback.format_exc())
                raise HTTPException(status_code=500, detail=str(e))

            # Format results according to LiteParse OCR API spec
            # Convert to: { text, bbox: [x1, y1, x2, y2], confidence }
            formatted = []

            if results and len(results) > 0:
                # Get the first result
                result = results[0]

                res_data = (
                    result.get("res", result) if isinstance(result, dict) else result
                )
                # Extract texts, scores, axis-aligned boxes, and quad polygons.
                # PaddleOCR exposes the 4-point detection polygons under
                # ``rec_polys`` (or ``dt_polys`` upstream) in TL→TR→BR→BL order
                # relative to each detection's upright reading frame. We forward
                # them so LiteParse can recover the rotation of vertical
                # sidebar text instead of treating it as a horizontal line.
                if isinstance(res_data, dict):
                    texts = res_data.get("rec_texts", [])
                    scores = res_data.get("rec_scores", [])
                    boxes = res_data.get("rec_boxes", [])
                    polys = res_data.get("rec_polys", res_data.get("dt_polys", []))
                else:
                    # Fallback for result object with attributes
                    texts = getattr(res_data, "rec_texts", []) or []
                    scores = getattr(res_data, "rec_scores", []) or []
                    boxes = getattr(res_data, "rec_boxes", []) or []
                    polys = (
                        getattr(res_data, "rec_polys", None)
                        or getattr(res_data, "dt_polys", None)
                        or []
                    )

                # Convert numpy arrays to lists if needed
                if hasattr(texts, "tolist"):
                    texts = texts.tolist()
                if hasattr(scores, "tolist"):
                    scores = scores.tolist()
                if hasattr(boxes, "tolist"):
                    boxes = boxes.tolist()
                if hasattr(polys, "tolist"):
                    polys = polys.tolist()

                # Combine them - they should be parallel arrays
                for i in range(len(texts)):
                    text = texts[i]
                    confidence = float(scores[i]) if i < len(scores) else 0.0

                    # Get bounding box coordinates
                    # rec_boxes format is typically [x_min, y_min, x_max, y_max]
                    if i < len(boxes):
                        box = boxes[i]
                        # Convert to list and ensure 4 coordinates
                        if hasattr(box, "tolist"):
                            bbox = box.tolist()
                        else:
                            bbox = list(box)
                    else:
                        bbox = [0, 0, 0, 0]

                    polygon = None
                    if i < len(polys):
                        poly = polys[i]
                        if hasattr(poly, "tolist"):
                            poly = poly.tolist()
                        # Expect a 4×2 sequence; coerce to floats and validate
                        # shape before forwarding.
                        if len(poly) == 4 and all(len(pt) == 2 for pt in poly):
                            polygon = [[float(pt[0]), float(pt[1])] for pt in poly]
                        # When rec_boxes is missing/zero (rotated detections in
                        # some PaddleOCR builds only populate rec_polys), derive
                        # an axis-aligned fallback from the polygon.
                        if polygon is not None and bbox == [0, 0, 0, 0]:
                            xs = [pt[0] for pt in polygon]
                            ys = [pt[1] for pt in polygon]
                            bbox = [min(xs), min(ys), max(xs), max(ys)]

                    item = {"text": text, "bbox": bbox, "confidence": confidence}
                    if polygon is not None:
                        item["polygon"] = polygon
                    formatted.append(item)

            return OcrResponse(results=formatted)

        @app.get("/health")
        def health() -> StatusResponse:
            return StatusResponse(status="healthy")

        return app

    def serve(self) -> None:
        app = self._create_ocr_server()
        uvicorn.run(app, host="0.0.0.0", port=8829)


if __name__ == "__main__":
    logging.basicConfig(
        level=logging.DEBUG,
    )
    logging.info("Starting server on port 8829")
    server = PaddleOCRServer()
    server.serve()
