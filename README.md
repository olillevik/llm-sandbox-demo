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
- sends HTTP and HTTPS traffic through a host-side approval proxy
- records blocked destinations per session
- lets you approve hosts live without restarting the running session in the common case

## User experience

When `./llm-box copilot` is launched interactively, `llm-box` keeps the provider in your terminal and opens a local browser companion for that session.

The browser companion shows:

- **Pending** blocked hosts for the active session
- **Allowed** hosts for the active session
- a **Dismiss** action to hide a blocked host until it appears again

You can also reopen the browser companion for the latest session in the current workspace:

```bash
./llm-box ui
```

## How it works

### Runtime

The current image installs `@github/copilot` and uses `copilot` as the container entrypoint.

### Session persistence

Container home state is stored at:

```bash
~/.llm-box/container-home
```

That means auth and provider-managed session data survive container restarts.

### Egress control

All HTTP and HTTPS traffic goes through a local proxy started by `llm-box`.

The proxy:

- allows a small default set of GitHub and Copilot hosts
- logs blocked destinations to per-session state
- reloads the allowlist on every request

Session state lives under:

```bash
~/.llm-box/sessions/<session-id>/
```

Important files:

- `allowed-hosts.txt` — allowlist for that session
- `pending.jsonl` — blocked outbound attempts
- `dismissed.json` — dismissed blocked hosts until they reappear
- `proxy.log` — proxy stderr/stdout
- `session-meta.json` — metadata about the session

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

Build the provider image:

```bash
./llm-box build
```

## Usage

Start Copilot in the current directory:

```bash
./llm-box copilot
```

See blocked outbound destinations for the latest session in the current workspace:

```bash
./llm-box pending
```

See the current allowlist for the latest session:

```bash
./llm-box allowed
```

See your user-managed defaults for future sessions:

```bash
./llm-box defaults list
```

Approve a destination for the latest session:

```bash
./llm-box allow objects-origin.githubusercontent.com
```

Remove an approved destination from the latest session:

```bash
./llm-box deny objects-origin.githubusercontent.com
```

`deny` removes the host from the active session allowlist and tears down active proxy tunnels for that host, so new traffic must be re-approved.

Dismiss a blocked destination from the latest session until it reappears:

```bash
./llm-box dismiss objects-origin.githubusercontent.com
```

If you prefer token-based auth, `GH_TOKEN` or `GITHUB_TOKEN` is passed through into the container. Otherwise, log in inside Copilot with `/login`.

## Default allowed hosts

Each new session starts with this intentionally narrow built-in default allowlist:

- `api.github.com`
- `api.business.githubcopilot.com`

You can also add your own defaults for future sessions:

```bash
./llm-box defaults add github.com
./llm-box defaults remove github.com
```

User-managed defaults are stored in:

```bash
~/.llm-box/default-allowed-hosts.txt
```

They are merged into the built-in defaults when a new session is created. Existing sessions keep their current allowlist.

If Copilot needs anything beyond the built-in baseline, it will show up in `./llm-box pending` and you can decide whether to allow it for just that session or add it as a user default.

If Copilot or your workflow needs something else, it will show up via `./llm-box pending`.

## Caveats

- live approvals currently cover HTTP and HTTPS traffic mediated by the proxy
- non-HTTP protocols are not yet mediated by the same approval path
- proxy-based control is useful and visible, but it is not a full host firewall

## Tests

There is a lightweight automated test script at:

```bash
./tests/test_box.sh
```

It covers:

- booting the real `llm-box` image
- user-managed defaults being inherited by new sessions, but not retroactively changing existing sessions
- allowlist persistence through `llm-box allow` and `llm-box deny`
- a live approval flow where a running session is blocked, the host approves the destination, and the same running session succeeds without restart

These tests are intentionally offline:

- they do not require Copilot login
- they do not send inference requests
- they do not consume model quota
