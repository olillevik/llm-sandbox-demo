FROM rust:1.86-slim-bookworm AS llm-box-builder

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM node:22-slim

LABEL io.github.llm-box.egress-broker="1" \
      io.github.llm-box.copilot-args-compatible="1"

RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates curl git openssh-client \
  && rm -rf /var/lib/apt/lists/*

RUN npm install -g @github/copilot
COPY --from=llm-box-builder /src/target/release/llm-box /usr/local/bin/llm-box

RUN useradd -m -s /bin/bash copilot
USER copilot

ENV HOME=/home/copilot
RUN mkdir -p /home/copilot/.copilot /home/copilot/.local/state/llm-box
WORKDIR /workspace

ENTRYPOINT ["copilot"]
