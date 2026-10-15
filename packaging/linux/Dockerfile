# Build environment for the self-contained Linux package (cargo xtask dist)
# and for CI's Linux tests. Debian 13 keeps the glibc baseline at 2.41, so
# the package runs on distributions from 2025 on. Used by CI and locally:
#
#   docker build -t nox-build-linux -f packaging/linux/Dockerfile packaging
#   docker run --rm -v "$PWD:/src" -w /src -e CARGO_TARGET_DIR=/src/target/debian13 \
#       nox-build-linux cargo xtask dist
#
# FFmpeg is built with packaging/ffmpeg/build.sh, as everywhere: with
# Vulkan Video encoders, which Debian's FFmpeg 7.1 lacks, NVENC for
# drivers from 570 on, and without the many optional libraries Debian's
# links.
FROM debian:13

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential ca-certificates curl git pkg-config clang libclang-dev \
        nasm libgtk-4-dev librsvg2-common \
        libx264-dev libx265-dev libdav1d-dev libsvtav1enc-dev libvpx-dev \
        libjxl-dev libopus-dev libmp3lame-dev libvulkan-dev libva-dev \
        libasound2-dev glslang-tools glslc libxml2-utils patchelf \
        # For tests: the ffmpeg CLI makes test clips; lavapipe is a Vulkan
        # driver on the CPU, for runners without a GPU.
        ffmpeg mesa-vulkan-drivers \
    && rm -rf /var/lib/apt/lists/*

# Newer Vulkan headers than Debian's (1.4.309): FFmpeg's AV1 Vulkan
# encoder and VP9 Vulkan decoder need 1.4.317. Header-only, and
# /usr/local/include comes first.
RUN git clone --quiet --depth 1 --branch v1.4.341 \
        https://github.com/KhronosGroup/Vulkan-Headers.git /tmp/vulkan-headers \
    && cp -r /tmp/vulkan-headers/include/vulkan /tmp/vulkan-headers/include/vk_video /usr/local/include/ \
    && rm -rf /tmp/vulkan-headers

COPY ffmpeg/build.sh /tmp/build-ffmpeg.sh
RUN /tmp/build-ffmpeg.sh /usr/local && ldconfig && rm /tmp/build-ffmpeg.sh

ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable --component rustfmt,clippy \
    && chmod -R a+w /usr/local/cargo /usr/local/rustup
