FROM node:22-slim

RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates curl git openssh-client \
  && rm -rf /var/lib/apt/lists/*

RUN npm install -g @github/copilot

RUN useradd -m -s /bin/bash copilot
USER copilot

ENV HOME=/home/copilot
RUN mkdir -p /home/copilot/.copilot /home/copilot/.local/state/copilot-box
WORKDIR /workspace

ENTRYPOINT ["copilot"]
