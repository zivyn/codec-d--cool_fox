# A/B inside one process: `VICTORY_AB`

Two runs of the same build differ by several ms/frame at the window spot (run-to-run sd 3.7 ms over 12 runs,
96 % of it between processes and not inside one: a process sits at its own level for its whole run). An A/B of
two runs therefore needs about ten pairs to show 5 ms. Changing the switch **inside** one run, every few
seconds, cancels that level: both arms are measured by the same process, in the same scene, with the same
core placement.

This is a diagnostics build feature (`victory-diag.exe`; the release build reads the switches from the
environment once, as before).

## Using it

```
VICTORY_AB=<seconds>,<switch>[=<value>][,<switch>..]
VICTORY_AB_START=<seconds>     # first phase begins this long after the world is found (default: at the first present)
VICTORY_AB_SKIP=<seconds>      # left out at the start of each phase (default 1)
VICTORY_AB_BUNDLE=1            # the phases alternate between all the named switches off and all on (a bundle against none)
```

- Phase 0 has every named switch **off** (A), phase 1 has bit 0 on (B), and so on: with one switch A, B, A, B, ...;
  with n switches all `2^n` combinations come one after another. A named switch does not go by its own variable
  any more. `<switch>=<value>`: what it is when on (a number for `VICTORY_REFRESH_NARROW`, `VICTORY_JOIN_RAMP=1`
  and so on; no value: `1`). What is off is what the switch is when its variable is not set.
- The world is found as the pilot's driver finds it: three seconds in a row of 3000-5500 draws per frame and under
  260 ms per frame. `VICTORY_AB_START=70` begins the alternation at world+70 s, the window spot.
- The switches change at the title's present call (`queue::frame_ends`), at any instant for the threads that read
  them: each switch below is read so that this is safe. The first `VICTORY_AB_SKIP` seconds of each phase are not
  counted (the exe marks those seconds `settled 0`).
