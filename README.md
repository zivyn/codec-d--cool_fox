# VIctory on NVIDIA

## What this fork changes, and how it gets its frame rate

Everything below is what `scripts\play-nvidia.ps1` turns on. Measured on an
RTX 5070 12 GB with a Ryzen 7 9800X3D, in the prologue:

| Setup | fps | GPU time a frame |
|---|---|---|
| The 7900 XTX fork's own settings, Series X | high, but thousands of draws a frame skipped (the picture jumps) | - |
| With the descriptor fix, Series X, full scale | 6.0 | 110 ms |
| The same, half render scale | 7.2 | 82-88 ms |
| Series S, half scale, own upscaler, ray walks skipped | 11.0 | 33 ms |
| Plus memory compares on the card | 21.8 | about 6 ms |

### Settings that lower the picture a bit

1. **Series S profile** (`--console series-s`). The title believes it runs on
   the smaller console and picks that console's settings. The biggest change
   is resolution: the game image is 1920x1080 instead of the Series X's
   4K-class target. Menus, the HUD and subtitles are still drawn at 4K on a
   separate plane, so they stay sharp.
2. **Half render scale** (`VICTORY_RENDER_SCALE=0.5`). A patch to the title's
   1.0.2.1 executable pins its internal render scale: the 3D world is drawn at
   half width and half height (960x540 on Series S), a quarter of the pixels,
   then stretched back to 1080p. This is why the world is softer than the menus.
3. **The title's own upscaler** (`VICTORY_OWN_UPSCALER=1`). On Series S the
   title upscales with AMD FidelityFX (FSR), whose compute programs cannot be
   translated here, so that step drew nothing. A patch makes the title use its
   own temporal upscaler on every profile. It is not a quality setting in
   itself, but Series S needs it for a scale below 1.
4. **Ray walks end at the root** (`VICTORY_BVH_MISS=1`). The part of the title
   that builds its ray-tracing structures is off, so their nodes are stale.
   Instead of walking stale nodes, every box and triangle test is a miss. That
   avoids a hang and saves about 10 ms a frame, but light bounced off nearby
   surfaces is lost, so some dim rooms look darker.

### Settings that make it faster without changing the picture

5. **Memory compares on the card** (`VICTORY_CARD_BUFFERS_MIN_MB=1`,
   `VICTORY_CARD_BUFFERS_MB=1536`). Once the settings above had cut the GPU's
   work, the CPU was the limit: every frame it compared about 190 MB of title
   memory to find what the title had changed. These move the compares of copies
   of 1 MiB or more to the graphics card. Copying back fell from about 27 to
   10 ms a frame, and the frame rate doubled (11 to 21.8 fps).
6. **Wait for new shaders** (`VICTORY_PIPELINE_WAIT_MS=10000`). Draws wait for
   a pipeline still being made instead of being skipped: nothing is missing,
   with a short hitch the first time a place is seen.
7. **NVIDIA Smooth Motion** (by default in `play-nvidia.ps1`). The script
   starts a copy of the build named `victory_sm.exe`; with Smooth Motion turned
   on for that exe in the NVIDIA App, the driver inserts a generated frame
   between real ones, so motion looks about twice as smooth. The title still
   runs at the real rate underneath, so controls respond at about 20 fps.

### Code changes (what makes it work on NVIDIA at all)

8. **No more skipped draws on NVIDIA** (`VICTORY_DYNAMIC_SLOTS=524288`,
   `crates/victory-gpu/src/descriptor/host.rs`). NVIDIA caps the shader-visible
   view heap at 1,000,000 views, below the 1,245,184 the title asks for. The
   pool of dynamic descriptor slots (2^18) then ran out, and 1,100-4,800
   descriptor tables a frame were refused, each one a dropped draw: the screen
   rarely updated and the character jumped from place to place. The pool can
   now take up to half the heap: 0-33 refused a frame.
9. **Series S interface plane** (`VICTORY_COMPOSE=1`,
   `crates/victory-gpu/src/compose.rs`, `host.rs`, `queue.rs`). A Series S
   title presents two planes: the game (1080p) and the interface (4K). Only the
   first was shown, so menus, loading screens and subtitles were black until
   gameplay. One compute pass now lays the interface over the game before it
   is shown. Ported from fxceless/victory-fork pull request 1189 (Tissma95),
   without its FSR stand-in (the own upscaler is used instead).
