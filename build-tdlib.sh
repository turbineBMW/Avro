#!/usr/bin/env bash
# Build Telegram's TDLib JSON interface (libtdjson.so) into ~/.local/lib.
# Needs: cmake gperf clang ninja zlib openssl (pacman -S --needed cmake gperf clang ninja).
# Avro dlopens the library at runtime, so `cargo build` works without it.
set -euo pipefail
SRC="${TDLIB_SRC:-$HOME/.cache/avro/tdlib}"
PREFIX="${TDLIB_PREFIX:-$HOME/.local}"
JOBS="${JOBS:-6}"   # td_api.cpp needs ~2 GB per job
if [ -d "$SRC/.git" ]; then git -C "$SRC" pull --ff-only; else git clone --depth 1 https://github.com/tdlib/td.git "$SRC"; fi
cmake -S "$SRC" -B "$SRC/build" -G Ninja -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_C_COMPILER=clang -DCMAKE_CXX_COMPILER=clang++ -DCMAKE_INSTALL_PREFIX="$PREFIX"
cmake --build "$SRC/build" --target tdjson -j "$JOBS"
install -d "$PREFIX/lib"
cp -P "$SRC"/build/libtdjson.so* "$PREFIX/lib/"
echo "installed: $(readlink -f "$PREFIX/lib/libtdjson.so") (td $(git -C "$SRC" rev-parse --short HEAD))"
