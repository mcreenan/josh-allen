<p align="center">
  <img src="assets/josh-allen-logo-ai-visor-master.png" alt="JOSH/ALLEN logo" width="256" height="256">
</p>

<h1 align="center">JOSH/ALLEN</h1>

JOSH/ALLEN is a language and host for agent-written programs. Programs have
typed inputs, declare every external effect, and receive only the capabilities
their host grants.

## What is this and Why should I use it?

Agent-driven work usually moves between two spaces. In the **agent space**, an
agent interprets a goal, makes judgment calls, and decides what to do next. In
the **deterministic space**, a program follows explicit rules and produces the
same result from the same input.

Agents can already write Bash, Python, or TypeScript and use those languages to
do the heavy lifting. But the overall workflow often stays in the agent space:
the agent runs a command, reads the result, decides on another command, and
repeats. Each trip through that loop takes time and gives the agent another
chance to misunderstand state or make a different choice.

JOSH/ALLEN binds the agent space and deterministic space closer together while
leaning more heavily on the deterministic space. Instead of making every step
an agent decision, the agent writes a small, typed ALLEN program up front. That
program can handle the repeatable work, enforce limits, and control when the
workflow may call back into the agent space for judgment, approval, or other
capabilities.

This can make the intended work finish sooner. The agent spends its time
defining the workflow once, then the runtime carries out more of it without a
new model round trip for every step. You still keep agentic capabilities where
they help, but filtering, grouping, checking, scheduling, and other rule-based
work stay in the deterministic space.

JOSH/ALLEN is most useful for bounded, multi-step work with clear rules and a
few places that need judgment. A short shell command or ordinary script is
still the simpler choice for a direct task.

## Status

JOSH/ALLEN is early alpha. During the `0.1.x` series, the repository supports
one source language, bytecode format, runtime contract, replay format, and
`josh/1.4` protocol. The current runtime rejects artifacts from older builds.

A breaking change replaces the previous language and artifacts. The same
change must update the implementation, specifications, tests, conformance
data, examples, editor support, and agent reference.

Do not use JOSH/ALLEN as a production security boundary. The worker applies
resource limits on macOS and Linux, but it is not an operating-system sandbox.

## Install

### Recommended installation

The prebuilt installer supports macOS on Apple silicon and Linux on x86_64.
It requires Python 3 and `curl`, but not Rust.

Install it with:

```sh
curl -fsSL https://raw.githubusercontent.com/mcreenan/josh-allen/main/install.sh | bash
```

The installer adds:

- the latest prebuilt `allen` and `josh` binaries in `~/.local/bin`
- the JOSH/ALLEN marketplace and plugin for each installed Codex or Claude
  Code CLI
- the `josh-allen` Agent Skill, ALLEN language reference, and `josh_allen` MCP
  server inside that plugin

The installer verifies the release checksum before copying either binary. You
can read [the installer](install.sh) before running it.

If neither agent CLI is installed, the command still installs both binaries.
Run it again after installing Codex or Claude Code to add the marketplace and
plugin. Restart each agent host after installation.

### Install from source

Build from source for another operating system or CPU, or when you want local
builds. You need Rust 1.85 or newer, Python 3, and Git.

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

The [installation guide](docs/install.md) covers local development and
nonstandard executable paths.

## Try it

From a repository checkout, check and run a standalone ALLEN program:

```sh
allen check examples/answer.allen
allen run examples/answer.allen
```

Run a source file through JOSH:

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

`allen run` executes a standalone program without the JOSH protocol. It can
grant local capabilities.

`josh run` runs source files, packages, or `.allenb` artifacts through JOSH. It
can grant bounded filesystem access and restricted HTTPS GET access. Another
host can also supply providers for an unattended run.

The `josh_allen` MCP server connects a running ALLEN program to the current
Codex or Claude task. It handles typed callbacks, but it does not grant
filesystem or network access. If a workflow needs both, run it in two stages.
Use `josh run` to gather and reduce evidence, then use the MCP bridge for the
typed callback.

## Language and safety model

Every external operation appears in the program's effect set. A manifest
requests capabilities and tools. The host decides which requests to grant. The
runtime validates typed boundary values, and the VM executes only verified
bytecode.

ALLEN provides deterministic scheduling, task scopes, cancellation, and
resource budgets. It also supports record and replay, exact JSON boundaries,
package locks, filesystem brokers, restricted HTTPS GET, typed prompts, and
terminal stopped outcomes. The language reference documents only the current
syntax and runtime contract.

## Name and component reference

The name has two parts:

- ALLEN means Agent-Level Language, Embedded Natively. It includes the
  language, compiler, bytecode format, verifier, virtual machine, and
  standalone CLI.
- JOSH means JSON-Oriented Session Host. It runs ALLEN programs and routes
  typed effects to an agent, model, user, tool, or child agent.

The uppercase spelling is intentional. JOSH and ALLEN are acronyms. Together,
JOSH/ALLEN is a tribute to the GOAT, Josh Allen.

ALLEN programs do not need an agent. JOSH connects an ALLEN program to a live
agent session without changing the language.

## Documentation

- [Install JOSH/ALLEN](docs/install.md)
- [Names and component boundaries](docs/naming.md)
- [ALLEN language specification](docs/language-spec.md)
- [JOSH/ALLEN implementation specification](docs/implementation-spec.md)
- [Rust architecture](docs/rust-architecture.md)
- [Agent entry point](docs/agents/README.md)

Agents should start with the agent entry point. It links to the detailed ALLEN
and JOSH references. Load only the reference needed for the current task.

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

The project uses the [MIT License](LICENSE).
