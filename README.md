# termcart

Game Boy, Game Boy Color, Game Boy Advance and Nintendo 64 in the terminal. The frontend is Rust;
emulation is the mGBA libretro core for the handhelds and ParaLLEl N64, with the Angrylion
software renderer, for N64. In Ghostty or kitty, frames go through the kitty graphics
protocol over POSIX shared memory, about 88 bytes per frame on the pty. Other terminals get
half-block characters.

On an M2, a GBA game takes about 5% of one core and 15 MB of RAM.

## Build

```sh
./scripts/build-cores.sh        # builds cores/mgba_libretro.dylib from mGBA 0.10.5
cargo build --release
./target/release/termcart path/to/game.gba
```

For N64, also run `./scripts/build-n64.sh`. N64 costs far more CPU than the handhelds; see
`docs/n64.md`.

## Keys

| Key | Button |
|---|---|
| Arrows or WASD | D-pad |
| X or K | A |
| Z or J | B |
| Q or U | L |
| E or I | R |
| Enter | Start |
| Backspace or Right Shift | Select |
| Tab | Fast forward, 4x |
| Esc or Ctrl+C | Quit |

N64 games use another layout:

| Key | Button |
|---|---|
| WASD | Analog stick, Left Shift held walks at 40% |
| Arrows | C buttons |
| K | A |
| J | B |
| Space | Z |
| Q / E | L / R |
| Enter | Start |
| T F G H | D-pad up, left, down, right |

Holding a key needs key release events, which Ghostty, kitty and WezTerm send through the kitty
keyboard protocol. Elsewhere a key counts as held for 130 ms after its last press or repeat.

## Saves

The save goes next to the ROM as `<name>.sav`. termcart writes it every 10 seconds when it
changed, and again on exit.

## Options

- `--core <path>` loads a specific libretro core.
- `--video auto|kitty|half` picks the renderer.
- `TERMCART_KITTY=<shm|file|direct|zlib>[,24|,32]` picks how kitty frames are sent. Over SSH
  the default is `zlib,24`.
- `TERMCART_KITTY_SCALE=auto|N` enlarges frames before sending them, because Ghostty smooths
  images it enlarges itself. The default is `auto`, which looks sharper and costs about 3x the
  draw time, still under 1% of a core. `TERMCART_KITTY_SCALE=1` turns it off.
- `TERMCART_LOG=stderr|<file>` turns on the core's log.
- `TERMCART_OPT_<key>=<value>` overrides a core option, e.g.
  `TERMCART_OPT_parallel_n64_angrylion_multithread=8`.

Bring your own ROMs, dumped from cartridges you own.
