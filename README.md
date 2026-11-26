# VIctory for an 8 GB NVIDIA card

A fork of [VIctory](https://github.com/Marvisak/VIctory), a compatibility layer
that runs an Xbox Series title natively on Windows from files the user
supplies. It is built on [avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX)
and on codec_d's RTX and 32 GB RAM work. The goal of this fork is the title
playable on an 8 GB NVIDIA card (RTX 4060 class, the Series S profile, 1080p),
aiming at 60 frames a second. It is tested on one PC with an 8 GB RTX 4060 and
is not there yet.

**This repository contains no game files, code or assets.** You need your own
copy of the game. It is not affiliated with or endorsed by the game's developer
or publisher.

## Download

Every push is built by GitHub Actions on a Windows runner. Two kinds of builds
are kept as rolling prereleases (each is replaced by the next push to its branch):

- [`build-main`](https://github.com/zivyn/codec-d--cool_fox/releases/tag/build-main):
  the `main` branch, what has been checked in the game.
- `build-integ-*` (see the [releases](https://github.com/zivyn/codec-d--cool_fox/releases)):
  the integration branches, newer and still under test.

Each release holds `victory.exe` (release build), `victory-diag.exe` (the same
with the `diagnostics` switches), `dxil.dll` (the DXIL validator, only for
`--validate` and `--pipelines`), `Play-VIctory.cmd` and `SHA256SUMS.txt`. Put
them in one folder. The game itself is not included: you supply your own dump.

## Run

Open PowerShell in that folder and point the launcher at your own complete dump:

```powershell
$env:VICTORY_GAME_DIR = 'D:\Games\MyDump'
$env:VICTORY_CACHE_DIR = 'D:\VIctory\cache'   # recommended: keeps the shader cache off the system drive
.\Play-VIctory.cmd pareto-residency
```

The launcher selects the Series S profile and the residency settings tested on
the RTX 4060. Without `VICTORY_CACHE_DIR` the cache goes to
`%LOCALAPPDATA%\VIctory\cache-pareto`, and it grows large. Requirements:
Windows 10/11 x64, a CPU with AVX2, a Direct3D 12 GPU with shader model 6.6 and
mesh shader support. The first run builds shader pipelines and stutters;
`victory warm --used` makes them ahead.

## Where it stands (2026-11-26)

Measured on the integration branch `integ/validation-2`, in the prologue cell,
one PC with an 8 GB RTX 4060. A frame took about 410 ms at the start of the day;
it now takes 168-175 ms (looking at a window) and 178-185 ms (dark cell).

Only on the integration branches so far (not yet on `main`; `main` is still the
codec_d build):

- FSR4's shared working buffer lives in video memory: its passes went from
  about 215 ms to about 24 ms in a microbenchmark, close to free on the card.
- Pipelines are made ahead, and compute dispatches wait for their own pipeline
  instead of being dropped; cold-start waits are shorter.
- The title's own hang watchdog no longer ends a session during our long stalls.
- Wave64 programs are lowered for NVIDIA's 32-lane waves.
- The descriptor heap stays under NVIDIA's 1,000,000-descriptor limit.
- 25,582 of 26,096 shader programs validate with the DXIL validator; the test
  branches validate more.

## Known wrong

- Rooms are too dark. The title's ray-traced indirect light has no data without
  the ray tracing path; an ambient stand-in is being fixed (test branch).
- Textures are black or missing on 8 GB; a fix that shows a lower mip is under test.
- Glass is grainy and colours band in some runs with a warm cache; being investigated.
- The CPU thread that submits the frame is now the main cost, about 150 of the
  175 ms.
- The title caps itself at 30 frames a second; lifting that without breaking
  physics, animation and cutscenes is the second half of the goal.

## Build from source

Install Rust (the toolchain is pinned in `rust-toolchain.toml`), the MSVC C++
build tools and a Windows SDK, then:

```powershell
cargo build --release --locked -p victory
```

`Play-VIctory.cmd` finds `target\release\victory.exe`. The diagnostics build is
`cargo build --profile diag --locked -p victory --features diagnostics`. CI
builds both and runs the tests on every push
([workflow](.github/workflows/build.yml)). Commands and debugging switches:
[docs/README-fxceless.md](docs/README-fxceless.md). The history of the AMD
RX 7900 XTX work is in avatarmanz's README.

## Credits and license

This is a tree of other people's work with more on top; the history is kept.

- [VIctory](https://github.com/Marvisak/VIctory) by Marvisak and its
  contributors: the loader, the runtime services, the Direct3D 12 back end and
  the shader recompiler.
- [avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX):
  the AMD-tuned fork this one comes from, and the people it credits
  (fxceless, DexNom, Belobug, myokex and others).
- codec_d: the RTX fidelity and 32 GB RAM residency work in the `pareto-residency` build.
- Changes in this fork were made with [Claude Code](https://claude.com/claude-code).

GPL-3.0-or-later, as VIctory. See [LICENSE](LICENSE); the one file that is not
covered is named in [docs/asset-policy.md](docs/asset-policy.md).
