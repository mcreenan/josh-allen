#!/usr/bin/env python3
"""Probe the pinned Codex host boundary required by the JOSH integration.

The probe deliberately consumes only host-owned CLI and app-server output. It
never asks a model to describe its tools and never reads a conversation
transcript. A blocked result is a successful probe outcome but uses exit status
3 so the integration gate cannot be mistaken for passing.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
from typing import Any, Iterable


PINNED_CLI_VERSION = "0.146.0"
PINNED_RELEASE_BINARY_SHA256 = (
    "6510f999e6f8d1a8d9b3759d90ed8f72d13bc0fa79433b3ddbb4220b1ac5b657"
)
PINNED_SOURCE_COMMIT = "e363b08c9175ac1cbe5893615dd2cb9ddf95043b"
HARNESS_WIRE_VERSION = "allen.codex-harness/0.1"
JOSH_MAX_TOOLS = 256
JOSH_MAX_DECODED_SCHEMA_BYTES = 3 * 1024 * 1024

REQUIRED_HARNESS_SCHEMA_DECLARATIONS = (
    "ResolvedToolCatalog",
    "ResolvedAgentAuthority",
    "CallerAttestation",
    "ToolCallReceipt",
    "SubAgentCreationReceipt",
    "SubAgentDeliveryReceipt",
    "SubAgentResponseReceipt",
)

REQUIRED_FEATURES = (
    "auth_elicitation",
    "hooks",
    "multi_agent",
    "tool_call_mcp_elicitation",
)


class ProbeError(RuntimeError):
    """The installed binary could not be inspected safely."""


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def schema_bundle_digest(root: Path) -> str:
    digest = hashlib.sha256()
    for path in sorted(candidate for candidate in root.rglob("*.json") if candidate.is_file()):
        relative = path.relative_to(root).as_posix().encode("utf-8")
        digest.update(len(relative).to_bytes(8, "big"))
        digest.update(relative)
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise ProbeError(f"invalid generated schema file {path.name}: {error}") from error
        # The aggregate schema contains maps whose emitted key order can vary
        # between processes. Hash canonical JSON rather than generator bytes.
        data = json.dumps(
            payload, ensure_ascii=False, separators=(",", ":"), sort_keys=True
        ).encode("utf-8")
        digest.update(len(data).to_bytes(8, "big"))
        digest.update(data)
    return digest.hexdigest()


def run_command(command: list[str], *, timeout: int = 60) -> str:
    command_label = " ".join([Path(command[0]).name, *command[1:3]])
    try:
        result = subprocess.run(
            command,
            check=False,
            capture_output=True,
            text=True,
            timeout=timeout,
            env=os.environ.copy(),
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ProbeError(f"could not execute {command_label}: {error}") from error
    if result.returncode != 0:
        detail = result.stderr.strip().splitlines()
        summary = detail[-1] if detail else f"exit status {result.returncode}"
        raise ProbeError(f"{command_label} failed: {summary}")
    return result.stdout


def parse_version(output: str) -> str:
    match = re.fullmatch(r"codex-cli\s+([0-9]+\.[0-9]+\.[0-9]+)\s*", output)
    if not match:
        raise ProbeError("unexpected `codex --version` output")
    return match.group(1)


def parse_features(output: str) -> dict[str, dict[str, Any]]:
    features: dict[str, dict[str, Any]] = {}
    for line in output.splitlines():
        match = re.fullmatch(r"(\S+)\s{2,}(.+?)\s{2,}(true|false)", line.strip())
        if match:
            name, maturity, enabled = match.groups()
            features[name] = {"maturity": maturity, "enabled": enabled == "true"}
    if not features:
        raise ProbeError("`codex features list` returned no parseable features")
    return features


def walk_objects(value: Any) -> Iterable[dict[str, Any]]:
    if isinstance(value, dict):
        yield value
        for item in value.values():
            yield from walk_objects(item)
    elif isinstance(value, list):
        for item in value:
            yield from walk_objects(item)


def load_declared_schema_types(schema_root: Path) -> set[str]:
    """Return actual schema titles and definition keys, not arbitrary strings.

    A declaration is still only a smoke-test observation. It does not prove a
    callable endpoint, correct field bindings, or any live security property.
    """

    declared_types: set[str] = set()
    for path in sorted(schema_root.rglob("*.json")):
        if path.is_file():
            try:
                payload = json.loads(path.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError) as error:
                raise ProbeError(f"invalid generated schema file {path.name}: {error}") from error
            for item in walk_objects(payload):
                title = item.get("title")
                if isinstance(title, str):
                    declared_types.add(title)
                definitions = item.get("definitions")
                if isinstance(definitions, dict):
                    declared_types.update(str(name) for name in definitions)
    return declared_types


def load_request_methods(schema_root: Path) -> list[str]:
    client_request = schema_root / "ClientRequest.json"
    try:
        payload = json.loads(client_request.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ProbeError(f"could not inspect ClientRequest.json: {error}") from error

    methods: set[str] = set()
    for item in walk_objects(payload):
        properties = item.get("properties")
        if not isinstance(properties, dict):
            continue
        method = properties.get("method")
        if not isinstance(method, dict):
            continue
        enum = method.get("enum")
        if isinstance(enum, list):
            methods.update(value for value in enum if isinstance(value, str))
    return sorted(methods)


def load_bundled_models(output: str) -> list[dict[str, Any]]:
    try:
        payload = json.loads(output)
        models = payload["models"]
    except (json.JSONDecodeError, KeyError, TypeError) as error:
        raise ProbeError("`codex debug models --bundled` returned an unexpected shape") from error
    if not isinstance(models, list):
        raise ProbeError("bundled model catalog is not a list")
    sanitized = []
    for model in models:
        if not isinstance(model, dict) or not isinstance(model.get("slug"), str):
            raise ProbeError("bundled model catalog contains an invalid entry")
        effort_entries = model.get("supported_reasoning_levels", [])
        efforts = (
            sorted(
                {
                    entry["effort"]
                    for entry in effort_entries
                    if isinstance(entry, dict) and isinstance(entry.get("effort"), str)
                }
            )
            if isinstance(effort_entries, list)
            else []
        )
        sanitized.append(
            {
                "slug": model["slug"],
                "supported_reasoning_levels": efforts,
            }
        )
    return sanitized


def evaluate_gate(
    *,
    version: str,
    binary_sha256: str,
    feature_inventory: dict[str, dict[str, Any]],
    declared_schema_types: set[str],
    request_methods: list[str],
) -> tuple[dict[str, Any], list[str]]:
    declared_harness_records = {
        name: name in declared_schema_types for name in REQUIRED_HARNESS_SCHEMA_DECLARATIONS
    }
    required_features = {
        name: bool(feature_inventory.get(name, {}).get("enabled")) for name in REQUIRED_FEATURES
    }
    artifact_checks = {
        "pinned_cli_version": version == PINNED_CLI_VERSION,
        "pinned_release_binary": binary_sha256 == PINNED_RELEASE_BINARY_SHA256,
        "required_features_enabled": all(required_features.values()),
        "mcp_boolean_elicitation_declared": (
            "McpElicitationBooleanSchema" in declared_schema_types
        ),
        "model_list_request_declared": "model/list" in request_methods,
    }
    static_preflight_passed = all(artifact_checks.values()) and all(
        declared_harness_records.values()
    )
    blockers = [
        f"artifact:{name}" for name, available in artifact_checks.items() if not available
    ]
    blockers.extend(
        f"schema_declaration:{name}"
        for name, declared in declared_harness_records.items()
        if not declared
    )
    # Schema declarations can never establish the behavioral and adversarial
    # properties required by Chunk -1. A patched host must replace this marker
    # with live evidence; this stock-host preflight deliberately cannot do so.
    blockers.append("live:chunk_minus_1_adversarial_matrix")
    return (
        {
            "phase": "static_public_surface_preflight",
            "artifact_checks": artifact_checks,
            "required_features": required_features,
            "declared_harness_records": declared_harness_records,
            "static_preflight_passed": static_preflight_passed,
            "live_requirements": {
                "active_host_context": "not_run",
                "exact_post_gating_catalog": "not_run",
                "root_attachment_and_rejection_matrix": "not_run",
                "real_tool_receipt_and_replay_rejection": "not_run",
                "boolean_elicitation_behavior_matrix": "not_run",
                "isolated_child_authority": "not_run",
                "sub_agent_receipt_state_machine": "not_run",
                "synthetic_multi_step_provider_fixture": "not_run",
            },
            "scope_limitation": (
                "schema declarations are heuristic smoke-test observations, not "
                "evidence that a capability is callable, complete, bound, or secure"
            ),
        },
        blockers,
    )


def probe(codex_binary: Path) -> dict[str, Any]:
    # Some managed installations use one argv-sensitive launcher for several
    # commands. Preserve the requested alias for execution while hashing the
    # resolved executable bytes.
    binary = codex_binary.absolute()
    digest_target = codex_binary.resolve(strict=True)
    binary_sha256 = sha256_file(digest_target)
    version = parse_version(run_command([str(binary), "--version"]))
    features = parse_features(run_command([str(binary), "features", "list"]))
    models = load_bundled_models(
        run_command([str(binary), "debug", "models", "--bundled"])
    )

    with tempfile.TemporaryDirectory(prefix="allen-codex-capability-schema-") as temporary:
        schema_root = Path(temporary)
        run_command(
            [
                str(binary),
                "app-server",
                "generate-json-schema",
                "--experimental",
                "--out",
                str(schema_root),
            ]
        )
        declared_schema_types = load_declared_schema_types(schema_root)
        request_methods = load_request_methods(schema_root)
        bundle_digest = schema_bundle_digest(schema_root)

    gate, blockers = evaluate_gate(
        version=version,
        binary_sha256=binary_sha256,
        feature_inventory=features,
        declared_schema_types=declared_schema_types,
        request_methods=request_methods,
    )
    gate_passed = not blockers
    return {
        "profile": HARNESS_WIRE_VERSION,
        "status": "passed" if gate_passed else "blocked",
        "pinned_codex": {
            "cli_version": version,
            "binary_sha256": binary_sha256,
            "source_commit": PINNED_SOURCE_COMMIT,
            "generated_app_server_schema_sha256": bundle_digest,
        },
        "host_context": {
            "active_model": {"status": "unavailable_from_public_host_api"},
            "reasoning_effort": {"status": "unavailable_from_public_host_api"},
            "collaboration_mode": {"status": "unavailable_from_public_host_api"},
            "permission_profile": {"status": "unavailable_from_public_host_api"},
            "project_trust": {"status": "unavailable_from_public_host_api"},
        },
        "catalog": {
            "status": "unavailable_from_public_host_api",
            "tool_count": None,
            "decoded_schema_bytes": None,
            "canonical_digest": None,
            "generation": None,
            "dispatch_classes": None,
            "limits": {
                "max_tools": JOSH_MAX_TOOLS,
                "max_decoded_schema_bytes": JOSH_MAX_DECODED_SCHEMA_BYTES,
            },
        },
        "bundled_models": models,
        "public_request_methods": request_methods,
        "gate": gate,
        "blockers": blockers,
        "next_step": (
            "revise_the_codex_harness_architecture_then_implement_and_rerun_chunk_minus_1"
            if blockers
            else "run_live_chunk_minus_1_adversarial_scenarios"
        ),
    }


def write_json(payload: dict[str, Any], output: Path | None) -> None:
    rendered = json.dumps(payload, indent=2, sort_keys=True) + "\n"
    if output is None:
        sys.stdout.write(rendered)
        return
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = output.with_suffix(output.suffix + ".tmp")
    temporary.write_text(rendered, encoding="utf-8")
    temporary.replace(output)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--codex",
        default=os.environ.get("CODEX_CAPABILITY_CODEX_BIN") or shutil.which("codex"),
        help="path to the pinned Codex binary",
    )
    parser.add_argument("--output", type=Path, help="write the sanitized JSON trace here")
    return parser.parse_args()


def main() -> int:
    arguments = parse_args()
    if not arguments.codex:
        print("codex binary was not found", file=sys.stderr)
        return 2
    try:
        result = probe(Path(arguments.codex))
        write_json(result, arguments.output)
    except ProbeError as error:
        print(f"codex capability probe failed: {error}", file=sys.stderr)
        return 2
    return 0 if result["status"] == "passed" else 3


if __name__ == "__main__":
    raise SystemExit(main())