10. **Title patches** (`crates/victory-loader/src/hooks.rs`, `lib.rs`,
    `windows.rs`): render scale, own upscaler, and `VICTORY_NO_WATCHDOG`
    (keeps the title's watchdog from ending a slow frame), taken from the
    victory-perf patches.
11. **Ray walks end at the root** (`crates/victory-shader/src/emit/intersect.rs`,
    `crates/victory-gpu/src/shader_cache.rs`): the `VICTORY_BVH_MISS` switch, made
    part of the shader cache key so translated shaders with and without it are
    kept apart.
12. **`scripts/play-nvidia.ps1`**: the launcher with all of the above.

---

A fork of [avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX),
itself a fork of [VIctory](https://github.com/Marvisak/VIctory), a compatibility
layer that runs an Xbox Series title natively on Windows from files the user
supplies. That fork is tuned for an AMD RX 7900 XTX. This one makes it **draw
everything on NVIDIA cards and run fastest there**, using the Series S profile.

**This repository contains no game files, code or assets.** You need your own
copy of the game. It is not affiliated with or endorsed by the game's developer
or publisher.

## Where it stands (2026-11-25)

Tested on one machine only: Windows 11, AMD Ryzen 7 9800X3D, 64 GB RAM,
GeForce RTX 5070 12 GB, the Series S profile at half render scale
(`scripts\play-nvidia.ps1`).

| | |
|---|---|
| Prologue, indoors | about 20-22 fps on average, 24-31 fps in lighter places |
| Device losses | none in the measured runs |
| Menus, loading screens, subtitles | shown (they were black on Series S before) |
| First time a place is seen | slow for a moment while its shaders are made |

On the same card the fork's own settings skipped thousands of draws a frame
(the picture jumped and rarely updated), and the Series X profile at full scale
ran at about 6 fps.

What is known to be wrong:

- The picture is softer than Series X: half render scale, upscaled by the
  title's own upscaler.
- Rooms with little direct light can be dark: ray walks end at the root while
  the title's structure builder is off (`VICTORY_BVH_MISS`).
- Everything listed in the 7900 XTX fork's README still applies (flicker of far
  plants outdoors, textures appearing late while the title streams, a stutter on
  the first run with a new driver or build).

## How to run it

You need Windows 10/11 x64, a CPU with AVX2, a Direct3D 12 GPU with shader
model 6.6 and mesh shaders, Rust (the toolchain is pinned), the MSVC build
tools, and your own dump of the title.

```powershell
git clone https://github.com/trustno1-coder/VIctory-NVIDIA
cd VIctory-NVIDIA
copy victory.example.toml victory.toml      # then set your paths, outside this folder
cargo build --locked --profile diag -p victory --features diagnostics
scripts\play-nvidia.ps1 -Exe <path of your copy's executable>
```

`scripts\play-nvidia.ps1` starts a copy of the build named `victory_sm.exe`, so
NVIDIA Smooth Motion applies every time once it is turned on for that exe in the
NVIDIA App. `-NoSmoothMotion` runs `victory.exe` itself; `-SeriesX` uses the
Series X profile (sharper, much slower). The fork's own `scripts\play.ps1` is
still there for AMD cards.

Before the first run, ahead of time (both can be stopped and go on later):

```powershell
target\diag\victory.exe shaders <the shader archives>   # every shader translated, minutes
target\diag\victory.exe warm --used                     # pipelines earlier runs drew with, for a new driver or build
```

Commands, debugging switches and upstream's own description:
[docs/README-fxceless.md](docs/README-fxceless.md). Every `VICTORY_...` switch is
described in a comment where it is read.

## What is different here

This fork's own changes are listed at the top of this README. Everything the
7900 XTX fork added is kept: translation of list calls on threads
of ours, submissions prepared and joined on a thread of ours, the write relay,
fills done on the card, textures on their own copy queue, texture read-back
(boats float), and frame record and replay. Its
[docs/findings.md](docs/findings.md) has the reasons and the measurements.

## Credits

This is a tree of other people's work with more on top. The history is kept, and
commits taken from elsewhere name the commit and its author.

- **[avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX)**:
  the fork this one started from, and everything in it.
- **[VIctory](https://github.com/Marvisak/VIctory)** by Marvisak and its
  contributors: the loader, the runtime services, the Direct3D 12 back end and
  the shader recompiler.
- **[fxceless/victory-fork](https://github.com/fxceless/victory-fork)**: the fork
  the 7900 XTX one started from, the people whose pull requests it carries, and
  pull request 1189 (Tissma95) for the Series S interface plane.
- **[DexNom/VIctory](https://github.com/DexNom/VIctory)**: ray tracing fixes,
  cube gradients, sparse textures.
- **[Belobug/VIctory-Belobug-AMD](https://github.com/Belobug/VIctory-Belobug-AMD)**:
  the ambient light idea, pipeline workers, and work by romainhedouin.
- **[FQY55kai/ZJOU-VICTORY](https://github.com/FQY55kai/ZJOU-VICTORY)**.
- **[myokex/victory-amd](https://github.com/myokex/victory-amd)**: the guard over
  every view of a page (with Spexivus, joroyt and tightfall), and Joshua Odom's
  reads into title memory.
- **Geckomacabre** and **cxy0072019-sketch**: the texture read-back.
- **[XCL](https://github.com/ivall/xcl)** (GPL-2.0-only) was read for how the same
  title behaves under another layer; no code of it is here.
- This fork's changes were made with [Claude Code](https://claude.com/claude-code).

## License

GPL-3.0-or-later, as VIctory. See [LICENSE](LICENSE); the one file that is not
covered is named in [docs/asset-policy.md](docs/asset-policy.md).
