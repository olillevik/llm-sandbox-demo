# Sandboxing LLM Agents (Claude Code / opencode)

Running LLM agents in Docker containers provides a clean separation between the agent's workspace and your host system. This guide covers practical setup for Claude Code and opencode with proper credential handling.

## What Container Isolation Provides

| Boundary | Description |
|----------|-------------|
| Filesystem | Agent only sees explicitly mounted directories |
| Processes | Cannot interact with host processes |
| User context | Runs as non-root user, mapped to your UID |
| Credentials | Only secrets you explicitly pass are available |
| Network | Full access for API calls, git, and integrations |

## Practical Security Model

The container acts as a disposable workspace. The key benefits:

1. **Clean slate each run** - Containers are created fresh and removed after use (`--rm`)
2. **Explicit resource access** - You choose exactly which directories and credentials to expose
3. **Easy rotation** - If you want fresh credentials, just generate new ones and update your env vars
4. **Reproducible environment** - Same container image works across machines

For additional caution, you can periodically discard containers and rotate any exposed credentials. This is straightforward since credentials are passed via environment variables rather than stored in the container.

---

## Quick Start (Vertex AI)

Tested and working:

```bash
# Build
docker build -t claude-sandbox .

# Run (interactive)
docker run -it --rm \
  --user "$(id -u):$(id -g)" \
  -e CLAUDE_CODE_USE_VERTEX=1 \
  -e ANTHROPIC_VERTEX_PROJECT_ID="$ANTHROPIC_VERTEX_PROJECT_ID" \
  -e CLOUD_ML_REGION="$CLOUD_ML_REGION" \
  -e GOOGLE_APPLICATION_CREDENTIALS="/home/claude/.config/gcloud/application_default_credentials.json" \
  -v "$HOME/.config/gcloud":/home/claude/.config/gcloud:ro \
  -v "$(pwd)":/workspace \
  --cap-drop=ALL \
  --security-opt=no-new-privileges \
  claude-sandbox
```

---

## Directory Structure

```
~/.claude-docker/
├── config-repo/          # Your skills, CLAUDE.md, commands (git repo)
│   ├── CLAUDE.md
│   ├── commands/
│   └── skills/
├── auth/                 # OAuth tokens (Copilot, etc.)
├── conversations/        # Persisted conversation history
└── secrets.env           # API keys (never commit this)
```

For opencode, the structure mirrors its config expectations:
```
~/.opencode-docker/
├── config-repo/
│   └── opencode.json     # or equivalent config
├── auth/
└── secrets.env
```

---

## Dockerfile

```dockerfile
FROM node:22-slim

RUN apt-get update && apt-get install -y git curl && rm -rf /var/lib/apt/lists/*
RUN npm install -g @anthropic-ai/claude-code

RUN useradd -m -s /bin/bash claude
USER claude

RUN mkdir -p /home/claude/.claude /home/claude/.config/gcloud
WORKDIR /workspace

ENTRYPOINT ["claude"]
```

Build:
```bash
docker build -t claude-sandbox .
```

---

## Authentication

### Direct API Keys

Pass key as environment variable:

```bash
docker run -it --rm \
  -e ANTHROPIC_API_KEY="$ANTHROPIC_API_KEY" \
  ...
```

### Vertex AI (Google Cloud)

Requires GCP credentials and the `CLAUDE_CODE_USE_VERTEX=1` flag. Use Application Default Credentials:

```bash
# On host first (one-time)
gcloud auth application-default login

# Mount credentials into container
docker run -it --rm \
  --user "$(id -u):$(id -g)" \
  -e CLAUDE_CODE_USE_VERTEX=1 \
  -e ANTHROPIC_VERTEX_PROJECT_ID="$ANTHROPIC_VERTEX_PROJECT_ID" \
  -e CLOUD_ML_REGION="${CLOUD_ML_REGION:-europe-west1}" \
  -e GOOGLE_APPLICATION_CREDENTIALS="/home/claude/.config/gcloud/application_default_credentials.json" \
  -v "$HOME/.config/gcloud":/home/claude/.config/gcloud:ro \
  ...
```

**Note**: The `--user "$(id -u):$(id -g)"` flag is required so the container can read your gcloud credentials.

Or use a service account key file:
```bash
-v "/path/to/service-account.json":/secrets/key.json:ro \
-e GOOGLE_APPLICATION_CREDENTIALS="/secrets/key.json" \
-e CLAUDE_CODE_USE_VERTEX=1
```

### AWS Bedrock

