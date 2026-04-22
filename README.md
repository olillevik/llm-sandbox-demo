# `llm-box`: boxed LLM CLIs with live approvals

`llm-box` is a host-side control plane for running assistant CLIs inside a container while keeping outbound access visible and operator-approved.

Today the built-in provider preset is `copilot`, so the primary flow is:

```bash
./llm-box copilot
./llm-box copilot --resume <session-id>
./llm-box copilot --experimental
```

Anything after `copilot` is passed through to the real `copilot` command inside the container.

## What it does

- runs the assistant CLI in a container
- keeps auth and session state outside the container
- denies direct outbound traffic from the agent container by default
- sends brokered HTTP and HTTPS traffic through a per-session approval sidecar
- exposes approved non-web destinations through broker-managed connector endpoints
- records blocked destinations per session
- lets you approve destinations live without restarting the running session in the common case

## User experience

When `./llm-box copilot` is launched interactively, `llm-box` keeps the provider in your terminal and opens or reuses a local browser UI.

The UI shows:

- active sessions only
- stacked labels for pending state, including total **Pending** count and **Unread** count for items you have not looked at yet
- **Pending** blocked destinations for the selected session
- **Allowed** destinations for the selected session
- connector endpoints for approved TCP-style destinations
- a **Dismiss** action to hide a blocked destination until it appears again

You can also reopen the UI for the latest session in the current workspace:

```bash
./llm-box ui
```

Or jump straight to a specific session inside the UI:

```bash
./llm-box ui --session <session-id>
```

## How it works

### Runtime

The current image installs `@github/copilot` and uses `copilot` as the container entrypoint.

If the current workspace contains `.llm-box/Dockerfile`, `llm-box` builds a repo-specific image on top of the managed base image and runs Copilot in that derived image instead.

Use this contract in the repo Dockerfile:

```dockerfile
ARG LLM_BOX_BASE_IMAGE
FROM ${LLM_BOX_BASE_IMAGE}
```

That lets the repo add tools like `gh`, `ripgrep`, or language toolchains without replacing the base `llm-box` runtime contract.

### Session persistence

Provider home state is stored per workspace at:

```bash
~/.llm-box/workspaces/<workspace-hash>/home
```

That means auth, provider settings, and provider-managed session data survive container restarts for that workspace without bleeding into other workspaces.

As a small shared convenience layer, `llm-box` also mounts host Copilot skills from `~/.copilot/skills` into each workspace container as read-only. You can override that source path with `LLM_BOX_SHARED_COPILOT_SKILLS_DIR`.

### Egress control

`llm-box` creates two per-session container networks:

- an **internal** network for the agent container
- an **external** network for the broker sidecar

The agent container is attached only to the internal network. That means it does not have a direct outbound route to the network on its own.

`llm-box` then starts a broker sidecar attached to both networks. Current HTTP and HTTPS traffic is sent through that broker using the standard proxy environment variables.

The broker:

- allows a small default set of GitHub and Copilot destinations
- logs blocked destinations to per-session state
- reloads the approved target set on every request
- carries the live-approval flow for web traffic without giving the agent direct outbound networking
- creates broker-local connector listeners for approved `tcp://`, `ssh://`, and `mcp://` destinations

Session state lives under:

```bash
~/.llm-box/sessions/<session-id>/
```

Important files:

- `allowed-targets.txt` — approved targets for that session
- `connectors.json` — broker-managed connector mappings for approved non-web destinations
- `pending-events.jsonl` — blocked outbound attempts
- `dismissed.json` — dismissed blocked destinations until they reappear
- `broker.log` — broker stderr/stdout
- `session-meta.json` — metadata about the session

These approval-session files stay on the host for the `llm-box` control plane. The broker sidecar mounts the session directory to read the approved target set and append pending events; the agent container does not.

Workspace links to the latest local session live under:

```bash
~/.llm-box/workspaces/<workspace-hash>/
```

### Ingress control

Ingress stays intentionally simple:

- the container runs with bridge networking
- no ports are published
- there is no in-session inbound approval flow

## Requirements

