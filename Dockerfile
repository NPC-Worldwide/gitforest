# Build stage
FROM rust:latest AS builder

RUN apt-get update && apt-get install -y \
    git \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/gitforest
COPY . .
RUN cargo build --release

# Runtime stage
FROM debian:trixie-slim

RUN apt-get update && apt-get install -y \
    git \
    curl \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/src/gitforest/target/release/gitforest /usr/local/bin/gitforest
COPY --from=builder /usr/src/gitforest/target/release/git-remote-gitforest /usr/local/bin/git-remote-gitforest
COPY --from=builder /usr/src/gitforest/target/release/gtfo /usr/local/bin/gtfo

ENV PATH="/usr/local/bin:${PATH}"

RUN mkdir -p /root/.config/gitforest /root/.local/share/gitforest /root/repos /root/clones

WORKDIR /root

COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
COPY docker/run-test.sh /usr/local/bin/run-test.sh
RUN chmod +x /usr/local/bin/entrypoint.sh /usr/local/bin/run-test.sh

HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD curl -fsS http://127.0.0.1:7878/api/status >/dev/null || exit 1

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