```bash
docker run -it --rm \
  -e AWS_ACCESS_KEY_ID="$AWS_ACCESS_KEY_ID" \
  -e AWS_SECRET_ACCESS_KEY="$AWS_SECRET_ACCESS_KEY" \
  -e AWS_REGION="us-east-1" \
  ...
```

### GitHub Copilot (OAuth)

Copilot requires interactive OAuth device flow:

```bash
# Run with auth persistence
docker run -it --rm \
  -v "$HOME/.claude-docker/auth":/home/claude/.claude/auth \
  claude-sandbox /login github-copilot
```

The token persists in `~/.claude-docker/auth/` for future sessions.

---

## Passing Secrets

### Environment Variables (Recommended)

Map host env vars to container env vars:

```bash
docker run -it --rm \
  -e ANTHROPIC_API_KEY="${MY_CORP_KEY}" \
  -e GITHUB_TOKEN="${GHE_TOKEN}" \
  -e OPENAI_API_KEY="${AZURE_KEY}" \
  ...
```

### Env File

Create `~/.claude-docker/secrets.env`:
```bash
ANTHROPIC_API_KEY=sk-ant-...
GITHUB_TOKEN=ghp_...
```

Use with:
```bash
docker run -it --rm \
  --env-file ~/.claude-docker/secrets.env \
  ...
```

### Combined Approach

Use env file for stable secrets, override specific ones via `-e`:

```bash
docker run -it --rm \
  --env-file ~/.claude-docker/secrets.env \
  -e ANTHROPIC_API_KEY="${DIFFERENT_KEY}" \
  ...
```

---

## Passing Configuration Files

### Claude Code

Mount your config repo to `/home/claude/.claude`:

```bash
-v "$HOME/.claude-docker/config-repo":/home/claude/.claude:ro
```

The repo contains:
- `CLAUDE.md` - Global instructions
- `commands/` - Custom slash commands
- `skills/` - Skill definitions

### opencode

Mount config to opencode's expected location:

```bash
-v "$HOME/.opencode-docker/config-repo":/home/opencode/.config/opencode:ro
```

### Project-Specific Config

Your project's `.claude/` directory is included when you mount the project:

```bash
-v "/path/to/project":/workspace
# Project's /path/to/project/.claude/ is accessible at /workspace/.claude/
```

---

## Run Script

Save as `~/bin/claude-sandbox` and `chmod +x`:

```bash
#!/bin/bash
set -euo pipefail

CONFIG_DIR="$HOME/.claude-docker"
PROJECT_DIR="${1:-$(pwd)}"

# Env var mappings (host var -> container var)
ENV_MAPPINGS=(
  -e "ANTHROPIC_API_KEY=${CORP_ANTHROPIC_KEY:-${ANTHROPIC_API_KEY:-}}"
  -e "GITHUB_TOKEN=${GHE_TOKEN:-}"
  -e "ANTHROPIC_VERTEX_PROJECT_ID=${ANTHROPIC_VERTEX_PROJECT_ID:-}"
  -e "CLOUD_ML_REGION=${CLOUD_ML_REGION:-europe-west1}"
  -e "CLAUDE_CODE_USE_VERTEX=${CLAUDE_CODE_USE_VERTEX:-}"
)

# Secrets file (optional)
SECRETS_OPTS=()
[[ -f "$CONFIG_DIR/secrets.env" ]] && SECRETS_OPTS=(--env-file "$CONFIG_DIR/secrets.env")

# GCP credentials (optional)
GCP_OPTS=()
if [[ -f "$HOME/.config/gcloud/application_default_credentials.json" ]]; then
  GCP_OPTS=(
    -v "$HOME/.config/gcloud":/home/claude/.config/gcloud:ro
    -e "GOOGLE_APPLICATION_CREDENTIALS=/home/claude/.config/gcloud/application_default_credentials.json"
  )
fi

# Ensure directories exist
mkdir -p "$CONFIG_DIR/auth" "$CONFIG_DIR/conversations"

docker run -it --rm \
  "${ENV_MAPPINGS[@]}" \
  "${SECRETS_OPTS[@]:-}" \
  "${GCP_OPTS[@]:-}" \
  -v "$PROJECT_DIR":/workspace \
  -v "$CONFIG_DIR/config-repo":/home/claude/.claude:ro \
  -v "$CONFIG_DIR/auth":/home/claude/.claude/auth \
  -v "$CONFIG_DIR/conversations":/home/claude/.claude/conversations \
  --cap-drop=ALL \
  --security-opt=no-new-privileges \
  --user "$(id -u):$(id -g)" \
  claude-sandbox "${@:2}"
```

