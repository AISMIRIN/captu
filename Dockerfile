# --- stage 1: build ffmpeg with libaribcaption (ARIB B24 decoder) ---
# Recipe mirrors 8d9bd30:Dockerfile.ffmpeg (the Phase 0-1 verified build).
# debian:bookworm-slim + gcc so ABI matches the runtime stage exactly.
FROM debian:bookworm-slim AS ffmpeg-builder
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y \
    build-essential cmake git pkg-config yasm nasm \
    libssl-dev zlib1g-dev \
    libx264-dev libx265-dev libvpx-dev \
    libass-dev libfreetype6-dev \
    && rm -rf /var/lib/apt/lists/*

RUN git clone --depth 1 https://github.com/xqq/libaribcaption.git /build/libaribcaption
WORKDIR /build/libaribcaption
RUN cmake -B build \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX=/usr/local \
    -DARIBCAPTION_SHARED=ON \
    && cmake --build build -j$(nproc) \
    && cmake --install build

RUN git clone --depth 1 --branch n7.1 https://github.com/FFmpeg/FFmpeg.git /build/ffmpeg
WORKDIR /build/ffmpeg
RUN ./configure \
    --prefix=/usr/local \
    --enable-gpl \
    --enable-libx264 \
    --enable-libx265 \
    --enable-libvpx \
    --enable-libass \
    --enable-libaribcaption \
    && make -j2 \
    && make install

# --- stage 2: build Rust app (glibc / debian bookworm, same libc as runtime) ---
FROM rust:1-slim-bookworm AS builder
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y \
    ca-certificates pkg-config libssl-dev \
    cmake clang libclang-dev g++ \
    libfreetype6-dev libfontconfig1-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
# Copy only source files needed for the build.
# config.toml is intentionally excluded (see .dockerignore) so that editing it
# does not invalidate the cargo build cache layer.
COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/
# crates/ includes aribcaption-sys/vendor/libaribcaption (C source, submodule).
# Do not drop crates/ from this list — it is needed at cmake build time.
COPY crates/ ./crates/
COPY migrations/ ./migrations/
COPY templates/ ./templates/
COPY static/ ./static/
RUN cargo build --release --bin captu

# --- stage 3: runtime ---
# debian:bookworm-slim — same base as ffmpeg-builder so shared libs (libass, libx264 …)
# are binary-compatible. No ABI mismatch.
FROM debian:bookworm-slim
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libx264-164 libx265-199 libvpx7 \
    libass9 libfreetype6 \
    fontconfig \
    && rm -rf /var/lib/apt/lists/*

# ffmpeg, ffprobe, libaribcaption — all required at runtime
COPY --from=ffmpeg-builder /usr/local/bin/ffmpeg  /usr/local/bin/ffmpeg
COPY --from=ffmpeg-builder /usr/local/bin/ffprobe /usr/local/bin/ffprobe
COPY --from=ffmpeg-builder /usr/local/lib/libaribcaption* /usr/local/lib/
RUN ldconfig

# Rounded M+ 1m for ARIB (custom Rounded M+ license) — vendored for ARIB subtitle burn-in via libass.
# fontconfig alias maps sans-serif → Rounded M+ 1m for ARIB so the ass= filter
# renders with the ARIB-optimised rounded gothic typeface.
COPY assets/fonts/rounded-mplus-1m-arib.ttf /usr/local/share/fonts/
COPY assets/fonts/99-captu-fonts.conf /etc/fonts/conf.d/
RUN fc-cache -f

COPY --from=builder /app/target/release/captu /usr/local/bin/captu
# static/ is served relative to WORKDIR via ServeDir::new("static")
COPY static/ /app/static/

WORKDIR /app
ENV TZ=Asia/Tokyo
CMD ["/usr/local/bin/captu"]
