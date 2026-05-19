#!/usr/bin/env python3
# Copyright 2026 The Kuasar Authors.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

"""
WarmFork example: Python LLM inference server.

Simulates the startup pattern of a model-serving framework such as vLLM or
TorchServe adapted to run as a WarmFork workload.

WarmFork lets a single expensive model load (Phase 1) be snapshotted once and
reused across many task instances.  Each restored instance either receives its
specific task_id and request context via the restore handshake (injection
mode) or self-starts without a task identity (autonomous mode), then serves
the restored workload instance (Phase 4).

Phase overview:
  Phase 1 — load model weights and warm up the inference engine
  Phase 2 — stop background threads, close connections, reach quiescent state
  Phase 3 — open inject socket; VM is snapshotted here
              (snapshot pod: kuasar.io/snapshot-type=warm-fork,
               kuasar.io/warm-fork-ready-protocol-version: 1)
  Phase 4 — serve the restored workload instance via HTTP

Usage:
  MODEL_NAME=llama-3-8b python3 llm_server.py

Environment variables:
  MODEL_NAME            Model identifier (default: gpt2-simulated)
  LISTEN_ADDR           HTTP listen address (default: :8080)
  WARMFORK_READINESS_SOCKET  Readiness socket path override (default: /run/warmfork-readiness.sock)
"""

from __future__ import annotations

import json
import logging
import os
import signal
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from typing import Any

import warmfork

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)s %(message)s",
)
logger = logging.getLogger(__name__)


# ── Simulated model ──────────────────────────────────────────────────────────
# Replace this section with real model initialisation, e.g.:
#
#   from transformers import AutoTokenizer, AutoModelForCausalLM
#   tokenizer = AutoTokenizer.from_pretrained(model_name)
#   model = AutoModelForCausalLM.from_pretrained(model_name, device_map="auto")


