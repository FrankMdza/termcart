#!/usr/bin/env bash
# Builds the libretro cores termcart loads at runtime into ./cores.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
src="$root/.cores-src"
out="$root/cores"
mkdir -p "$src" "$out"

MGBA_TAG="${MGBA_TAG:-0.10.5}"
if [ ! -d "$src/mgba" ]; then
  git clone --depth 1 --branch "$MGBA_TAG" https://github.com/mgba-emu/mgba.git "$src/mgba"
fi
cmake -S "$src/mgba" -B "$src/mgba/build-libretro" \
  -DCMAKE_BUILD_TYPE=Release -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
  -DBUILD_LIBRETRO=ON -DBUILD_QT=OFF -DBUILD_SDL=OFF -DBUILD_GL=OFF -DBUILD_GLES2=OFF -DBUILD_GLES3=OFF \
  -DUSE_FFMPEG=OFF -DUSE_LIBZIP=OFF -DUSE_MINIZIP=OFF -DUSE_EDITLINE=OFF -DUSE_LUA=OFF -DUSE_DISCORD_RPC=OFF \
  -DSKIP_LIBRARY=ON -DBUILD_SHARED=OFF -DBUILD_STATIC=OFF
cmake --build "$src/mgba/build-libretro" --target mgba_libretro -j "$(sysctl -n hw.ncpu 2>/dev/null || nproc)"
cp "$src/mgba/build-libretro/mgba_libretro".* "$out/"
echo "built: $(ls "$out")"
