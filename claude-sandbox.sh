#!/bin/bash
set -euo pipefail

PROJECT_DIR="${1:-$(pwd)}"

# Use -it for interactive, just -i for piped/scripted use
TTY_FLAG="-it"
if [[ ! -t 0 ]] || [[ "${*}" == *"-p"* ]] || [[ "${*}" == *"--print"* ]]; then
  TTY_FLAG=""
fi

docker run $TTY_FLAG --rm \
  --user "$(id -u):$(id -g)" \
  -e CLAUDE_CODE_USE_VERTEX="${CLAUDE_CODE_USE_VERTEX:-}" \
  -e ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY:-}" \
  -e ANTHROPIC_VERTEX_PROJECT_ID="${ANTHROPIC_VERTEX_PROJECT_ID:-}" \
  -e CLOUD_ML_REGION="${CLOUD_ML_REGION:-europe-west1}" \
  -e GOOGLE_APPLICATION_CREDENTIALS="/home/claude/.config/gcloud/application_default_credentials.json" \
  -v "$HOME/.config/gcloud":/home/claude/.config/gcloud:ro \
  -v "$PROJECT_DIR":/workspace \
  --network=bridge \
  --cap-drop=ALL \
  --security-opt=no-new-privileges \
  claude-sandbox "${@:2}"
