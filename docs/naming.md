# JOSH/ALLEN names

JOSH/ALLEN is the project. ALLEN is the language and runtime. JOSH is the host.
Both names are acronyms, and the combined name is a tribute to Josh Allen.

## ALLEN

ALLEN means Agent-Level Language, Embedded Natively. It includes the source
language, compiler, bytecode, verifier, virtual machine, standard operations,
local capability brokers, and standalone CLI.

ALLEN is agent-native, but it does not require an agent. The same language can
run standalone, under an unattended host, or inside a live agent session.

Public ALLEN names are:

- `allen`, the language CLI
- `.allen`, source files
- `.allenb`, bytecode artifacts
- `allen.toml`, package manifests
- `allen.lock`, package lockfiles
- `allen-*`, core Rust crates

## JOSH

JOSH means JSON-Oriented Session Host. It links an ALLEN execution to an agent
session through the `josh/1` protocol. It binds the invoking session, freezes
the tool catalog, projects approved transcript data, routes typed effects, and
returns the terminal outcome.

JOSH does not change ALLEN semantics or bypass bytecode verification, type
checks, effect checks, or capability checks. Another host may implement the
same provider contracts or the `josh/1` protocol.

Public JOSH names are:

- `josh`, the session-host executable
- `josh/1`, the protocol family
- `josh-*`, host and protocol Rust crates

Peers currently negotiate `josh/1.6`. They reject other protocol identifiers.
The `/1` suffix is not part of the executable name.

## Dependency rule

JOSH may depend on ALLEN. The ALLEN language core must not depend on JOSH. A
shared protocol crate may expose wire types without adding live-agent behavior
to the compiler or virtual machine.