- `docker` or `podman`
- Rust and Cargo for running the wrapper from this repository
- a local browser for the companion UI during interactive use

`llm-box` auto-detects `docker` first and falls back to `podman`.

## Build

Build the provider image for the current workspace:

```bash
./llm-box build
```

If `.llm-box/Dockerfile` is present, this builds both the managed base image and the repo-specific derived image.

Scaffold a repo-local overlay Dockerfile:

```bash
./llm-box init-image
```

That creates `.llm-box/Dockerfile` with a starter template that extends the managed base image.

## Usage

Start Copilot in the current directory:

```bash
./llm-box copilot
```

`llm-box` opens or reuses the shared UI automatically for interactive sessions.

Open the UI:

```bash
./llm-box ui
./llm-box ui --session <session-id>
```

Create a starter overlay Dockerfile in the current repo:

```bash
./llm-box init-image
```

See blocked outbound destinations for the latest session in the current workspace:

```bash
./llm-box pending
```

See the current approved target set for the latest session:

```bash
./llm-box allowed
```

See your user-managed defaults for future sessions:

```bash
./llm-box defaults list
```

Approve a destination for the latest session:

```bash
./llm-box allow https://objects-origin.githubusercontent.com:443
```

Remove an approved destination from the latest session:

```bash
./llm-box deny https://objects-origin.githubusercontent.com:443
```

`deny` removes the destination from the active approved set and tears down active broker tunnels or connector listeners for that destination, so new traffic must be re-approved.

Dismiss a blocked destination from the latest session until it reappears:

```bash
./llm-box dismiss https://objects-origin.githubusercontent.com:443
```

Create or resolve a broker endpoint for an approved non-web destination:

```bash
./llm-box endpoint tcp://db.internal.example:5432
./llm-box endpoint ssh://github.com:22
```

If you prefer token-based auth, `GH_TOKEN` or `GITHUB_TOKEN` is passed through into the container. Otherwise, log in inside Copilot with `/login`.

Destinations are canonicalized as `scheme://host:port`.

- bare hosts default to `https://host:443`
- `http://` and `https://` use the web proxy path
- `tcp://`, `ssh://`, and `mcp://` use broker-managed connector endpoints

## Default allowed destinations

Each new session starts with this intentionally narrow built-in default target set:

- `https://api.github.com:443`
- `https://api.business.githubcopilot.com:443`

You can also add your own defaults for future sessions:

```bash
./llm-box defaults add github.com
./llm-box defaults remove github.com
```

User-managed defaults are stored in:

```bash
~/.llm-box/default-allowed-targets.txt
```

They are merged into the built-in defaults when a new session is created. Existing sessions keep their current approved target set.

If Copilot needs anything beyond the built-in baseline, it will show up in `./llm-box pending` and you can decide whether to allow it for just that session or add it as a user default.

If Copilot or your workflow needs something else, it will show up via `./llm-box pending`.

## Caveats

- approvals are destination-based as `protocol + host + port`
- direct outbound traffic from the agent container is denied by the network topology; non-web workflows must use broker-managed connector endpoints
- SSH destinations are scoped and brokered, but host-key policy is not yet enforced by `llm-box`
- MCP support here is transport-level: stdio needs no networking, HTTP uses the web proxy path, and TCP/WebSocket-style transports use connector endpoints
- this is still container-based isolation and brokered egress control, not a host firewall or a hardened VM boundary

## Tests

There is a lightweight automated test script at:

```bash
./tests/test_box.sh
```

It covers:

- booting the real `llm-box` image
- scaffolding a repo-local overlay Dockerfile with `llm-box init-image`
- shared Copilot skills mounted read-only into workspace containers
- provider home isolation between workspaces
- user-managed defaults being inherited by new sessions, but not retroactively changing existing sessions
- allowlist persistence through `llm-box allow` and `llm-box deny`
- direct outbound bypasses failing from the agent container
- a live approval flow where a running session is blocked, the host approves the destination, and the same running session succeeds without restart

These tests are intentionally offline:

- they do not require Copilot login
- they do not send inference requests
- they do not consume model quota
