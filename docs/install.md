# Install or update JOSH/ALLEN

## Recommended install and update

The prebuilt installer supports:

- macOS on Apple silicon
- Linux on x86_64

You need Python 3 and `curl`. Rust is not required. Run one command:

```sh
curl -fsSL https://raw.githubusercontent.com/mcreenan/josh-allen/main/hike.sh | bash
```

The installer downloads the latest `0.1.x` release, verifies its SHA-256
checksum, and installs `allen` and `josh` in `~/.local/bin`. It then checks for
the Codex and Claude Code CLIs. For each one it finds, it installs or updates
the `josh-allen` marketplace and plugin. The plugin contains the Agent Skill,
its packaged ALLEN agent language reference, and the `josh_allen` MCP server.
You can read [the installer](../hike.sh) before running it.

Run the same command again whenever you want to update everything. It replaces
both binaries and refreshes the plugin for every installed agent CLI.

If neither agent CLI is installed, the binaries still work. Run the command
again after installing an agent CLI to add its marketplace and plugin.

Restart Codex or Claude Code after installation or update. Check the binaries
with:

```sh
allen --help
josh --help
```

## Install from source

Use this path on an unsupported operating system or CPU, or when you want to
build the binaries yourself. You need Rust 1.85 or newer, Python 3, and Git.

Clone the repository and install both binaries from the checkout:

```sh
git clone https://github.com/mcreenan/josh-allen.git
cd josh-allen
cargo install --locked --path crates/allen-cli --bin allen
cargo install --locked --path crates/josh --bin josh
```

Add the local marketplace for each agent you use.

For Codex:

```sh
codex plugin marketplace add "$PWD"
codex plugin add josh-allen@josh-allen
```

For Claude Code:

```sh
claude plugin marketplace add "$PWD"
claude plugin install josh-allen@josh-allen
```

Restart each agent host after installation.

Codex discovers the shared skill through `.agents/skills/josh-allen` and reads
the MCP settings from `.codex/config.toml`. Claude Code discovers the same
skill through `.claude/skills/josh-allen`.

## Nonstandard paths

The prebuilt installer writes to `~/.local/bin`. To choose another directory,
set `JOSH_ALLEN_INSTALL_DIR` for the `bash` process:

```sh
curl -fsSL https://raw.githubusercontent.com/mcreenan/josh-allen/main/hike.sh | JOSH_ALLEN_INSTALL_DIR=/absolute/bin bash
```

The MCP server looks for `josh` on `PATH`. Set `JOSH_ALLEN_JOSH_BIN` to its
absolute path if it is installed elsewhere.

By default, the server treats the current project as its workspace. Set
`JOSH_ALLEN_WORKSPACE` to an absolute directory when the host launches the
server from another location.

## Inject a host projection into the MCP bridge

By default, the packaged bridge projects only its built-in
`allen_integration_echo` tool and reports empty inventories for resources,
attachments, transcript, models, user interaction, agents, roots, permissions,
and telemetry. A native host adapter can set
`JOSH_ALLEN_HOST_PROJECTION_PATH` to a host-owned JSON file that replaces that
default projection and catalog:

```sh
JOSH_ALLEN_HOST_PROJECTION_PATH=/absolute/path/host-projection.json
```

The file is read once when the bridge object is constructed, before any JOSH
process is spawned. All sessions created by that bridge use this immutable
snapshot; changing the file later has no effect until the agent host restarts
or reconstructs the bridge. The file must be one UTF-8 JSON value no larger
than 1,048,576 bytes, with no duplicate object keys and exactly these three
top-level keys:

- `projection`: the exact `josh.host-projection/0.1` parameters sent through
  `host/project`;
- `catalog`: the complete canonical parameters sent through `catalog/set`; and
- `granted_tools`: at most 256 sorted unique nonempty tool names authorized by
  host policy for executions through this bridge.

The projection must use `session_binding: "prompt_assisted"`, because the MCP
bridge can return callbacks through the current prompt but does not
authenticate the invoking actor or session. The projection host becomes the
`initialize.host` identity. The catalog metadata and tool count must exactly
match the projection's `tools` section. JOSH performs the detailed projection,
catalog, and artifact validation.

After `program/load`, the bridge reads the exact `required_tools` names derived
from the verified artifact. It rejects any requirement absent from
`granted_tools` and otherwise sends exactly the required list in
`execution/start`; mentioning a tool name in source text never grants it.
Discovery and authorization remain separate, and this injection does not add
native provider dispatch: supported callbacks still return `next_action` to
the current prompt.

This minimal bundle projects an empty tool catalog:

```json
{
  "projection": {
    "profile": "josh.host-projection/0.1",
    "projection_id": "host-snapshot-1",
    "host": {"name": "example-host", "version": "1.0.0"},
    "session_binding": "prompt_assisted",
    "sections": [
      {"kind": "tools", "source": "example-host", "source_revision": "snapshot-1", "observed_at_unix_ms": 1770000000000, "freshness": "current", "complete": true, "item_count": 0},
      {"kind": "resources", "source": "example-host", "source_revision": "snapshot-1", "observed_at_unix_ms": 1770000000000, "freshness": "current", "complete": true, "item_count": 0},
      {"kind": "attachments", "source": "example-host", "source_revision": "snapshot-1", "observed_at_unix_ms": 1770000000000, "freshness": "current", "complete": true, "item_count": 0},
      {"kind": "transcript", "source": "example-host", "source_revision": "snapshot-1", "observed_at_unix_ms": 1770000000000, "freshness": "current", "complete": true, "item_count": 0},
      {"kind": "models", "source": "example-host", "source_revision": "snapshot-1", "observed_at_unix_ms": 1770000000000, "freshness": "current", "complete": true, "item_count": 0},
      {"kind": "user_interaction", "source": "example-host", "source_revision": "snapshot-1", "observed_at_unix_ms": 1770000000000, "freshness": "current", "complete": true, "item_count": 0},
      {"kind": "agents", "source": "example-host", "source_revision": "snapshot-1", "observed_at_unix_ms": 1770000000000, "freshness": "current", "complete": true, "item_count": 0},
      {"kind": "roots", "source": "example-host", "source_revision": "snapshot-1", "observed_at_unix_ms": 1770000000000, "freshness": "current", "complete": true, "item_count": 0},
      {"kind": "permissions", "source": "example-host", "source_revision": "snapshot-1", "observed_at_unix_ms": 1770000000000, "freshness": "current", "complete": true, "item_count": 0},
      {"kind": "telemetry", "source": "example-host", "source_revision": "snapshot-1", "observed_at_unix_ms": 1770000000000, "freshness": "current", "complete": true, "item_count": 0}
    ]
  },
  "catalog": {
    "schema_dialect": "https://json-schema.org/draft/2020-12/schema",
    "metadata": {
      "source": "example-host",
      "source_revision": "snapshot-1",
      "observed_at_unix_ms": 1770000000000,
      "freshness": "current",
      "complete": true
    },
    "tools": []
  },
  "granted_tools": []
}
```

Treat this file as authority-bearing host configuration: create it from a
trusted adapter, restrict who can modify it, and avoid secrets in projection
metadata. The current bridge follows symlinks and does not verify file owner or
permission bits; the launching host is responsible for path, ownership, mode,
and replacement safety.

The MCP bridge accepts manifest-first `.allen` files inside that workspace. It
supports typed agent, model, user, tool, and child-agent callbacks. It does not
grant filesystem or network capabilities. Use `josh run` for those effects.
