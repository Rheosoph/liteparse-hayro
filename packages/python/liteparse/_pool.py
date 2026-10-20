"""Process-isolated worker pool for LiteParse.

The pool is not public API; use ``LiteParse(pool_size=..., parse_timeout=...)``.
"""

import os
import pickle
import queue
import struct
import subprocess
import sys
import threading
from typing import Any, BinaryIO, Dict, List, Optional, Tuple, Union

from .types import ParseError, ParseTimeoutError

_FRAME_HEADER = struct.Struct("!I")

# Sentinel posted by a worker's reader thread when its stdout hits EOF (the
# process exited or was killed).
_DEAD = object()

# How long to allow a worker to spawn, import liteparse, and construct its
# native parser. Deliberately generous: init time is excluded from
# parse_timeout, and a machine slow enough to trip this has bigger problems.
_INIT_TIMEOUT_S = 120.0


class _WorkerTimeout(Exception):
    """Internal: the worker did not respond within the parse deadline."""


class _WorkerCrashed(Exception):
    """Internal: the worker process exited before responding."""


class _WorkerInitFailed(Exception):
    """Internal: the worker's native parser could not be constructed."""


def _write_frame(stream: BinaryIO, obj: Any) -> None:
    data = pickle.dumps(obj, protocol=pickle.HIGHEST_PROTOCOL)
    stream.write(_FRAME_HEADER.pack(len(data)))
    stream.write(data)
    stream.flush()


def _read_frame(stream: BinaryIO) -> Optional[Any]:
    """Read one frame, or None on EOF/truncation."""
    header = b""
    while len(header) < _FRAME_HEADER.size:
        chunk = stream.read(_FRAME_HEADER.size - len(header))
        if not chunk:
            return None
        header += chunk
    (length,) = _FRAME_HEADER.unpack(header)
    data = b""
    while len(data) < length:
        chunk = stream.read(length - len(data))
        if not chunk:
            return None
        data += chunk
    return pickle.loads(data)


def _worker_main() -> int:
    """Entry point of a worker subprocess (``python -m liteparse._pool``)."""
    # Reserve the real stdout for protocol frames and point fd 1 at stderr so
    # that nothing else in the process can write into the frame stream.
    out = os.fdopen(os.dup(1), "wb")
    os.dup2(2, 1)
    stdin = sys.stdin.buffer

    config = _read_frame(stdin)
    if config is None:
        return 1
    try:
        from liteparse._liteparse import LiteParse as _NativeLiteParse

        native = _NativeLiteParse(**config)
    except Exception as e:  # noqa: BLE001 - forwarded to the parent verbatim
        _write_frame(out, ("init_error", f"{type(e).__name__}: {e}"))
        return 1
    _write_frame(out, ("ready", None))

    from liteparse.parser import _convert_native_result

    while True:
        msg = _read_frame(stdin)
        if msg is None or msg[0] == "stop":
            return 0
        _, payload = msg
        try:
            if isinstance(payload, bytes):
                native_result = native.parse_bytes(payload)
            else:
                native_result = native.parse(payload)
            _write_frame(out, ("ok", _convert_native_result(native_result)))
        except Exception as e:  # noqa: BLE001 - forwarded to the parent verbatim
            _write_frame(out, ("err", f"{type(e).__name__}: {e}"))


