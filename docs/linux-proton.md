# Linux and Proton (experimental)

Notes from first runs on one Linux PC on 2026-11-27. Windows remains what the
project is written for. These runs were short and do **not** show that the
title is stable or playable under Proton.

## Tested on

| | |
|---|---|
| System | CachyOS, kernel 7.3.6 |
| Card and driver | GeForce RTX 5080, NVIDIA 620.73.04 |
| Build | commit `29d8dbf`, cross-compiled on Linux with `cargo-xwin` |
| Launcher | `umu-launcher` |
| Settings | Series X profile, DLSS off, frame generation not made ready |

## Building on Linux

The pinned toolchain targets MSVC. With `clang`, `lld` and
[cargo-xwin](https://github.com/rust-cross/cargo-xwin) installed:

```sh
cargo install --locked cargo-xwin
cargo xwin build --release --locked -p victory --target x86_64-pc-windows-msvc
```

`cargo-xwin` downloads Microsoft's C runtime and Windows SDK, which come under
Microsoft's licence terms.

The program is then at
`target/x86_64-pc-windows-msvc/release/victory.exe`.

## Running

Use your own dump. Change the three paths to yours:

```sh
env GAMEID=umu-default \
    WINEPREFIX="$HOME/Games/victory-prefix" \
    PROTONPATH="$HOME/.local/share/Steam/steamapps/common/Proton - Experimental" \
    umu-run /path/to/victory.exe \
    play /path/to/dump/game_scarlett_master.exe --dlss off --no-framegen-ready
```

Logs are in the prefix, under
`drive_c/users/steamuser/AppData/Local/victory/logs`.

## Shader compiler crash with proton-cachyos

| Proton | vkd3d-proton | What happened |
|---|---|---|
| `proton-cachyos` 11.0-20261015 (slr) | 3.1.0 | crashed after about a minute or less, in both runs |
| Proton Experimental 11.0-20261123b | 3.2.0 | built both shaders below and went on |

With `proton-cachyos`, the Proton log (`PROTON_LOG=1`) showed an illegal
instruction inside NVIDIA's shader compiler (`libnvidia-gpucomp`):

```text
err:vulkan:vkCreateComputePipelines Exception 0xc000001d in Unix call.
```

In each run, one translated compute shader had no `host pipeline` line after it
in the layer's log, so it is the likely trigger (the driver does not name it):

1. first run: `4e296b1885c37b29`;
2. second run, with that one added to `VICTORY_LEAVE_OUT`: `8bb272e763f76148`.

Switching to Proton Experimental avoided this without leaving either shader
out. The two setups differ in more than the vkd3d-proton version, so which
difference matters is not known.

The Experimental run lasted a little over two minutes, with about 670 host
pipelines logged. The Proton log had no such error when checked about 90
seconds in. The run ended with `title suspended; ending the title`.

If you have to stay on the other Proton, a shader can be skipped, at the cost
of whatever it draws or computes. Keep the two the launcher already leaves out:

```sh
--set VICTORY_LEAVE_OUT=02384a60fda6f2d0,731a39171b568298,<hash>
```

This got past the first shader only to stop at the second.

## Also seen

- `no host pipeline library: pipelines are not kept` appears in the layer's
  log, with Proton Experimental too.
- `--preload` was started once. The Proton log showed `vkd3d-proton` failing to
  open its cache's write file (`Failed to open stream archive write file
  exclusively`), probably because several processes compile at once. How much
  of a preload is kept under Proton has not been measured.
- A full `PROTON_LOG=1` log reached about 290 MB in one short run.
  `PROTON_LOG='-seh,-unwind'` kept it far smaller.

## Not tested

DLSS, frame generation (DLSS or FSR 3), HDR, other cards and drivers, and long
play sessions. `Get-NVIDIA-DLSS.cmd` and the built-in download need Windows
PowerShell, which was not set up in these runs.
