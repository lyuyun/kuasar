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

"""Minimal vLLM-style continuation workload example.

This server simulates a warmed LLM worker that keeps per-session state in
memory.  It is meant to show why ContinuationSnapshot is different from
WarmFork:

- the process is already serving traffic at snapshot time
- the in-memory session map matters
- the restored pod must resume the same workload identity

Endpoints:
- GET  /healthz
- GET  /v1/sessions/<session_id>
- POST /v1/sessions/<session_id>
- POST /v1/chat/completions
"""

from __future__ import annotations

import json
import logging
import os
import threading
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, HTTPServer
from typing import Any

import continuation

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
logger = logging.getLogger("continuation")


@dataclass
class SessionState:
    prompt_count: int = 0
    history: list[dict[str, str]] = field(default_factory=list)
    last_response: str = ""


class SimulatedVLLM:
    """Tiny stand-in for a vLLM worker."""

    def __init__(self, model_name: str) -> None:
        self.model_name = model_name
        self._warm = False

    def load(self) -> None:
        logger.info("[phase1] loading model %r", self.model_name)
        self._warm = True

    def generate(self, prompt: str, session: SessionState) -> str:
        session.prompt_count += 1
        response = (
            f"{self.model_name}: reply #{session.prompt_count} "
            f"to {prompt!r} (history={len(session.history)})"
        )
        session.last_response = response
        session.history.append({"prompt": prompt, "response": response})
        return response


MODEL_NAME = os.environ.get("MODEL_NAME", "llama3-simulated")
LISTEN_ADDR = os.environ.get("LISTEN_ADDR", ":8000")
_IDENTITY = continuation.load_identity_from_env()
WORKLOAD_ID = _IDENTITY.key if _IDENTITY is not None else os.environ.get("KUASAR_WORKLOAD_ID", "pod-unknown:0")

MODEL = SimulatedVLLM(MODEL_NAME)
SESSIONS: dict[str, SessionState] = {}
SESSIONS_LOCK = threading.Lock()


class ContinuationHandler(BaseHTTPRequestHandler):
    def log_message(self, fmt: str, *args: Any) -> None:
        logger.debug(fmt, *args)

    def do_GET(self) -> None:
        if self.path == "/healthz":
            self._send_json(200, {
                "status": "ok",
                "workload_id": WORKLOAD_ID,
                "model": MODEL_NAME,
            })
            return
        if self.path.startswith("/v1/sessions/"):
            session_id = self.path.rsplit("/", 1)[-1]
            with SESSIONS_LOCK:
                session = SESSIONS.get(session_id)
            if session is None:
                self._send_json(404, {"error": "session not found"})
                return
            self._send_json(200, {
                "session_id": session_id,
                "prompt_count": session.prompt_count,
                "history": session.history,
                "last_response": session.last_response,
                "workload_id": WORKLOAD_ID,
            })
            return
        self._send_json(404, {"error": "not found"})

    def do_POST(self) -> None:
        if self.path.startswith("/v1/sessions/"):
            session_id = self.path.rsplit("/", 1)[-1]
            self._handle_session(session_id)
            return
        if self.path == "/v1/chat/completions":
            self._handle_chat()
            return
        self._send_json(404, {"error": "not found"})

    def _handle_session(self, session_id: str) -> None:
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)
        try:
            req = json.loads(body)
        except json.JSONDecodeError:
            self._send_json(400, {"error": "invalid JSON"})
            return

        prompt = req.get("prompt", "")
        if not prompt:
            self._send_json(400, {"error": "missing prompt"})
            return

        with SESSIONS_LOCK:
            session = SESSIONS.setdefault(session_id, SessionState())
            response = MODEL.generate(prompt, session)

        self._send_json(200, {
            "session_id": session_id,
            "model": MODEL_NAME,
            "response": response,
            "workload_id": WORKLOAD_ID,
        })

    def _handle_chat(self) -> None:
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)
        try:
            req = json.loads(body)
        except json.JSONDecodeError:
            self._send_json(400, {"error": "invalid JSON"})
            return

        session_id = req.get("session_id", "default")
        messages = req.get("messages", [])
        if not messages:
            self._send_json(400, {"error": "missing messages"})
            return
        prompt = messages[-1].get("content", "")
        with SESSIONS_LOCK:
            session = SESSIONS.setdefault(session_id, SessionState())
            response = MODEL.generate(prompt, session)

        self._send_json(200, {
            "id": f"chatcmpl-{session.prompt_count}",
            "object": "chat.completion",
            "workload_id": WORKLOAD_ID,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": response},
            }],
        })

    def _send_json(self, status: int, payload: dict[str, Any]) -> None:
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main() -> None:
    model = MODEL
    model.load()
    host, _, port_str = LISTEN_ADDR.partition(":")
    port = int(port_str or "8000")
    server = HTTPServer((host or "0.0.0.0", port), ContinuationHandler)
    logger.info("[phase1] serving %s on %s", MODEL_NAME, LISTEN_ADDR)
    logger.info("[phase1] workload identity: %s", WORKLOAD_ID)
    server.serve_forever()


if __name__ == "__main__":
    main()
