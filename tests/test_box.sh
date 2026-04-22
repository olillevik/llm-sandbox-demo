#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

TEST_IMAGE="llm-box-test-shell"
TEST_HOME="$(mktemp -d)"
HTTP_ROOT="$(mktemp -d)"
SESSION_OUT="$(mktemp)"
CONNECTOR_OUT="$(mktemp)"
HTTP_LOG="$(mktemp)"
WORKSPACE_A="$(mktemp -d)"
WORKSPACE_B="$(mktemp -d)"
WORKSPACE_C="$(mktemp -d)"
WORKSPACE_D="$(mktemp -d)"
WORKSPACE_E="$(mktemp -d)"
WORKSPACE_F="$(mktemp -d)"
WORKSPACE_G="$(mktemp -d)"
SHARED_SKILLS_DIR="$(mktemp -d)"
BINARY="$ROOT_DIR/target/debug/llm-box"

find_free_port() {
  ./llm-box __test-free-port
}

latest_session_dir() {
  ./llm-box __test-latest-session-dir "$1" "$2"
}

workspace_home_dir() {
  ./llm-box __test-workspace-home "$1" "$2"
}

fail() {
  echo "test failed: $*" >&2
  exit 1
}

wait_for_pending_target() {
  local target="$1"
  for _ in $(seq 1 20); do
    if ./llm-box pending | grep -q "$target"; then
      return 0
    fi
    sleep 1
  done
  return 1
}

wait_for_pending_clear() {
  local target="$1"
  for _ in $(seq 1 20); do
    if ! ./llm-box pending | grep -q "$target"; then
      return 0
    fi
    sleep 1
  done
  return 1
}

assert_allowed_contains() {
  local target="$1"
  ./llm-box allowed | grep -q "$target" || fail "$target was not reported by llm-box allowed"
}

