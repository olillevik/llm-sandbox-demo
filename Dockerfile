FROM node:22-slim

RUN apt-get update && apt-get install -y git curl && rm -rf /var/lib/apt/lists/*
RUN npm install -g @anthropic-ai/claude-code

RUN useradd -m -s /bin/bash claude
USER claude

RUN mkdir -p /home/claude/.claude /home/claude/.config/gcloud
WORKDIR /workspace

ENTRYPOINT ["claude"]
