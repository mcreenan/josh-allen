# Install JOSH/ALLEN

## Recommended installation

The prebuilt installer supports:

- macOS on Apple silicon
- Linux on x86_64

You need Python 3 and `curl`. Rust is not required. Run one command:

```sh
curl -fsSL https://raw.githubusercontent.com/mcreenan/josh-allen/main/install.sh | bash
```

The installer downloads the latest `0.1.x` release, verifies its SHA-256
checksum, and installs `allen` and `josh` in `~/.local/bin`. It then checks for
the Codex and Claude Code CLIs. For each one it finds, it adds the `josh-allen`
marketplace and installs the `josh-allen` plugin. The plugin contains the Agent
Skill and `josh_allen` MCP server. You can read [the installer](../install.sh)
before running it.

If neither agent CLI is installed, the binaries still work. Run the same
command again after installing an agent CLI to add its marketplace and plugin.

Restart Codex or Claude Code after installation. Check the binaries with:

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
curl -fsSL https://raw.githubusercontent.com/mcreenan/josh-allen/main/install.sh | JOSH_ALLEN_INSTALL_DIR=/absolute/bin bash
```

The MCP server looks for `josh` on `PATH`. Set `JOSH_ALLEN_JOSH_BIN` to its
absolute path if it is installed elsewhere.

By default, the server treats the current project as its workspace. Set
`JOSH_ALLEN_WORKSPACE` to an absolute directory when the host launches the
server from another location.

The MCP bridge accepts manifest-first `.allen` files inside that workspace. It
supports typed agent, model, user, tool, and child-agent callbacks. It does not
grant filesystem or network capabilities. Use `josh run` for those effects.
