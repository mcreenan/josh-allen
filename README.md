# JOSH/ALLEN

JOSH/ALLEN is a small language and host for running agent-written programs
with typed inputs, declared effects, and narrow capabilities.

The name has two parts:

- ALLEN means Agent-Level Language, Embedded Natively. ALLEN is the language,
  compiler, bytecode format, verifier, virtual machine, and standalone CLI.
- JOSH means JSON-Oriented Session Host. JOSH runs ALLEN programs and can
  connect their typed effects to an agent, model, user, tool, or child agent.

The uppercase spelling is intentional. JOSH and ALLEN are acronyms. Together,
JOSH/ALLEN is a tribute to the GOAT, Josh Allen.

The technical split still matters. ALLEN programs can run without an agent.
JOSH can attach an ALLEN program to a live agent session without changing the
language.

## Status

JOSH/ALLEN is experimental and in early alpha. During the `0.1.x` series, the
repository supports one current source language, bytecode format, runtime
contract, replay format, and `josh/1.3` protocol. Files produced by older
builds are unsupported.

The single-current-version policy is temporary. Once the project is mature
enough to make compatibility promises, releases will preserve supported
versions and define migrations instead of treating every older build as
unsupported. Until then, breaking changes update the implementation, specs,
conformance data, examples, editor support, and agent reference together.

The project is ready for experimentation, not production deployment. The
worker process adds resource limits on macOS and Linux, but it is not an
operating-system sandbox.

## Install

### Recommended installation

The prebuilt installer supports macOS on Apple silicon and Linux on x86_64.
You need Python 3 and `curl`, but you do not need Rust. Run one command:

```sh
curl -fsSL https://raw.githubusercontent.com/mcreenan/josh-allen/main/install.sh | bash
```

The installer adds:

- the latest prebuilt `allen` and `josh` binaries in `~/.local/bin`
- the JOSH/ALLEN marketplace and plugin for every installed Codex or Claude
  Code CLI
- the shared `josh-allen` Agent Skill and `josh_allen` MCP server through that
  plugin

The installer verifies the release checksum before copying either binary. You
can read [the installer](install.sh) before running it.

If neither agent CLI is installed, the command still installs both binaries.
Run it again after installing Codex or Claude Code to add the marketplace and
plugin. Restart each agent host after installation.

### Install from source

Use this path on another operating system or CPU, or when you want to build the
binaries yourself. You need Rust 1.85 or newer, Python 3, and Git.

Clone the repository and install both binaries from the checkout:

```sh
git clone https://github.com/mcreenan/josh-allen.git
cd josh-allen
cargo install --locked --path crates/allen-cli --bin allen
cargo install --locked --path crates/josh --bin josh
```

Then add the checkout as a marketplace for the agent you use.

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

See [installation and troubleshooting](docs/install.md) for local development
and nonstandard executable paths.

## Try it

From a repository checkout, check and run a standalone ALLEN program:

```sh
allen check examples/answer.allen
allen run examples/answer.allen
```

Run the same kind of source through the complete JOSH protocol lifecycle:

```sh
josh run examples/josh-answer.allen
```

Pass exact JSON input:

```sh
josh run \
  --input '{"value":41}' \
  task.allen
```

Build, inspect, and run a verified bytecode artifact:

```sh
allen build examples/functions-and-effects/main.allen -o main.allenb
allen inspect main.allenb
allen run main.allenb
```

Use `josh run --help` and `allen --help` for the full command lists.

## What runs where

`allen run` executes a program without the JOSH protocol. Use it for ordinary
language work and local capabilities.

`josh run` runs source, packages, or `.allenb` artifacts through JOSH. It can
grant bounded filesystem access and restricted HTTPS GET access. It can also
run unattended programs whose providers are supplied by another host.

The `josh_allen` MCP server connects a running ALLEN program to the current
Codex or Claude task. It handles typed callbacks, but it does not grant
filesystem or network access. A workflow that needs both uses two stages.
`josh run` gathers and reduces the evidence, then the MCP bridge handles the
typed callback.

## Language and safety model

Every external operation is visible in the program's effect set. A manifest
declares requested capabilities and tools. The host chooses the actual grants.
The runtime validates typed boundary values and the VM only executes verified
bytecode.

ALLEN has deterministic scheduling, task scopes, cancellation, resource
budgets, record and replay, exact JSON boundaries, package locks, filesystem
brokers, restricted HTTPS GET, typed prompts, and terminal stopped outcomes.
The language reference lists the supported syntax and operations without
retaining older contracts.

## Documentation

- [Install JOSH/ALLEN](docs/install.md)
- [Names and component boundaries](docs/naming.md)
- [ALLEN language specification](docs/language-spec.md)
- [JOSH/ALLEN implementation specification](docs/implementation-spec.md)
- [Rust architecture](docs/rust-architecture.md)
- [Agent entry point](docs/agents/README.md)

Agents should start with the agent entry point. It loads the detailed ALLEN or
JOSH reference only when the task needs it.

## Development

The workspace requires Rust 1.85 or newer.

```sh
cargo test --workspace
./scripts/source-conformance.sh
./scripts/host-conformance.sh
./scripts/test-installer.sh
python3 -m unittest plugins/josh-allen/tests/test_server.py
```

The VS Code grammar and packaged extension live under `editors/vscode`.

Tags matching `v0.1.*` run the release workflow. The tag must match the
workspace and plugin versions. Each release publishes macOS arm64 and static
Linux x86_64 archives with SHA-256 checksums.

JOSH/ALLEN is licensed under the [MIT License](LICENSE).
