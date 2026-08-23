#!/usr/bin/env python3
"""A small stdio MCP bridge between an agent host and JOSH."""

from __future__ import annotations

import json
import os
import queue
import secrets
import shutil
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, BinaryIO


MAX_FRAME_BYTES = 1_048_576
MAX_HEADER_BYTES = 8_192
MAX_SOURCE_BYTES = 1_048_576
MAX_SESSIONS = 16
MAX_RETAINED_SESSIONS = 64
DEFAULT_WALL_MS = 600_000
MAX_WALL_MS = 3_600_000
JOSH_RESPONSE_TIMEOUT_SECONDS = 30
# MCP version used by the local stdio clients supported by this plugin.
MCP_PROTOCOL_VERSION = "2025-06-18"
JOSH_PROTOCOL = "josh/1"
JOSH_LIMITS = {
    "max_frame_bytes": MAX_FRAME_BYTES,
    "max_active_requests": 64,
    "max_loaded_programs": 1,
    "max_total_executions": 1,
    "max_catalog_tools": 1,
    "max_catalog_bytes": MAX_FRAME_BYTES,
}
JOSH_ERROR_CODES = {
    "request.invalid",
    "request.method_not_found",
    "request.invalid_state",
    "request.limit",
    "request.cancelled",
    "catalog.invalid",
    "catalog.mismatch",
    "program.invalid",
    "program.unsatisfied",
    "execution.duplicate",
    "execution.failed",
    "tool.denied",
    "tool.unavailable",
    "agent.denied",
    "agent.unavailable",
    "model.denied",
    "model.unavailable",
    "user.denied",
    "user.unavailable",
    "sub_agent.denied",
    "sub_agent.unavailable",
    "replay.diverged",
    "permission.unavailable",
    "protocol.violation",
}
SUPPORTED_PROVIDER_METHODS = {
    "agent/message",
    "agent/ask",
    "model/request",
    "user/ask",
    "tool/invoke",
    "sub_agent/create",
    "sub_agent/run",
    "sub_agent/message",
    "sub_agent/ask",
}


class BridgeError(Exception):
    """A safe, user-actionable bridge error."""