class _Worker:
    """One persistent worker subprocess plus its reader thread."""

    def __init__(self, config: Dict[str, Any]):
        self._proc = subprocess.Popen(
            [sys.executable, "-m", "liteparse._pool"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=None,  # inherit: parse logs and tracebacks stay visible
        )
        self._responses: "queue.Queue[Any]" = queue.Queue()
        self._ready = False
        reader = threading.Thread(
            target=self._read_loop, daemon=True, name="liteparse-pool-reader"
        )
        reader.start()
        _write_frame(self._proc.stdin, config)

    def _read_loop(self) -> None:
        while True:
            frame = _read_frame(self._proc.stdout)
            if frame is None:
                self._responses.put(_DEAD)
                return
            self._responses.put(frame)

    def _ensure_ready(self) -> None:
        """Consume the init handshake. Init time never counts toward the
        parse deadline — the deadline is a promise about parsing, not about
        interpreter startup."""
        if self._ready:
            return
        try:
            frame = self._responses.get(timeout=_INIT_TIMEOUT_S)
        except queue.Empty:
            raise _WorkerCrashed("worker failed to initialize in time") from None
        if frame is _DEAD:
            raise _WorkerCrashed("worker exited during initialization")
        kind, data = frame
        if kind == "init_error":
            # The worker process has already exited; the caller must retire
            # this worker rather than reuse it.
            raise _WorkerInitFailed(data)
        self._ready = True

    def request(
        self, payload: Union[str, bytes], timeout: Optional[float]
    ) -> Tuple[str, Any]:
        self._ensure_ready()
        try:
            _write_frame(self._proc.stdin, ("parse", payload))
        except (BrokenPipeError, OSError) as e:
            raise _WorkerCrashed(str(e)) from None
        try:
            frame = self._responses.get(timeout=timeout)
        except queue.Empty:
            raise _WorkerTimeout() from None
        if frame is _DEAD:
            raise _WorkerCrashed("worker exited mid-parse")
        return frame

    def kill(self) -> None:
        self._proc.kill()
        try:
            self._proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass
        self._close_pipes()

    def stop(self) -> None:
        """Graceful shutdown; falls back to kill if the worker doesn't exit."""
        try:
            _write_frame(self._proc.stdin, ("stop", None))
            self._proc.stdin.close()
        except (BrokenPipeError, OSError):
            pass
        try:
            self._proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self._proc.kill()
        self._close_pipes()

    def _close_pipes(self) -> None:
        for stream in (self._proc.stdin, self._proc.stdout):
            try:
                if stream:
                    stream.close()
            except OSError:
                pass


class WorkerPool:
    """Fixed-size pool of parse worker processes with a hard kill deadline."""

    def __init__(
        self,
        config: Dict[str, Any],
        pool_size: int,
        parse_timeout: Optional[float] = None,
    ):
        if pool_size < 1:
            raise ValueError("pool_size must be >= 1")
        if parse_timeout is not None and parse_timeout <= 0:
            raise ValueError("parse_timeout must be > 0 seconds")
        self._config = dict(config)
        self._timeout = parse_timeout
        self._idle: "queue.Queue[_Worker]" = queue.Queue()
        self._lock = threading.Lock()
        self._workers: List[_Worker] = []
        self._closed = False
        # Spawn eagerly: children import and construct their native parsers
        # concurrently while the caller goes on with its own startup.
        for _ in range(pool_size):
            self._spawn_worker()

    def _spawn_worker(self) -> None:
        worker = _Worker(self._config)
        with self._lock:
            self._workers.append(worker)
        self._idle.put(worker)

    def _retire_worker(self, worker: _Worker) -> None:
        worker.kill()
        with self._lock:
            if worker in self._workers:
                self._workers.remove(worker)

    def parse(self, payload: Union[str, bytes], source: str) -> Any:
        """Run one parse on an idle worker.

        Blocks until a worker is free; ``parse_timeout`` bounds the parse
        itself, not the wait for a free worker.
        """
        if self._closed:
            raise ParseError("parser pool is closed")
        worker = self._idle.get()
        replace = False
        try:
            status, data = worker.request(payload, self._timeout)
        except _WorkerTimeout:
            replace = True
            timeout = self._timeout
            raise ParseTimeoutError(
                f"parse of {source} exceeded {timeout}s; "
                "the worker process was killed",
                source=source,
                timeout=timeout,
            ) from None
        except _WorkerCrashed as e:
            replace = True
            raise ParseError(
                f"liteparse worker process died while parsing {source}: {e}"
            ) from None
        except _WorkerInitFailed as e:
            replace = True
            raise ParseError(f"liteparse worker failed to initialize: {e}") from None
        finally:
            if replace:
                self._retire_worker(worker)
                if not self._closed:
                    self._spawn_worker()
            elif self._closed:
                worker.stop()
            else:
                self._idle.put(worker)
        if status == "ok":
            return data
        raise ParseError(data)

    def warm_up(self) -> None:
        """Block until every worker has finished initializing.

        Optional — the first parse per worker waits for init anyway. Useful
        before latency-sensitive traffic or benchmarks.
        """
        workers = list(self._workers)
        for worker in workers:
            worker._ensure_ready()

    def close(self) -> None:
        """Shut down all workers. Idempotent."""
        if self._closed:
            return
        self._closed = True
        # Stop the workers we can grab; busy workers are stopped by parse()'s
        # finally-block when they come back (see the _closed check there).
        with self._lock:
            workers = list(self._workers)
            self._workers.clear()
        while True:
            try:
                self._idle.get_nowait()
            except queue.Empty:
                break
        for worker in workers:
            worker.stop()


if __name__ == "__main__":
    sys.exit(_worker_main())
