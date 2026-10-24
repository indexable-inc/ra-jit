#!/usr/bin/env python3
"""ci.py: the harnesses CI runs, in parts that take about the same time on a hosted runner.

usage: ci.py PART                 run the harnesses of PART for RA_JIT_BACKEND on this machine
       ci.py --parts [TIER]       the parts this machine has harnesses in, as a JSON list (CI's
                                  matrix): TIER `full` (the default) or `fast`
       ci.py --list               which harness runs in which part, on which hosts and backends

The fast tier, of every pull request and push, is the harnesses that take a few minutes, in the
parts quick-1 and quick-2 of about four minutes each on a hosted runner, and on Linux quick-3,
the reduced programs fuzzing found built wrong. The full tier, nightly
and by hand, is every harness, in the parts core, keys-N, buildmodel, store-kills, store and
daemon.

Every harness in a part runs even when one before it fails; the part fails at the end and names
them. RA_JIT_BIN is the ra-jit the harnesses run, as for each harness on its own.
"""
import json, os, platform, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
SYSTEM = {"linux": "linux", "darwin": "macos", "win32": "windows"}[sys.platform]
ARCH = "aarch64" if platform.machine().lower() in ("arm64", "aarch64") else "x86_64"
BACKEND = os.environ.get("RA_JIT_BACKEND") or "cranelift"
KEYS = 3


def on(*hosts, backend=None):
    """Whether a harness runs here: hosts are "linux", "macos", "windows" or "linux-aarch64"..."""
    return lambda: (not hosts or SYSTEM in hosts or f"{SYSTEM}-{ARCH}" in hosts) and backend in (None, BACKEND)


EVERYWHERE = on()
# (part, arguments, environment, where it runs). The order is the order of each part's run.
HARNESSES = [
    ("core", ["difftest.py", "corpus", "--bin", "runner", "--crate", "cases", "--layouts"], {}, EVERYWHERE, 2),
    ("core", ["programs.py"], {}, EVERYWHERE, 1),
    # The daemon compiles every function of both programs, main too, and LLVM is optimizing: an
    # edit of what both use has to reach each program, whatever its callers had built into them.
    ("core", ["programs.py"], {"RA_JIT_ALONE": "1"}, on("linux", "macos", backend="llvm"), 1),
    ("core", ["breakers.py"], {}, EVERYWHERE, 1),
    ("core", ["unwinds.py"], {}, EVERYWHERE, 1),
    ("core", ["growth.py"], {}, on(backend="cranelift"), 1),
    ("core", ["wholeprogram.py"], {}, on("linux", "macos"), 2),
    ("core", ["splices.py"], {}, EVERYWHERE, 2),
    ("core", ["live_e2e.py"], {}, on("linux-x86_64"), 1),
    # Apple's linker leaves no more room than it is told, and gives a newer macOS chained fixups.
    ("core", ["headerpad.py"], {}, on("macos"), 1),
    # bun links with `-no_compact_unwind`, which leaves `__eh_frame` alone to the unwinder.
    ("core", ["nocompact.py"], {}, on("macos"), 1),
    # A branch reaches 128 MB on AArch64, and the splice has to get past that. Mach-O is not spliced.
    ("core", ["farsplices.py"], {"RA_JIT_SPLICE": "1"}, on("linux-aarch64"), 1),
    # A file 2.3 GB past what it loads, as bevy's debug builds are; about 9 GB of disk at once.
    ("core", ["farfile.py"], {}, on("linux"), None),
    # 220 MB of code, 16 MB of frames and, on Linux, 4.2 GB of DWARF: no size is a full link.
    ("core", ["bigbinary.py"], {}, on("linux", "macos"), None),
    # `cargo ra-jit` hands Windows builds to cargo, so there the daemon links nothing to test.
    ("core", ["unreached.py"], {}, on("linux", "macos"), 1),
    ("core", ["loadedits.py"], {}, on("linux", "macos"), 2),
    ("core", ["asmedits.py"], {}, EVERYWHERE, 1),
    # The assembler of a build of ra-jit without LLVM, which is the release's.
    ("core", ["asmedits.py"], {"RA_JIT_ASSEMBLER": "cc"}, on("linux", "macos", backend="cranelift"), 1),
    ("core", ["includes.py"], {}, EVERYWHERE, 1),
    ("core", ["latelisted.py"], {}, on("linux", "macos"), 1),
    ("core", ["aheadready.py"], {}, on("linux", "macos"), 1),
    ("core", ["rustsrc.py"], {}, EVERYWHERE, 1),
    # Each scenario runs rustc and rustdoc no more often than norustc/baseline.json says.
    ("core", ["norustc.py"], {}, on("linux", "macos"), 2),
    # `cargo ra-jit check`'s diagnostics, in each `--message-format`, against rustc's.
    ("core", ["check_json.py"], {}, on("linux", "macos", backend="cranelift"), 2),
    # ra-jit answers what is asked of rustc as rustc answers it, on every target it knows.
    ("core", ["answers.py"], {}, on("linux", "macos", backend="cranelift"), 1),
    ("core", ["worktrees.py"], {}, EVERYWHERE, 2),
    ("core", ["worktrees.py", "--join", "--together"], {}, on("macos"), 2),
    ("core", ["worktrees.py", "--overlay"], {}, on("macos"), 2),
    # Tiers take live edits, which are macOS's alone; the harness picks its backends.
    ("core", ["tiers.py"], {}, on("macos", backend="cranelift"), 1),
    # Generated programs on fixed seeds, and generated edits under a daemon against cold builds.
    # Only x86-64 Linux has been fuzzed so far.
    ("core", ["fuzz/fuzz.py", "smoke", "--backends", BACKEND], {}, on("linux-x86_64"), 2),
    ("core", ["fuzz/edits.py", "smoke", "--backends", BACKEND], {}, on("linux-x86_64"), 2),
    # Every reduced program fuzzing found ra-jit building wrong, on both architectures they
    # were found on; a part of its own, as it takes minutes.
    ("core", ["fuzz/fuzz.py", "check", "--backends", BACKEND], {}, on("linux"), 3),
    # How much of the corpus LLVM compiled itself, and what it left to Cranelift and why.
    ("core", ["backends.py", "corpus", "runner", "cases", "--bin", "runner", "--levels", "2f"], {}, on(backend="llvm"), 2),
    # DWARF read by gdb out of an ELF executable, or a PDB read by cdb.
    ("core", ["debuginfo.py"], {}, on("linux", "windows"), None),
    *((f"keys-{k}", ["keys.py", "--shard", f"{k}/{KEYS}"], {}, EVERYWHERE, None) for k in range(1, KEYS + 1)),
    # Builds for Linux targets besides the host; the cases of another host say so.
    ("buildmodel", ["buildmodel.py", "--backend", BACKEND], {}, on("linux", "windows"), None),
    ("store-kills", ["storekills.py", "--kills", "100"], {}, EVERYWHERE, None),
    ("store", ["storecorrupts.py", "--rounds", "4"], {}, EVERYWHERE, None),
    # It mounts a tmpfs, or fills a disk image on Windows. In the fast tier: what a build
    # keeps grows with features that write the store, and a full disk is where that shows.
    ("store", ["storefull.py"], {}, on("linux", "windows"), 2),
    ("store", ["storeshared.py"], {}, EVERYWHERE, None),
    ("store", ["storeguards.py"], {}, on("linux"), None),
    ("daemon", ["stress.py"], {}, EVERYWHERE, 2),
    ("daemon", ["selects.py"], {}, on("linux"), 2),
    # MinIO as one program in ~/s3tools, which CI puts there on Linux.
    ("daemon", ["remote.py"], {}, on("linux"), None),
]


