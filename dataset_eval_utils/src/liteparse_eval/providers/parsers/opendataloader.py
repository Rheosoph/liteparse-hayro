import os
import re
import shutil
import subprocess
import tempfile
from pathlib import Path

import opendataloader_pdf

from .base import ParserProvider


def _java_major(java_bin: str) -> int:
    """Major version of a java binary, or 0 if it cannot be determined."""
    try:
        out = subprocess.run([java_bin, "-version"], capture_output=True, text=True,
                             timeout=30).stderr
    except Exception:
        return 0
    m = re.search(r'version "(?:1\.)?(\d+)', out)
    return int(m.group(1)) if m else 0


def ensure_java_11() -> str:
    """Put a Java 11+ runtime on PATH for opendataloader-pdf, and return its version.

    opendataloader-pdf shells out to whatever `java` is first on PATH. Its jar is
    compiled for Java 11+, so on a machine whose default is Java 8 every document
    fails with UnsupportedClassVersionError -- which surfaces as a row of ERRORs
    rather than as an obvious "wrong Java" message. Prefer JAVA_HOME, then the
    usual JDK install locations, and fail with an actionable message if none works.
    """
    candidates = []
    if os.environ.get("JAVA_HOME"):
        candidates.append(os.path.join(os.environ["JAVA_HOME"], "bin", "java"))
    current = shutil.which("java")
    if current:
        candidates.append(current)
    candidates += [
        "/opt/homebrew/opt/openjdk@11/bin/java",   # Homebrew (Apple silicon)
        "/opt/homebrew/opt/openjdk/bin/java",
        "/usr/local/opt/openjdk@11/bin/java",      # Homebrew (Intel)
        "/usr/local/opt/openjdk/bin/java",
        "/usr/lib/jvm/default-java/bin/java",      # Debian/Ubuntu
    ]
    for java_bin in candidates:
        if not java_bin or not os.path.exists(java_bin):
            continue
        version = _java_major(java_bin)
        if version >= 11:
            bin_dir = os.path.dirname(java_bin)
            if shutil.which("java") != java_bin:
                os.environ["PATH"] = bin_dir + os.pathsep + os.environ.get("PATH", "")
            return f"java {version}"
    found = _java_major(current) if current else 0
    raise RuntimeError(
        "opendataloader-pdf needs Java 11+, but the java on PATH is "
        + (f"Java {found}." if found else "missing.")
        + " Install one (macOS: `brew install openjdk@11`) or point JAVA_HOME at it."
    )


class OpenDataLoaderProvider(ParserProvider):
    """
    Parse provider using OpenDataLoader PDF.

    Install with: pip install opendataloader-pdf
    Requires: Java 11+
    """

    def __init__(self):
        """Initialize the parse provider, ensuring a usable Java is on PATH."""
        self.java_version = ensure_java_11()

    def extract_text(self, file_path: Path) -> str:
        """Extract markdown from a document using OpenDataLoader PDF."""
        with tempfile.TemporaryDirectory() as tmp_dir:
            opendataloader_pdf.convert(
                input_path=[str(file_path)],
                output_dir=tmp_dir,
                format="markdown",
                # image_output defaults to "external", which renders and writes an image
                # file per figure -- real work unrelated to text extraction that inflates
                # the timings. "cluster" tables and "xycut" reading order are what the
                # benchmarks score; hybrid="off" (already the default) keeps the
                # docling-fast ML backend out of the pipeline.
                image_output="off",
                table_method="cluster",
                reading_order="xycut",
                hybrid="off",
                quiet=True,
            )
            # Read the output markdown file
            output_file = Path(tmp_dir) / f"{file_path.stem}.md"
            if output_file.exists():
                return output_file.read_text(encoding="utf-8")
            # Fallback: find any .md file in the output dir
            md_files = list(Path(tmp_dir).glob("*.md"))
            if md_files:
                return md_files[0].read_text(encoding="utf-8")
            return ""