assert_allowed_absent() {
  local target="$1"
  if ./llm-box allowed | grep -q "$target"; then
    fail "$target remained in llm-box allowed unexpectedly"
  fi
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

host_loopback_alias() {
  if [[ "$RUNTIME" == "podman" ]]; then
    echo "host.containers.internal"
  else
    echo "host.docker.internal"
  fi
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
  rm -rf "$TEST_HOME" "$HTTP_ROOT" "$WORKSPACE_A" "$WORKSPACE_B" "$WORKSPACE_C" "$WORKSPACE_D" "$WORKSPACE_E" "$WORKSPACE_F" "$WORKSPACE_G" "$SHARED_SKILLS_DIR"
  rm -f "$SESSION_OUT" "$CONNECTOR_OUT" "$HTTP_LOG"
}

trap cleanup EXIT

RUNTIME="$(detect_runtime)"
HOST_LOOPBACK_ALIAS="$(host_loopback_alias)"
export LLM_BOX_RUNTIME="$RUNTIME"
export LLM_BOX_HOME="$TEST_HOME"
export LLM_BOX_NO_BROWSER=1
export LLM_BOX_SHARED_COPILOT_SKILLS_DIR="$SHARED_SKILLS_DIR"

if [[ ! -x "$BINARY" ]]; then
  cargo build >/dev/null
fi

echo "[1/12] verifying $RUNTIME, the real image, and the real wrapper launch path"
ensure_runtime_ready
./llm-box build >/dev/null
IMAGE_HELP="$("$RUNTIME" run --rm llm-box --help 2>&1)"
echo "$IMAGE_HELP" | grep -q "GitHub Copilot CLI" || fail "real llm-box image did not boot"
WRAPPER_VERSION_OUTPUT="$(./llm-box copilot --version 2>&1)"
echo "$WRAPPER_VERSION_OUTPUT" | grep -q "GitHub Copilot CLI" || fail "real llm-box wrapper did not launch the real image"

echo "[2/12] verifying init-image scaffolds a repo overlay Dockerfile"
INIT_OUTPUT="$(cd "$WORKSPACE_G" && "$BINARY" init-image)"
INIT_FILE="$WORKSPACE_G/.llm-box/Dockerfile"
CANONICAL_INIT_FILE="$(cd "$WORKSPACE_G" && pwd -P)/.llm-box/Dockerfile"
[[ "$INIT_OUTPUT" == "$CANONICAL_INIT_FILE" ]] || fail "init-image did not print the created Dockerfile path"
grep -qx 'ARG LLM_BOX_BASE_IMAGE' "$INIT_FILE" || fail "init-image did not write the base image arg"
grep -qx 'FROM ${LLM_BOX_BASE_IMAGE}' "$INIT_FILE" || fail "init-image did not write the base image FROM line"
(cd "$WORKSPACE_G" && "$BINARY" build >/dev/null)

echo "[3/12] verifying repo overlay images extend the llm-box base"
mkdir -p "$WORKSPACE_D/.llm-box"
cat >"$WORKSPACE_D/.llm-box/Dockerfile" <<'EOF'
ARG LLM_BOX_BASE_IMAGE
FROM ${LLM_BOX_BASE_IMAGE}
ENTRYPOINT ["sh", "-lc", "echo overlay-entrypoint; exec copilot \"$@\"", "--"]
EOF

(cd "$WORKSPACE_D" && "$BINARY" build >/dev/null)
OVERLAY_OUTPUT="$(cd "$WORKSPACE_D" && "$BINARY" copilot --version 2>&1)"
echo "$OVERLAY_OUTPUT" | grep -q "overlay-entrypoint" || fail "repo overlay image was not used"

"$RUNTIME" build -q -t "$TEST_IMAGE" -f - . <<'EOF' >/dev/null
FROM llm-box
ENTRYPOINT ["bash", "-lc"]
EOF

echo "[4/12] verifying shared skills are mounted read-only"
mkdir -p "$SHARED_SKILLS_DIR/git-commit"
printf 'shared skill\n' >"$SHARED_SKILLS_DIR/git-commit/SKILL.md"
(cd "$WORKSPACE_E" && LLM_BOX_IMAGE="$TEST_IMAGE" "$BINARY" copilot 'grep -q "shared skill" "$HOME/.copilot/skills/git-commit/SKILL.md" && ! sh -lc "echo nope > \"$HOME/.copilot/skills/git-commit/SHOULD-NOT-WRITE\"" 2>/dev/null' )
if [[ -e "$SHARED_SKILLS_DIR/git-commit/SHOULD-NOT-WRITE" ]]; then
  fail "shared skills mount was unexpectedly writable"
fi

echo "[5/12] verifying provider home is isolated per workspace"
(cd "$WORKSPACE_E" && LLM_BOX_IMAGE="$TEST_IMAGE" "$BINARY" copilot 'git config --global user.name workspace-e && git config --global user.email e@example.com')
(cd "$WORKSPACE_F" && LLM_BOX_IMAGE="$TEST_IMAGE" "$BINARY" copilot 'if [ -f "$HOME/.gitconfig" ]; then ! grep -q workspace-e "$HOME/.gitconfig"; fi && git config --global user.name workspace-f && git config --global user.email f@example.com')
WORKSPACE_HOME_E="$(workspace_home_dir "$WORKSPACE_E" "$TEST_HOME")"
WORKSPACE_HOME_F="$(workspace_home_dir "$WORKSPACE_F" "$TEST_HOME")"
grep -q "workspace-e" "$WORKSPACE_HOME_E/.gitconfig" || fail "workspace E did not persist its own git config in its workspace home"
grep -q "workspace-f" "$WORKSPACE_HOME_F/.gitconfig" || fail "workspace F did not persist its own git config in its workspace home"
if grep -q "workspace-e" "$WORKSPACE_HOME_F/.gitconfig"; then
  fail "workspace F unexpectedly saw workspace E state"
fi

echo "[6/12] verifying user defaults seed only new sessions"
(cd "$WORKSPACE_A" && "$BINARY" copilot --version >/dev/null)
ALLOW_FILE_A="$("$BINARY" __test-latest-session-dir "$WORKSPACE_A" "$TEST_HOME")/allowed-targets.txt"
if grep -qx "https://defaults.example:443" "$ALLOW_FILE_A"; then
  fail "user default appeared in an existing session before it was added"
fi

"$BINARY" defaults add "https://Defaults.Example:443/path" >/dev/null
"$BINARY" defaults list | grep -qx "https://defaults.example:443" || fail "defaults list did not report normalized user default"
if grep -qx "https://defaults.example:443" "$ALLOW_FILE_A"; then
  fail "existing session picked up a newly added user default"
fi

(cd "$WORKSPACE_B" && "$BINARY" copilot --version >/dev/null)
ALLOW_FILE_B="$("$BINARY" __test-latest-session-dir "$WORKSPACE_B" "$TEST_HOME")/allowed-targets.txt"
grep -qx "https://defaults.example:443" "$ALLOW_FILE_B" || fail "new session did not inherit user default"

"$BINARY" defaults remove "https://defaults.example" >/dev/null
if "$BINARY" defaults list | grep -qx "https://defaults.example:443"; then
  fail "user default remained after removal"
fi

(cd "$WORKSPACE_C" && "$BINARY" copilot --version >/dev/null)
ALLOW_FILE_C="$("$BINARY" __test-latest-session-dir "$WORKSPACE_C" "$TEST_HOME")/allowed-targets.txt"
if grep -qx "https://defaults.example:443" "$ALLOW_FILE_C"; then
  fail "removed user default still appeared in a new session"
fi

echo "[7/12] verifying persistent allowlist behavior"
./llm-box copilot --version >/dev/null
ALLOW_FILE="$(latest_session_dir "$ROOT_DIR" "$TEST_HOME")/allowed-targets.txt"
./llm-box allow https://example.com >/dev/null
grep -qx "https://example.com:443" "$ALLOW_FILE" || fail "example.com was not persisted to the allowlist"
./llm-box allowed | grep -qx "https://example.com:443" || fail "example.com was not reported by llm-box allowed"
./llm-box deny https://example.com >/dev/null
if ./llm-box allowed | grep -qx "https://example.com:443"; then
  fail "example.com remained in the allowlist after deny"
fi

echo "[8/12] verifying direct outbound bypass is denied"
HTTP_PORT="$(find_free_port)"
printf 'ok\n' >"$HTTP_ROOT/index.html"
./llm-box __serve-static --listen-host 127.0.0.1 --listen-port "$HTTP_PORT" --directory "$HTTP_ROOT" >"$HTTP_LOG" 2>&1 &
HTTP_PID=$!
sleep 1
(cd "$ROOT_DIR" && LLM_BOX_IMAGE="$TEST_IMAGE" "$BINARY" copilot "! curl --noproxy '*' -fsS http://${HOST_LOOPBACK_ALIAS}:${HTTP_PORT}/ >/dev/null")

echo "[9/12] verifying connector-based live approval without restarting the session"
export LLM_BOX_IMAGE="$TEST_IMAGE"
: >"$CONNECTOR_OUT"
SESSION_COMMAND='CONNECTOR_ENDPOINT="$(llm-box endpoint "tcp://127.0.0.1:'"${HTTP_PORT}"'" | cut -f2)"; until curl --noproxy "*" -fsS "http://${CONNECTOR_ENDPOINT}/"; do echo blocked-connector; sleep 1; done'
./llm-box copilot "$SESSION_COMMAND" >"$CONNECTOR_OUT" 2>&1 &
SESSION_PID=$!

wait_for_pending_target "tcp://127.0.0.1:${HTTP_PORT}" \
  || fail "blocked connector request never appeared in llm-box pending"

./llm-box allow "tcp://127.0.0.1:${HTTP_PORT}" >/dev/null
wait "$SESSION_PID"
SESSION_PID=

grep -q "blocked-connector" "$CONNECTOR_OUT" || fail "running connector session never showed a blocked attempt"
grep -q "^ok$" "$CONNECTOR_OUT" || fail "running connector session did not succeed after approval"
assert_allowed_contains "tcp://127.0.0.1:${HTTP_PORT}"
wait_for_pending_clear "tcp://127.0.0.1:${HTTP_PORT}" \
  || fail "approved connector destination still appears in pending output"
CONNECTOR_ENDPOINT="$(./llm-box endpoint "tcp://127.0.0.1:${HTTP_PORT}" | cut -f2)"

echo "[10/12] verifying connector traffic is denied again after revoke"
: >"$CONNECTOR_OUT"
./llm-box deny "tcp://127.0.0.1:${HTTP_PORT}" >/dev/null
assert_allowed_absent "tcp://127.0.0.1:${HTTP_PORT}"
wait_for_pending_clear "tcp://127.0.0.1:${HTTP_PORT}" \
  || fail "revoked connector destination reappeared in pending before a new blocked attempt"
OLD_CONNECTOR_URL="http://${CONNECTOR_ENDPOINT}/"
ATTEMPTS=0
until ! curl --noproxy "*" -fsS "$OLD_CONNECTOR_URL" >/dev/null 2>&1; do
  ATTEMPTS=$((ATTEMPTS + 1))
  [ "$ATTEMPTS" -ge 20 ] && fail "revoked connector endpoint remained live after deny"
  sleep 0.2
done
./llm-box copilot 'CONNECTOR_ENDPOINT="$(llm-box endpoint "tcp://127.0.0.1:'"${HTTP_PORT}"'" | cut -f2)"; sleep 1; ! curl --noproxy "*" -fsS "http://${CONNECTOR_ENDPOINT}/" >/dev/null' >"$CONNECTOR_OUT" 2>&1
wait_for_pending_target "tcp://127.0.0.1:${HTTP_PORT}" \
  || fail "revoked connector destination did not reappear in pending output"

echo "[11/12] verifying HTTP live approval without restarting the session"
: >"$SESSION_OUT"

SESSION_COMMAND="until curl --noproxy '' -fsS http://127.0.0.1:${HTTP_PORT}/; do echo blocked; sleep 1; done"
./llm-box copilot "$SESSION_COMMAND" >"$SESSION_OUT" 2>&1 &
SESSION_PID=$!

wait_for_pending_target "http://127.0.0.1:${HTTP_PORT}" \
  || fail "blocked request never appeared in llm-box pending"

./llm-box allow "http://127.0.0.1:${HTTP_PORT}" >/dev/null
wait "$SESSION_PID"
SESSION_PID=

grep -q "blocked" "$SESSION_OUT" || fail "running session never showed a blocked attempt"
grep -q "^ok$" "$SESSION_OUT" || fail "running session did not succeed after approval"
wait_for_pending_clear "http://127.0.0.1:${HTTP_PORT}" \
  || fail "approved destination still appears in pending output"

echo "[12/12] verifying HTTP traffic is denied again after revoke"
./llm-box deny "http://127.0.0.1:${HTTP_PORT}" >/dev/null
assert_allowed_absent "http://127.0.0.1:${HTTP_PORT}"
wait_for_pending_clear "http://127.0.0.1:${HTTP_PORT}" \
  || fail "revoked HTTP destination reappeared in pending before a new blocked attempt"
./llm-box copilot "! curl --noproxy '' -fsS http://127.0.0.1:${HTTP_PORT}/ >/dev/null"
wait_for_pending_target "http://127.0.0.1:${HTTP_PORT}" \
  || fail "revoked HTTP destination did not reappear in pending output"

echo "all llm-box tests passed"
