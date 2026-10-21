#!/usr/bin/env bash
# Builds the ParaLLEl N64 libretro core into ./cores with only the software
# renderer (angrylion) and the aarch64 dynarec. No OpenGL, no Vulkan, so the
# core hands RGB frames to retro_video_refresh like mGBA does.
# See docs/n64.md for why these flags.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
src="$root/.cores-src"
out="$root/cores"
mkdir -p "$src" "$out"

PN64_REF="${PN64_REF:-6f23339}"
if [ ! -d "$src/parallel-n64" ]; then
  git clone https://github.com/libretro/parallel-n64.git "$src/parallel-n64"
fi
git -C "$src/parallel-n64" checkout --quiet "$PN64_REF"

# The core prints a debug line to stdout at load, which is termcart's screen.
sed -i.bak '/printf("plugin_start_gfx\\n");/d' "$src/parallel-n64/mupen64plus-core/src/plugin/plugin.c"

# Without GL, libretro.c still includes glsm.h, which pulls in CoreFoundation
# after r4300.h has defined hi and lo as macros, and MacTypes.h stops compiling.
patch="$root/scripts/patches/parallel-n64-macos-corefoundation.patch"
if ! git -C "$src/parallel-n64" apply --reverse --check "$patch" 2>/dev/null; then
  git -C "$src/parallel-n64" apply "$patch"
fi

# The project is plain make. CMake is not involved, so CMAKE_POLICY_VERSION_MINIMUM
# does not apply here.
# HAVE_OPENGL=0   drops GLideN64, Glide64, gln64 and Rice (all need a GL context).
# HAVE_PARALLEL=0 drops ParaLLEl-RDP (needs a Vulkan context).
# HAVE_PARALLEL_RSP=0 drops the RSP JIT, whose Lightning backend targets x86_64.
# HAVE_THR_AL=1   keeps angrylion, the software RDP.
make -C "$src/parallel-n64" -j "$(sysctl -n hw.ncpu 2>/dev/null || nproc)" \
  platform=osx \
  HAVE_OPENGL=0 HAVE_PARALLEL=0 HAVE_PARALLEL_RSP=0 HAVE_THR_AL=1
cp "$src/parallel-n64/parallel_n64_libretro.dylib" "$out/"
echo "built: $(ls "$out")"
