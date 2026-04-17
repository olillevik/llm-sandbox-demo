#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

TEST_IMAGE="llm-box-test-shell"
TEST_HOME="$(mktemp -d)"
HTTP_ROOT="$(mktemp -d)"
SESSION_OUT="$(mktemp)"
HTTP_LOG="$(mktemp)"
WORKSPACE_A="$(mktemp -d)"
WORKSPACE_B="$(mktemp -d)"
WORKSPACE_C="$(mktemp -d)"
BINARY="$ROOT_DIR/target/debug/llm-box"

find_free_port() {
  ./llm-box __test-free-port
}

latest_session_dir() {
  ./llm-box __test-latest-session-dir "$1" "$2"
}

fail() {
  echo "test failed: $*" >&2
  exit 1
}

detect_runtime() {
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

ensure_runtime_ready() {
  local info_output status
  info_output="$(mktemp)"

  if "$RUNTIME" info >"$info_output" 2>&1; then
    rm -f "$info_output"
    return
  fi

  status=$?
  if [[ "$RUNTIME" == "podman" && "$(uname -s)" == "Darwin" ]] \
    && grep -Eqi "cannot connect to podman|unable to connect to podman socket|podman machine" "$info_output"; then
    cat "$info_output" >&2
    rm -f "$info_output"
    fail "Podman is installed but not running. Run 'podman machine start' and retry."
  fi

  cat "$info_output" >&2
  rm -f "$info_output"
  exit "$status"
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
  rm -rf "$TEST_HOME" "$HTTP_ROOT" "$WORKSPACE_A" "$WORKSPACE_B" "$WORKSPACE_C"
  rm -f "$SESSION_OUT" "$HTTP_LOG"
}

trap cleanup EXIT

RUNTIME="$(detect_runtime)"
export LLM_BOX_RUNTIME="$RUNTIME"
export LLM_BOX_HOME="$TEST_HOME"
export LLM_BOX_NO_BROWSER=1

if [[ ! -x "$BINARY" ]]; then
  cargo build >/dev/null
fi

echo "[1/4] verifying $RUNTIME, the real image, and the real wrapper launch path"
ensure_runtime_ready
if ! image_exists llm-box; then
  ./llm-box build >/dev/null
fi
"$RUNTIME" run --rm llm-box --help 2>&1 | grep -q "GitHub Copilot CLI" || fail "real llm-box image did not boot"
./llm-box copilot --version 2>&1 | grep -q "GitHub Copilot CLI" || fail "real llm-box wrapper did not launch the real image"

echo "[2/4] verifying user defaults seed only new sessions"
(cd "$WORKSPACE_A" && "$BINARY" copilot --version >/dev/null)
ALLOW_FILE_A="$("$BINARY" __test-latest-session-dir "$WORKSPACE_A" "$TEST_HOME")/allowed-hosts.txt"
if grep -qx "defaults.example" "$ALLOW_FILE_A"; then
  fail "user default appeared in an existing session before it was added"
fi

"$BINARY" defaults add "https://Defaults.Example:443/path" >/dev/null
"$BINARY" defaults list | grep -qx "defaults.example" || fail "defaults list did not report normalized user default"
if grep -qx "defaults.example" "$ALLOW_FILE_A"; then
  fail "existing session picked up a newly added user default"
fi

(cd "$WORKSPACE_B" && "$BINARY" copilot --version >/dev/null)
ALLOW_FILE_B="$("$BINARY" __test-latest-session-dir "$WORKSPACE_B" "$TEST_HOME")/allowed-hosts.txt"
grep -qx "defaults.example" "$ALLOW_FILE_B" || fail "new session did not inherit user default"

"$BINARY" defaults remove defaults.example >/dev/null
if "$BINARY" defaults list | grep -qx "defaults.example"; then
  fail "user default remained after removal"
fi

(cd "$WORKSPACE_C" && "$BINARY" copilot --version >/dev/null)
ALLOW_FILE_C="$("$BINARY" __test-latest-session-dir "$WORKSPACE_C" "$TEST_HOME")/allowed-hosts.txt"
if grep -qx "defaults.example" "$ALLOW_FILE_C"; then
  fail "removed user default still appeared in a new session"
fi

echo "[3/4] verifying persistent allowlist behavior"
./llm-box copilot --version >/dev/null
ALLOW_FILE="$(latest_session_dir "$ROOT_DIR" "$TEST_HOME")/allowed-hosts.txt"
./llm-box allow example.com >/dev/null
grep -qx "example.com" "$ALLOW_FILE" || fail "example.com was not persisted to the allowlist"
./llm-box allowed | grep -qx "example.com" || fail "example.com was not reported by llm-box allowed"
./llm-box deny example.com >/dev/null
if ./llm-box allowed | grep -qx "example.com"; then
  fail "example.com remained in the allowlist after deny"
fi

echo "[4/4] verifying live approval without restarting the session"
"$RUNTIME" build -q -t "$TEST_IMAGE" -f - . <<'EOF' >/dev/null
FROM llm-box
ENTRYPOINT ["bash", "-lc"]
EOF

export LLM_BOX_IMAGE="$TEST_IMAGE"
HTTP_PORT="$(find_free_port)"
printf 'ok\n' >"$HTTP_ROOT/index.html"
./llm-box __serve-static --listen-host 127.0.0.1 --listen-port "$HTTP_PORT" --directory "$HTTP_ROOT" >"$HTTP_LOG" 2>&1 &
HTTP_PID=$!
sleep 1

SESSION_COMMAND="until curl --noproxy '' -fsS http://127.0.0.1:${HTTP_PORT}/; do echo blocked; sleep 1; done"
./llm-box copilot "$SESSION_COMMAND" >"$SESSION_OUT" 2>&1 &
SESSION_PID=$!

PENDING_FOUND=0
for _ in $(seq 1 20); do
  if ./llm-box pending | grep -q "127.0.0.1:${HTTP_PORT}"; then
    PENDING_FOUND=1
    break
  fi

  if ! kill -0 "$SESSION_PID" >/dev/null 2>&1; then
    break
  fi

  sleep 1
done

[[ "$PENDING_FOUND" -eq 1 ]] || fail "blocked request never appeared in llm-box pending"

./llm-box allow 127.0.0.1 >/dev/null
wait "$SESSION_PID"

grep -q "blocked" "$SESSION_OUT" || fail "running session never showed a blocked attempt"
grep -q "^ok$" "$SESSION_OUT" || fail "running session did not succeed after approval"

if ./llm-box pending | grep -q "127.0.0.1:${HTTP_PORT}"; then
  fail "approved destination still appears in pending output"
fi

echo "all llm-box tests passed"
