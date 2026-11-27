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

Builds are made by GitHub Actions on a Windows runner, started by hand. Two kinds of builds
are kept as rolling prereleases (each is replaced by the next published build of its branch):

- [`build-main`](https://github.com/zivyn/codec-d--cool_fox/releases/tag/build-main):
  the `main` branch, what has been checked in the game.
- `build-integ-*` (see the [releases](https://github.com/zivyn/codec-d--cool_fox/releases)):
  the integration branches, newer and still under test.

Each release holds `victory.exe` (release build), `victory-diag.exe` (the same
with the `diagnostics` switches), `dxil.dll` (the DXIL validator, only for
`--validate` and `--pipelines`) and `Play-VIctory.cmd`. Put
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

## Where it stands (2026-11-27)

Measured on `integ/validation-3` to `-10`, in the prologue cell, one PC with an 8 GB
RTX 4060. A frame took about 410 ms at the start of the day; it now takes
88-91 ms looking at the window (about 11 frames a second), with the card
working about 67 ms of it (95-113 ms before the card-copy changes) and the CPU
about 770 ms of thread time a frame (940-990 before).

On `main` (integrations #2, #3, #6, #8 and #10 are merged, so `main` and the `build-main`
download have all of this):

- FSR4's shared working buffer lives in video memory: its passes went from
  about 215 ms to about 24 ms in a microbenchmark, close to free on the card.
- Pipelines are made ahead, and compute dispatches wait for their own pipeline
  instead of being dropped; cold-start waits are shorter.
- The title's own hang watchdog no longer ends a session during our long stalls.
- Wave64 programs are lowered for NVIDIA's 32-lane waves.
- Compute programs with several console waves in a group (the defragmentation and
  triangle-collection passes among them) now translate on 32-lane hosts: the two
  host waves of a console wave meet by counters of their own. The 195 geometry
  programs (and their export stages) translate, and the 71 mesh programs whose
  output passes 32 KiB create pipelines on NVIDIA (40 KiB of mesh output). Only
  three compute programs still do not translate (one is the adaptive
  shadow-ray variant).
- The descriptor heap stays under NVIDIA's 1,000,000-descriptor limit.
- The heap sync and the refreshes of copies skip work that has nothing to do
  (heap wait 26 to 16-22 ms, refreshes 52 to 40-45 ms). A copy on the card of
  a buffer the title's CPU cannot write is not taken whole again until a write
  is announced (window spot 158 to 142.7 ms, the submit thread 153.6 to 135.8 ms).
- The ambient stand-in for the title's indirect light: the dark cell now
  matches the console frames, and the window shows the rippled glass and the
  chain-link.
- A lower mip is shown instead of a black texture when the card is short of
  room, and textures are made level by level on demand.
- More shader programs validate (`shaders/validator-clean`,
  `shaders/rt-shadow-view-addresses`); 25,582 of 26,096 validated with the DXIL
  validator before these two.
- The helper threads a submission waits for run above the title's threads, and a
  submission's parts and lists are joined (16 parts, 96 lists): the window spot
  went from about 152 to 114-135 ms. Threads that wait for new card memory at the
  same time share one `MakeResident` (peaks of 11.3 s a frame became 1.7 s), a
  title buffer's copy is made with the buffer map free, and big copies of title
  buffers get their card room first.
- `victory warm --used --compute` makes the pipelines before playing (476 s on the
  test PC): launching with it first removes the frozen minutes of a cold cache.
- The shader translator also builds on Linux (`victory-translate`, with the
  validator as `libdxil.so`); 26,515 programs came out identical to the Windows
  build's.
- Card copies of title buffers of 1 MiB and more get the big copies' margin
  (the "medium" buffers), which took about 29 ms (23 %) off a frame; the AVX2
  compare of copies against their snapshots and the re-marking of kept snapshot
  units are on, about 6 and 7.5 ms. Mesh output and lens-flare scan programs
  (the exact wave-wide form) were fixed, and the ambient air light has a cap
  (`VICTORY_AMBIENT_AIR=<mult>,max:<level>`).

## Known wrong

- Hair is blocky in some frames, intermittently.
- Colours band on some machines, and glass is grainy in some runs with a warm cache;
  being investigated.
- Bright rooms show an orange fog.
- A full-screen white-out was seen about once in a few first runs on a fresh cache;
  it recovers by itself. A rare defect that predates the multi-wave work; being tracked.
- The CPU thread that submits the frame is still a large share of the frame.
- The title caps itself at 30 frames a second; lifting that without breaking
  physics, animation and cutscenes is the second half of the goal.

## Build from source

Install Rust (the toolchain is pinned in `rust-toolchain.toml`), the MSVC C++
build tools and a Windows SDK, then:

```powershell
cargo build --release --locked -p victory

# with the diag build, to have the pipelines made ahead:
target\diag\victory.exe shaders <the shader archives>   # every shader translated, minutes
target\diag\victory.exe warm --used --compute --now       # the pipelines earlier runs drew with (VICTORY_PIPELINE_BOOK=1 writes
                                                          # them down) and every compute pipeline of shaders.rpf, into the
                                                          # pipeline library; `warm --verify` asks the library for them
```

`Play-VIctory.cmd` finds `target\release\victory.exe`. The diagnostics build is
`cargo build --profile diag --locked -p victory --features diagnostics`. CI
builds both and runs the tests on demand
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