---

## Usage

```bash
# Interactive session in current directory
claude-sandbox

# Specific project
claude-sandbox /path/to/project

# With initial prompt
claude-sandbox . "explain this codebase"

# One-shot command
claude-sandbox /path/to/project "fix the failing tests"
```

---

## Additional Network Controls (Optional)

For scenarios requiring tighter control over network access, you can restrict outbound connections to specific hosts:

```bash
docker run -it --rm \
  --dns=127.0.0.1 \
  --add-host="api.anthropic.com:$(dig +short api.anthropic.com | head -1)" \
  --add-host="europe-west1-aiplatform.googleapis.com:$(dig +short europe-west1-aiplatform.googleapis.com | head -1)" \
  ...
```

This allows API calls while limiting other outbound traffic. Note that this requires maintaining the allowlist as endpoints change.

---

## Quick Setup

```bash
# 1. Build image
docker build -t claude-sandbox .

# 2. Create directory structure
mkdir -p ~/.claude-docker/{config-repo,auth,conversations}

# 3. Create minimal config
echo "# My Claude Config" > ~/.claude-docker/config-repo/CLAUDE.md

# 4. Create secrets file
cat > ~/.claude-docker/secrets.env << 'EOF'
ANTHROPIC_API_KEY=sk-ant-...
EOF
chmod 600 ~/.claude-docker/secrets.env

# 5. Install run script
cp claude-sandbox.sh ~/bin/claude-sandbox
chmod +x ~/bin/claude-sandbox

# 6. Run
claude-sandbox /path/to/project
```

---

## opencode Setup

opencode is an alternative LLM coding agent. Key differences from Claude Code:

| Feature | Claude Code | opencode |
|---------|-------------|----------|
| Claude via Vertex | ✅ `CLAUDE_CODE_USE_VERTEX=1` | ❌ Not supported |
| Claude via API | ✅ `ANTHROPIC_API_KEY` | ✅ `ANTHROPIC_API_KEY` |
| Gemini via Vertex | ❌ | ✅ `GOOGLE_CLOUD_PROJECT` |
| Install method | npm | npm |

### Dockerfile.opencode

```dockerfile
FROM node:22-slim

RUN apt-get update && apt-get install -y git curl && rm -rf /var/lib/apt/lists/*
RUN npm install -g opencode-ai@latest

RUN useradd -m -s /bin/bash opencode
USER opencode

RUN mkdir -p /home/opencode/.config/opencode /home/opencode/.config/gcloud
WORKDIR /workspace

ENTRYPOINT ["opencode"]
```

Build:
```bash
docker build -t opencode-sandbox -f Dockerfile.opencode .
```

### opencode-sandbox.sh

```bash
#!/bin/bash
set -euo pipefail

PROJECT_DIR="${1:-$(pwd)}"

TTY_FLAG="-it"
if [[ ! -t 0 ]] || [[ "${*}" == *"run"* ]]; then
  TTY_FLAG=""
fi

docker run $TTY_FLAG --rm \
  --user "$(id -u):$(id -g)" \
  -e ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY:-}" \
  -e GOOGLE_CLOUD_PROJECT="${GOOGLE_CLOUD_PROJECT:-}" \
  -e VERTEX_LOCATION="${VERTEX_LOCATION:-europe-west1}" \
  -e GOOGLE_APPLICATION_CREDENTIALS="/home/opencode/.config/gcloud/application_default_credentials.json" \
  -v "$HOME/.config/gcloud":/home/opencode/.config/gcloud:ro \
  -v "$PROJECT_DIR":/workspace \
  --cap-drop=ALL \
  --security-opt=no-new-privileges \
  opencode-sandbox "${@:2}"
```

### Usage

```bash
# Interactive with Gemini (uses your Vertex credentials)
./opencode-sandbox.sh /path/to/project

# Non-interactive with specific model
./opencode-sandbox.sh . run --model google-vertex/gemini-2.5-flash "explain this code"

# With Anthropic API key (if you have one)
ANTHROPIC_API_KEY=sk-ant-... ./opencode-sandbox.sh . run --model anthropic/claude-sonnet-4-5 "hello"
```

### Available Models

```bash
# List Gemini models on Vertex
docker run --rm opencode-sandbox models google-vertex

# List Anthropic models (requires ANTHROPIC_API_KEY)
docker run --rm -e ANTHROPIC_API_KEY=sk-ant-... opencode-sandbox models anthropic
```
