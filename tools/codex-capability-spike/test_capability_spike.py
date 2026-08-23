from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


MODULE_PATH = Path(__file__).with_name("capability_spike.py")
SPEC = importlib.util.spec_from_file_location("capability_spike", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
SPIKE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SPIKE)


class CapabilitySpikeTests(unittest.TestCase):
    def test_schema_digest_ignores_json_object_key_order(self) -> None:
        with (
            tempfile.TemporaryDirectory() as left_root,
            tempfile.TemporaryDirectory() as right_root,
        ):
            Path(left_root, "schema.json").write_text(
                json.dumps({"alpha": 1, "beta": {"x": 2, "y": 3}}),
                encoding="utf-8",
            )
            Path(right_root, "schema.json").write_text(
                json.dumps({"beta": {"y": 3, "x": 2}, "alpha": 1}),
                encoding="utf-8",
            )

            self.assertEqual(
                SPIKE.schema_bundle_digest(Path(left_root)),
                SPIKE.schema_bundle_digest(Path(right_root)),
            )

    def test_feature_parser_preserves_maturity_and_enabled_state(self) -> None:
        parsed = SPIKE.parse_features(
            "hooks                                stable             true\n"
            "multi_agent                          stable             false\n"
        )

        self.assertEqual(parsed["hooks"], {"maturity": "stable", "enabled": True})
        self.assertEqual(
            parsed["multi_agent"], {"maturity": "stable", "enabled": False}
        )

    def test_schema_inventory_ignores_arbitrary_string_values(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            Path(root, "schema.json").write_text(
                json.dumps(
                    {
                        "description": "ResolvedToolCatalog",
                        "definitions": {"ActuallyDeclared": {"type": "object"}},
                    }
                ),
                encoding="utf-8",
            )

            declared = SPIKE.load_declared_schema_types(Path(root))

        self.assertNotIn("ResolvedToolCatalog", declared)
        self.assertIn("ActuallyDeclared", declared)

    def test_request_methods_come_from_method_discriminators(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            Path(root, "ClientRequest.json").write_text(
                json.dumps(
                    {
                        "description": "fake/method",
                        "oneOf": [
                            {
                                "properties": {
                                    "method": {"type": "string", "enum": ["model/list"]}
                                }
                            }
                        ],
                    }
                ),
                encoding="utf-8",
            )

            methods = SPIKE.load_request_methods(Path(root))

        self.assertEqual(methods, ["model/list"])

    def test_missing_host_owned_declarations_blocks_the_preflight(self) -> None:
        gate, blockers = SPIKE.evaluate_gate(
            version=SPIKE.PINNED_CLI_VERSION,
            binary_sha256=SPIKE.PINNED_RELEASE_BINARY_SHA256,
            feature_inventory={
                name: {"maturity": "stable", "enabled": True}
                for name in SPIKE.REQUIRED_FEATURES
            },
            declared_schema_types={"McpElicitationBooleanSchema"},
            request_methods=["model/list"],
        )

        self.assertTrue(gate["artifact_checks"]["mcp_boolean_elicitation_declared"])
        self.assertIn("schema_declaration:ResolvedToolCatalog", blockers)
        self.assertIn("schema_declaration:ToolCallReceipt", blockers)
        self.assertIn("live:chunk_minus_1_adversarial_matrix", blockers)

    def test_schema_declarations_cannot_pass_the_live_gate(self) -> None:
        declared_schema_types = {
            "McpElicitationBooleanSchema",
            *SPIKE.REQUIRED_HARNESS_SCHEMA_DECLARATIONS,
        }
        gate, blockers = SPIKE.evaluate_gate(
            version=SPIKE.PINNED_CLI_VERSION,
            binary_sha256=SPIKE.PINNED_RELEASE_BINARY_SHA256,
            feature_inventory={
                name: {"maturity": "stable", "enabled": True}
                for name in SPIKE.REQUIRED_FEATURES
            },
            declared_schema_types=declared_schema_types,
            request_methods=["model/list"],
        )

        self.assertEqual(blockers, ["live:chunk_minus_1_adversarial_matrix"])
        self.assertTrue(gate["static_preflight_passed"])


if __name__ == "__main__":
    unittest.main()
