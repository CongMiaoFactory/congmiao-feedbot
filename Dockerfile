FROM rust:1.90-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY migrations ./migrations
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates ffmpeg python3 python3-pip \
    && pip3 install --break-system-packages --no-cache-dir yt-dlp \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/congmiao-feedbot /usr/local/bin/congmiao-feedbot
RUN mkdir -p /app/data /app/tmp
ENV DATABASE_URL=sqlite://data/feedbot.db?mode=rwc TEMP_DIR=/app/tmp RUST_LOG=congmiao_feedbot=info
VOLUME ["/app/data"]
ENTRYPOINT ["congmiao-feedbot"]

