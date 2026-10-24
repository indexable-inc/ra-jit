# The harnesses

ra-jit is tested by comparison with rustc. Almost every script under `harness/` builds
something with cargo and rustc, builds it again with ra-jit's code in place of rustc's for
some crates, runs both, and compares standard output, standard error and exit status. A
difference is a failure. So is a function ra-jit refused, in the harnesses that count them.

There are also Rust unit tests in `crates/ra-jit` and in the code generator; the harnesses
are what carries the weight.

## Running them

You need Python 3, Rust 1.99.0 with `rust-src`, a C compiler and linker, and a release build
of ra-jit. Some harnesses need more: `debuginfo.py` needs gdb and `llvm-dwarfdump`,
`buildmodel.py` needs the `aarch64-unknown-linux-gnu` and `x86_64-unknown-linux-musl`
targets with their cross tools and qemu, `remote.py` needs a MinIO or SeaweedFS binary,
`storefull.py` needs passwordless `sudo` to mount a tmpfs.

    cargo +1.99.0 build -p ra-jit --release
    cd harness
    export RA_JIT_BIN=$PWD/../target/release/ra-jit

`RA_JIT_BIN` names the ra-jit under test and defaults to `../target/release/ra-jit`. The
scripts pick the toolchain from `harness/toolchain` unless `RUSTUP_TOOLCHAIN` is set. They
also default `RUSTFLAGS` to `-C link-dead-code`, which `cargo ra-jit` does not use, and
`CARGO_INCREMENTAL` to 0. Scratch directories go under `TMPDIR`; several of the harnesses
need a few gigabytes there.

### The gate

