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
WarmFork readiness and injection protocol client (v1).

Protocol flow per connection::

    workload → CAPABILITIES
    sandboxer → PREPARE  (injection mode)
    workload → READY     (injection mode; no external side effects before this point)
    sandboxer → COMMIT   (injection mode after READY, or directly in autonomous mode)
    workload → STARTED   (formal commit point; execution begins after this)

Copy this file into your workload and call :func:`wait_for_injection` at the
end of your quiescent phase (Phase 3).  See the protocol specification at
``docs/proposals/warmfork_readiness_and_injection_protocol.md`` for the full description.

Example usage::

    import warmfork

    # Phase 3: open inject socket; snapshot taken here
    params = warmfork.wait_for_injection()

    # Phase 4: apply task identity if present and start serving
    params.apply_env_overrides()
    run_task(params.task_id, params.context)
"""

from __future__ import annotations

import json
import logging
import os
import socket
import struct
import sys
from dataclasses import dataclass, field

logger = logging.getLogger(__name__)

DEFAULT_SOCKET_PATH: str = "/run/warmfork-readiness.sock"
PROTOCOL_VERSION: str = "1"
_MAX_FRAME_LEN: int = 4 * 1024 * 1024  # 4 MiB


@dataclass
class TaskParams:
    """Per-task parameters delivered by the sandboxer after VM restore."""

    task_id: str
    env_overrides: dict[str, str] = field(default_factory=dict)
    context: str = ""

    def apply_env_overrides(self) -> None:
        """Apply :attr:`env_overrides` to the current process environment."""
        for key, value in self.env_overrides.items():
            os.environ[key] = value


def wait_for_injection(socket_path: str = DEFAULT_SOCKET_PATH) -> TaskParams:
    """Open the inject socket and block until restore commits.

    Handles the full WarmFork readiness and injection protocol:

    * Binds and listens on *socket_path*.
    * On each connection: sends ``CAPABILITIES``, then reads ``PREPARE``,
      ``COMMIT``, ``CANCEL``, or EOF.
    * Probe connections (sandboxer closes after ``CAPABILITIES``) loop back
      to ``accept()`` without altering task-related state.
    * Injection mode: validates ``PREPARE``, sends ``READY``, then waits for
      ``COMMIT`` and sends ``STARTED`` before beginning execution.
    * Autonomous mode: receives ``COMMIT`` immediately, sends ``STARTED``,
      and begins execution without a task identity.
    * ``STARTED`` is the formal commit point.
    * On ``CANCEL``: logs and exits cleanly.

    Args:
        socket_path: Unix socket path.  Defaults to
            ``/run/warmfork-readiness.sock``.  Override with the
            ``kuasar.io/warm-fork-readiness-socket`` pod annotation.

    Returns:
        :class:`TaskParams` with the injected task identity, or an empty
        ``task_id`` in autonomous mode (returned after ``STARTED`` has been
        sent).

    Raises:
        OSError: If the socket cannot be created or ``accept()`` fails.
    """
    try:
        os.unlink(socket_path)
    except FileNotFoundError:
        pass

    server_sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server_sock.bind(socket_path)
    server_sock.listen(1)
    logger.info("[warmfork] listening on %s", socket_path)

    try:
        while True:
            conn, _ = server_sock.accept()
            try:
                params = _handle_connection(conn)
            except Exception as exc:
                logger.warning("[warmfork] connection error: %s; continuing", exc)
                params = None
            finally:
                conn.close()

            if params is not None:
                return params

            logger.info("[warmfork] probe complete; back to READY")
    finally:
        server_sock.close()


# ── Internal helpers ──────────────────────────────────────────────────────────


def _handle_connection(conn: socket.socket) -> TaskParams | None:
    """Handle one accepted connection.

    Returns:
        :class:`TaskParams` if injection completed (STARTED sent).
        ``None`` if this was a probe connection (EOF after CAPABILITIES).
    """
    _write_msg(conn, {
        "type": "CAPABILITIES",
        "protocol_version": PROTOCOL_VERSION,
        "supported_features": ["prepare", "commit", "cancel"],
    })

    msg = _read_msg(conn)
    if msg is None:
        # EOF: probe connection closed by sandboxer after reading CAPABILITIES.
        return None

    msg_type = msg.get("type", "")

    if msg_type == "PREPARE":
        return _handle_prepare(conn, msg)

    if msg_type == "COMMIT":
        return _handle_autonomous(conn)

    if msg_type == "CANCEL":
        logger.info("[warmfork] CANCEL received (reason=%r); exiting cleanly",
                    msg.get("reason", ""))
        sys.exit(0)

    raise ValueError(f"unexpected message type {msg_type!r}")


def _handle_prepare(conn: socket.socket, msg: dict) -> TaskParams:
    """Validate PREPARE, send READY, wait for COMMIT, send STARTED."""
    task_id: str = msg.get("task_id", "")

    # Validate before sending READY. No external side effects before this point.
    # This is a protocol-violation defense: PREPARE in injection mode must carry
    # a non-empty task_id. Autonomous mode never reaches this branch because it
    # skips PREPARE entirely.
    if not task_id:
        _write_msg(conn, {
            "type": "REJECT",
            "reason": "invalid_task_id",
            "message": "task_id is absent or empty",
        })
        raise ValueError("PREPARE has empty task_id; sent REJECT")

    # Send READY — commit to no side effects up to this point.
    # Sandboxer will wait until all targets reply READY before sending COMMIT.
    _write_msg(conn, {"type": "READY"})

    # Wait for COMMIT or CANCEL.
    commit = _read_msg(conn)
    if commit is None:
        raise EOFError("connection closed while waiting for COMMIT")

    commit_type = commit.get("type", "")

    if commit_type == "CANCEL":
        logger.info("[warmfork] CANCEL after READY (reason=%r); exiting cleanly",
                    commit.get("reason", ""))
        sys.exit(0)

    if commit_type != "COMMIT":
        raise ValueError(f"expected COMMIT or CANCEL, got {commit_type!r}")

    # Send STARTED before beginning execution. This is the formal commit point:
    # the sandboxer considers the restore committed on receipt of STARTED.
    _write_msg(conn, {"type": "STARTED"})

    return TaskParams(
        task_id=task_id,
        env_overrides=msg.get("env_overrides") or {},
        context=msg.get("context") or "",
    )


def _handle_autonomous(conn: socket.socket) -> TaskParams:
    """Handle autonomous mode where the sandboxer sends COMMIT directly."""
    _write_msg(conn, {"type": "STARTED"})
    return TaskParams(task_id="")


def _read_msg(conn: socket.socket) -> dict | None:
    """Read one length-prefixed JSON message. Returns None on clean EOF."""
    header = _recv_exact(conn, 4)
    if header is None:
        return None  # peer closed cleanly

    (length,) = struct.unpack(">I", header)
    if length > _MAX_FRAME_LEN:
        raise ValueError(f"frame too large: {length} bytes")

    body = _recv_exact(conn, length)
    if body is None:
        raise EOFError("connection closed mid-frame")

    return json.loads(body.decode("utf-8"))


def _write_msg(conn: socket.socket, obj: dict) -> None:
    """Write one length-prefixed JSON message."""
    body = json.dumps(obj, separators=(",", ":")).encode("utf-8")
    conn.sendall(struct.pack(">I", len(body)) + body)


def _recv_exact(conn: socket.socket, n: int) -> bytes | None:
    """Read exactly *n* bytes from *conn*. Returns None on clean EOF."""
    buf = bytearray()
    while len(buf) < n:
        chunk = conn.recv(n - len(buf))
        if not chunk:
            return None
        buf.extend(chunk)
    return bytes(buf)
