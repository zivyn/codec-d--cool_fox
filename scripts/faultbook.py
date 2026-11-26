"""Reads the books of write faults a diagnostics run writes
(`VICTORY_FAULT_BOOK=<file>`, `crates/victory-gpu/src/faultbook.rs`): what the
title's faults on guarded view memory cost, per frame, and which blocks fault
again and again.

    python -I scripts/faultbook.py <file>.0.bin [<file>.1.bin ...] [--freq HZ]

Records of 40 bytes: ticks of the performance counter, three words, the
thread, what. The counter runs at 10 MHz on the Windows versions we run on;
`--freq` says otherwise. Only the standard library is used.
"""

import argparse
import bisect
import collections
import struct
import sys

RECORD = struct.Struct("<qQQQII")
WHAT = {
    0: "frame",
    1: "fault",
    2: "taken",
    3: "known",
    4: "asked",
    5: "ours",
    6: "alias",
    7: "open",
    9: "site",
    10: "stored",
    11: "unlocked",
}


def records(path):
    data = open(path, "rb").read()
    usable = len(data) - len(data) % RECORD.size
    return [RECORD.unpack_from(data, at) for at in range(0, usable, RECORD.size)]


def spread(values):
    """p50, p90, p99, max and sum of `values`."""
    if not values:
        return "none"
    v = sorted(values)

    def q(p):
        return v[min(len(v) - 1, int(p * len(v)))]

    return "n %d, p50 %.1f, p90 %.1f, p99 %.1f, max %.1f, sum %.2f ms" % (
        len(v), q(0.5), q(0.9), q(0.99), v[-1], sum(v) / 1000)


def book(path, freq):
    rs = records(path)
    print("== %s: %d records" % (path, len(rs)))
    if not rs:
        return
    us = lambda ticks: ticks * 1e6 / freq
    frames = [r for r in rs if r[5] == 0]
    n_frames = max(1, len(frames) - 1)
    if len(frames) >= 2:
        span = us(frames[-1][0] - frames[0][0]) / 1000
        print("frames: %d over %.1f ms (%.1f ms a frame)" % (len(frames), span, span / n_frames))
    counts = collections.Counter(WHAT.get(r[5], str(r[5])) for r in rs)
    print("records per frame: " + ", ".join("%s %.1f" % (k, v / n_frames) for k, v in counts.most_common()))

    # Faults opened under the lock: b = blocks opened << 32 | ns waiting; c = ns holding.
    locked = [r for r in rs if r[5] == 1]
    waits = [(r[2] & 0xFFFFFFFF) / 1000 for r in locked]
    holds = [r[3] / 1000 for r in locked]
    print("faults under the guards' lock, per frame %.1f" % (len(locked) / n_frames))
    print("  waiting for it (us): " + spread(waits))
    print("  holding it (us):     " + spread(holds))
    print("  per frame: %.2f ms waiting, %.2f ms holding" % (sum(waits) / 1000 / n_frames, sum(holds) / 1000 / n_frames))
    unlocked = [r for r in rs if r[5] == 11]
    if unlocked:
        took = [r[3] / 1000 for r in unlocked]
        print("faults without the lock (VICTORY_FAULT_UNLOCKED), per frame %.1f" % (len(unlocked) / n_frames))
        print("  in it (us): " + spread(took))
        print("  per frame: %.2f ms; other views told: %.2f a fault" % (
            sum(took) / 1000 / n_frames, sum(r[2] for r in unlocked) / len(unlocked)))
    opened = [r for r in rs if r[5] == 7]
    if opened:
        print("faults on a block another thread had opened: %.1f per frame, waited (us): %s" % (
            len(opened) / n_frames, spread([r[2] / 1000 for r in opened])))

    # By thread.
    by_thread = collections.defaultdict(lambda: [0, 0.0])
    for r in locked:
        by_thread[r[4]][0] += 1
        by_thread[r[4]][1] += ((r[2] & 0xFFFFFFFF) + r[3]) / 1e6
    for r in unlocked:
        by_thread[r[4]][0] += 1
        by_thread[r[4]][1] += r[3] / 1e6
    print("by thread (faults, ms in the lock or the unlocked open, per frame):")
    for thread, (n, ms) in sorted(by_thread.items(), key=lambda t: -t[1][1])[:12]:
        print("  %6d: %6.1f faults, %6.2f ms" % (thread, n / n_frames, ms / n_frames))

    # Blocks that fault again and again: in how many frames each faulted.
    marks = [r[0] for r in frames]
    faulted_in = collections.defaultdict(set)
    for r in locked + unlocked:
        faulted_in[r[1]].add(bisect.bisect_right(marks, r[0]))
    hist = collections.Counter(len(f) for f in faulted_in.values())
    faults_of = collections.Counter(r[1] for r in locked + unlocked)
    print("blocks that faulted: %d; by the frames they faulted in (frames: blocks, faults):" % len(faulted_in))
    for k in sorted(hist):
        blocks = [b for b, f in faulted_in.items() if len(f) == k]
        print("  %3d: %6d blocks, %7d faults" % (k, len(blocks), sum(faults_of[b] for b in blocks)))
    hot = [b for b, f in faulted_in.items() if len(f) >= n_frames * 3 // 4]
    print("blocks faulting in three frames of four or more: %d, %.1f faults a frame of %.1f" % (
        len(hot), sum(faults_of[b] for b in hot) / n_frames, (len(locked) + len(unlocked)) / n_frames))

    # From a block's taking (made read-only) to its next fault: what a longer
    # rest (VICTORY_GUARD_REST) would leave out.
    taken_at = {}
    after = []
    for r in rs:
        if r[5] == 2:
            for b in range(r[1], max(r[2], r[1] + 1)):
                taken_at[b] = r[0]
        elif r[5] in (1, 11) and r[1] in taken_at:
            after.append(us(r[0] - taken_at.pop(r[1])))
    if after:
        edges = [1000, 3000, 10000, 30000, 100000]
        cum = [sum(1 for a in after if a < e) for e in edges]
        print("from a block's taking to its next fault (us): " + spread(after))
        print("  " + ", ".join("under %d ms: %.0f%%" % (e // 1000, 100 * c / len(after)) for e, c in zip(edges, cum)))

    sites = collections.Counter(r[1] for r in rs if r[5] == 9)
    if sites:
        print("busiest places in the code (address: faults per frame): " + ", ".join(
            "%#x: %.1f" % (a, n / n_frames) for a, n in sites.most_common(10)))
    taken = sum(max(1, r[2] - r[1]) for r in rs if r[5] == 2)
    print("blocks made read-only per frame %.1f, other views made read-only %.1f, opened by ourselves %.1f" % (
        taken / n_frames, counts["alias"] / n_frames, counts["ours"] / n_frames))


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("books", nargs="+")
    parser.add_argument("--freq", type=float, default=1e7, help="ticks a second (10 MHz unless given)")
    args = parser.parse_args()
    for path in args.books:
        book(path, args.freq)
    return 0


if __name__ == "__main__":
    sys.exit(main())
