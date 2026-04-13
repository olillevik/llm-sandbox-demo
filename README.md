# `copilot-box`: GitHub Copilot CLI in a controlled container

`copilot-box` is a thin wrapper around the GitHub Copilot CLI that keeps the Copilot experience familiar while adding container isolation and operator-controlled networking.

## What changed

This repository is now intentionally **GitHub Copilot-specific**:

- one container image
- one launcher: `./copilot-box`
- one auth/session home inside the container
- static inbound policy: no published ports
- dynamic outbound approvals through a host-side proxy

The wrapper is designed to feel Copilot-like. Anything that is not a `copilot-box` policy command is passed straight through to `copilot`, so flows such as:

```bash
./copilot-box
./copilot-box --resume <session-id>
./copilot-box --experimental
```

stay close to the normal Copilot CLI mental model.

## How it works

### Runtime

The image installs `@github/copilot` and runs `copilot` as the container entrypoint.

### Session persistence

Copilot state is stored outside the container at:

```bash
~/.copilot-box/container-home
```

That means auth and Copilot-managed session data survive container restarts, and the container always gets a writable home directory.

### Egress control

All HTTP and HTTPS traffic is sent through a local proxy started by `./copilot-box`.

The proxy:

- allows a small default set of GitHub/Copilot hosts
- logs blocked destinations to per-project state
- reloads the allowlist on every request

That last point is what makes live approval work: approving a hostname updates the file the proxy reads, so the running session can continue without restarting in the common case.

Per-project policy state lives under:

```bash
~/.copilot-box/projects/<workspace-hash>/
```

Important files:

- `allowed-hosts.txt` — persistent allowlist for that workspace
- `pending.jsonl` — blocked outbound attempts
- `proxy.log` — proxy stderr/stdout
- `session-meta.json` — wrapper-level metadata about the last launch

### Ingress control

Ingress is intentionally simple and static:

- the container runs with bridge networking
- no ports are published
- there is no in-session inbound approval flow

## Requirements

- `docker` or `podman`
- `python3`
- `node` only for building the image locally if you want to inspect or extend it; it is not required by the wrapper itself

`./copilot-box` auto-detects `docker` first and falls back to `podman`.

## Build

```bash
./copilot-box build
```

## Usage

Start Copilot in the current directory:

```bash
./copilot-box
```

Resume a Copilot session:

```bash
./copilot-box --resume <session-id>
```

See blocked outbound destinations for the current workspace:

```bash
./copilot-box pending
```

See the current allowlist:

```bash
./copilot-box allowed
```

Approve a destination for future and current requests:

```bash
./copilot-box allow objects-origin.githubusercontent.com
```

Remove an approved destination:

```bash
./copilot-box deny objects-origin.githubusercontent.com
```

If you prefer token-based auth, `GH_TOKEN` or `GITHUB_TOKEN` is passed through into the container. Otherwise, log in inside Copilot with `/login`.

## Default allowed hosts

The initial workspace allowlist contains:

- `api.github.com`
- `api.githubcopilot.com`
- `codeload.github.com`
- `github.com`
- `githubcopilot.com`
- `objects.githubusercontent.com`
- `raw.githubusercontent.com`
- `uploads.github.com`

If Copilot or your workflow needs something else, it will show up via `./copilot-box pending`.

## Caveats

This implementation is intentionally simple and practical:

- live approvals are implemented for HTTP/HTTPS traffic mediated by the proxy
- non-HTTP protocols are not yet mediated by the same approval path
- proxy-based control is strong for visibility and ergonomics, but it is not the same thing as a full host firewall

If you later want stricter enforcement, the next step is to combine this with runtime-specific firewalling or a dedicated network namespace policy layer.

## Tests

There is a lightweight automated test script at:

```bash
./tests/test_box.sh
```

It covers:

- booting the real `copilot-box` image
- allowlist persistence through `copilot-box allow` and `copilot-box deny`
- a live approval flow where a running container is blocked, the host approves the destination, and the same running session succeeds without restart

These tests are intentionally **offline**:

- they do not require Copilot login
- they do not send inference requests
- they do not consume model quota
