# `llm-box`

Run Copilot CLI in a container with live network approvals.

`llm-box` keeps Copilot in your terminal, opens a small browser UI for approvals, and lets you approve or revoke outbound destinations while the session is running.

## Why use it?

Use `llm-box` when you want:

- Copilot CLI isolated in a container
- outbound access visible and reviewable
- approvals without restarting the running session
- live revocation of approved web and connector destinations

## Install

### Requirements

- `docker` or `podman`
- Rust and Cargo
- a local browser
- GitHub Copilot access

`llm-box` auto-detects `docker` first and falls back to `podman`.

### Install globally

```bash
cargo install --git https://github.com/olillevik/llm-box.git
```

Confirm it is available everywhere:

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

1. `llm-box` starts Copilot in your terminal
2. it starts the local approval components it needs
3. it opens or reuses the browser UI
4. blocked destinations appear as pending so you can decide what to allow

You can also resume a prior Copilot session:

```bash
llm-box copilot --resume <session-id>
```

Anything after `copilot` is passed through to the real `copilot` command inside the container.

## Everyday usage

Start Copilot:

```bash
llm-box copilot
```

Open the UI:

```bash
llm-box ui
llm-box ui --session <session-id>
```

See blocked outbound destinations for the latest session in the current workspace:

```bash
llm-box pending
```

See the current approved target set for the latest session:

```bash
llm-box allowed
```

Approve a destination for the latest session:

```bash
llm-box allow https://objects-origin.githubusercontent.com:443
```

Revoke an approved destination:

```bash
llm-box deny https://objects-origin.githubusercontent.com:443
```

Dismiss a blocked destination until it appears again:

```bash
llm-box dismiss https://objects-origin.githubusercontent.com:443
```

## What the UI shows

The browser UI shows:

- active sessions only
- stacked **Pending** and **Unread** labels in the session list
- **Pending** blocked destinations for the selected session
- **Allowed** destinations for the selected session
- connector endpoints for approved TCP-style destinations
- a **Dismiss** action for blocked destinations you do not want to keep seeing

## Common approval flows

### Approve web access

If Copilot tries to reach a web destination that is not currently approved:

1. the request is blocked
2. the destination appears in `llm-box pending` and in the browser UI
3. you approve it with `llm-box allow https://objects-origin.githubusercontent.com:443` or from the UI
4. the running session can retry without being restarted

### Approve a TCP, SSH, or MCP destination

For `tcp://`, `ssh://`, and `mcp://` destinations, `llm-box` uses broker-managed connector endpoints instead of direct outbound networking.

Resolve or create a connector endpoint:

```bash
llm-box endpoint tcp://db.internal.example:5432
llm-box endpoint ssh://github.com:22
llm-box endpoint mcp://mcp.internal.example:8080
```

If the destination is approved, `llm-box` returns a local broker endpoint. If it is not approved yet, the attempted access will appear as pending so you can approve it.

### Revoke access

`deny` removes the destination from the active approved set.

For approved web traffic, new requests are blocked again. For approved connector-style destinations, `deny` also tears down the active broker connector listener for that destination, so the old endpoint stops being usable and new traffic must be re-approved.

## Optional: customize the runtime for one repo

If a project needs extra tools inside the container, create a repo-local overlay image:

```bash
llm-box init-image
```

That creates:

```bash
.llm-box/Dockerfile
```

Use this contract in the repo Dockerfile:

```dockerfile
ARG LLM_BOX_BASE_IMAGE
FROM ${LLM_BOX_BASE_IMAGE}
```

Then add the tools or language runtimes that project needs.

You can also prebuild the image for the current workspace:

```bash
llm-box build
```

If `.llm-box/Dockerfile` is present, this builds both the managed base image and the repo-specific derived image.

## Default allowed destinations

Each new session starts with this intentionally narrow built-in default target set:

- `https://api.github.com:443`
- `https://api.business.githubcopilot.com:443`

You can also add your own defaults for future sessions:

```bash
llm-box defaults list
llm-box defaults add github.com
llm-box defaults remove github.com
```

User-managed defaults are stored in:

```bash
~/.llm-box/default-allowed-targets.txt
```

They are merged into the built-in defaults when a new session is created. Existing sessions keep their current approved target set.

## How `llm-box` works

Short version:

- Copilot runs in a container
- `llm-box` starts a per-session broker sidecar
- current HTTP and HTTPS traffic goes through that broker
- approved `tcp://`, `ssh://`, and `mcp://` destinations are exposed through broker-managed connector endpoints
- blocked destinations are recorded in host-side session state and shown in the local browser UI

The current image installs `@github/copilot` and uses `copilot` as the container entrypoint.

## Destination model

Destinations are canonicalized as `scheme://host:port`.

- bare hosts default to `https://host:443`
- `http://` and `https://` use the web proxy path
- `tcp://`, `ssh://`, and `mcp://` use broker-managed connector endpoints

## Caveats

- approvals are destination-based as `protocol + host + port`
- direct outbound traffic from the agent container is denied by the network topology; non-web workflows must use broker-managed connector endpoints
- SSH destinations are scoped and brokered, but host-key policy is not yet enforced by `llm-box`
- MCP support here is transport-level: stdio needs no networking, HTTP uses the web proxy path, and TCP/WebSocket-style transports use connector endpoints
- this is still container-based isolation and brokered egress control, not a host firewall or a hardened VM boundary

## Troubleshooting

### `llm-box` command not found

Make sure Cargo's bin directory is on your `PATH`:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
```

### Podman is installed but not running

On macOS, start the Podman machine and retry:

```bash
podman machine start
```

### The browser UI did not open

Open it manually:

```bash
llm-box ui
```

### A destination is blocked and you want to inspect it from the terminal

Use:

```bash
llm-box pending
llm-box allowed
```

### You want to see whether a destination is approved by default or only for one session

User defaults affect new sessions only:

```bash
llm-box defaults list
```

Per-session approvals are shown with:

```bash
llm-box allowed
```

## For contributors

If you want to work on `llm-box` itself rather than use it as a product:

### Develop locally

```bash
git clone https://github.com/olillevik/llm-box.git
cd llm-box
cargo test
bash ./tests/test_box.sh
```

### Useful local commands

```bash
cargo build
./llm-box build
./llm-box copilot
cargo run -- copilot
```

### Test coverage

The shell test script covers:

- booting the real `llm-box` image
- scaffolding a repo-local overlay Dockerfile with `llm-box init-image`
- shared Copilot skills mounted read-only into workspace containers
- provider home isolation between workspaces
- user-managed defaults being inherited by new sessions, but not retroactively changing existing sessions
- allowlist persistence through `llm-box allow` and `llm-box deny`
- direct outbound bypasses failing from the agent container
- live approval and live revoke flows for both web and connector destinations
