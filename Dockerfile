FROM rust:1.86-bookworm AS builder
WORKDIR /app
COPY . .
RUN cargo build --release -p feedbot

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/feedbot /usr/local/bin/feedbot
COPY config /app/config
ENV FEEDBOT__DATABASE__PATH=/data/feedbot.db
VOLUME ["/data"]
ENTRYPOINT ["feedbot"]
