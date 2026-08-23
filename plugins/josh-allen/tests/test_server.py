"""Regression tests for the JOSH/ALLEN MCP bridge."""

from __future__ import annotations

import io
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).parents[1] / "mcp"))
import server  # noqa: E402


class FramingTests(unittest.TestCase):
    def test_mcp_newline_json_round_trip(self) -> None:
        output = io.BytesIO()
        server.write_mcp_line(output, {"jsonrpc": "2.0", "id": 1, "result": {"text": "hé"}})
        self.assertEqual(
            server.read_mcp_line(io.BytesIO(output.getvalue())),
            {"jsonrpc": "2.0", "id": 1, "result": {"text": "hé"}},
        )

    def test_josh_frame_is_byte_counted_and_rejects_bad_content_type(self) -> None:
        output = io.BytesIO()
        server.write_josh_frame(output, {"protocol": "josh/1", "kind": "notification", "method": "x", "params": {"text": "hé"}})
        self.assertEqual(server.read_josh_frame(io.BytesIO(output.getvalue()))["params"], {"text": "hé"})
        with self.assertRaisesRegex(server.BridgeError, "headers are invalid"):
            server.read_josh_frame(io.BytesIO(b"Content-Length: 2\r\nContent-Type: application/json\r\n\r\n{}"))

    def test_josh_receive_timeout_terminates_the_child(self) -> None:
        process = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(60)"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        endpoint = server.JoshProcess(process)
        with self.assertRaisesRegex(server.BridgeError, "did not produce a complete frame"):
            endpoint.receive(0.01)
        self.assertIsNotNone(process.poll())

    def test_closed_josh_stdin_is_reported_as_a_bridge_error(self) -> None:
        process = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(60)"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        endpoint = server.JoshProcess(process)
        assert process.stdin is not None
        process.stdin.close()
        try:
            with self.assertRaisesRegex(server.BridgeError, "cannot write to JOSH"):
                endpoint.send({"protocol": "josh/1", "kind": "response", "id": "r-1", "result": {}})
        finally:
            endpoint.close()

    def test_successful_force_kill_is_not_a_cleanup_failure(self) -> None:
        class Pipe:
            def close(self) -> None:
                pass

        class ForceKilledProcess:
            def __init__(self) -> None:
                self.stdin = Pipe()
                self.stdout = Pipe()
                self.pid = 123
                self.code: int | None = None

            def poll(self) -> int | None:
                return self.code

            def terminate(self) -> None:
                pass

            def wait(self, timeout: float) -> int:
                if self.code is None:
                    raise subprocess.TimeoutExpired("fake-josh", timeout)
                return self.code

            def kill(self) -> None:
                self.code = -9

        endpoint = server.JoshProcess(ForceKilledProcess())
        self.assertIsNone(endpoint.close())


class BridgeValidationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.workspace = Path(self.directory.name)
        self.bridge = server.Bridge(self.workspace, self.workspace / "missing-josh")

    def tearDown(self) -> None:
        self.bridge.close_all()
        self.directory.cleanup()

    def test_source_must_be_contained_and_manifest_first(self) -> None:
        source = self.workspace / "task.allen"
        source.write_text("export fn main() returns Int { 1 }\n", encoding="utf-8")
        with self.assertRaisesRegex(server.BridgeError, "inline manifest"):
            self.bridge.start({"source_path": "task.allen"})
        with self.assertRaisesRegex(server.BridgeError, "inside the workspace"):
            server.contained_source_path(self.workspace, "../outside.allen")

    def test_terminal_session_retention_is_bounded(self) -> None:
        class NoProcess:
            def close(self) -> str | None:
                return None

        for index in range(server.MAX_RETAINED_SESSIONS):
            token = f"token-{index}"
            self.bridge.sessions[token] = server.Session(
                token,
                NoProcess(),
                "start-1",
                "exec-1",
                None,
                None,
                state="terminal",
                terminal={"outcome": "completed", "output": index},
                finished_at=float(index),
            )
        self.bridge._evict_terminal_sessions()
        self.assertEqual(len(self.bridge.sessions), server.MAX_RETAINED_SESSIONS - 1)
        self.assertNotIn("token-0", self.bridge.sessions)

    def test_echo_shape_and_mcp_structured_content(self) -> None:
        self.assertEqual(self.bridge.echo({"text": "hello"}), {"text": "hello"})
        response = server.dispatch(self.bridge, {
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {"name": "allen_integration_echo", "arguments": {"text": "hello"}},
        })
        assert response is not None
        payload = response["result"]
        self.assertEqual(payload["structuredContent"], {"text": "hello"})
        self.assertEqual(json.loads(payload["content"][0]["text"]), {"text": "hello"})

    def test_start_tool_explains_zero_parameter_input(self) -> None:
        start = next(tool for tool in server.TOOLS if tool["name"] == "allen_session_start")
        schema = start["inputSchema"]
        self.assertNotIn("input", schema["required"])
        self.assertIn("zero parameters", schema["properties"]["input"]["description"])
        self.assertEqual(schema["properties"]["entry"]["default"], "main")

    def test_prompt_showcases_are_short_mechanics_free_requests(self) -> None:
        prompt_dir = Path(__file__).parents[3] / "examples" / "josh-allen" / "showcases" / "prompts"
        prompts = sorted(prompt_dir.glob("*.prompt"))
        self.assertEqual(len(prompts), 8)
        for prompt in prompts:
            with self.subTest(prompt=prompt.name):
                text = prompt.read_text(encoding="utf-8")
                self.assertLessEqual(len(text.split()), 80)
                lowered = text.lower()
                for mechanical_term in (
                    ".allen",
                    "josh",
                    "manifest",
                    "showcases/generated",
                    "write a program",
                    "execute it through",
                ):
                    self.assertNotIn(mechanical_term, lowered)

    def test_resume_requires_matching_pending_request(self) -> None:
        class NoProcess:
            def send(self, message: dict[str, object]) -> None:
                self.message = message

            def close(self) -> None:
                pass

        endpoint = NoProcess()
        session = server.Session("token", endpoint, "start-1", "exec-1", None, None, state="waiting", pending={"id": "provider-1", "method": "model/request", "params": {}})
        self.bridge.sessions[session.token] = session
        with self.assertRaisesRegex(server.BridgeError, "does not match"):
            self.bridge.resume({"session_token": "token", "request_id": "wrong", "result": {}})
        with self.assertRaisesRegex(server.BridgeError, "exactly one"):
            self.bridge.resume({"session_token": "token", "request_id": "provider-1", "result": {}, "error": {}})

    def test_cancelled_session_rejects_a_late_or_duplicate_resume(self) -> None:
        class NoProcess:
            def send(self, message: dict[str, object]) -> None:
                self.message = message

            def close(self) -> str | None:
                return None

        session = server.Session(
            "token",
            NoProcess(),
            "start-1",
            "exec-1",
            None,
            None,
            state="waiting",
            pending={"id": "provider-1", "method": "agent/message", "params": {}},
        )
        self.bridge.sessions[session.token] = session
        cancelled = self.bridge.cancel({"session_token": "token"})
        self.assertEqual(cancelled["state"], "cancelled")
        with self.assertRaisesRegex(server.BridgeError, "no outstanding"):
            self.bridge.resume({
                "session_token": "token",
                "request_id": "provider-1",
                "result": {"accepted": True},
            })
        with self.assertRaisesRegex(server.BridgeError, "unknown"):
            self.bridge.status({"session_token": "wrong-token"})

    def test_status_turns_a_dead_child_into_a_terminal_failure(self) -> None:
        class DeadProcess:
            def exit_code(self) -> int:
                return -9

            def close(self) -> str | None:
                return None

        session = server.Session(
            "token",
            DeadProcess(),
            "start-1",
            "exec-1",
            None,
            None,
            state="waiting",
            pending={"id": "provider-1", "method": "agent/message", "params": {}},
        )
        self.bridge.sessions[session.token] = session
        status = self.bridge.status({"session_token": "token"})
        self.assertEqual(status["state"], "terminal")
        self.assertEqual(status["terminal"]["error"]["code"], "bridge.child_exited")
        self.assertNotIn("next_action", status)

    def test_status_and_resume_fail_an_expired_waiting_session(self) -> None:
        class NoProcess:
            def close(self) -> str | None:
                return None

        session = server.Session(
            "token",
            NoProcess(),
            "start-1",
            "exec-1",
            None,
            None,
            state="waiting",
            pending={"id": "provider-1", "method": "user/ask", "params": {}},
            execution_deadline=server.time.monotonic() - 1,
        )
        self.bridge.sessions[session.token] = session
        with self.assertRaisesRegex(server.BridgeError, "deadline has expired"):
            self.bridge.resume({
                "session_token": "token",
                "request_id": "provider-1",
                "result": {"value": True},
            })
        status = self.bridge.status({"session_token": "token"})
        self.assertEqual(status["state"], "terminal")
        self.assertEqual(status["terminal"]["error"]["code"], "bridge.deadline_exceeded")
        self.assertNotIn("next_action", status)

    def test_resume_transport_failure_is_not_recorded_as_user_cancel(self) -> None:
        class BrokenProcess:
            def send(self, message: dict[str, object]) -> None:
                raise server.BridgeError("closed pipe")

            def close(self) -> str | None:
                return None

        session = server.Session(
            "token",
            BrokenProcess(),
            "start-1",
            "exec-1",
            None,
            None,
            state="waiting",
            pending={"id": "provider-1", "method": "agent/message", "params": {}},
            execution_deadline=server.time.monotonic() + 60,
        )
        self.bridge.sessions[session.token] = session
        with self.assertRaisesRegex(server.BridgeError, "closed pipe"):
            self.bridge.resume({
                "session_token": "token",
                "request_id": "provider-1",
                "result": {"accepted": True},
            })
        status = self.bridge.status({"session_token": "token"})
        self.assertEqual(status["terminal"]["error"]["code"], "bridge.execution_failed")
        self.assertNotEqual(status["terminal"]["outcome"], "cancelled")

    def test_unsupported_provider_request_is_failed_without_exposure(self) -> None:
        class ScriptedProcess:
            def __init__(self) -> None:
                self.messages = iter([
                    {
                        "protocol": "josh/1",
                        "kind": "request",
                        "id": "provider-1",
                        "method": "agent/transcript",
                        "params": {},
                    },
                    {
                        "protocol": "josh/1",
                        "kind": "response",
                        "id": "start-1",
                        "result": {"outcome": "completed", "output": None},
                    },
                ])
                self.sent: list[dict[str, object]] = []
                self.timeouts: list[float] = []

            def receive(self, timeout_seconds: float) -> dict[str, object]:
                self.timeouts.append(timeout_seconds)
                return next(self.messages)

            def send(self, message: dict[str, object]) -> None:
                self.sent.append(message)

            def close(self) -> str | None:
                return None

        endpoint = ScriptedProcess()
        session = server.Session(
            "token",
            endpoint,
            "start-1",
            "exec-1",
            None,
            None,
            execution_deadline=server.time.monotonic() + 120,
        )
        result = self.bridge._advance(session)
        self.assertEqual(result["state"], "terminal")
        self.assertEqual(endpoint.sent[0]["error"]["code"], "agent.unavailable")
        self.assertNotIn("next_action", result)
        self.assertTrue(all(119 < timeout <= 120 for timeout in endpoint.timeouts))

    def test_subagent_action_carries_only_prompt_governed_defaults(self) -> None:
        class NoProcess:
            def close(self) -> str | None:
                return None

        session = server.Session(
            "token", NoProcess(), "start-1", "exec-1", "gpt-5.6", "high",
            state="waiting", pending={"id": "provider-1", "method": "sub_agent/run", "params": {"prompt": {}}},
        )
        action = session.next_action()
        self.assertEqual(action["prompt_governed_defaults"], {"model": "gpt-5.6", "reasoning_effort": "high"})
        self.assertEqual(
            action["resume_arguments_shape"],
            {
                "session_token": "token",
                "request_id": "provider-1",
                "result": {"value": "VALUE_MATCHING_response_schema"},
            },
        )
        self.assertNotIn("authority", action)