class SimulatedModel:
    """Stand-in for a real LLM (e.g. a Hugging Face or vLLM model)."""

    def __init__(self, name: str) -> None:
        self.name = name
        self._weights: list[float] = []  # placeholder for real weights

    def load(self) -> None:
        logger.info("[phase1] downloading/loading weights for %r ...", self.name)
        # Simulate slow disk I/O or network fetch
        for shard in range(1, 5):
            time.sleep(0.5)
            logger.info("[phase1]   shard %d/4 loaded", shard)
        self._weights = [float(i) * 0.001 for i in range(1_000_000)]
        logger.info("[phase1] model %r ready (%dM params)", self.name, len(self._weights) // 1_000)

    def warmup(self, passes: int = 3) -> None:
        """Run warm-up inference passes to fill GPU/CPU caches."""
        logger.info("[phase1] running %d warm-up inference passes ...", passes)
        for i in range(1, passes + 1):
            time.sleep(0.2)
            logger.info("[phase1]   warm-up pass %d/%d complete", i, passes)

    def generate(self, prompt: str, max_tokens: int = 64) -> str:
        """Simulate token generation. Replace with real model.generate()."""
        time.sleep(0.05 * max(1, len(prompt) // 20))  # simulate latency
        return f"[{self.name}] response to: {prompt!r} (simulated {max_tokens} tokens)"


# ── Background monitoring thread (Phase 1 only) ──────────────────────────────
# Threads like this are common in real inference servers (GPU utilisation
# monitoring, connection pool keepalives, metric exporters).  They MUST be
# stopped in Phase 2 before the snapshot is taken.


class MetricsThread(threading.Thread):
    def __init__(self) -> None:
        super().__init__(daemon=True, name="metrics")
        self._stop_event = threading.Event()

    def run(self) -> None:
        while not self._stop_event.wait(timeout=30):
            logger.info("[metrics] GPU utilisation: simulated 72%%")

    def stop(self) -> None:
        self._stop_event.set()
        self.join(timeout=5)
        logger.info("[phase2] metrics thread stopped")


# ── HTTP inference handler ────────────────────────────────────────────────────
# In a real vLLM / TorchServe deployment this would be the async HTTP/gRPC
# server.  Here we use http.server for zero-dependency simplicity.

_model: SimulatedModel | None = None
_task_id: str = ""


class InferenceHandler(BaseHTTPRequestHandler):
    def log_message(self, fmt: str, *args: Any) -> None:  # suppress default logging
        logger.debug("[http] " + fmt, *args)

    def do_POST(self) -> None:
        if self.path == "/generate":
            self._handle_generate()
        else:
            self._send_json(404, {"error": "not found"})

    def do_GET(self) -> None:
        if self.path == "/healthz":
            self._send_json(200, {"status": "ok", "task_id": _task_id})
        else:
            self._send_json(404, {"error": "not found"})

    def _handle_generate(self) -> None:
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)
        try:
            req = json.loads(body)
        except json.JSONDecodeError:
            self._send_json(400, {"error": "invalid JSON"})
            return

        prompt = req.get("prompt", "")
        max_tokens = int(req.get("max_tokens", 64))
        result = _model.generate(prompt, max_tokens)  # type: ignore[union-attr]
        self._send_json(200, {"task_id": _task_id, "generated_text": result})

    def _send_json(self, status: int, data: dict) -> None:
        body = json.dumps(data).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


# ── Entrypoint ────────────────────────────────────────────────────────────────


def main() -> None:
    model_name = os.environ.get("MODEL_NAME", "gpt2-simulated")
    socket_path = os.environ.get("WARMFORK_READINESS_SOCKET", warmfork.DEFAULT_SOCKET_PATH)
    listen_addr = os.environ.get("LISTEN_ADDR", ":8080")

    # ── Phase 1: Heavy initialisation ────────────────────────────────────────
    # Threads, I/O, GPU context initialisation, and network connections are
    # all permitted here.
    global _model
    _model = SimulatedModel(model_name)
    _model.load()
    _model.warmup()

    metrics = MetricsThread()
    metrics.start()

    # ── Phase 2: Reach quiescent state ───────────────────────────────────────
    # Stop all background threads. Close all outbound connections. Flush I/O.
    # Install deferred signal handlers so SIGTERM/SIGINT are held until Phase 4.
    logger.info("[phase2] stopping background threads ...")
    metrics.stop()
    # Real workload checklist:
    #   executor.shutdown(wait=True)
    #   db_pool.close()
    #   redis_client.close()
    #   object_storage_client.close()

    deferred_signals: list[int] = []

    def _defer(signum: int, _frame: Any) -> None:
        deferred_signals.append(signum)

    signal.signal(signal.SIGTERM, _defer)
    signal.signal(signal.SIGINT, _defer)

    logger.info("[phase2] quiescent state reached")

    # ── Phase 3: Open inject socket and block ─────────────────────────────────
    # The kuasar sandboxer snapshots the VM while the process is suspended
    # inside wait_for_injection().  No external side effects may occur before
    # this function returns and STARTED is sent.
    #
    # Snapshot pod annotations required:
    #   kuasar.io/snapshot-type: "warm-fork"
    #   kuasar.io/warm-fork-ready-protocol-version: "1"
    logger.info("[phase3] waiting for WarmFork restore on %s ...", socket_path)
    params = warmfork.wait_for_injection(socket_path)

    # ── Phase 4: Post-restore execution ─────────────────────────────────────
    # STARTED has been sent; restore is committed. Apply task identity if the
    # restore ran in injection mode, restore signal handlers, and start serving.
    global _task_id
    params.apply_env_overrides()
    _task_id = params.task_id
    if params.task_id:
        logger.info("[phase4] injected: task_id=%r context=%r", params.task_id, params.context)
    else:
        logger.info("[phase4] autonomous restore (no task identity)")

    # Restore default signal handling and deliver any deferred signals.
    signal.signal(signal.SIGTERM, signal.SIG_DFL)
    signal.signal(signal.SIGINT, signal.SIG_DFL)
    for sig in deferred_signals:
        logger.info("[phase4] delivering deferred signal %d", sig)
        os.kill(os.getpid(), sig)

    host, _, port_str = listen_addr.partition(":")
    port = int(port_str) if port_str else 8080
    srv = HTTPServer((host, port), InferenceHandler)
    display_task_id = _task_id or "autonomous"
    logger.info("[phase4] serving task %s on %s", display_task_id, listen_addr)

    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        logger.info("[phase4] shutting down task %s", display_task_id)
    finally:
        srv.server_close()


if __name__ == "__main__":
    main()
