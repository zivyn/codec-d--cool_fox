# VIctory on NVIDIA

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

- **No more skipped draws on NVIDIA** (`VICTORY_DYNAMIC_SLOTS=524288`): NVIDIA
  caps the shader-visible view heap at 1,000,000 views, below the 1,245,184 the
  title asks for. The pool of dynamic descriptor slots then ran out and
  1,100-4,800 descriptor tables a frame were refused, each one a dropped draw.
  The pool can now take up to half the heap: 0-33 refused a frame.
- **Series S interface plane** (`VICTORY_COMPOSE=1`): a Series S title presents
  the game (1080p) and the interface (4K) as two planes. Only the first was
  shown, so menus, loading screens and subtitles were black. The interface is
  now laid over the game. Ported from fxceless/victory-fork pull request 1189,
  without its FSR stand-in.
- **Render scale and upscaler** (`VICTORY_RENDER_SCALE`, `VICTORY_OWN_UPSCALER`,
  `VICTORY_NO_WATCHDOG`): patches to the 1.0.2.1 executable, from the
  victory-perf patches. Series S needs the title's own upscaler at a scale
  below 1.
- **Ray walks end at the root** (`VICTORY_BVH_MISS=1`) while the structure
  builder is off. Its nodes are stale, so this avoids a hang and saves about
  10 ms a frame.
- **Memory compares on the card** (`VICTORY_CARD_BUFFERS_MIN_MB=1`,
  `VICTORY_CARD_BUFFERS_MB=1536`): this took the Series S profile from 11 to
  about 22 fps.

Everything the 7900 XTX fork added is kept: translation of list calls on threads
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
