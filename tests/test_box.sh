#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

TEST_IMAGE="copilot-box-test-shell"
TEST_HOME="$(mktemp -d)"
HTTP_ROOT="$(mktemp -d)"
SESSION_OUT="$(mktemp)"
HTTP_LOG="$(mktemp)"

find_free_port() {
  python3 <<'PY'
import socket

sock = socket.socket()
sock.bind(("127.0.0.1", 0))
print(sock.getsockname()[1])
sock.close()
PY
}

project_state_dir() {
  python3 - "$1" "$2" <<'PY'
import hashlib
import os
import sys

workspace = os.path.realpath(sys.argv[1])
root = sys.argv[2]
print(os.path.join(root, "projects", hashlib.sha256(workspace.encode()).hexdigest()))
PY
}

fail() {
  echo "test failed: $*" >&2
  exit 1
}

detect_runtime() {
  if [[ -n "${COPILOT_BOX_RUNTIME:-}" ]]; then
    echo "$COPILOT_BOX_RUNTIME"
    return
  fi

  if [[ -n "${LLM_BOX_RUNTIME:-}" ]]; then
    echo "$LLM_BOX_RUNTIME"
    return
  fi

  if command -v docker >/dev/null 2>&1; then
    echo docker
    return
  fi

  if command -v podman >/dev/null 2>&1; then
    echo podman
    return
  fi

  fail "no supported container runtime found"
}

image_exists() {
  "$RUNTIME" image inspect "$1" >/dev/null 2>&1
}

cleanup() {
  if [[ -n "${SESSION_PID:-}" ]]; then
    kill "$SESSION_PID" 2>/dev/null || true
    wait "$SESSION_PID" 2>/dev/null || true
  fi

  if [[ -n "${HTTP_PID:-}" ]]; then
    kill "$HTTP_PID" 2>/dev/null || true
    wait "$HTTP_PID" 2>/dev/null || true
  fi

  if [[ -n "${RUNTIME:-}" ]]; then
    "$RUNTIME" rmi -f "$TEST_IMAGE" >/dev/null 2>&1 || true
  fi
  rm -rf "$TEST_HOME" "$HTTP_ROOT"
  rm -f "$SESSION_OUT" "$HTTP_LOG"
}

trap cleanup EXIT

RUNTIME="$(detect_runtime)"
export COPILOT_BOX_RUNTIME="$RUNTIME"
export COPILOT_BOX_HOME="$TEST_HOME"

echo "[1/3] verifying $RUNTIME, the real image, and the real wrapper launch path"
"$RUNTIME" info >/dev/null
if ! image_exists copilot-box; then
  ./copilot-box build >/dev/null
fi
"$RUNTIME" run --rm copilot-box --help 2>&1 | grep -q "GitHub Copilot CLI" || fail "real copilot-box image did not boot"
./copilot-box --version 2>&1 | grep -q "GitHub Copilot CLI" || fail "real copilot-box wrapper did not launch the real image"

echo "[2/3] verifying persistent allowlist behavior"
ALLOW_FILE="$(project_state_dir "$ROOT_DIR" "$TEST_HOME")/allowed-hosts.txt"
./copilot-box allow example.com >/dev/null
grep -qx "example.com" "$ALLOW_FILE" || fail "example.com was not persisted to the allowlist"
./copilot-box allowed | grep -qx "example.com" || fail "example.com was not reported by copilot-box allowed"
./copilot-box deny example.com >/dev/null
if ./copilot-box allowed | grep -qx "example.com"; then
  fail "example.com remained in the allowlist after deny"
fi

echo "[3/3] verifying live approval without restarting the session"
"$RUNTIME" build -q -t "$TEST_IMAGE" -f - . <<'EOF' >/dev/null
FROM copilot-box
ENTRYPOINT ["bash", "-lc"]
EOF

export COPILOT_BOX_IMAGE="$TEST_IMAGE"
HTTP_PORT="$(find_free_port)"
printf 'ok\n' >"$HTTP_ROOT/index.html"
python3 -m http.server "$HTTP_PORT" --bind 127.0.0.1 --directory "$HTTP_ROOT" >"$HTTP_LOG" 2>&1 &
HTTP_PID=$!
sleep 1

SESSION_COMMAND="until curl --noproxy '' -fsS http://127.0.0.1:${HTTP_PORT}/; do echo blocked; sleep 1; done"
./copilot-box "$SESSION_COMMAND" >"$SESSION_OUT" 2>&1 &
SESSION_PID=$!

PENDING_FOUND=0
for _ in $(seq 1 20); do
  if ./copilot-box pending | grep -q "127.0.0.1:${HTTP_PORT}"; then
    PENDING_FOUND=1
    break
  fi

  if ! kill -0 "$SESSION_PID" >/dev/null 2>&1; then
    break
  fi

  sleep 1
done

[[ "$PENDING_FOUND" -eq 1 ]] || fail "blocked request never appeared in copilot-box pending"

./copilot-box allow 127.0.0.1 >/dev/null
wait "$SESSION_PID"

grep -q "blocked" "$SESSION_OUT" || fail "running session never showed a blocked attempt"
grep -q "^ok$" "$SESSION_OUT" || fail "running session did not succeed after approval"

if ./copilot-box pending | grep -q "127.0.0.1:${HTTP_PORT}"; then
  fail "approved destination still appears in pending output"
fi

echo "all copilot-box tests passed"