def _compact_json(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def _json_size_is_bounded(value: Any, maximum: int = MAX_FRAME_BYTES) -> bool:
    try:
        return len(_compact_json(value)) <= maximum
    except (TypeError, ValueError):
        return False


def read_mcp_line(stream: BinaryIO, maximum: int = MAX_FRAME_BYTES) -> dict[str, Any] | None:
    """Read one newline-delimited JSON-RPC message from MCP stdio."""
    body = stream.readline(maximum + 1)
    if not body:
        return None
    if len(body) > maximum or not body.endswith(b"\n"):
        raise BridgeError("MCP message exceeds limit or lacks a newline")
    try:
        value = json.loads(body.decode("utf-8", "strict"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise BridgeError(f"MCP body is not JSON: {error.msg if hasattr(error, 'msg') else 'invalid UTF-8'}") from error
    if not isinstance(value, dict):
        raise BridgeError("MCP message must be a JSON object")
    return value


def write_mcp_line(stream: BinaryIO, value: dict[str, Any]) -> None:
    body = _compact_json(value)
    if len(body) > MAX_FRAME_BYTES:
        raise BridgeError("refusing to write oversized MCP message")
    stream.write(body + b"\n")
    stream.flush()


def _read_exact(stream: BinaryIO, length: int) -> bytes:
    chunks: list[bytes] = []
    remaining = length
    while remaining:
        chunk = stream.read(remaining)
        if not chunk:
            raise BridgeError("JOSH closed a frame before completion")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def read_josh_frame(stream: BinaryIO, maximum: int = MAX_FRAME_BYTES) -> dict[str, Any] | None:
    """Read the strict two-header Content-Length frame used by current JOSH."""
    header = bytearray()
    while not header.endswith(b"\r\n\r\n"):
        byte = stream.read(1)
        if not byte:
            if not header:
                return None
            raise BridgeError("unexpected EOF inside JOSH header")
        header.extend(byte)
        if len(header) > MAX_HEADER_BYTES:
            raise BridgeError("JOSH header exceeds limit")
    lines = bytes(header[:-4]).decode("ascii", "strict").split("\r\n")
    if len(lines) != 2 or not lines[0].startswith("Content-Length: ") or lines[1] != "Content-Type: application/josh+json; charset=utf-8":
        raise BridgeError("JOSH frame headers are invalid")
    digits = lines[0][len("Content-Length: "):]
    if not digits.isdigit() or (len(digits) > 1 and digits.startswith("0")):
        raise BridgeError("JOSH Content-Length is invalid")
    length = int(digits)
    if not 0 < length <= maximum:
        raise BridgeError("JOSH frame exceeds limit")
    body = _read_exact(stream, length)
    try:
        value = json.loads(body.decode("utf-8", "strict"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise BridgeError(f"JOSH body is not JSON: {error.msg if hasattr(error, 'msg') else 'invalid UTF-8'}") from error
    if not isinstance(value, dict):
        raise BridgeError("JOSH message must be a JSON object")
    if value.get("protocol") != JOSH_PROTOCOL:
        raise BridgeError("JOSH message has an unexpected protocol")
    return value


def write_josh_frame(stream: BinaryIO, value: dict[str, Any]) -> None:
    body = _compact_json(value)
    if not body or len(body) > MAX_FRAME_BYTES:
        raise BridgeError("refusing to write oversized JOSH frame")
    stream.write(
        f"Content-Length: {len(body)}\r\nContent-Type: application/josh+json; charset=utf-8\r\n\r\n".encode("ascii")
    )
    stream.write(body)
    stream.flush()


def _require_object(value: Any, name: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise BridgeError(f"{name} must be an object")
    return value


def _require_string(value: Any, name: str, maximum: int = 4_096) -> str:
    if not isinstance(value, str) or not value or len(value.encode("utf-8")) > maximum or any(char in value for char in "\x00\r\n"):
        raise BridgeError(f"{name} must be a bounded nonempty string")
    return value


def contained_source_path(workspace: Path, source_path: Any) -> Path:
    """Resolve one non-symlink .allen file below the configured workspace."""
    raw = _require_string(source_path, "source_path")
    candidate = Path(raw)
    if not candidate.is_absolute():
        candidate = workspace / candidate
    try:
        resolved = candidate.resolve(strict=True)
        resolved.relative_to(workspace)
    except (FileNotFoundError, OSError, ValueError) as error:
        raise BridgeError("source_path must resolve to a file inside the workspace") from error
    if not resolved.is_file() or resolved.suffix != ".allen":
        raise BridgeError("source_path must be a workspace-contained .allen file")
    return resolved


def inline_manifest_source(path: Path) -> str:
    try:
        content = path.read_bytes()
    except OSError as error:
        raise BridgeError(f"cannot read source_path: {error}") from error
    if len(content) > MAX_SOURCE_BYTES:
        raise BridgeError("source_path exceeds the 1 MiB bridge limit")
    try:
        source = content.decode("utf-8")
    except UnicodeDecodeError as error:
        raise BridgeError("source_path must be UTF-8") from error
    # Comments may precede a source manifest, but a loose program is deliberately
    # rejected: this bridge needs an explicit source-side capability declaration.
    non_comment = "\n".join(line for line in source.splitlines() if not line.lstrip().startswith("///")).lstrip()
    if not non_comment.startswith("manifest"):
        raise BridgeError("source_path must begin with an inline manifest")
    return source


def _catalog() -> list[dict[str, Any]]:
    schema = {
        "type": "object",
        "properties": {"text": {"type": "string"}},
        "required": ["text"],
        "additionalProperties": False,
    }
    return [{
        "name": "allen_integration_echo",
        "version": "1.0.0",
        "input_schema": schema,
        "output_schema": schema,
        "error_schema": schema,
        "effects": [],
        "idempotency": "idempotent",
    }]


def _result_payload(value: dict[str, Any]) -> dict[str, Any]:
    return {"content": [{"type": "text", "text": json.dumps(value, ensure_ascii=False, sort_keys=True)}], "structuredContent": value}


@dataclass
class JoshProcess:
    process: subprocess.Popen[bytes]

    @classmethod
    def spawn(cls, executable: Path) -> "JoshProcess":
        try:
            process = subprocess.Popen(
                [str(executable), "serve"],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                # Keep runtime diagnostics in the host's MCP stderr log without
                # risking a bounded PIPE filling and deadlocking the child.
                stderr=None,
            )
        except OSError as error:
            raise BridgeError(f"cannot launch JOSH: {error}") from error
        if process.stdin is None or process.stdout is None:
            process.kill()
            raise BridgeError("cannot open JOSH stdio")
        return cls(process)

    @property
    def stdin(self) -> BinaryIO:
        assert self.process.stdin is not None
        return self.process.stdin

    @property
    def stdout(self) -> BinaryIO:
        assert self.process.stdout is not None
        return self.process.stdout

    def send(self, message: dict[str, Any]) -> None:
        try:
            write_josh_frame(self.stdin, message)
        except (OSError, ValueError) as error:
            raise BridgeError(f"cannot write to JOSH: {error}") from error

    def receive(self, timeout_seconds: float = JOSH_RESPONSE_TIMEOUT_SECONDS) -> dict[str, Any]:
        outcome: queue.Queue[tuple[str, Any]] = queue.Queue(maxsize=1)

        def read_one() -> None:
            try:
                outcome.put(("message", read_josh_frame(self.stdout)))
            except Exception as error:
                outcome.put(("error", error))

        reader = threading.Thread(target=read_one, name="josh-frame-reader", daemon=True)
        reader.start()
        try:
            kind, value = outcome.get(timeout=timeout_seconds)
        except queue.Empty as error:
            cleanup_error = self.close()
            detail = f"; cleanup failed: {cleanup_error}" if cleanup_error else ""
            raise BridgeError(
                f"JOSH did not produce a complete frame within {timeout_seconds:g} seconds{detail}"
            ) from error
        if kind == "error":
            if isinstance(value, BridgeError):
                raise value
            raise BridgeError(f"cannot read from JOSH: {value}") from value
        message = value
        if message is None:
            exit_code = self.process.poll()
            raise BridgeError(f"JOSH closed before responding (exit {exit_code})")
        return message

    def close(self) -> str | None:
        errors: list[str] = []
        try:
            if self.process.stdin:
                self.process.stdin.close()
        except (OSError, ValueError) as error:
            errors.append(f"stdin close: {error}")
        terminate_error: str | None = None
        if self.process.poll() is None:
            try:
                self.process.terminate()
                self.process.wait(timeout=1)
            except (OSError, subprocess.TimeoutExpired) as error:
                terminate_error = str(error)
        if self.process.poll() is None:
            try:
                self.process.kill()
                self.process.wait(timeout=1)
            except (OSError, subprocess.TimeoutExpired) as error:
                errors.append(f"kill: {error}")
        if terminate_error and self.process.poll() is None:
            errors.append(f"terminate: {terminate_error}")
        elif terminate_error:
            print(
                f"josh-allen-mcp: forced kill was required for pid {self.process.pid}",
                file=sys.stderr,
            )
        try:
            if self.process.stdout and self.process.poll() is not None:
                self.process.stdout.close()
        except (OSError, ValueError) as error:
            errors.append(f"stdout close: {error}")
        if self.process.poll() is None:
            errors.append(f"pid {self.process.pid} is still running")
        return "; ".join(errors) or None

    def exit_code(self) -> int | None:
        return self.process.poll()


@dataclass
class Session:
    token: str
    endpoint: JoshProcess
    start_id: str
    execution_id: str
    subagent_model: str | None
    subagent_reasoning_effort: str | None
    state: str = "running"
    pending: dict[str, Any] | None = None
    terminal: dict[str, Any] | None = None
    execution_deadline: float | None = None
    finished_at: float | None = None

    def public_status(self) -> dict[str, Any]:
        value: dict[str, Any] = {"session_token": self.token, "state": self.state}
        if self.pending is not None:
            value["next_action"] = self.next_action()
        if self.terminal is not None:
            value["terminal"] = self.terminal
        return value

    def next_action(self) -> dict[str, Any]:
        assert self.pending is not None
        method = self.pending["method"]
        action = {
            "request_id": self.pending["id"],
            "method": method,
            "params": self.pending["params"],
        }
        result_shape: dict[str, Any]
        if method in {"agent/message", "sub_agent/message"}:
            result_shape = {"accepted": True}
        elif method == "tool/invoke":
            result_shape = {"outcome": "ok", "value": "STRUCTURED_TOOL_OUTPUT"}
        elif method == "sub_agent/create":
            result_shape = {"sub_agent_id": "NATIVE_CHILD_ID"}
        else:
            result_shape = {"value": "VALUE_MATCHING_response_schema"}
        action["resume_arguments_shape"] = {
            "session_token": self.token,
            "request_id": self.pending["id"],
            "result": result_shape,
        }
        if method.startswith("sub_agent/"):
            defaults = {}
            if self.subagent_model is not None:
                defaults["model"] = self.subagent_model
            if self.subagent_reasoning_effort is not None:
                defaults["reasoning_effort"] = self.subagent_reasoning_effort
            if defaults:
                action["prompt_governed_defaults"] = defaults
        return action

    def finish(self, terminal: dict[str, Any]) -> dict[str, Any]:
        self.state = "terminal"
        self.pending = None
        self.terminal = terminal
        self.finished_at = time.monotonic()
        cleanup_error = self.endpoint.close()
        if cleanup_error:
            self.terminal["cleanup_error"] = cleanup_error
        return self.public_status()


class Bridge:
    def __init__(self, workspace: Path | None = None, executable: Path | None = None) -> None:
        root = workspace or Path(os.environ.get("JOSH_ALLEN_WORKSPACE", os.getcwd()))
        try:
            self.workspace = root.resolve(strict=True)
        except OSError as error:
            raise BridgeError(f"workspace cannot be resolved: {error}") from error
        if not self.workspace.is_dir():
            raise BridgeError("workspace must be a directory")
        configured = os.environ.get("JOSH_ALLEN_JOSH_BIN")
        if executable is not None:
            self.executable = executable
        elif configured:
            self.executable = Path(configured)
        else:
            installed = shutil.which("josh")
            self.executable = Path(installed) if installed else self.workspace / "target" / "debug" / "josh"
        self.sessions: dict[str, Session] = {}

    def _evict_terminal_sessions(self) -> None:
        while len(self.sessions) >= MAX_RETAINED_SESSIONS:
            oldest = next(
                (
                    token
                    for token, session in self.sessions.items()
                    if session.state in {"terminal", "cancelled"}
                ),
                None,
            )
            if oldest is None:
                return
            self.sessions.pop(oldest)

    def _fail_session(self, session: Session, code: str, message: str) -> None:
        session.state = "terminal"
        session.pending = None
        session.terminal = {
            "outcome": "failed",
            "error": {"code": code, "message": message},
        }
        session.finished_at = time.monotonic()
        cleanup_error = session.endpoint.close()
        if cleanup_error:
            session.terminal["cleanup_error"] = cleanup_error

    def _expire_session(self, session: Session) -> bool:
        if (
            session.state in {"running", "waiting"}
            and session.execution_deadline is not None
            and time.monotonic() >= session.execution_deadline
        ):
            self._fail_session(
                session,
                "bridge.deadline_exceeded",
                "the JOSH execution wall-time deadline expired",
            )
            return True
        return False

    def _endpoint(self) -> JoshProcess:
        executable = self.executable
        try:
            executable = executable.resolve(strict=True)
        except OSError as error:
            raise BridgeError("JOSH is missing; install `josh` on PATH or set JOSH_ALLEN_JOSH_BIN") from error
        if not executable.is_file() or not os.access(executable, os.X_OK):
            raise BridgeError("JOSH is not executable; install `josh` on PATH or set JOSH_ALLEN_JOSH_BIN")
        return JoshProcess.spawn(executable)

    def _send_request(self, session: Session, request_id: str, method: str, params: dict[str, Any]) -> None:
        session.endpoint.send({"protocol": JOSH_PROTOCOL, "kind": "request", "id": request_id, "method": method, "params": params})

    def _expect_response(self, session: Session, request_id: str, method: str) -> dict[str, Any]:
        while True:
            message = session.endpoint.receive()
            if message.get("kind") == "notification":
                continue
            if message.get("kind") == "response" and message.get("id") == request_id:
                if "error" in message:
                    error = _require_object(message["error"], "JOSH error")
                    detail = json.dumps(error, ensure_ascii=False, sort_keys=True)
                    if len(detail.encode("utf-8")) > 8_192:
                        detail = detail[:8_192] + "..."
                    raise BridgeError(f"JOSH {method} failed: {detail}")
                result = message.get("result")
                return _require_object(result, f"JOSH {method} result")
            raise BridgeError(f"unexpected JOSH message while awaiting {method}")

    def _advance(self, session: Session) -> dict[str, Any]:
        while True:
            timeout = JOSH_RESPONSE_TIMEOUT_SECONDS
            if session.execution_deadline is not None:
                timeout = max(0.001, session.execution_deadline - time.monotonic())
            message = session.endpoint.receive(timeout)
            kind = message.get("kind")
            if kind == "notification":
                continue
            if kind == "request":
                request_id = _require_string(message.get("id"), "JOSH provider request id", 128)
                method = _require_string(message.get("method"), "JOSH provider method", 256)
                params = _require_object(message.get("params"), "JOSH provider params")
                if not _json_size_is_bounded(params):
                    raise BridgeError("JOSH provider request exceeds bridge limit")
                if method not in SUPPORTED_PROVIDER_METHODS:
                    if method == "agent/transcript":
                        code = "agent.unavailable"
                    elif method == "permission/request":
                        code = "permission.unavailable"
                    else:
                        code = "request.method_not_found"
                    session.endpoint.send({
                        "protocol": JOSH_PROTOCOL,
                        "kind": "response",
                        "id": request_id,
                        "error": {
                            "code": code,
                            "message": f"{method} is outside the prompt-assisted provider allowlist",
                        },
                    })
                    continue
                session.state = "waiting"
                session.pending = {"id": request_id, "method": method, "params": params}
                return session.public_status()
            if kind == "response" and message.get("id") == session.start_id:
                if "error" in message:
                    error = _require_object(message["error"], "JOSH execution error")
                    return session.finish({"outcome": "failed", "error": error})
                return session.finish(_require_object(message.get("result"), "JOSH execution result"))
            raise BridgeError("unexpected JOSH message while executing")

    def start(self, arguments: Any) -> dict[str, Any]:
        args = _require_object(arguments, "arguments")
        allowed = {"source_path", "entry", "input", "wall_ms", "subagent_model", "subagent_reasoning_effort"}
        if set(args) - allowed or "source_path" not in args:
            raise BridgeError("allen_session_start accepts source_path, entry, input, wall_ms, subagent_model, and subagent_reasoning_effort")
        self._evict_terminal_sessions()
        if sum(session.state in {"running", "waiting"} for session in self.sessions.values()) >= MAX_SESSIONS:
            raise BridgeError("too many active bridge sessions")
        source = inline_manifest_source(contained_source_path(self.workspace, args["source_path"]))
        entry = args.get("entry", "main")
        if not isinstance(entry, str) or not entry or len(entry) > 128:
            raise BridgeError("entry must be a bounded nonempty string")
        input_value = args.get("input", None)
        if not _json_size_is_bounded(input_value):
            raise BridgeError("input exceeds bridge limit")
        wall_ms = args.get("wall_ms", DEFAULT_WALL_MS)
        if not isinstance(wall_ms, int) or isinstance(wall_ms, bool) or not 1_000 <= wall_ms <= MAX_WALL_MS:
            raise BridgeError("wall_ms must be an integer from 1000 through 3600000")
        model = args.get("subagent_model")
        effort = args.get("subagent_reasoning_effort")
        if model is not None:
            model = _require_string(model, "subagent_model", 128)
        if effort is not None:
            effort = _require_string(effort, "subagent_reasoning_effort", 128)
        endpoint = self._endpoint()
        token = secrets.token_urlsafe(32)
        execution_id = f"exec-{secrets.token_hex(12)}"
        session = Session(token, endpoint, "start-1", execution_id, model, effort)
        try:
            ready = endpoint.receive()
            if ready.get("kind") != "notification" or ready.get("method") != "runtime/ready":
                raise BridgeError("JOSH did not announce runtime/ready")
            self._send_request(session, "init-1", "initialize", {
                "host": {"name": "josh-allen-mcp", "version": "0.1.1"},
                "protocol_versions": ["josh/1.3"],
                "language_versions": [">=0.1.0, <0.2.0"],
                "execution_mode": "attached",
                "invoking_session_id": f"mcp-{token}",
                "standard_capabilities": [],
                "limits": JOSH_LIMITS,
                "extensions": [],
            })
            self._expect_response(session, "init-1", "initialize")
            self._send_request(session, "catalog-1", "catalog/set", {
                "schema_dialect": "https://json-schema.org/draft/2020-12/schema",
                "tools": _catalog(),
            })
            self._expect_response(session, "catalog-1", "catalog/set")
            self._send_request(session, "load-1", "program/load", {
                "format": "source_bundle",
                "files": [{"path": "src/main.allen", "encoding": "utf8", "content": source}],
            })
            loaded = self._expect_response(session, "load-1", "program/load")
            self._send_request(session, session.start_id, "execution/start", {
                "execution_id": execution_id,
                "program_id": _require_string(loaded.get("program_id"), "JOSH program_id", 128),
                "artifact_digest": _require_string(loaded.get("artifact_digest"), "JOSH artifact_digest", 128),
                "entry": entry,
                "input": input_value,
                # Source containment is enforced by this bridge. A JOSH workdir
                # would itself require the matching filesystem capability.
                "working_directory": None,
                "granted_capabilities": [],
                # The source manifest, not the host catalog, selects tools.
                # This one-tool bridge grants it only when its exact catalog name
                # occurs in the source-side manifest/program contract.
                "granted_tools": ["allen_integration_echo"] if "allen_integration_echo" in source else [],
                "allowed_http_origins": [],
                "limits": {"wall_ms": wall_ms},
            })
            session.execution_deadline = time.monotonic() + wall_ms / 1_000
            self.sessions[token] = session
            return self._advance(session)
        except Exception:
            self.sessions.pop(token, None)
            cleanup_error = endpoint.close()
            if cleanup_error:
                print(f"josh-allen-mcp: startup cleanup failed: {cleanup_error}", file=sys.stderr)
            raise

    def _session(self, arguments: Any, required: set[str], optional: set[str] = set()) -> tuple[Session, dict[str, Any]]:
        args = _require_object(arguments, "arguments")
        if set(args) - required - optional or not required <= set(args):
            allowed = ", ".join(sorted(required | optional))
            raise BridgeError(f"invalid arguments; expected {allowed}")
        token = _require_string(args.get("session_token"), "session_token", 256)
        session = self.sessions.get(token)
        if session is None:
            raise BridgeError("session_token is unknown")
        return session, args

    def resume(self, arguments: Any) -> dict[str, Any]:
        session, args = self._session(arguments, {"session_token", "request_id"}, {"result", "error"})
        if self._expire_session(session):
            raise BridgeError("session execution deadline has expired")
        if session.state != "waiting" or session.pending is None:
            raise BridgeError("session has no outstanding next_action")
        request_id = _require_string(args["request_id"], "request_id", 128)
        if request_id != session.pending["id"]:
            raise BridgeError("request_id does not match the outstanding next_action")
        has_result = "result" in args
        has_error = "error" in args
        if has_result == has_error:
            raise BridgeError("resume requires exactly one of result or error")
        if has_result and not _json_size_is_bounded(args["result"]):
            raise BridgeError("result exceeds bridge limit")
        message: dict[str, Any] = {"protocol": JOSH_PROTOCOL, "kind": "response", "id": request_id}
        if has_result:
            message["result"] = args["result"]
        else:
            error = _require_object(args["error"], "error")
            if set(error) - {"code", "message", "data"} or not {"code", "message"} <= set(error):
                raise BridgeError("error must contain code and message, with optional data")
            code = _require_string(error["code"], "error.code", 128)
            if code not in JOSH_ERROR_CODES:
                raise BridgeError("error.code is not a JOSH wire error code")
            _require_string(error["message"], "error.message", 1_024)
            if "data" in error and not _json_size_is_bounded(error["data"]):
                raise BridgeError("error.data exceeds bridge limit")
            message["error"] = error
        try:
            session.endpoint.send(message)
            session.pending = None
            session.state = "running"
            return self._advance(session)
        except Exception as error:
            self._fail_session(session, "bridge.execution_failed", str(error))
            raise

    def cancel(self, arguments: Any) -> dict[str, Any]:
        session, _ = self._session(arguments, {"session_token"})
        if session.state in {"terminal", "cancelled"}:
            return session.public_status()
        try:
            session.endpoint.send({"protocol": JOSH_PROTOCOL, "kind": "cancel", "id": session.start_id, "reason": "cancelled by MCP caller"})
        except (BridgeError, OSError):
            pass
        session.state = "cancelled"
        session.pending = None
        session.terminal = {"outcome": "cancelled", "reason": "cancelled by MCP caller"}
        session.finished_at = time.monotonic()
        cleanup_error = session.endpoint.close()
        if cleanup_error:
            session.state = "terminal"
            session.terminal = {
                "outcome": "failed",
                "error": {
                    "code": "bridge.cleanup_failed",
                    "message": cleanup_error,
                },
            }
        return session.public_status()

    def status(self, arguments: Any) -> dict[str, Any]:
        session, _ = self._session(arguments, {"session_token"})
        if self._expire_session(session):
            return session.public_status()
        if session.state in {"running", "waiting"}:
            exit_code = session.endpoint.exit_code()
            if exit_code is not None:
                self._fail_session(
                    session,
                    "bridge.child_exited",
                    f"JOSH exited unexpectedly with code {exit_code}",
                )
        return session.public_status()

    def echo(self, arguments: Any) -> dict[str, Any]:
        args = _require_object(arguments, "arguments")
        if set(args) != {"text"} or not isinstance(args["text"], str):
            raise BridgeError("allen_integration_echo requires exactly {text: string}")
        if len(args["text"].encode("utf-8")) > 16_384:
            raise BridgeError("echo text exceeds limit")
        return {"text": args["text"]}

    def close_all(self) -> None:
        for session in self.sessions.values():
            if session.state not in {"terminal", "cancelled"}:
                result = self.cancel({"session_token": session.token})
                if result["state"] == "terminal":
                    print(
                        f"josh-allen-mcp: session cleanup failed: {result['terminal']}",
                        file=sys.stderr,
                    )


TOOLS = [
    {
        "name": "allen_session_start",
        "description": (
            "Start one manifest-first ALLEN source session. Inspect the exported entry first: "
            "omit input for a zero-parameter entry, and otherwise pass its exact JSON input."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "source_path": {
                    "type": "string",
                    "description": "Workspace-contained path to the manifest-first .allen source file.",
                },
                "entry": {
                    "type": "string",
                    "default": "main",
                    "description": "Exported entry function; omit to use main.",
                },
                "input": {
                    "description": (
                        "Exact JSON value for the entry's single parameter. Omit this property when "
                        "the entry has zero parameters; do not use an empty object as a placeholder."
                    ),
                },
                "wall_ms": {
                    "type": "integer",
                    "minimum": 1000,
                    "maximum": MAX_WALL_MS,
                    "default": DEFAULT_WALL_MS,
                },
                "subagent_model": {
                    "type": "string",
                    "description": "Optional prompt-governed default for sub_agent actions.",
                },
                "subagent_reasoning_effort": {
                    "type": "string",
                    "description": "Optional prompt-governed default for sub_agent actions.",
                },
            },
            "required": ["source_path"],
            "additionalProperties": False,
        },
    },
    {"name": "allen_session_resume", "description": "Submit exactly one completion for the current JOSH next_action. Copy next_action.resume_arguments_shape as this tool's outer arguments, replacing only placeholder values; never flatten its nested result object.", "inputSchema": {"type": "object", "properties": {"session_token": {"type": "string"}, "request_id": {"type": "string"}, "result": {"description": "The nested JOSH provider result object from next_action.resume_arguments_shape; do not flatten its fields into this tool's outer arguments."}, "error": {"type": "object"}}, "required": ["session_token", "request_id"], "oneOf": [{"required": ["result"]}, {"required": ["error"]}], "additionalProperties": False}},
    {"name": "allen_session_cancel", "description": "Cancel and clean up a JOSH session.", "inputSchema": {"type": "object", "properties": {"session_token": {"type": "string"}}, "required": ["session_token"], "additionalProperties": False}},
    {"name": "allen_session_status", "description": "Read the terminal result or outstanding next_action for a JOSH session.", "inputSchema": {"type": "object", "properties": {"session_token": {"type": "string"}}, "required": ["session_token"], "additionalProperties": False}},
    {"name": "allen_integration_echo", "description": "Deterministically echo text; also the sole JOSH catalog tool.", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"], "additionalProperties": False}},
]


def mcp_error(request_id: Any, code: int, message: str) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}}


def dispatch(bridge: Bridge, request: dict[str, Any]) -> dict[str, Any] | None:
    request_id = request.get("id")
    if request.get("jsonrpc") != "2.0" or not isinstance(request.get("method"), str):
        return mcp_error(request_id, -32600, "invalid JSON-RPC request")
    method = request["method"]
    if method == "notifications/initialized":
        return None
    if method == "initialize":
        result = {"protocolVersion": MCP_PROTOCOL_VERSION, "capabilities": {"tools": {}}, "serverInfo": {"name": "josh-allen-mcp", "version": "0.1.1"}, "instructions": "For each next_action, perform the real host operation. Copy next_action.resume_arguments_shape as the complete allen_session_resume arguments and replace only its placeholder. Keep the provider response nested under result. Never fabricate provider results. Pause for real user input for user/ask. Use the host's native agent tools for sub_agent/*; prompt_governed_defaults are hints, not authority."}
    elif method == "tools/list":
        result = {"tools": TOOLS}
    elif method == "tools/call":
        params = request.get("params")
        if not isinstance(params, dict) or not isinstance(params.get("name"), str):
            return mcp_error(request_id, -32602, "tools/call requires a tool name")
        arguments = params.get("arguments", {})
        handlers = {
            "allen_session_start": bridge.start,
            "allen_session_resume": bridge.resume,
            "allen_session_cancel": bridge.cancel,
            "allen_session_status": bridge.status,
            "allen_integration_echo": bridge.echo,
        }
        handler = handlers.get(params["name"])
        if handler is None:
            return mcp_error(request_id, -32602, "unknown tool")
        try:
            result = _result_payload(handler(arguments))
        except BridgeError as error:
            value = {"error": str(error)}
            result = {**_result_payload(value), "isError": True}
    else:
        return mcp_error(request_id, -32601, "method not found")
    if request_id is None:
        return None
    return {"jsonrpc": "2.0", "id": request_id, "result": result}


def main() -> int:
    try:
        bridge = Bridge()
    except BridgeError as error:
        print(f"josh-allen-mcp: {error}", file=sys.stderr)
        return 2
    try:
        while True:
            request = read_mcp_line(sys.stdin.buffer)
            if request is None:
                return 0
            response = dispatch(bridge, request)
            if response is not None:
                write_mcp_line(sys.stdout.buffer, response)
    except BridgeError as error:
        print(f"josh-allen-mcp: {error}", file=sys.stderr)
        return 1
    finally:
        bridge.close_all()


if __name__ == "__main__":
    raise SystemExit(main())
