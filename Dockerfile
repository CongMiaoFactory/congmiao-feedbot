# syntax=docker/dockerfile:1
#
# Runtime tools are bundled: a minimal ffmpeg/ffprobe built from source,
# yt-dlp (standalone binary) and QuickJS-ng (yt-dlp needs a JavaScript runtime
# for YouTube; QuickJS is far smaller than Deno, but slower). Downloads are
# pinned and checked against SHA-256 sums; to update a tool, change its version
# and every sum of it.

ARG RUST_VERSION=1.99
ARG DEBIAN_RELEASE=trixie

FROM rust:${RUST_VERSION}-${DEBIAN_RELEASE} AS builder
WORKDIR /app
# Only what the compile reads (migrations are embedded by sqlx::migrate!), so
# edits to docs or config do not invalidate this stage.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates crates
COPY src src
COPY migrations migrations
# rust-toolchain.toml asks for "stable"; build with the image's toolchain
# instead of downloading another one.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    RUSTUP_TOOLCHAIN="$RUST_VERSION" cargo build --release --locked -p feedbot \
    && cp target/release/feedbot /usr/local/bin/feedbot

FROM debian:${DEBIAN_RELEASE}-slim AS ffmpeg
# Release tarball from ffmpeg.org; the sum was taken after checking the
# release signature (key FCF9 86EA 15E6 E293 A564 4F10 B432 2F04 D676 58D8).
ARG FFMPEG_VERSION=9.0.2
ARG FFMPEG_SHA256=8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       build-essential ca-certificates curl nasm pkg-config xz-utils \
       libdav1d-dev libx264-dev zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
RUN curl -fsSL -o ffmpeg.tar.xz "https://ffmpeg.org/releases/ffmpeg-${FFMPEG_VERSION}.tar.xz" \
    && echo "${FFMPEG_SHA256}  ffmpeg.tar.xz" | sha256sum -c - \
    && tar -xf ffmpeg.tar.xz --strip-components=1 \
    && rm ffmpeg.tar.xz
# Only what the bot and yt-dlp use:
# - crates/media/src/ffmpeg.rs: remux (-c copy to mp4), ugoira (concat of
#   jpg/png frames -> libx264), transcode (libx264 + aac, scale), thumbnail
#   (mjpeg), probe (ffprobe JSON);
# - yt-dlp: merging video+audio into mp4 and extracting m4a audio.
# Decoders cover the codecs these sites serve (AV1 through libdav1d), so
# transcoding works whatever the source. Add components here when the code
# starts using new ones.
RUN ./configure \
      --prefix=/opt/ffmpeg \
      --disable-everything --disable-autodetect --disable-network \
      --disable-doc --disable-debug --disable-ffplay \
      --enable-gpl --enable-libx264 --enable-libdav1d --enable-zlib \
      --enable-protocol=file,pipe \
      --enable-demuxer=mov,matroska,flv,mpegts,mp3,aac,ogg,wav,concat,image2,image_jpeg_pipe,image_png_pipe,image_webp_pipe,gif \
      --enable-muxer=mp4,mov,ipod,matroska,webm,adts,image2,mjpeg,null \
      --enable-decoder=h264,hevc,vp8,vp9,libdav1d,aac,aac_latm,mp3,mp3float,opus,vorbis,flac,pcm_s16le,mjpeg,png,webp,gif \
      --enable-encoder=libx264,aac,mjpeg \
      --enable-parser=h264,hevc,vp8,vp9,av1,aac,aac_latm,mpegaudio,opus,vorbis,flac,mjpeg,png,webp,gif \
      --enable-bsfs \
      --enable-filter=scale,format,null,anull,aformat,aresample,copy,acopy \
    && make -j"$(nproc)" \
    && make install

FROM debian:${DEBIAN_RELEASE}-slim AS tools
ARG TARGETARCH
ARG YTDLP_VERSION=2026.10.26
ARG YTDLP_SHA256_AMD64=767ba9dce7052aa196f8b12652f433d76de128ba168966fa84b6394d5e6c6795
ARG YTDLP_SHA256_ARM64=e87e9533e89fe5889abaa21f5114dfe7756d6aa6219b83551add04d48b18e483
# Static builds from github.com/quickjs-ng/quickjs releases.
ARG QJS_VERSION=0.18.0
ARG QJS_SHA256_AMD64=85ded23b9fabe8c1644fc40a8d2bbdd71157affcbd807359c618cc717ee24d7d
ARG QJS_SHA256_ARM64=2c005c7171424a598d31c53c8dc4a0ae2e4958eb6fbb37908c885a307c78187c
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /tools
RUN set -eu; \
    case "${TARGETARCH:-amd64}" in \
      amd64) ytdlp=yt-dlp_linux; ytdlp_sum="$YTDLP_SHA256_AMD64"; \
             qjs=qjs-linux-x86_64; qjs_sum="$QJS_SHA256_AMD64" ;; \
      arm64) ytdlp=yt-dlp_linux_aarch64; ytdlp_sum="$YTDLP_SHA256_ARM64"; \
             qjs=qjs-linux-aarch64; qjs_sum="$QJS_SHA256_ARM64" ;; \
      *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o yt-dlp "https://github.com/yt-dlp/yt-dlp/releases/download/${YTDLP_VERSION}/${ytdlp}"; \
    echo "${ytdlp_sum}  yt-dlp" | sha256sum -c -; \
    curl -fsSL -o qjs "https://github.com/quickjs-ng/quickjs/releases/download/v${QJS_VERSION}/${qjs}"; \
    echo "${qjs_sum}  qjs" | sha256sum -c -; \
    chmod 0755 yt-dlp qjs

FROM debian:${DEBIAN_RELEASE}-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libdav1d7 libx264-164 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home --home-dir /home/feedbot feedbot \
    && mkdir -p /data \
    && chown feedbot:feedbot /data
COPY --from=ffmpeg /opt/ffmpeg/bin/ffmpeg /opt/ffmpeg/bin/ffprobe /usr/local/bin/
COPY --from=tools /tools/yt-dlp /tools/qjs /usr/local/bin/
COPY --from=builder /usr/local/bin/feedbot /usr/local/bin/feedbot
WORKDIR /app
COPY config /app/config
# The database and the generated master.key live in /data; see README on
# passing FEEDBOT__SECRETS__MASTER_KEY instead of keeping the key there.
ENV FEEDBOT__DATABASE__PATH=/data/feedbot.db \
    FEEDBOT__YTDLP__JS_RUNTIME=quickjs
VOLUME ["/data"]
# Webhook mode listens on telegram.webhook.addr (default 0.0.0.0:8080).
EXPOSE 8080
USER feedbot
ENTRYPOINT ["feedbot"]
