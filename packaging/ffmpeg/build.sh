#!/bin/sh
# Builds the FFmpeg Nox ships, the same everywhere (the Flatpak manifest
# mirrors these options): FFmpeg 9.0.2 with x264, x265, dav1d and the
# platform's hardware APIs, NVENC for drivers from 570 on (nv-codec-headers
# 13.0; FFmpeg's newest headers would need 610).
#
#   packaging/ffmpeg/build.sh PREFIX
#
# Needs a C compiler, make, nasm, pkg-config and git, with x264, x265 and
# dav1d (and on Linux libva, Vulkan headers) where pkg-config finds them.
# Used by the Linux build image, CI's Windows (MSYS2) and macOS jobs.
set -eu

PREFIX=$1
FFMPEG=9.0.2
FFMPEG_SHA256=8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e
NV_CODEC_HEADERS=n13.0.19.1

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"

options="--prefix=$PREFIX --enable-shared --disable-static --disable-programs
    --disable-doc --enable-gpl --enable-libx264 --enable-libx265 --enable-libdav1d"
case "$(uname -s)" in
    Linux)
        options="$options --enable-vulkan --enable-vaapi"
        with_nvenc=yes ;;
    MINGW*|MSYS*|CYGWIN*)
        options="$options --enable-vulkan --enable-d3d11va --enable-d3d12va"
        # NVENC exists for x86-64 only.
        [ "$(uname -m)" = x86_64 ] && with_nvenc=yes || with_nvenc=no ;;
    Darwin)
        options="$options --enable-videotoolbox"
        with_nvenc=no ;;
esac

if [ "$with_nvenc" = yes ]; then
    git clone --quiet --depth 1 --branch "$NV_CODEC_HEADERS" \
        https://github.com/FFmpeg/nv-codec-headers.git nv
    make -C nv install PREFIX="$PREFIX"
    export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
    options="$options --enable-ffnvcodec --enable-nvenc --enable-nvdec"
else
    options="$options --disable-ffnvcodec"
fi

curl -sSfL -o ffmpeg.tar.xz "https://ffmpeg.org/releases/ffmpeg-$FFMPEG.tar.xz"
if command -v sha256sum >/dev/null; then
    echo "$FFMPEG_SHA256  ffmpeg.tar.xz" | sha256sum -c -
else
    echo "$FFMPEG_SHA256  ffmpeg.tar.xz" | shasum -a 256 -c -
fi
tar xJf ffmpeg.tar.xz
cd "ffmpeg-$FFMPEG"
# shellcheck disable=SC2086 # options are words
./configure $options
make -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu)"
make install
