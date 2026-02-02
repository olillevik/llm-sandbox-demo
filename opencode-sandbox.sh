#!/bin/bash
set -euo pipefail

PROJECT_DIR="${1:-$(pwd)}"

# Use -it for interactive, skip for piped/scripted use
TTY_FLAG="-it"
if [[ ! -t 0 ]] || [[ "${*}" == *"run"* ]]; then
  TTY_FLAG=""
fi

# opencode supports:
# - anthropic/* models via ANTHROPIC_API_KEY
# - google-vertex/* models via GOOGLE_CLOUD_PROJECT + gcloud ADC
# Note: Claude on Vertex (Anthropic via GCP) is NOT supported by opencode

docker run $TTY_FLAG --rm \
  --user "$(id -u):$(id -g)" \
  -e ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY:-}" \
  -e GOOGLE_CLOUD_PROJECT="${ANTHROPIC_VERTEX_PROJECT_ID:-${GOOGLE_CLOUD_PROJECT:-}}" \
  -e VERTEX_LOCATION="${CLOUD_ML_REGION:-${VERTEX_LOCATION:-europe-west1}}" \
  -e GOOGLE_APPLICATION_CREDENTIALS="/home/opencode/.config/gcloud/application_default_credentials.json" \
  -v "$HOME/.config/gcloud":/home/opencode/.config/gcloud:ro \
  -v "$PROJECT_DIR":/workspace \
  --network=bridge \
  --cap-drop=ALL \
  --security-opt=no-new-privileges \
  opencode-sandbox "${@:2}"
