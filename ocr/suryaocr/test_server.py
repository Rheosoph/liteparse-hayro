import io
from types import SimpleNamespace

import pytest
from fastapi.testclient import TestClient
from PIL import Image

from server import _html_to_text
from server import SuryaOCRServer


def test_html_to_text_strips_tags_and_unescapes() -> None:
    assert _html_to_text("<p>Hello&nbsp;World</p>") == "Hello World"
    assert _html_to_text("Line one<br>Line two") == "Line one Line two"
    assert _html_to_text("<p>a</p><p>b</p>") == "a b"
    assert _html_to_text("Total: &amp; $42") == "Total: & $42"


def test_html_to_text_empty_for_markup_only() -> None:
    assert _html_to_text("") == ""
    assert _html_to_text("<br>") == ""
    assert _html_to_text("   ") == ""


class MockRecognitionPredictor:
    def __init__(self, blocks: list) -> None:
        self._blocks = blocks

    def __call__(self, images, *args, **kwargs) -> list:
        return [SimpleNamespace(blocks=self._blocks, image_bbox=[0, 0, 100, 100])]


def _make_server(blocks: list) -> SuryaOCRServer:
    server = SuryaOCRServer.__new__(SuryaOCRServer)  # skip model load
    server.recognition_predictor = MockRecognitionPredictor(blocks)  # type: ignore
    return server


def _png_bytes() -> io.BytesIO:
    image = Image.new("RGB", (4, 4), color=(255, 255, 255))
    buffer = io.BytesIO()
    image.save(buffer, format="PNG")
    buffer.seek(0)
    return buffer


def test_server_health_endpoint() -> None:
    server = _make_server([])
    client = TestClient(server._create_ocr_server())
    response = client.get("/health")
    assert response.status_code == 200
    assert response.json() == {"status": "healthy"}


def test_server_ocr_endpoint_maps_blocks() -> None:
    blocks = [
        SimpleNamespace(
            html="<p>Hello&nbsp;World</p>",
            bbox=[10.0, 20.0, 200.0, 40.0],
            polygon=[[10, 20], [200, 20], [200, 40], [10, 40]],
            confidence=0.97,
            skipped=False,
        ),
    ]
    server = _make_server(blocks)
    client = TestClient(server._create_ocr_server())
    response = client.post(
        "/ocr",
        files={"file": ("test.png", _png_bytes(), "image/png")},
        data={"language": "en"},
    )
    assert response.status_code == 200
    results = response.json()["results"]
    assert len(results) == 1
    assert results[0]["text"] == "Hello World"
    assert results[0]["bbox"] == [10, 20, 200, 40]
    assert results[0]["confidence"] == pytest.approx(0.97)
    assert results[0]["polygon"] == [
        [10.0, 20.0], [200.0, 20.0], [200.0, 40.0], [10.0, 40.0]
    ]


def test_server_defaults_missing_confidence() -> None:
    blocks = [
        SimpleNamespace(
            html="<p>x</p>",
            bbox=[0.0, 0.0, 5.0, 5.0],
            polygon=[[0, 0], [5, 0], [5, 5], [0, 5]],
            confidence=None,
            skipped=False,
        ),
    ]
    server = _make_server(blocks)
    client = TestClient(server._create_ocr_server())
    response = client.post(
        "/ocr", files={"file": ("t.png", _png_bytes(), "image/png")}
    )
    assert response.json()["results"][0]["confidence"] == pytest.approx(1.0)


def test_server_skips_empty_and_skipped_blocks() -> None:
    blocks = [
        SimpleNamespace(html="", bbox=[0.0, 0.0, 5.0, 5.0],
                        polygon=[[0, 0], [5, 0], [5, 5], [0, 5]],
                        confidence=0.9, skipped=False),
        SimpleNamespace(html="<p>image</p>", bbox=[0.0, 0.0, 5.0, 5.0],
                        polygon=[[0, 0], [5, 0], [5, 5], [0, 5]],
                        confidence=0.9, skipped=True),
        SimpleNamespace(html="<p>keep</p>", bbox=[1.0, 1.0, 9.0, 9.0],
                        polygon=[[1, 1], [9, 1], [9, 9], [1, 9]],
                        confidence=0.9, skipped=False),
    ]
    server = _make_server(blocks)
    client = TestClient(server._create_ocr_server())
    response = client.post(
        "/ocr", files={"file": ("t.png", _png_bytes(), "image/png")}
    )
    results = response.json()["results"]
    assert len(results) == 1
    assert results[0]["text"] == "keep"


def test_server_derives_bbox_from_polygon_when_bbox_missing() -> None:
    blocks = [
        SimpleNamespace(html="<p>z</p>", bbox=None,
                        polygon=[[3, 4], [30, 4], [30, 40], [3, 40]],
                        confidence=0.8, skipped=False),
    ]
    server = _make_server(blocks)
    client = TestClient(server._create_ocr_server())
    response = client.post(
        "/ocr", files={"file": ("t.png", _png_bytes(), "image/png")}
    )
    assert response.json()["results"][0]["bbox"] == [3, 4, 30, 40]


def test_server_keeps_zero_bbox_without_deriving_from_polygon() -> None:
    blocks = [
        SimpleNamespace(html="<p>q</p>", bbox=[0, 0, 0, 0],
                        polygon=[[3, 4], [30, 4], [30, 40], [3, 40]],
                        confidence=0.9, skipped=False),
    ]
    server = _make_server(blocks)
    client = TestClient(server._create_ocr_server())
    response = client.post(
        "/ocr", files={"file": ("t.png", _png_bytes(), "image/png")}
    )
    assert response.json()["results"][0]["bbox"] == [0, 0, 0, 0]


def test_server_skips_whitespace_only_html() -> None:
    blocks = [
        SimpleNamespace(html="<p>   </p>", bbox=[1, 1, 9, 9],
                        polygon=[[1, 1], [9, 1], [9, 9], [1, 9]],
                        confidence=0.9, skipped=False),
    ]
    server = _make_server(blocks)
    client = TestClient(server._create_ocr_server())
    response = client.post(
        "/ocr", files={"file": ("t.png", _png_bytes(), "image/png")}
    )
    assert response.json()["results"] == []
