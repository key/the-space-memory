FROM rust:1-bookworm AS builder
WORKDIR /app
# cmake and clang are required by aws-lc-sys (transitive dep via rustls) on arm64
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake clang \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY tests/ tests/
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends libssl3 ca-certificates \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/tsm /usr/local/bin/tsm
COPY --from=builder /app/target/release/tsmd /usr/local/bin/tsmd
ENTRYPOINT ["tsm"]
