# `llm-box`

Run Copilot CLI in a container with live network approvals.

`llm-box` keeps Copilot in your terminal and opens a small browser UI where you can approve, dismiss, or revoke outbound destinations while the session keeps running.

## Why use it?

- run Copilot in a container
- see what outbound access it asks for
- approve access without restarting the session
- revoke access later if you want

## Install

### Requirements

- a container runtime you can start from the command line: `docker` or `podman`
- Rust and Cargo
- a local browser
- GitHub Copilot access

`llm-box` uses `docker` if it is available, otherwise it falls back to `podman`.

Today `llm-box` is aimed at macOS and Linux. Native Windows is not supported. WSL is the most likely way to run it on Windows, but it is not tested or documented yet.

### Install globally

```bash
cargo install --git https://github.com/olillevik/llm-box.git
```

Confirm it works:

```bash
llm-box --help
```

## Quick start

From any project directory:

```bash
cd /path/to/your/project
llm-box copilot
```

What happens:

1. Copilot starts in your terminal
2. `llm-box` opens or reuses the browser UI
3. new outbound destinations show up as pending
4. you decide what to allow

You can also resume a prior session:

```bash
llm-box copilot --resume <session-id>
```

Anything after `copilot` is passed through to the real `copilot` command inside the container.

## Basic commands

```bash
llm-box copilot
llm-box ui
llm-box ui --session <session-id>
llm-box pending
llm-box allowed
llm-box allow https://objects-origin.githubusercontent.com:443
llm-box deny https://objects-origin.githubusercontent.com:443
llm-box dismiss https://objects-origin.githubusercontent.com:443
```

## Approvals

When Copilot tries to reach a new destination, `llm-box` shows it as pending.

From there you can:

- **approve** it
- **dismiss** it for now
- **revoke** it later

The session keeps running while you make these decisions.

## Defaults

Each new session starts with a small built-in allowlist for GitHub and Copilot endpoints.

You can also manage your own defaults for future sessions:

```bash
llm-box defaults list
llm-box defaults add github.com
llm-box defaults remove github.com
```

## Optional: customize one repo

If a project needs extra tools inside the container:

```bash
llm-box init-image
```

That creates:

```bash
.llm-box/Dockerfile
```

If you want to prebuild the image for the current workspace:

```bash
llm-box build
```

## Troubleshooting

### `llm-box` command not found

Make sure Cargo's bin directory is on your `PATH`:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
```

### Your container runtime is not running

Make sure the runtime you want to use is available and running.

- For Docker, start Docker Desktop or your Docker daemon.
- For Podman on macOS:

```bash
podman machine start
```

### The browser UI did not open

Open it manually:

```bash
llm-box ui
```

## For contributors

If you want to work on `llm-box` itself:

```bash
git clone https://github.com/olillevik/llm-box.git
cd llm-box
cargo test
bash ./tests/test_box.sh
```

Useful local commands:

```bash
cargo build
./llm-box build
./llm-box copilot
cargo run -- copilot
```
