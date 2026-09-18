#!/bin/sh
# Install whatever Owl Player's build needs and this machine lacks.
#
# `imlazy build` and `imlazy install` (and `make`) run this first, so a
# fresh Raven machine needs nothing but the one command. On a machine that
# already has everything it checks and says so; nothing is installed.
#
# What the build needs, and why each is not obvious:
#
#   gtk4, libadwaita  the toolkit. The image carries the libraries; the build
#                     also needs their .pc files and headers.
#   ffmpeg            ffmpeg-sys-next finds FFmpeg through pkg-config, so the
#                     shared libraries alone -- which the image ships, copied
#                     beside Owl Player -- are not enough: it wants the
#                     headers and .pc files the ffmpeg package brings.
#   clang             ffmpeg-sys-next reads those headers with bindgen, which
#                     dlopens libclang.so. Raven ships llvm-libs, not clang.
#   lld               .cargo/config.toml links with -fuse-ld=lld.
#
# Installed system-wide with `rvn`, which as a member of wheel goes through
# rvnd without a password prompt. Files the image copied in that no package
# owns (the FFmpeg libraries, for one) are taken over by the package, which
# puts them under package management too.
set -eu

missing=""
need() { missing="${missing} $1"; }

have_pc() { pkg-config --exists "$@" 2>/dev/null; }

have_libclang() {
    # Where clang-sys looks, plus the per-user rvn prefix .cargo/config.toml
    # points LIBCLANG_PATH at.
    for dir in "${LIBCLANG_PATH:-}" "${HOME}/.local/share/rvn/root/usr/lib" \
               /usr/lib /usr/lib64 /usr/local/lib /usr/lib/llvm*/lib; do
        [ -n "$dir" ] || continue
        for lib in "$dir"/libclang.so "$dir"/libclang.so.*; do
            [ -e "$lib" ] && return 0
        done
    done
    return 1
}

command -v pkg-config >/dev/null 2>&1 || need pkgconf
if command -v pkg-config >/dev/null 2>&1; then
    have_pc "gtk4 >= 4.12" || need gtk4
    have_pc "libadwaita-1 >= 1.5" || need libadwaita
    have_pc libavcodec libavdevice libavfilter libavformat libavutil \
        libswresample libswscale || need ffmpeg
else
    need gtk4
    need libadwaita
    need ffmpeg
fi
have_libclang || need clang
command -v ld.lld >/dev/null 2>&1 || need lld

if [ -z "${missing}" ]; then
    echo "deps: everything Owl Player's build needs is installed"
    exit 0
fi

echo "deps: installing${missing}"
if ! command -v rvn >/dev/null 2>&1; then
    echo "deps: rvn is not available; install these with your package manager:${missing}" >&2
    exit 1
fi
# shellcheck disable=SC2086 # word splitting is the point: one name each
rvn install --repo-only -y ${missing}
