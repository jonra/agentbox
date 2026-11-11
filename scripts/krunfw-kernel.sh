#!/bin/sh
# Builds the libkrunfw guest kernel bundle (kernel.c) from a libkrunfw repo and ref.
# Runs on Linux: inside a debian container (`make krunfw`) or on a CI runner.
# Usage: krunfw-kernel.sh REPO REF OUTDIR
set -eu
repo=$1 ref=$2 out=$3

if command -v apt-get >/dev/null && [ "$(id -u)" = 0 ]; then
  apt-get update -qq
  apt-get install -y -qq git curl xz-utils patch build-essential python3 python3-pyelftools bc kmod cpio \
    flex libncurses5-dev libelf-dev libssl-dev dwarves bison >/dev/null
fi

# Build outside any macOS mount: a kernel tree needs a case-sensitive filesystem.
work=$(mktemp -d)
git clone -q --depth 1 --branch "$ref" "$repo" "$work/libkrunfw"
make -C "$work/libkrunfw" -j"$(nproc)" kernel.c
mkdir -p "$out"
cp "$work/libkrunfw/kernel.c" "$out/kernel.c"