def part_of(harness, tier):
    part, _, _, _, quick = harness
    return part if tier == "full" else quick and f"quick-{quick}"


def parts(tier="full"):
    seen = []
    for harness in HARNESSES:
        part = part_of(harness, tier)
        if part and harness[3]() and part not in seen:
            seen.append(part)
    return seen


def run(part):
    tier = "fast" if part.startswith("quick-") else "full"
    chosen = [(h[1], h[2]) for h in HARNESSES if part_of(h, tier) == part and h[3]()]
    if not chosen:
        sys.exit(f"ci.py: no harness of part {part!r} runs on {SYSTEM} {ARCH} with {BACKEND}; the parts here are {parts(tier)}")
    failed, times = [], []
    for args, env in chosen:
        what = " ".join(args)
        print(f"::group::{what}", flush=True)
        start = time.monotonic()
        status = subprocess.run([sys.executable, *args], cwd=HERE, env={**os.environ, **env}).returncode
        times.append((time.monotonic() - start, what))
        print("::endgroup::", flush=True)
        if status:
            failed.append(what)
            print(f"::error::{what} failed (exit status {status})", flush=True)
    for seconds, what in times:
        print(f"{seconds:7.0f} s  {what}")
    print(f"failed: {', '.join(failed) or 'none'}")
    sys.exit(1 if failed else 0)


def main():
    arg = sys.argv[1] if len(sys.argv) in (2, 3) else "--help"
    if arg == "--parts" and sys.argv[2:] in ([], ["full"], ["fast"]):
        print(json.dumps(parts(*sys.argv[2:])))
    elif arg == "--list" and len(sys.argv) == 2:
        for part, args, env, _, quick in HARNESSES:
            print(f"{part:12} {f'quick-{quick}' if quick else '':8} {' '.join(f'{k}={v}' for k, v in env.items())} {' '.join(args)}")
    elif arg.startswith("-"):
        print(__doc__)
        sys.exit(0 if arg == "--help" else 2)
    else:
        run(arg)


if __name__ == "__main__":
    main()