- Lines in the log: `A/B begins`, one `A/B second: phase N bits B settled S: X ms/frame over F frames, D draws a frame` every
  second, `A/B phase N bits B on [..]: ... mean ...` at each phase's end, and, with one switch, `A/B so far (<switch>): B
  minus A +x.xx ms/frame +-y.yy (95%, n pairs ...)` (each B phase against the mean of the A phases before and after
  it, so a drift that is straight over the three cancels; the interval is Student's t on n-1 degrees). The other
  per-second lines of the log (what the submit thread worked, the card, the CPU) are given to their arm by time with
  `bin\bench1-ab.py <tag>` (outside the repo, in `G:\victory-work\bin`). The pilot kills the process, so there is no summary
  at exit: the last `so far` line is the summary.

### Recipe (window spot)

```
$env:VICTORY_AB = '10,VICTORY_HOLDER_INDEX'; $env:VICTORY_AB_START = '70'; $env:VICTORY_AB_SKIP = '2'
```
(through `pilot2-spot.ps1 -Extra 'VICTORY_AB=10,VICTORY_HOLDER_INDEX;VICTORY_AB_START=70;VICTORY_AB_SKIP=2'`, the run
kept to world+70+240 s with `-EndSec 320`). 240 s of alternation is 24 phases, 11 drift-corrected pairs of 8 settled
seconds. A second of the window spot has a robust sd of ~2.7 ms (11 frames), so a phase mean carries ~1 ms, a pair
~1.2 ms and the 95 % interval of the mean of 11 pairs is about +-0.8 ms/frame: effects of 1.5 to 2 ms show in one run.
Phases of 20 s with 3 s left out make 11 phases (5 pairs, +-2 ms): use them for switches that may need seconds to warm
(`VICTORY_ASK_CACHE`, `VICTORY_VIEW_CACHE`: caches that fill) and compare the means of the two arms for
the first and the last settled seconds of the B phases: a difference between them is a warm-up that the skip does not cover.
Repeat in two or three processes: the effect is measured at one process's level of luck, and a switch that helps only
at one placement should show as different effects in different runs. Pictures and stability are still judged from the
shots of the run (both arms are in every shot; the arm is in the `A/B second` line of the shot's second).

## The switches that may be named

`ab::live_flag!` (off unless `=1`) and `ab::live_value!` (a value) read the phase's value at each use, with
`crate::ab::on(bit)`, an atomic load. Where a decision has more than one read in it, it reads once and keeps the answer.

| switch | read at | why it may change at any instant | tests |
|---|---|---|---|
| `VICTORY_HOLDER_INDEX` | `card_steps::plan`, once per planning (`holder_index_on`) | the index of writers is built in that planning from its own writers, used there, and dropped with it; both ways find the same holder | `the_index_of_writers_finds_what_a_look_at_all_of_them_finds` |
| `VICTORY_STEPS_MERGE_READS` | `card_steps`, once per list of steps (`merge_reads_on`) | the list's reads are recorded in one go, whole or with those that lie together joined; no state outlives the list (the counter line only counts what was joined) | `reads_that_lie_together_are_one_copy` |
| `VICTORY_REFRESH_NARROW` (gap, a number of KiB; `=1` 64) | `host::Batch` (`refresh_narrow_gap`, `narrow_on`), `prefetch::spans_of` | each use makes spans of what it reads or writes for the batch at hand; narrow spans hold what the steps refer to, wide ones are a superset, so a batch that begins one way and ends the other still refreshes what its steps read (`Batch::parts` is taken out at its flush whichever way the switch stands; a gap read as "everything" joins all into one span); the write watch's askings that were not made over the gaps are made by the next wide one (it reports what was written since the last asking of a page) | `narrow_spans_hold_what_the_steps_refer_to_and_the_union_is_as_before` |
| `VICTORY_PIPELINE_PREPARE` | `prefetch::offer` (once), `behind` per item, `prefetch::join` | **join**: a refresh offered while the switch was on is joined by every later preparing, whatever the switch says then (`must_join`: on, or anything was ever offered), so the next preparing never meets copies the prefetch thread is refreshing; `submit` reads the switch once (`early`) for both what it excludes and what it offers; `AHEAD` is set and taken inside one item | `a_join_looks_at_the_gate_after_the_switch_went_off_once_anything_was_offered`, `the_next_preparing_waits_for_what_was_offered` |
| `VICTORY_HEAPS_EARLY` | `behind::Job::round`, once when a submission comes into the line | the number of the round of the heaps' records is stored in the entry when it comes in; what takes it later (`ROUND`) does not look at the switch | `a_submission_is_done_with_the_round_asked_for_what_it_took_along` |
| `VICTORY_JOIN_RAMP` (`=1`, first `VICTORY_JOIN_RAMP_FIRST`) | `behind::ramp_first`, per take-along, present and submission | `RAMP` is set again at every present (when on); a value left from before only limits how many parts a submission takes along up to what it would anyway; off: no limit | `the_ramp_after_a_present_doubles_up_to_the_joins` |
| `VICTORY_ASK_CACHE` | `shadow::loose` (once per ask), `found_most` | what a thread remembers is stamped (`stamp`) and every answer is checked with `still` before and after it is made strong; the table sizes follow the switch and `remember` clears a table whose size is not the switch's | `asks_remembered_by_range_give_the_one_window_while_others_change` and the cache's own |
| `VICTORY_RANGE_VALID` | `shadow::stamp`, `still`, `found_stamp`, `found_still` | the two kinds of stamp are different numbers: the top bit of a stamp says which kind it is, and a stamp is taken as holding only by the kind that made it (`stamped_so`); one of the other kind is made again | `a_stamp_made_the_other_way_never_holds`, `a_buffer_changes_the_stamps_of_its_addresses_only` |
| `VICTORY_VIEW_CACHE` (`1`, or `=2` to compare) | `descriptor::ListViews::begin` | the recording that begins takes the mode (`ListViews::mode`) and keeps it to its end: the kept views' slots are begun again, and views are kept or made fresh, by that one mode; what was kept is checked with `shadow::still` when found, so views kept before a stretch with the switch off are used only if the copies are still as they were | `a_buffer_view_is_kept_from_one_recording_to_the_next` |
| `VICTORY_DROP_EARLY` | `list::thin::translate`, once per stream (`drop_early_on`; only with the structure builder off) | a stream decides alone which of its calls are left out and keeps the state it leaves identical (every compute root that a later dispatch can read is set, the last set of each; the pipelines are all kept; at its end every root counts as read), so streams translated with the switch on and off can follow each other on one list | `the_next_dispatch_that_runs_sees_what_it_saw_with_none_left_out`, `stages_that_are_dropped_leave_the_last_set_of_each_parameter` |

## Not converted, and why

- `VICTORY_STATE_FILTER`: with the filter off the host list's shadow of its roots, vertex and index buffers is not kept up
  (`State::root_changed` returns before it), so turning it on in the middle of a list would compare with a stale shadow
  and leave out calls the list needs. It would need a mode per list and a shadow cleared when it begins, on all
  recording threads at once.
- `VICTORY_KEEP_FAST`: the array of the raw pointers it keeps per slot is made with the descriptor heap, when the switch
  is read (`kept_raw`); turned on later its entries would be those of a time it was not kept up. It needs the array
  always made and cleared at the flip.
- `VICTORY_ALIAS_FAST` is not in this branch (`perf/cut3-mem` has it).
- Anything that changes the translation or the cache key (the pipeline book, the translated programs) and
  anything that is read when an object is made (heaps, copies, pools).

## What is not tested

The tests are of the pieces (the parsing of the switches and values, the world, B minus A with its interval and a
drift that cancels, the join, the stamps, the equivalence of the two ways of the switches that choose between two ways
of the same answer). That a flip leaves the picture and the game's own results as they are is not a test of CI (no game,
no GPU): it is looked at in the run, where both arms are in the same shots, and by the stability of the runs.