@unittest.skipUnless((Path(__file__).parents[3] / "target" / "debug" / "josh").is_file(), "target/debug/josh is not built")
class JoshEndToEndTests(unittest.TestCase):
    repo_root = Path(__file__).parents[3]
    josh = repo_root / "target" / "debug" / "josh"

    def _complete_source_through_josh(self, source_path: str, responses: list[tuple[str, dict[str, Any]]]) -> dict[str, Any]:
        bridge = server.Bridge(self.repo_root, self.josh)
        try:
            state = bridge.start({"source_path": source_path})
            actual_methods = []
            for expected_method, result in responses:
                self.assertEqual(state["state"], "waiting")
                action = state["next_action"]
                actual_methods.append(action["method"])
                self.assertEqual(action["method"], expected_method)
                state = bridge.resume({
                    "session_token": state["session_token"],
                    "request_id": action["request_id"],
                    "result": result,
                })
        finally:
            bridge.close_all()

        self.assertEqual(actual_methods, [method for method, _ in responses])
        self.assertEqual(state["state"], "terminal")
        self.assertEqual(state["terminal"]["outcome"], "completed")
        return state["terminal"]

    def test_manifest_source_completes_through_real_josh(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            workspace = Path(temporary)
            (workspace / "task.allen").write_text(
                'manifest {\n  language: "0.1"\n  entry: main\n  capabilities: []\n}\nexport fn main() returns Int { 42 }\n',
                encoding="utf-8",
            )
            bridge = server.Bridge(workspace, Path(__file__).parents[3] / "target" / "debug" / "josh")
            try:
                outcome = bridge.start({"source_path": "task.allen"})
            finally:
                bridge.close_all()
            self.assertEqual(outcome["state"], "terminal")
            self.assertEqual(outcome["terminal"], {"outcome": "completed", "output": 42})

    def test_tool_echo_preserves_every_unattended_input_entry(self) -> None:
        schema = {
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": False,
        }
        catalog = {
            "schema_dialect": "https://json-schema.org/draft/2020-12/schema",
            "metadata": {
                "source": "test-adapter",
                "source_revision": "revision-13",
                "observed_at_unix_ms": 1,
                "freshness": "current",
                "complete": True,
            },
            "tools": [
                {
                    "name": f"example.tool_{index:02}",
                    "version": "1.0.0",
                    "description": f"Example tool {index}.",
                    "input_schema": schema,
                    "output_schema": schema,
                    "error_schema": schema,
                    "effects": [],
                    "idempotency": "idempotent",
                }
                for index in range(13)
            ],
        }
        with tempfile.TemporaryDirectory() as temporary:
            catalog_path = Path(temporary) / "catalog.json"
            catalog_path.write_text(json.dumps(catalog), encoding="utf-8")
            completed = subprocess.run(
                [
                    str(self.josh),
                    "run",
                    "--catalog",
                    str(catalog_path),
                    "--catalog-input",
                    "examples/josh-allen/tool-echo.allen",
                ],
                cwd=self.repo_root,
                check=True,
                capture_output=True,
                text=True,
            )
        output = json.loads(completed.stdout)["output"]
        self.assertEqual(output["metadata"], catalog["metadata"])
        self.assertEqual(output["tool_count"], 13)
        self.assertEqual(
            output["tools"],
            [
                {
                    "name": tool["name"],
                    "version": tool["version"],
                    "description": tool["description"],
                }
                for tool in catalog["tools"]
            ],
        )
        self.assertTrue(output["catalog_digest"].startswith("sha256:"))

    def test_tool_request_round_trips_through_real_josh(self) -> None:
        source = '''manifest { language: "0.1" entry: main capabilities: [] tools: { required: [ { name: "allen_integration_echo", version: ">=1.0.0, <2.0.0" } ] } }
export async fn main(value: String) returns Result<tools.allen_integration_echo.Output, tools.allen_integration_echo.Error> effects [tool.allen_integration_echo@1] { await tools.allen_integration_echo.call({ text: value }) }
'''
        with tempfile.TemporaryDirectory() as temporary:
            workspace = Path(temporary)
            (workspace / "task.allen").write_text(source, encoding="utf-8")
            bridge = server.Bridge(workspace, Path(__file__).parents[3] / "target" / "debug" / "josh")
            try:
                waiting = bridge.start({"source_path": "task.allen", "input": "hello"})
                action = waiting["next_action"]
                self.assertEqual(action["method"], "tool/invoke")
                outcome = bridge.resume({
                    "session_token": waiting["session_token"],
                    "request_id": action["request_id"],
                    "result": {"outcome": "ok", "value": {"text": "hello"}},
                })
            finally:
                bridge.close_all()
            self.assertEqual(outcome["terminal"], {"outcome": "completed", "output": {"tag": "Ok", "value": {"text": "hello"}}})

    def test_repository_mvp_example_routes_every_provider_action(self) -> None:
        bridge = server.Bridge(self.repo_root, self.josh)
        try:
            state = bridge.start({
                "source_path": "examples/codex-agent-mvp.allen",
                "input": "MVP input",
                "subagent_model": "gpt-5.6-luna",
                "subagent_reasoning_effort": "low",
            })
            methods: list[str] = []
            results = {
                "agent/message": {"accepted": True},
                "agent/ask": {"value": "Codex acknowledged."},
                "user/ask": {"value": True},
                "tool/invoke": {"outcome": "ok", "value": {"text": "MVP input"}},
                "sub_agent/run": {"value": "Independent review complete."},
            }
            while state["state"] == "waiting":
                action = state["next_action"]
                method = action["method"]
                methods.append(method)
                if method == "sub_agent/run":
                    self.assertEqual(
                        action["prompt_governed_defaults"],
                        {"model": "gpt-5.6-luna", "reasoning_effort": "low"},
                    )
                state = bridge.resume({
                    "session_token": state["session_token"],
                    "request_id": action["request_id"],
                    "result": results[method],
                })
        finally:
            bridge.close_all()

        self.assertEqual(
            methods,
            ["agent/message", "agent/ask", "user/ask", "tool/invoke", "sub_agent/run"],
        )
        self.assertEqual(state["state"], "terminal")
        self.assertEqual(state["terminal"]["outcome"], "completed")
        self.assertEqual(
            state["terminal"]["output"],
            {
                "message_status": "sent",
                "agent_reply": "Codex acknowledged.",
                "user_approved": True,
                "echoed": "MVP input",
                "subagent_reply": "Independent review complete.",
            },
        )

    def test_single_feature_examples_complete_through_real_josh(self) -> None:
        cases = [
            (
                "agent-message.allen",
                "agent/message",
                {"accepted": True},
                "message accepted",
            ),
            (
                "agent-ask.allen",
                "agent/ask",
                {"value": "Hello from Codex."},
                "Hello from Codex.",
            ),
            (
                "model-request.allen",
                "model/request",
                {"value": "Typed boundaries reduce ambiguity."},
                "Typed boundaries reduce ambiguity.",
            ),
            (
                "user-ask.allen",
                "user/ask",
                {"value": True},
                True,
            ),
            (
                "tool-call.allen",
                "tool/invoke",
                {"outcome": "ok", "value": {"text": "hello through JOSH"}},
                "hello through JOSH",
            ),
            (
                "subagent-run.allen",
                "sub_agent/run",
                {"value": "A native Codex child returned this sentence."},
                "A native Codex child returned this sentence.",
            ),
        ]

        for filename, method, result, expected_output in cases:
            with self.subTest(filename=filename):
                bridge = server.Bridge(self.repo_root, self.josh)
                try:
                    state = bridge.start({
                        "source_path": f"examples/josh-allen/{filename}",
                    })
                    action = state["next_action"]
                    self.assertEqual(action["method"], method)
                    if filename == "subagent-run.allen":
                        self.assertEqual(action["params"]["prompt"]["data"], {"tag": "None"})
                        self.assertNotIn("prompt_governed_defaults", action)
                    state = bridge.resume({
                        "session_token": state["session_token"],
                        "request_id": action["request_id"],
                        "result": result,
                    })
                finally:
                    bridge.close_all()

                self.assertEqual(state["state"], "terminal")
                self.assertEqual(state["terminal"], {
                    "outcome": "completed",
                    "output": expected_output,
                })

        bridge = server.Bridge(self.repo_root, self.josh)
        try:
            state = bridge.start({
                "source_path": "examples/josh-allen/tool-echo.allen",
                "catalog_input": True,
            })
        finally:
            bridge.close_all()
        session_token = state.pop("session_token")
        self.assertIsInstance(session_token, str)
        self.assertEqual(state["state"], "terminal")
        catalog = state["terminal"]["output"]
        self.assertEqual(catalog["metadata"]["source"], "josh-allen-mcp")
        self.assertEqual(catalog["metadata"]["source_revision"], "0.1.2")
        self.assertEqual(catalog["metadata"]["freshness"], "current")
        self.assertTrue(catalog["metadata"]["complete"])
        self.assertGreater(catalog["metadata"]["observed_at_unix_ms"], 0)
        self.assertEqual(catalog["tool_count"], 1)
        self.assertEqual(catalog["tools"][0]["name"], "allen_integration_echo")

    def test_incomplete_catalog_adapter_fails_before_program_load(self) -> None:
        class IncompleteAdapter(server.BridgeCatalogAdapter):
            def snapshot(self) -> dict[str, Any]:
                catalog = super().snapshot()
                catalog["metadata"]["complete"] = False
                return catalog

        bridge = server.Bridge(self.repo_root, self.josh, IncompleteAdapter())
        try:
            with self.assertRaisesRegex(server.BridgeError, "tool catalog is incomplete"):
                bridge.start({
                    "source_path": "examples/josh-allen/tool-echo.allen",
                    "catalog_input": True,
                })
        finally:
            bridge.close_all()

    def test_real_world_showcases_complete_through_real_josh(self) -> None:
        cases = {
            "guarded-repository-migration.allen": (
                [
                    ("agent/ask", {"value": {"approve": True, "reason": "retry behavior is preserved"}}),
                    ("agent/message", {"accepted": True}),
                ],
                {
                    "approved_changes": 3,
                    "deterministic_changes": 2,
                    "status": "reviewed plan ready",
                },
            ),
            "test-failure-minimization.allen": (
                [
                    ("agent/ask", {"value": {"case_id": "timeout-race", "reason": "narrower timing signal"}}),
                ],
                {
                    "minimum_steps": 3,
                    "selected_case": "timeout-race",
                    "selection": "agent-prioritized bounded tie",
                },
            ),
            "incident-triage.allen": (
                [
                    ("model/request", {"value": {"component": "checkout-api", "confidence": 92, "evidence": "checkout rate is isolated"}}),
                    ("user/ask", {"value": True}),
                ],
                {
                    "action": "isolate checkout-api",
                    "approved": True,
                    "severity": "SEV-1",
                    "suspected_component": "checkout-api",
                },
            ),
            "invoice-reconciliation.allen": (
                [
                    ("model/request", {"value": {"amount_cents": 129900, "currency": "USD", "invoice_id": "INV-8841", "vendor_id": "ACME-CLOUD"}}),
                    ("agent/message", {"accepted": True}),
                ],
                {
                    "discrepancy_cents": 0,
                    "matched": True,
                    "status": "reconciled",
                    "vendor_id": "ACME-CLOUD",
                },
            ),
            "deployment-risk-gate.allen": (
                [
                    ("sub_agent/run", {"value": {"approve": True, "risk_score": 20, "summary": "all gates pass"}}),
                    ("user/ask", {"value": True}),
                ],
                {
                    "decision": "deploy",
                    "policy_passed": True,
                    "reviewer_risk": 20,
                    "user_approved": True,
                },
            ),
            "bulk-customer-operation-planning.allen": (
                [
                    ("agent/ask", {"value": {"approve": False, "customer_id": "C-104", "reason": "not eligible"}}),
                    ("tool/invoke", {"outcome": "ok", "value": {"text": "dry-run CRM credit writes: 2"}}),
                ],
                {
                    "blocked_accounts": 1,
                    "planned_writes": 2,
                    "receipt": "dry-run CRM credit writes: 2",
                    "status": "bounded dry run complete",
                },
            ),
            "infrastructure-drift-remediation.allen": (
                [
                    ("agent/ask", {"value": {"blocked_ids": ["db-encryption"], "note": "only reversible drift is safe", "safe_ids": ["sg-web"]}}),
                    ("user/ask", {"value": True}),
                    ("tool/invoke", {"outcome": "ok", "value": {"text": "dry-run apply drift item: sg-web"}}),
                ],
                {
                    "approved": True,
                    "receipt": "dry-run apply drift item: sg-web",
                    "status": "safe remediation dry run complete",
                },
            ),
        }

        for filename, (responses, expected_output) in cases.items():
            with self.subTest(filename=filename):
                terminal = self._complete_source_through_josh(
                    f"examples/josh-allen/showcases/{filename}",
                    responses,
                )
                self.assertEqual(terminal, {
                    "outcome": "completed",
                    "output": expected_output,
                })

        terminal = self._complete_source_through_josh(
            "examples/josh-allen/showcases/generated/test-failure-reduction.allen",
            [
                (
                    "agent/ask",
                    {
                        "value": {
                            "case_id": "runner-cli",
                            "reason": "protocol timeout points at JOSH transport first",
                        },
                    },
                ),
            ],
        )
        self.assertEqual(terminal, {
            "outcome": "completed",
            "output": {
                "evidence_count": 5,
                "groups": 2,
                "first": {
                    "group": "parse-tree-mismatch",
                    "members": 2,
                    "minimum_steps": 2,
                    "selected_case": {"tag": "None"},
                    "basis": "deterministic grouping; tied group not selected",
                },
                "second": {
                    "group": "protocol-timeout",
                    "members": 2,
                    "minimum_steps": 2,
                    "selected_case": {"tag": "Some", "value": "runner-cli"},
                    "basis": "protocol timeout points at JOSH transport first",
                },
                "limitation": "MVP reduction only: Codex embedded 5 bounded observations; ALLEN did not run tests or access files, shell, or network.",
            },
        })

        terminal = self._complete_source_through_josh(
            "examples/josh-allen/showcases/generated/customer-bulk-action-plan.allen",
            [
                (
                    "agent/ask",
                    {
                        "value": {
                            "account_id": "SYNTH-CRM-0003",
                            "disposition": "review",
                            "rationale": "insufficient fixture evidence",
                        },
                    },
                ),
            ],
        )
        self.assertEqual(terminal, {
            "outcome": "completed",
            "output": {
                "approved_ids": ["SYNTH-CRM-0001", "SYNTH-CRM-0004"],
                "batch_limit": 2,
                "dry_run": "No CRM query or update occurred. This MVP cannot invoke arbitrary tools, access permissions, or reach the network from inside ALLEN.",
                "fixture": "synthetic customer accounts; exactly 6 records",
                "rejected_ids": ["SYNTH-CRM-0002", "SYNTH-CRM-0005", "SYNTH-CRM-0006"],
                "review_required_ids": ["SYNTH-CRM-0003"],
            },
        })

        terminal = self._complete_source_through_josh(
            "examples/josh-allen/showcases/generated/infrastructure-drift-review.allen",
            [
                (
                    "agent/ask",
                    {
                        "value": {
                            "resource_id": "aws_instance.worker",
                            "disposition": "defer",
                            "rationale": "intent is not present in the bounded fixture",
                        },
                    },
                ),
            ],
        )
        output = terminal["output"]
        self.assertEqual(output["ambiguous_decision"], {
            "resource_id": "aws_instance.worker",
            "disposition": "defer",
            "rationale": "intent is not present in the bounded fixture",
        })
        self.assertEqual(len(output["findings"]), 5)
        self.assertEqual(output["dry_run_steps"], [
            {
                "resource_id": "aws_s3_bucket.logs",
                "action": "update",
                "desired": "encryption=AES256",
            },
            {
                "resource_id": "aws_cloudwatch_log_group.app",
                "action": "create",
                "desired": "retention_days=30",
            },
        ])

        terminal = self._complete_source_through_josh(
            "examples/josh-allen/showcases/generated/synthetic-invoice-reconciliation.allen",
            [
                (
                    "agent/ask",
                    {
                        "value": {
                            "invoice_id": "SYN-INV-1003",
                            "vendor_id": "NORTHSTAR-SUPPLIES",
                            "disposition": "reconcile",
                            "rationale": "the bounded description says office supply",
                        },
                    },
                ),
            ],
        )
        self.assertEqual(terminal, {
            "outcome": "completed",
            "output": {
                "fixture": "synthetic invoice-like records; no accounting-system access",
                "reconciled_invoice_ids": ["SYN-INV-1001", "SYN-INV-1002", "SYN-INV-1003"],
                "reconciled_total_cents": 24949,
                "ledger_total_cents": 28849,
                "invoice_total_cents": 28824,
                "exception_invoice_ids": ["SYN-INV-1004"],
                "exception_count": 1,
                "net_discrepancy_cents": -25,
                "judgment_note": "the bounded description says office supply",
            },
        })

if __name__ == "__main__":
    unittest.main()