The gate is what CI runs: its fast tier on every pull request and push to main, all of it
nightly ([CI](#ci)). A change is not done until all of it exits 0 under both backends.
`harness/ci.py` is the one list of it: which harness runs in which part, in which tier, on
which hosts and under which backend.

    python3 ci.py --list            # every harness, its part, its quick part, its environment
    python3 ci.py --parts           # the parts that have harnesses on this machine
    python3 ci.py --parts fast      # the fast tier's parts here (quick-1, quick-2)
    python3 ci.py core              # run one part here, as CI does

A part runs all of its harnesses even when one fails, then names the ones that failed and how
long each took. Alongside the harnesses, CI runs `cargo fmt --all -- --check`,
`python3 harness/imports.py`, which imports every script of the harness, and
`python3 harness/unittests.py`, which runs the Rust unit tests and holds them to its `OPEN`
list (below).

### Under Cranelift

Cranelift is the default. Either leave `RA_JIT_BACKEND` unset or set it:

    export RA_JIT_BACKEND=cranelift

### Under LLVM

Build with the `llvm` feature in the flake's dev shell, which has the LLVM 20 it pins
([contributing.md](contributing.md#llvm-for---features-llvm)), then select the backend. The gate is run at `-O2` with fast instruction selection, and that is the
only LLVM configuration it covers.

    nix develop -c cargo +1.99.0 build -p ra-jit --release --features llvm
    export RA_JIT_BACKEND=llvm RA_JIT_OPT=2 RA_JIT_EMIT=fast

A binary built with `--features llvm` still uses Cranelift unless `RA_JIT_BACKEND=llvm` is
set, so one build serves both runs. Under LLVM also run

    python3 backends.py corpus runner cases --bin runner --levels 2f

which reports how many functions LLVM compiled itself and how many it left to Cranelift,
with reasons. The daemon's log says `backend: llvm -O2 fast emit` when LLVM is in use.

## What each one checks

### Correctness of generated code

| Script | What it checks |
|---|---|
| `difftest.py <workspace> --bin <pkg> --crate <name>` | The base comparison. Builds the binary with cargo, compiles the named crates with ra-jit, links ra-jit's object ahead of rustc's rlibs, runs both. `--layouts` also compares the size and alignment of every type ra-jit laid out with rustc's `-Zprint-type-sizes`. `--bisect` links each ra-jit function in on its own to name the ones that change behaviour. `--mixed N` runs N builds with a random half of the functions left to rustc. `--memcheck` runs under valgrind |
| `harness/corpus/` | The workspace `difftest.py corpus` runs: a library with one module per area of the code generator and a runner that calls every function and prints the result |
| `breakers.py` | Every reduction under `harness/breakers/`, each run on its own so a crash in one does not hide the others. These are minimal cases of bugs found in real crates, rustc's tests and the fuzzer |
| `unwinds.py` | Edits to functions a panic unwinds through |
| `growth.py` | That the code of one function grows in a straight line with its source: the `serialize` `#[derive(Serialize)]` gives an internally tagged enum, a `?` in every arm, at 160 and 320 arms (issue #5); and a `load` filling a struct of 200 and 400 fields, each a call's `?` while a value with a destructor is live, as x11-dl opens Xlib's 781 functions (the way out of each `?` once dropped every field before it on its own: 2,986,013 blocks, and 364 GB for bevy) |
| `asmedits.py` | Edits to `asm!` blocks and the code around them; with `RA_JIT_ASSEMBLER=cc`, assembled by the C compiler, as a build without LLVM does |
| `answers.py` | That ra-jit's own answers to rustc's questions (`-vV`, `--print cfg`, cargo's probe of file names, `target-spec-json`) are rustc's, byte for byte, on every target ra-jit has them of; `--write` regenerates them from the pinned rustc ([no-rustc.md](no-rustc.md), step B) |
| `norustc.py` | How often rustc and rustdoc run when `harness/norustc/fixture` is built, edited, tested, doctested and checked with `cargo ra-jit`, through a copy of the toolchain that logs every run; a count above `norustc/baseline.json` fails ([no-rustc.md](no-rustc.md), step A). `--record` lowers the baseline |
| `fuzz/fuzz.py` | Generated programs, see below |
| `uitest.py <rust checkout>` | rustc's own `tests/ui` run-pass tests. Every file is either run or left out with a counted reason |
| `libsuite.py <rust checkout> coretests\|alloctests\|stdtests` | The standard library's test suites, cut down to what builds on stable |
| `cratetest.py <crate-dir>` | A crate's own integration tests: each test binary is relinked with ra-jit's objects and every test must report the same result |
| `cratesweep.sh`, `vendor.sh`, `sweepreport.py` | `cratetest.py` over a list of crates.io releases, and a summary |
| `bintest.py <name>` | A real program from `breakers/bins.json`: the same command lines given to cargo's binary and ra-jit's |
| `alone.py` | A program whose workspace crates rustc compiles nothing of, so no refused function can hide behind rustc's copy |
| `allrajit.py [whole] [unit] [stored]` | Programs with nothing rustc built in them, the standard library compiled by ra-jit ([no-rustc.md](no-rustc.md), steps F and G): a hello and one of collections, panics, threads and channels, each built with the library in the same build, over the library built alone (`--prebuilt`), and on a store and a remote that start empty, where the first build compiles and keeps the library and the later ones, one on a second store, take it. Each has to print and exit as cargo's build does. Linux |
| `macroserver.py` | The release's proc-macro server (`vendor/proc-macro-srv`) built by ra-jit, and the libraries ra-jit links of serde_derive, thiserror-impl, tokio-macros and clap_derive ([no-rustc.md](no-rustc.md), step J1): every call of a package's derives (with helper attributes), attributes and function-like macros is expanded by the toolchain's server with rustc's library and by ra-jit's with ra-jit's, and the outputs have to be equal, spans included. Needs crates.io. Linux |
| `frozenwarm.py` | Frozen def maps (#40, #161) of a dependency whose items, and a `macro_rules!` it exports, are in files `include!` reads and the VFS does not load at first, as the icu `*_data` crates': built on one store cold, warm twice, warm with `RA_JIT_FROZEN=check` and `=verify`, and warm after an included file changed at the same path, which has its def map refused. Each has to print what cargo's build does. Linux |
| `wholeprogram.py` | Programs ra-jit builds whole, with its own `main` (as `runbench.py` builds them): one on the default allocator, one with a `#[global_allocator]` of its own, one with a library's, one whose allocator is under a `#[cfg]` that is false. Each has to print and exit as cargo's build does; `--release` builds both in the release profile |
| `bun_repros.py` | Nightly fixtures, one per thing bun's whole-program build found ra-jit doing wrong (`harness/bun-repros/`: `adt_const_params`, `core::mem::type_info`, macros, exported statics, frame pointers, …). Each compares output with cargo's binary and checks the report's refusals against the case's. A case can set its own `rustflags` and `env` (the frame-pointer case runs LLVM), and `alone` builds it as runbench builds bun, the link taking only ra-jit's objects. It uses `nightly-2026-09-15` and sets `RA_JIT_OTHER_TOOLCHAIN=1`. |
| `backends.py` | One program compiled by each code producer: counts, time per function, and what LLVM left to Cranelift |
| `lints_ui.py <rust checkout>` | The lints rust-analyzer finds at the level a source denies them, against rustc's UI tests that `deny` or `forbid` one: for each lint, the errors (lint and line) ra-jit reports that rustc does too (right), that rustc does not (wrong) and that only rustc reports (missed). A lint ra-jit fails builds with (`DENIED` in main.rs) has none wrong; one in a macro's expansion is not counted, as it fails no build. `--toolchain 1.99.0` scores against the stable rustc. `lints_ui.py harness/lints_probes` scores the units written for the lints with few UI tests, one for each place a lint applies. `--deny LINT` scores one lint on every UI test, each read with `#![deny(LINT)]` first, for a lint few tests deny (`unused_mut`) |
| `borrowck_ui.py <rust checkout>` | The borrow checker ([borrowck.md](borrowck.md)) against rustc's borrow-check UI tests: each unit passes when ra-jit reports the errors (code and line) the pinned nightly rustc reports, under Polonius and NLL. `--min-pass N` fails below N Polonius passes; a panic, or an error on a unit rustc accepts, fails it too |
| `scoreboard.py` | Runs the targets in `targets.json` and fails when behaviour differs or coverage drops below the committed `scoreboard.json` |
| `mirparity.py <workspace> --crate <name> [--level N] [--rustc-level 1\|2]` | ra-jit's MIR (`ra-jit mir`, at `RA_JIT_MIR_OPT` level N) against rustc's `-Zunpretty=mir`, function by function: the share that match in shape, statement counts, statements and types, and the most frequent differences. Not a gate: the numbers are in [research/mir-gap.md](research/mir-gap.md) |

### The daemon following edits

| Script | What it checks |
|---|---|
| `stress.py` | 100 edits of the kinds people make (bodies, signatures, new items, moved lines, broken code, reverts, an editor's edit while another client's build runs) applied in order to a three-crate workspace under one warm daemon, each compared with `cargo build` of the same source. The first edit cargo builds after broken code has to be a patch of the build before it ([#31](https://github.com/indexable-inc/ra-jit/issues/31)), and the edit sent during a build has to be taken ([#32](https://github.com/indexable-inc/ra-jit/issues/32)); it exits 1 when a step is wrong |
| `programs.py` | Two programs of one workspace built by one daemon. With `RA_JIT_ALONE=1` rustc builds stand-ins for the workspace's crates and the daemon compiles every function of both programs, `main` too: under LLVM an edit of what both use has to reach each ([#63](https://github.com/indexable-inc/ra-jit/issues/63)) |
| `worktrees.py` | Several checkouts of one workspace built by one daemon, edited independently |
| `includes.py` | Edits to a file only `include!` names |
| `latelisted.py` | What a file named by `include!` through a build script's variable declares is listed in its crate though the daemon listed the crate beside cargo's build before it read the file, as bun's generated bindings are: an edit that adds a function then patches a few functions, not every one the file declares |
| `aheadready.py` | A function the daemon compiles ahead while it reads the symbols rustc built (held with `RA_JIT_SYMBOLS_PAUSE_MS`, each such function made to take ten minutes with `RA_JIT_AHEAD_PAUSE_MS`) is given up on once they are read: the daemon is ready within two minutes of the build, and the builds after it, an edit's too, are its own and run as cargo's. bevy's derived `release_state` of a read-only query struct took two minutes and held the second warm-store run's readiness to 148 s. Linux and macOS |
| `rustsrc.py` | A toolchain without `rust-src` is turned down, saying how to install it |
| `splices.py` | Edits that are hard on the splice: thread-locals, statics, vtables, function pointers, closures, panics, foreign calls. Counts how each step was linked |
| `farsplices.py` | Splices into a binary whose code is further from its end than an AArch64 branch reaches |
| `farfile.py` | Edits of a program whose file is 2.3 GB of a section nothing loads, as a debug build of bevy's is mostly debug info: bodies, a caller, a new function and taking it back each have to be spliced, which a patch placed as far from the code as it is behind the file is not ([#79](https://github.com/indexable-inc/ra-jit/issues/79)). Linux |
| `bigbinary.py` | Edits of a program larger than any place of the splice is sized for: 220 MB of code, over 16 MB of frames the unwinder reads from `__eh_frame` or `.eh_frame`, and on Linux 4.2 GB of `.debug_abbrev`. A function shorter than the long jump, one that panics through its replacement, 3,000 functions added and a new function each have to be spliced ([architecture.md](architecture.md#no-size-is-a-reason-to-link-in-full)). Nightly, Linux and macOS |
| `unreached.py` | Edits under `cargo ra-jit` that call instances of another crate's generic function the executable did not reach when it was linked: each is spliced, not linked in full. The daemon's own links, with `RA_JIT_RUSTC_BASE=0`. Linux and macOS; on Windows `cargo ra-jit` hands builds to cargo |
| `loadedits.py` | A file written while a worker loads, after the worker read it and before it took its timestamp (held there with `RA_JIT_LOAD_PAUSE_MS`), is built as written: the next build, and the one after an edit of another crate, print the new text ([#53](https://github.com/indexable-inc/ra-jit/issues/53)). Linux and macOS |
| `agents.py` | Random edits of the kind coding agents make, on a real workspace |
| `invalidation.py` | How much the database redoes after different kinds of edit |
| `movedlines.py` | A panic reports its new line after lines are added above its function (seven ways to panic, each against cargo), and moving a function compiles nothing |
| `buildmodel.py` | One small cargo project per feature of cargo's build model, under both backends, with steps that change one input at a time |
| `cargo_ra_jit.py` | `cargo ra-jit` against plain cargo on seven real projects, 22 flag combinations, and 11 daemon lifecycle checks. Needs the network and about 20 minutes; not in CI |
| `debuginfo.py` | Backtraces, `llvm-dwarfdump --verify`, gdb stopping and stepping, locals |
| `live_e2e.py` | Edits of a running program that links `ra-jit-live`, each applied as a patch; then another program built meanwhile, a start again from the executable, and a panic that unwinds through the patched code. Linux x86-64 and macOS AArch64 |
| `live_latency.py` | Edit-to-running time of live edits on `harness/live`, `demo/life`, `harness/live-bevy` and `harness/live-egui`: isolated edits, bursts of valid keystrokes, real typing replayed key by key (most texts in between do not compile; a program told to restart is started again), and a session's restarts against `cargo run`. Writes p50 to p99 and every sample, with the machine and its battery, to `LATENCY_JSON`. Measures; checks nothing; macOS on AArch64 ([live.md](live.md#latency-under-typing)) |
| `fncache.py` | `#[ra_jit::cache]` ([fn-cache.md](fn-cache.md)): a cold run computes and a warm one finds the result on disk; comment, format, moved-line and unrelated edits keep it, an edit of a function it reaches through another drops it; generic instances are kept apart; impure functions are refused saying where; a restarted daemon finds the same results; `cargo ra-jit cache` lists them by function and `clear --stale` forgets the older code's alone; cargo's build caches nothing. `--live`: a live edit computes again and drops the old results, macOS on AArch64 |
| `spaces.py` | A two-crate workspace under a directory whose name has spaces, `$$`, `#` and quotes, built and edited with `cargo ra-jit build`; every edit has to be applied by the daemon against rustc's symbols ([#14](https://github.com/indexable-inc/ra-jit/issues/14)) |
| `rustcbase.py` | The first edits after a cold `cargo ra-jit build`, spliced into the executable rustc linked ([#11](https://github.com/indexable-inc/ra-jit/issues/11)), against plain cargo: bodies, closures, a dependency crate, function-local statics, two edits in a row, a new function of the program's crate, also in a package with a library it uses, a body of a program whose library has its name, and a `#[ra_jit::cache]` function and what it reaches have to be patches; generic, `#[inline]` and default trait functions, statics, a new function of a crate another depends on or of a name the crate already writes, other new items, `async`, `impl Trait` and `include_str!` edits have to be built in full and say why; and the daemon's own base, built while nothing waits, takes the edits after it; an edit that comes as it builds is a patch of rustc's executable, or of the base when it had begun to link ([#79](https://github.com/indexable-inc/ra-jit/issues/79)), also when a crate it loads includes a file no build script wrote (`unbuilt-member`), and is given up once it holds more memory than `RA_JIT_OWN_BASE_MEMORY` (`own-base-budget`), after which an edit built in full is spliced into rustc's executable and the patches after it chain onto that (`own-base-budget-full`, [#159](https://github.com/indexable-inc/ra-jit/issues/159)). On Linux each build in full of the scenarios is spliced whole into what rustc linked, not linked. Every edit's output and exit status against cargo's, macOS and Linux |
| `nocompact.py` | A program linked with `-no_compact_unwind`, as bun is, which has `__eh_frame` and no `__unwind_info`: its edits are spliced, and the unwinder finds the frame of the edited function where its symbol now is, behind the program's frames and, once a thousand new functions outgrow that room, with all of them moved into the header's ([#33](https://github.com/indexable-inc/ra-jit/issues/33)). macOS on AArch64 |
| `headerpad.py` | A program whose unwind table does not fit the room its link leaves after the load commands: the first link is done again with room, the edits after it are spliced, and a restarted daemon starts from the room kept beside the executable ([#13](https://github.com/indexable-inc/ra-jit/issues/13)). It is built for macOS 14, as nix's shell builds, which Apple's linker gives chained fixups unless told not to ([#30](https://github.com/indexable-inc/ra-jit/issues/30)). The daemon's own links, with `RA_JIT_RUSTC_BASE=0`. macOS on AArch64 |
| `tiers.py` | Tiers ([tiers.md](tiers.md)): `demo/lifebench` running under the daemon reaches levels 2 and 3 with output identical to cargo's, an edit of a tiered function demotes it and is applied at once, and `RA_JIT_TIER=off` changes nothing. macOS on AArch64 with `--features llvm`; it exits 0 elsewhere |
| `tierbench.py` | The benchmark table of [tiers.md](tiers.md): `demo/lifebench` at level 1, 2 and 3 under a daemon and as cargo builds it (release, and `-O3` with the dev profile's checks), in interleaved rounds, the steady rate of each; `--together` runs a round's builds at once, for a loaded machine. Measures; checks nothing; not in CI |

### The store and the remote

| Script | What it checks |
|---|---|
| `keys.py` | Whether the store ever hands a build code compiled from something else. One case per thing code depends on besides its file's text |
| `storekills.py` | The daemon killed at random moments while writing its store |
| `storecorrupts.py` | Packs damaged on disk, and stores of other formats |
| `storefull.py` | A store on a disk with no room |
| `storeshared.py` | Several daemons and `ra-jit cache` commands on one store at once |
| `deepnames.py` | Closures nested 20 deep keep the store under 64 MB, and a second daemon finds their entries again: a type spelled out longer than 1 KB is named by its hash (issue #29) |
| `expansions.py` | A second daemon on one store finds every derive and attribute expansion the first kept, and after an edit, with `RA_JIT_PROC_MACRO_CACHE=check`, none expands otherwise than it was kept (issue #38) |
| `remote.py`, `s3fake.py` | A store shared by two machines through an S3 service, with the transfer broken, objects spoiled and manifests forged |
| `packs.py` | Not a check: the sealed pack's layout ([cache.md](cache.md)) read and written, for `remote.py`, `storeguards.py` and `bench.py` |
| `deps.py` | cargo's builds of registry crates pushed and pulled. Builds offline from cargo's registry cache, so it does not run on a clean CI machine |

### Timing

`bench.py`, `editloop.py`, `edittimes.py`, `scaling.py`, `dxedits.py`, `linkbench.py`,
`shapes.py`, `heap.py`, `heapfold.py`, `memsplit.py`, `fnincr.py`, `realbench.py`, `spare.py` and `live_latency.py` measure and do not gate.
`memsplit.py` says what each process of the daemon holds (the daemon, the project's worker, the
spare) and of what kind of page, once ready, once the daemon has built its own base and after
edits, with `--env` to try an allocator setting
([research/daemon-memory.md](research/daemon-memory.md#2026-10-23-the-own-base-and-the-allocator)).
`spare.py` starts a daemon with a tiny project, then builds one of `bench.py`'s projects under it
from an empty target directory and again once cargo has built it, each until the daemon is
ready, for builds of ra-jit given as `--variant LABEL:BIN[:VAR=VALUE...]` in interleaved
rounds: the second project's cold and warm start and what the daemon's processes hold before and
after, which is how the spare worker's default was chosen ([cargo.md](cargo.md#what-a-build-does)).
`heap.py` gives a heap profile's bytes to the salsa query whose value or record holds them, and to
ra-jit's own structures; `heapfold.py` makes of a profile on macOS what `jeprof --collapsed` makes
of it on Linux, for `heap.py` to read ([research/daemon-memory.md](research/daemon-memory.md)); `heap.py --syntax`
says what of it is syntax trees, of which kind and from which query, and `cpufold.py` folds a
`perf script` the same way for time ([flat-syntax.md](flat-syntax.md)). `fnincr.py` counts
the queries an edit inside one function of a big file runs (the file comes from `genbig.py`),
against the same functions in many small files ([reports/INCREMENTAL.md](reports/INCREMENTAL.md)). `realbench.py` times
`cargo ra-jit build` against `cargo build` on ripgrep, just and nushell (cold, no-op and
three kinds of edit); the numbers are in [results.md](results.md#11-real-projects). `coldprof.py`
times a cold `cargo ra-jit build` of a real project until the daemon is ready and its first
edit, from an empty or a filled store, with every line the daemon logged and the cores busy
along the way: the readiness check of [#11](https://github.com/indexable-inc/ra-jit/issues/11),
whose numbers are in [cargo.md](cargo.md#what-a-build-does). With `--max-wait S` it fails when
the daemon was ready more than S seconds after the command returned; `--until TEXT` waits for a
line the daemon logs, and `--memory` follows what its processes hold. Their docstrings say how to run them.
`bench.py` on real projects, with `benchreport.py`, is also what writes the README's build
table: [benchmarks.md](benchmarks.md). Besides the edits `bench.py` times repeated cold builds
against fresh cargo with an empty and with a warm store (`--cold-runs`), the same steps under
the fastest cargo setups (`--setups`: mold, wild, `-Zthreads=8`, cg_clif and `cargo check`) and
the test suites under both (`--tests`). `bench.py --runtime` runs `runbench.py`, which times
programs ra-jit builds whole with LLVM against `cargo build --release`, and `benchreport.py`
writes that result to the README's runtime table, refusing it when any program's output differs
from cargo's. `abbench.py` runs two or more builds of ra-jit against each other on `bench.py`'s
edits, in interleaved rounds: an edit's wall time, the daemon's own time and (macOS) its
workers' cycles, the cold start, and the workers' peak memory. It is how the release build's
profile was chosen ([reports/RELEASE-BUILD.md](reports/RELEASE-BUILD.md)), and with `--train` it
is the training of its PGO. `hotpatch.py` times an edit of a running program under Dioxus's
subsecond against `cargo ra-jit build` and `cargo build` with a restart; its program is
`harness/hotpatch`. `benchhistory.py` appends a run to `docs/benchmarks/history/` (what the
site's /bench page charts) and, with `check`, fails when a median moved past its noise band.
`memory.py` is how every harness measures memory: `memory.run` runs a command and returns its
time with the exact peak of its largest process (`wait4`) and the peak of its whole tree
(sampled), `memory.Sampler` follows a long-lived process such as the daemon with its workers,
and `memory.in_child` measures a function that runs commands itself
([benchmarks.md](benchmarks.md#memory-and-disk)); a harness that reports memory uses it rather
than a way of its own. Every README benchmark is a matrix over the platforms of
`benchplatforms.py` (macOS arm64, Linux aarch64, Linux x86-64): it names a result's platform from
its host, files it as `docs/benchmarks/<platform>.json`, and gives the workflows the platforms
whose runner is registered. `benchreport.py` writes each table from the newest stored result of
every platform, so one that did not run keeps its last numbers. `benchcharts.py` draws the
README's charts (SVG, a light and a dark version, a bar per platform) from the same results;
`benchreport.py` regenerates them whenever it writes a table, `--charts` on its own, and
`--render` every table and chart.
`live_latency.py` times live edits on macOS on AArch64, per keystroke under typing (`replay`,
`burst`) as well as one edit at a time (`socket`, or `cargo` through `cargo ra-jit build`), and
the restarts of a session against `cargo run` (`restart`); its JSON goes to `benchhistory.py
live`, and the numbers measured on a MacBook are in [live.md](live.md#latency-under-typing).
`nix develop .#bench` has everything `bench.py` and `hotpatch.py` need (the nightly, mold, wild,
`dx`, bevy's libraries; zed's are in `.#bench-zed`, since every library directory slows each
link cargo makes), and `live_latency.py` runs under it as under rustup's 1.99.0. Under another
toolchain the daemon builds nothing, and the harness stops at once with what `cargo ra-jit`
says of it.

## The fuzzer

`fuzz/fuzz.py` builds generated programs with cargo and with ra-jit under each backend and
compares what they print. `fuzz/fuzz.py smoke` is part of the gate. How to run longer
runs, what a finding is and what the fuzzer does not cover are in
[fuzzing.md](fuzzing.md).

## Adding a corpus case

The corpus is for things the code generator must handle that are small enough to state as
a function. Add a `pub fn` to the module for its area in
`harness/corpus/cases/src/lib.rs`, or a new `pub mod`. Call it from
`harness/corpus/runner/src/main.rs` inside `case!(...)`, which prints the expression and its
value. The result has to be deterministic and `Debug`. Then run

    python3 difftest.py corpus --bin runner --crate cases --layouts

It must print `MATCH`, and the count of functions from ra-jit must go up by what you added,
with none left to rustc.

## Adding a breaker

A breaker is the reduction of a bug found somewhere else. Add a file
`harness/breakers/cases/src/<name>.rs` with a `//!` comment at the top that says where the
bug was found and what the cause is, and a `pub fn run()` returning something `Debug`. Add
`<name>` to the `cases![...]` list in `harness/breakers/cases/src/lib.rs`. Then

    python3 breakers.py <name>

A breaker should fail before the fix and pass after. If you are recording a difference you
are not fixing, add the name to `OPEN` in `breakers.py`, or to `LEFT` if ra-jit refuses the
function and says so. Both lists are the project's list of known wrong or missing
behaviour, so keep the comment in the case honest about why it is open.

A bug that only shows in a crate's own tests goes in `CRATES` in `breakers.py` as a small
crate under `harness/breakers/`.

## Reproducing a failure

Start from the script's last lines. Each daemon harness prints the path of the daemon's log
and of the scratch copy of the source it was editing, for example

    99 of 99 right, 0 restarts; log /tmp/ra-jit-stress-out-XXXX/daemon.log; source /tmp/ra-jit-stress-XXXX

and the stepwise ones take `--only TEXT`, `--from N` and `--keep` (see each docstring).

For a difference in generated code:

1. Run `difftest.py` on the workspace with `--bisect`. It names the functions whose ra-jit
   version changes the output, with the first differing line.
2. `ra-jit --report FILE.tsv` writes the outcome of every function (compiled, skipped and
   why). `--deny FILE` leaves the functions listed in a file to rustc, and `--isolate DIR`
   writes one object per function.
3. `RA_JIT_DUMP_MIR=<function name>` prints the MIR of functions with exactly that name
   (`{closure}` for closures). `RA_JIT_DUMP_LLVM=1` prints the LLVM module of every function
   compiled, and `RA_JIT_DUMP_CLIF=1` the Cranelift IR of drop glue.
4. Reduce it to a function, add it as a breaker, and fix it there.

For a fuzzer finding, `fuzz.py repro SEED-ROUND` rebuilds the program and
`fuzz.py check found/FILE.rs` runs a stored reduction.

For a CI failure, the last line of the harness step lists which scripts failed.

## CI

`.github/workflows/ra-jit.yaml` runs in two tiers. It calls `ra-jit-platform.yaml` once for
each platform, each on a machine of its own kind.

A change reaches main through the **merge queue** alone
([contributing.md](contributing.md#landing-a-change)). The queue takes pull requests in order,
merges each onto main and the entries ahead of it, runs the fast tier on that, and moves main to
it once it passes; an entry that fails leaves the queue, and the ones behind it are tested again
without it. So main is always a commit the fast tier passed on. A pull request's own run is
`fmt` alone, which lets it join the queue within a minute: the queue tests it merged anyway, and
testing it alone first would make every landing wait for the fast tier twice. For the fast tier
on a branch before queueing it, `gh workflow run ra-jit.yaml --ref <branch> -f tier=fast`.

The **fast tier** runs on every entry of the merge queue, every push to main and by hand, and
takes about ten minutes. On Linux x86-64, Linux AArch64 and macOS arm64 it:

- builds ra-jit once, with the `llvm` feature, and hands the binary to its other jobs;
- runs `harness/unittests.py`;
- runs the parts of `ci.py --parts fast` (`quick-1` and `quick-2`, three to six minutes each)
  under each backend, as a job each;
- on Linux x86-64, runs rustc's borrow-check tests with a floor under the passes, and the
  run-pass tests of `harness/ui-pinned.txt`, against that build.

`fork alone` builds and tests `vendor/rust-analyzer` outside this repository, and the `fmt`
job checks formatting and has `harness/imports.py` import every script of the harness, so one
that only a seldom tier or a person runs can't break unnoticed. It also runs
`harness/rustcsites.py`, which fails when ra-jit's code gains a place that starts rustc, rustdoc
or cargo ([no-rustc.md](no-rustc.md)). A script does nothing when
imported: its work is under `if __name__ == "__main__":`.

The **full tier** runs nightly and by hand (`gh workflow run ra-jit.yaml --ref <branch> -f
tier=full`, or `-f tier=fast`), and takes about an hour. It adds Windows x86-64, every part
of `ci.py --parts` (`keys.py`, `buildmodel.py`, the store and the daemon harnesses,
`debuginfo.py`), `cargo_ra_jit.py` on its checks, on ripgrep and hyperfine and on fd (Linux
x86-64 and macOS), and on ripgrep, fd, bat, hyperfine, just, axum and nushell with each
project's tests (Linux AArch64), and the ripgrep bench. A failed nightly opens an issue named
"Nightly ra-jit failed", or comments on it while it is open
(`.github/actions/nightly-issue`).

A change to nothing but documentation or the site runs `fmt` and nothing else. On a pull
request a newer push cancels the run before it. Every push to main and every merge-queue entry
runs to the end in a concurrency group of its own: with one group for main, a merge every few
minutes replaced each waiting run with the next, and 7 of 10 runs on main were cancelled.

The build and the unit tests compile through sccache, which keeps rustc's output in the
Actions cache; rust-cache keeps the dependencies from crates.io and leaves out the crates of
this repository and `vendor/`, which are most of the build. The harnesses run without the
wrapper, since `cargo_ra_jit.py` tests a user's own. Main alone writes either cache (sccache is
read-only elsewhere, `SCCACHE_GHA_RW_MODE`): what a pull request or a queue entry wrote would
be kept for its own ref, which nothing reads again, and would push main's entries out of the
repository's 10 GB. The harness jobs' Ubuntu packages (valgrind and gdb) come through
`.github/actions/apt`, which keeps them in the cache per runner image: the runners' mirror
sometimes serves a few tens of kB/s, and installing them took 8 to 12 minutes in a cluster of
runs.

`ci` is the job that says whether everything that ran passed, and it is the one check branch
rules and the merge queue require. A job that is renamed or split does not change it. Measured
on 2026-10-22 ([reports/CI.md](reports/CI.md)): the fast tier took 13.3 minutes at the median
and 20.4 at p90 on main, where the longest path is the macOS build (4 minutes) and then its
slowest harness part (6), and the p90 is the package mirror. No job waits for a runner.

The repository ruleset `main` (Settings, Rules) is active and holds the default branch to four
rules: a change comes through the merge queue, a push needs `ci` to have passed on the commit
pushed, the branch can't be deleted, and it can't be force-pushed. The queue merges with a
merge commit, builds up to five entries at once, merges up to five together, needs every entry
green, and counts an entry failed after 60 minutes without `ci`. Repository admins bypass the
rules (`bypass_actors`), for repairs; work lands through the queue. A ruleset can't exempt the
token a workflow runs with, so the workflows that commit to main (`pgo.yaml`'s pin,
`bench-readme.yaml`, `bench-bun.yaml`, `live-latency.yaml`) check out with the secret
`RA_JIT_BOT_TOKEN`, a fine-grained token of a repository admin (Contents read and write on
ra-jit alone) that the admin bypass lets past; without it their push is refused. `gh api repos/indexable-inc/ra-jit/rulesets/25516056` shows the rules, and Insights,
Rule insights what they refused.

What is known to be wrong is written down, says why, and does not fail the run. Once it comes
out right, it fails the run until it is taken off the list:

- `OPEN` in `unittests.py`: Rust unit tests that fail;
- `OPEN` and `LEFT` in `breakers.py`: breakers that differ from rustc or that ra-jit refuses;
- `OPEN` in `debuginfo.py`: checks whose frames or variables are wrong;
- `"open"` in a `buildmodel/*/case.json`: a case that comes out WRONG, on every host or by
  host (`{"linux": "why"}`).

The other workflows: `bench-readme.yaml` measures real projects on each platform's dedicated
benchmark machine and commits the README's numbers to main. It runs a platform only once the
repository variable `BENCH_RUNNERS` names it ([benchmarks.md](benchmarks.md#the-machines)). `bench.yaml` (the store's
microbenchmarks, two minutes) runs on pull requests and pushes too. `claims.yaml` (all of
rustc's `tests/ui`, both backends, and the scoreboard) and `nix-live.yaml` (the flake, the demo
and live edits, 40 minutes; also on a change to the flake, `nix/` or the dependencies) run
nightly and open an issue when they fail, like ra-jit.yaml's full tier. `fork-sync.yaml` runs
on pushes to main that touch `vendor/`, `fuzz.yaml` nightly, and `live-*.yaml` on their own
branches. None of them is part of `ci`; the few `tests/ui` programs that once differed from
rustc (`harness/ui-pinned.txt`, `uitest.py --tests`) run in ra-jit.yaml's `pinned-ui` job,
on Linux x86-64 and macOS, which is. `site.yaml` deploys the
site, and `release.yaml` builds releases from tags, each target through
`.github/scripts/build_release.py` (the `dist` profile with PGO, [reports/RELEASE-BUILD.md](reports/RELEASE-BUILD.md)),
the recipe the flake's `dist` runs too. `pgo.yaml` trains the profiles it uses, by hand and on
pushes to main that change Cargo.lock, a manifest or the toolchain, and pins them in
`pgo/profiles.json`.
