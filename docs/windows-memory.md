# Windows memory troubleshooting

If the menu works and the game closes during the first scene load, check
Windows' **Committed** counter in Task Manager under **Performance > Memory**.
It shows current system commit and the limit. Free physical RAM can remain
while committed memory is near its limit. A Windows page file raises that
limit. See [Microsoft's page file guidance](https://learn.microsoft.com/en-us/troubleshoot/windows-client/performance/how-to-determine-the-appropriate-page-file-size-for-64-bit-versions-of-windows).

## Check the failure

In Event Viewer, check **Windows Logs > System** for **Resource-Exhaustion-Detector,
event 2004** near the exit. In the VIctory log, look for `memory allocation of`
or `left to commit`. An application error with `0xc0000409` needs context:
the allocation failure and the system event can identify a Rust out-of-memory
abort. For GPU pressure, check **Dedicated GPU memory** and **Shared GPU memory**
under **Performance > GPU**, and compare them with the application's log.

If commit is near the limit and paging is disabled, open `sysdm.cpl` and go to
**Advanced > Performance Settings > Advanced > Virtual memory > Change**.
Enable **Automatically manage paging file size for all drives**, restart Windows,
and check that the file is active:

```powershell
Get-CimInstance Win32_PageFileUsage | Select-Object Name, AllocatedBaseSize
```

Repeat the same load after restarting. Keep the first and second run's commit
and GPU memory readings so the effect can be checked.

## A starting profile for 8 GB GPUs

Start with the Series S profile and frame generation disabled to keep the
initial test small. The following command uses the title's upscaler and a
30 FPS cap. Check the startup log's `console` and `switches` line, since saved
F1 preferences can override launcher options.

```powershell
.\target\release\victory.exe play 'D:\Games\MyDump\game_scarlett_master.exe' --series-s --dlss off --no-framegen-ready --fps 30
```

## Reproduction on 32 GB RAM and an 8 GB RTX 3060 Ti

On Windows 11 with NVIDIA driver 617.14, the Series S configuration above
reached the menu, then exited while loading the first scene. Windows logged
event 2004 before the exit. The system commit limit was 32,686 MiB, with no
active page file. A retry with two pipeline workers reached 32,647 MiB of
system commit while 7,620 MiB of physical RAM was still available. Its next
reported failure was a 2,228,240-byte allocation. Turning off `VICTORY_THIN`
and setting `VICTORY_JOIN_PARTS=1` also reached the commit limit and exited.
The last in-process report before the original exit showed approximately
1,380 MiB of local GPU memory used against a 7,256 MiB budget.

After enabling automatic paging and restarting, Windows reported an active
2,048 MiB page file and a system commit limit of 34,734 MiB. With two pipeline
workers, the same build loaded past the previous crash point and reached the
opening cutscene with audio and subtitles. The highest sampled commit was
30,716 MiB, leaving 4,018 MiB available. The restart also reduced background
commit, so the run measures the combined effect of the restart and paging.

The 3D picture remained black during the observed cutscene. The log reported
over 1,200 pipelines pending and a texture queue blocked by its VRAM budget.
In one sample, 1,815 MiB was reserved for future render targets, while the
driver reported about 4,844 MiB used out of a 4,923 MiB local budget. The run
ended when the window was closed. This result verifies progress past the
allocation failure. Rendering in the cutscene still needs testing.

### Follow-up on the invisible scene

The same checkpoint, with two pipeline workers and frame generation off,
gave these results during the opening cutscene:

| Configuration | Observed result |
| --- | --- |
| Original Series S path after warming 1,561 recorded pipelines | Black picture with subtitles |
| DLSS Balanced, with its own cache | Black picture with subtitles despite successful DLSS dispatches |
| Internal upscaler from #68, default 3,072 MiB target reserve | Black picture with subtitles |
| Internal upscaler, 1,536 MiB target reserve | Black picture even after the unused target reserve went to zero |
| Internal upscaler, 1,536 MiB target reserve, `VICTORY_THIN=0`, `VICTORY_JOIN_PARTS=1` | 3D street scene visible, with dark areas and the texture queue still waiting for room |
| DLSS Balanced, 1,536 MiB target reserve, `VICTORY_THIN=0`, `VICTORY_JOIN_PARTS=1` | Black picture returned. DLSS dispatches continued to succeed |

The internal-upscaler runs with the smaller reserve used the same build and
save, with a cache that warmed between runs. Synchronous recording is a
candidate for reproducing visible output; the test does not isolate it from
that cache history. The Windows commit peak in the visible run was about
32 GiB. The texture queue still reported pressure and later gameplay remains
unverified.

## Related work

- fxceless reports a similar allocation abort during a scene transition with
  32 GB RAM and a 16 GB GPU. An open proposal throttles pipeline workers and
  pauses speculative prewarming as commit headroom falls.
  [Report #771](https://github.com/fxceless/victory-fork/issues/771),
  [proposal #777](https://github.com/fxceless/victory-fork/pull/777)
- A VIctory-NVIDIA report on a 16 GB RAM machine found pressure on shared GPU
  memory. The author used CPU-mapped VRAM for some buffers. The launcher work
  was later made opt-in and predates the current foxhound base.
  [Report #6](https://github.com/trustno1-coder/VIctory-NVIDIA/pull/6),
  [follow-up #8](https://github.com/trustno1-coder/VIctory-NVIDIA/pull/8)
- DexNom reports a repeatable game breakpoint after reaching the world on a
  different 8 GB GPU. Matching the failure signature helps separate that
  issue from Windows commit exhaustion.
  [Report #104](https://github.com/DexNom/VIctory/pull/104)
- Two open proposals on VIctory-NVIDIA address the Series S upscaler and the
  VRAM kept for future render targets. Both can be evaluated separately from
  Windows paging.
  [Upscaler #68](https://github.com/trustno1-coder/VIctory-NVIDIA/pull/68),
  [render target reserve #76](https://github.com/trustno1-coder/VIctory-NVIDIA/pull/76)
- Another tester saw missing character geometry that returned with `THIN`
  and `JOIN_PARTS` disabled on a different GPU.
  [Recording mode #66](https://github.com/trustno1-coder/VIctory-NVIDIA/pull/66)
