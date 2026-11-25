# Recording a frame and running it again

A diagnostics build can write down one frame's D3D12 work (what the host
layer sends to the card, and the memory it reads) and run it again without
the title, to see where the card's time goes and to try changes in seconds.
Code: `crates/victory-gpu/src/replay`.

## Recording

Off unless `VICTORY_RECORD` is set. Build with
`cargo build --locked --profile diag -p victory --features diagnostics`.

| variable | meaning |
|---|---|
| `VICTORY_RECORD=<folder>` | on; the capture goes to `<folder>\capture-<n>` (outside the repository) |
| `VICTORY_RECORD_AT=<seconds>` or `f<frame>` | when: seconds after the device was made, or a number of presents. Unset: only the trigger file |
| file `<folder>\go` | when it appears, the capture starts (always watched) |
| `VICTORY_RECORD_FRAMES=<n>` | frames to capture (1) |
| `VICTORY_RECORD_ALWAYS=1` | record lists from the start: needed only if notes say lists were not recorded (a title that submits old lists again) |

At the time given, lists are recorded for a few frames, until a frame's lists
were all recorded; the next frame is captured: everything sent to the drawing
queue between two presents (the first may be the swap chain's own work for
the frame before). One capture per process. At the frame's first submission
the card is drained and its memory copied out; before each later one the card
is drained again and what the CPU changed is written. The title stands still
meanwhile. `frame recorder: the frame is captured` in the log and `frame.bin`
in the folder mean it is done; `failed.txt` that it failed.

**Read `notes.txt`**: everything that could not be recorded is listed there
(and in every replay's report). Only the line with the totals, waits of the
drawing queue and lists of another queue (the relay's copies) are expected.

Measured on a made-up frame of a title's size (12,000 resources, 1.5 million
views, 30 submissions, 40,000 calls): a frame takes 3-10 ms more with the
recorder on, the capture stands still for 5.4 s and is 9 GB (8 GB of the card's
memory at 2.3 GB/s, 1 GB the CPU wrote). The recorder holds a copy of the
memory both sides see that the frame reads, and 1 GB or more of read-back
buffers, until it is done.

## Running it

    victory replay <folder> [--loops 5] [--timing detailed] [experiments]

`<folder>` is the one given as `VICTORY_RECORD` (its last capture) or a
capture. Each loop puts back what the one before changed and sends the frame
without waiting between submissions. Reported, from timestamps on the card:
the frame (first list's start to last list's end), each submission, copies
that stand for what the CPU wrote between submissions (not the title's cost),
and gaps. With `--timing detailed`, each command: sums by kind and the
costliest pipelines with the names the runtime logs and their DXIL hashes
(blake3). A command's time is from the end of the one before to its own end:
draws overlap, so it is a share of the list, not a cost alone; the extra
timestamps cost a little too. The picture goes to `replay.png` and is compared
with the presented one.

| switch | does |
|---|---|
| `--compare` | runs the frame unchanged first and prints the difference |
| `--skip-clamps` | no argument clamps and no barriers of their scratch buffers; draws read arguments kept from a first run |
| `--skip-hash-passes` | no compare/rehash passes over card buffers |
| `--no-barriers` | no unordered-access barriers |
| `--merge-barriers` | joins barrier calls with no work between them |
| `--buffers vram` / `sysmem` | all buffers in the card's / the PC's memory |
| `--one-submit` | one `ExecuteCommandLists`, with a full wait between the former ones |
| `--only 3..7` | runs these lists; of the others only barriers |
| `--skip-shader <text>` | leaves out what runs pipelines with the text in their name or DXIL hash (repeatable) |
| `--no-restore` | leaves what a loop changed |
| `--debug-layer` | D3D12's debug layer; its errors in the report |

Use the median of several loops; the first is slower. An experiment that
leaves out waits or work can draw another picture and, with wild data, lose
the device: save work in other programs first. Do not run the title at the
same time.

## Limits

- Not recorded, but reported when used: reserved (tiled) resources, bundles,
  ray tracing, render passes, enhanced barriers, predication, stream output,
  `ClearUnorderedAccessView`, shading rate, newer creation methods
  (`CreateCommittedResource1` and on).
- Not kept: contents of multisampled textures, of textures in memory the CPU
  sees, and of texture levels over 256 MB; resources of other queues' work.
- Texture states come from the barriers seen. A resource that overlaps the
  one last used (aliasing) is taken as without contents.
- Addresses in memory are not rewritten: indirect arguments with addresses
  (vertex buffers, root views) are reported, and would be wrong. Descriptors
  and root arguments are.
- Views replaced during the frame get a heap of their own per submission in
  the replay (64 MB for two million views).
- Upload heaps become custom write-combined ones in the replay, as the card
  writes what the CPU wrote.
- What the CPU changes in memory both see is taken before each submission, by
  pages of 4 KiB, of what that submission reads of it (all of a buffer that a
  descriptor names). Pages the card wrote there are part of it: they count as
  copies, and set the card's own results to the recorded ones.
- The card's clocks are the system's: other programs on the card change the
  times.

Tests: `cargo test -p victory-gpu --features diagnostics` (`tests/replay.rs`
runs a small program and compares every byte; `VICTORY_TEST_D3D_DEBUG=1` adds
the debug layer) and `tests/replay_big.rs` (`--ignored`).
