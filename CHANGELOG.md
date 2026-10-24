# Changelog

All notable changes to ra-jit are written here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- The SSE and AVX compares compile (#87): `llvm.x86.sse.cmp.ps` and `.ss`, `sse2.cmp.pd` and
  `.sd` and AVX's 256-bit ones give a lane of all ones where the immediate's predicate holds, as
  rustc's do. glam's `Vec4::cmpeq` and `is_finite_mask`, which bevy calls, were left to rustc;
  `harness/breakers` checks each predicate, NaN lanes included, against rustc's program.
- `cargo ra-jit check --message-format=json` (and `short`, `json-diagnostic-short`,
  `json-diagnostic-rendered-ansi`, `json-render-diagnostics`) writes the daemon's verdict as
  `cargo check` does: each error a `compiler-message` with rustc's code, level, spans (bytes,
  lines, columns counted as rustc counts them, the source lines and their highlights) and
  `rendered`, cargo's package id and target, the failure notes, `build-finished` and the
  `could not compile` lines. rust-analyzer's check-on-save and editors read it, so they show
  ra-jit's verdict and not rustc's. `-Z`, `--exclude` and several `--target`s are the
  checker's too (a checker for each target, four held at once), where they went to cargo.
  `harness/check_json.py` compares it with `cargo check` on the same errors: 7 packages, every
  format, every compared field the same; the labels, help and notes, other spans and
  `code.explanation` are rustc's only, and the messages are rust-analyzer's wording.

- `RA_JIT_UNITS=1` compiles a whole program's functions many to an LLVM module, as rustc
  compiles a crate's codegen units (#176): the functions are found from their IR before any
  is compiled, cut into units of about 20,000 instructions in the order they were found in,
  and compiled on every core, each unit with the union of its functions' imports linked in.
  An import LLVM leaves a call of is kept as the code of its function, so the functions are
  found in one round, and each function's code is read out of the unit's object and put in
  the program's as a compile of it alone is. It is not the default: on 16 cores helix builds
  in 71.1 s against 69.7 s and runs 1.28x of cargo's release build against 1.21x, and LLVM
  takes four times rustc's time for code of the same size either way
  ([codegen-units.md](docs/research/codegen-units.md)). Imports are linked into a compile's
  module once, after being linked to each other two at a time, which took the lowering of a
  module of many functions from 23.7 s to 8.1 s summed over ripgrep's cores.

- An edit that needs a build in full no longer links in full when the daemon has no base of
  its own (it gave it up, or hasn't built it yet) (#159): on Linux the program is built onto
  the executable rustc linked and spliced into it whole, and the edits after it are patches of
  that. nushell's signature edit with its base given up at 2 GB is spliced in 1.5 s, and the
  edits after splice in about 10 ms. macOS still links such an edit in full, but no longer
  twice: the link takes the room for splices the executable there wanted. `rustcbase.py`
  checks that every build in full of the rustc phase is spliced whole on Linux, and
  `own-base-budget-full` a given-up base's.
- Const eval answers nightly's reflection, `core::mem::type_info` (#34): a type's variants
  and fields, a field's type, name and offset, a type's size, whether it is signed, an array's
  length and element, what a pointer points to, a variant's name, as rustc does, and two of
  its `TypeId`s compare. Bun's `MultiArrayList` asks it of every type it holds, which left 907
  of bun's functions to rustc; `harness/bun-repros/type_info` asks what it does and now
  compiles. A constant that holds a `TypeId` is still refused: the evaluator's ids are not
  rustc's hashes.
- The daemon's workers hold at most half of the machine's memory together, or what
  `RA_JIT_MEMORY_MAX` says (a size, or `off`). Over it, the worker asked longest ago is
  stopped; `cargo ra-jit status` shows the limit and what each worker holds. `docs/reports/SOAK.md`
  described the variable, but it was never brought over from the soak branch (#60).
- `RA_JIT_OTHER_TOOLCHAIN=1` has `cargo ra-jit` build with a rustc other than 1.99.0, for a
  project pinned to a nightly of the next release that 1.99.0 can't build (bun's
  `nightly-2026-09-15`). Without it such a build goes to cargo, as before, and says why. The
  builds of a nightly cargo are the daemon's: `-Z embed-metadata=no`, which it passes every
  crate, no longer sends them to cargo, and build scripts' output and the seeded target are
  found in its second build-directory layout (`soakrows.py`, "a project pinned to a nightly").
- The `bench-bun` workflow measures bun nightly on the benchmark machine (edits and runtime),
  in `nix develop .#bench-bun`: bun's nightly and what its own build of its C and C++ needs.
  Too slow for every push.
- `benchreport.py --name NAME` stores a project measured on its own in
  `docs/benchmarks/NAME-<os>-<arch>.json` (`runtime-NAME-…` for the runtime), and every README
  table and chart shows it as a project like the others, in the geometric means too, with a
  footnote naming the machine, commit and date it was measured at.
- `bench.py bun` times the edits on bun (body, signature, leaf, generic, new function) in a copy
  of the tree `bunprep.py` prepares, under bun's pinned nightly; bun is measured only when
  named.
- bun (its Rust port, 1.4.3: 1.25M lines in 103 crates, with a JavaScript runtime's C and C++)
  in `apps.json`, so `apps.py bun` and `runbench.py bun` build it whole-program and compare
  its runs with cargo's. An entry's `prepare` names a module of the harness that makes what
  the program's own build makes before cargo: `bunprep.py` runs bun's build of its C and C++
  once per revision and adds a member, `bun-exe`, that rustc links with them.
- `RA_JIT_MEMORY` reports after every request a worker answers too (`request-N`), and on
  macOS its resident figure is the physical footprint rather than 0. A `heap-profile` build
  writes where it was loaded beside each heap profile, which `harness/heapfold.py` needs to
  name the stacks on macOS, and `harness/heap.py` gives the bytes to the salsa query whose
  value or record holds them. What they found
  ([#39](https://github.com/indexable-inc/ra-jit/issues/39),
  [docs/research/daemon-memory.md](docs/research/daemon-memory.md)): nushell's worker holds
  10.0 GB live once ready, 2.7 GB of it what a debugger is told of types; ripgrep's 1.6 GB;
  and the spare worker the daemon starts after the first build holds 940 MB more on ripgrep.
  salsa's `lru` cannot evict `infer` or MIR (they have cycle handling), and on `described`
  it freed 206 MB of ripgrep's live heap and none of its footprint, so no eviction is on.
- Proc-macro expansions are kept in the store
  ([#38](https://github.com/indexable-inc/ra-jit/issues/38)): a derive or attribute macro's
  output is found again by the next daemon, keyed by the macro library's content, the
  macro, its input tokens and the shape of their spans, the environment and the directory.
  An expansion that asked the compiler anything, or failed, is not kept. Function-like macros
  are kept with `RA_JIT_PROC_MACRO_CACHE=all`, a macro is left out by name with
  `RA_JIT_PROC_MACRO_CACHE_SKIP`, and `RA_JIT_PROC_MACRO_CACHE=check` expands anyway and
  reports any that differ ([docs/cache.md](docs/cache.md), `harness/expansions.py`). On a
  warm start helix's 1455 kept expansions take resolve from 6.0 to 4.3 s and the time to
  ready from 9.6 to 8.0 s, nushell's 1588 from 5.8 to 4.5 s and from 10.1 to 8.8 s (Linux
  x86-64, 16 cores of an otherwise idle host, medians of four runs each); cold starts
  (helix 35.5 and 35.2 s, nushell 36.8 and 36.7 s) and memory do not change.
- `#[ra_jit::cache]` ([#17](https://github.com/indexable-inc/ra-jit/issues/17)): the
  `ra-jit-cache` crate's attribute keeps what a pure function returns, in memory and under
  `target/ra-jit/fn-cache`, by its arguments and a hash of the code ra-jit compiled for it and
  for everything it reaches. Comments, formatting and edits elsewhere keep the results; an
  edit to anything it reaches drops them, live edits too. Functions that read statics that
  change, thread-locals or the system (I/O, time, randomness) are refused with where that is
  written. Under plain cargo it does nothing. `cargo ra-jit cache clear` forgets what was kept
  ([docs/fn-cache.md](docs/fn-cache.md), `harness/fncache.py`).
- `harness/abbench.py` runs builds of ra-jit against each other on `bench.py`'s edits, in
  interleaved rounds: edit and cold-start times, the workers' cycles on macOS and their peak
  memory. With `--train` it is the training of the release build's PGO.
- rustc's MIR optimizations, optionally: `RA_JIT_MIR_OPT=1` runs what rustc runs on a debug
  build's MIR (copy propagation, removing unused locals, storage markers and drops of what
  needs none, folding constant branches, simplifying the control flow and instructions),
  `RA_JIT_MIR_OPT=2` what it runs above `-Copt-level=0` that is ported (also dead stores and
  unreachable blocks). Off by default. The interpreter's tests run every program at all three
  levels and require the same result ([docs/research/mir-gap.md](docs/research/mir-gap.md)).
- `ra-jit mir <dir> --crate <name> [--level N]` prints a crate's MIR as ra-jit compiles it,
  in rustc's `-Zunpretty=mir` format, and `harness/mirparity.py` compares it with rustc's
  function by function: at level 1, 21.5% of functions are identical to rustc's, against 4.0%
  without the passes.
- The README has a table of how fast programs ra-jit builds with LLVM run against
  `cargo build --release`, written by `bench.py --runtime` and `benchreport.py` and kept current
  by the `bench-readme` workflow on the Linux benchmark machine. A run in which any program's
  output differs from cargo's is never published. The first, on Linux x86-64, has every
  program's output the same as cargo's; on macOS arm64, ripgrep, fd, bat and jsonrt do not
  build whole yet (NEON intrinsics ra-jit does not have).
- [docs/research/](docs/research/README.md): what 2026's Rust compiler work means for ra-jit,
  and the MIR gap with rustc with the production gaps ranked; rustc's pass sources in
  [docs/upstream/rustc-mir-transform/](docs/upstream/rustc-mir-transform/SOURCE.md).

- Tiers ([#6](https://github.com/indexable-inc/ra-jit/issues/6)): a program that takes
  edits with `ra-jit-live` is sampled while it runs, and the functions it spends its time in
  are compiled again with LLVM, at `-O3` with what they call built in (level 2) and then with
  the branch counts level 2 took as `!prof` (level 3), and swapped in as patches without a
  restart. An edit sends an edited function back to Cranelift in the same build.
  `RA_JIT_TIER=off` turns it off. The Game of Life window shows each hot function's level
  and the promotions as they happen; `demo/lifebench` and `harness/tiers.py` measure and
  check it ([docs/tiers.md](docs/tiers.md)).
- `ra_jit_live::tiers()`, and the sampler that the daemon turns on in a running program.
  It stops once the worker that reads it is gone.
- Benchmarks on real projects that keep the README current: `harness/bench.py` measures
  ripgrep, fd, bat, hyperfine, just, axum and nushell at pinned commits (cold build, no-op,
  four kinds of edit, and how fast ripgrep's program runs) against incremental cargo, on both
  tiers, keeping every sample; `harness/benchreport.py` writes the README table and
  [docs/benchmarks.md](docs/benchmarks.md) from the result; the `bench-readme` workflow runs
  them on a dedicated Linux x86-64 machine after pushes to main and nightly, and commits the
  numbers to main when they moved beyond noise.
- `harness/bench.py` measures what [#7](https://github.com/indexable-inc/ra-jit/issues/7)
  asks of real projects: helix, a bevy example and zed (1.7M lines) join the seven, every project gets two more
  edits (a new function, a struct field set wherever the struct is built), the cold build is
  run `--cold-runs` times against fresh cargo, each with an empty store, and as many times
  in a second checkout with the store the first filled, the store's size is kept, and `--tests`
  runs each project's test suite under `cargo ra-jit test` and `cargo test` and fails on any
  test that ends differently. A project whose build fails keeps what it measured, with the
  error, and the run goes on to the next. A run with anything wrong or failed still writes its
  result and ends with exit code 3; `benchreport.py` publishes a test suite that ended differently in the tests
  table (a program that prints something else is still refused), and the workflow commits the
  numbers, then fails.
- `bench.py --setups` builds every step again with the fastest ways to build Rust there
  already are ([#8](https://github.com/indexable-inc/ra-jit/issues/8)): mold, wild,
  `-Zthreads=8`, rustc_codegen_cranelift on a pinned nightly, all three at once, and
  `cargo check`, each into its own target directory; `harness/hotpatch.py` times an edit of
  a running program under Dioxus's subsecond (`dx serve --hotpatch`) against `cargo ra-jit
  build` and `cargo build` with a restart, stopping the program before the build as `cargo
  watch` does. The bench shell has mold, wild, `dx` and the nightly, on Linux and on macOS.
  Measured on a MacBook (`docs/benchmarks/hotpatch/macbook.json`): subsecond 346 ms
  median, `cargo ra-jit build` with a restart 469 ms, `cargo build` with a restart 520 ms; a
  live patch through `cargo ra-jit build` 12.5 ms and through the editor's socket 4.0 ms
  (`docs/benchmarks/live/macbook-probe-cargo.json`). On Linux, 16 cores of a c7i.16xlarge
  (`docs/benchmarks/hotpatch/linux-x86_64.json`): subsecond 289 ms, `cargo ra-jit build`
  with a restart 40 ms, `cargo build` with a restart 361 ms. On ripgrep's cold build mold,
  wild, `-Zthreads=8` and cg_clif together are fastest (3.25 s against cargo's 5.56 s), and
  every edit there and on nushell builds faster under ra-jit than under any of them, `cargo
  check` included; with nothing changed, on ripgrep, cargo with mold is faster (28 ms against 33).
- Benchmark history and regression alerts
  ([#10](https://github.com/indexable-inc/ra-jit/issues/10)): `harness/benchhistory.py`
  appends each run's medians to `docs/benchmarks/history/*.jsonl` and checks the last against
  the runs before it on the same machine; the `bench-readme` workflow commits the history on
  every run, starts the site's deploy, and on a regression past the noise band opens (or
  comments on) an issue labelled `bench-regression` and fails. The live-edit latency workflow
  runs every night on main on a macOS arm64 runner and does the same with its history.
- [ra-jit.ix.dev/bench](https://ra-jit.ix.dev/bench): the benchmark history over commits, from
  `docs/benchmarks/history`: the latest run's table per project, and charts of each edit,
  the cold build, the daemon's peak memory, the store size and live edit-to-running latency
  ([#10](https://github.com/indexable-inc/ra-jit/issues/10)).
- LLVM is given what rustc gives it: reference parameters marked `noalias`, `readonly`,
  `dereferenceable`, `align`, `nonnull` and `noundef` as rustc marks them, addresses as
  pointers rather than integers, branches threaded before `-O3`, unsigned overflow checked as
  rustc checks it, a second round of cleanup after `-O3`, and, at a level above 1, the bodies
  of the callees it keeps calling and of what those call. Level 3 of `demo/lifebench` runs
  at 88% of a release build and 94 to 96% of one that checks overflow as the dev profile
  does, from 71 to 74% and 77 to 81%, measured side by side on a loaded machine
  ([docs/tiers.md](docs/tiers.md)).
- `harness/tierbench.py`, the benchmark table of docs/tiers.md; `--together` runs the builds
  of a round at once, so that a loaded machine slows them alike.
- `RA_JIT_DUMP_LLVM=after` prints the LLVM IR as the passes left it.
- Live-edit latency under real typing ([#9](https://github.com/indexable-inc/ra-jit/issues/9)):
  `harness/live_latency.py replay` types scripted edits one key at a time at 30 to 100 ms a
  key, with pauses at word ends, typos and backspaces, sends the whole file on each key as an
  editor does, and times every key whose text compiles to the program running it, by patch or
  by a restart as a launcher would do one; it counts the keys that do not compile, the
  superseded edits and the restarts. `restart` times a session's struct changes against
  `cargo run`. Every table has p95 and p99, and `LATENCY_JSON` carries the machine, its
  battery, the commit and the run, which `benchhistory.py live` reads. `harness/live-bevy`
  (bevy 0.20, generic systems) and `harness/live-egui` (egui 0.37, generic widgets) are real
  programs that take edits. Measured on a MacBook Pro in
  [docs/live.md](docs/live.md#latency-under-typing).
- A borrow checker, the crate `hir-ty-borrowck` in the rust-analyzer subtree, written to give
  rustc's verdict: Polonius alpha by default as nightly rustc does, NLL with
  `RA_JIT_BORROWCK=nll`. It gives rustc's errors on
  1539 of 1581 of rustc's own borrow-check tests that it can run, and none on any of the 246
  rustc accepts, scored by `harness/borrowck_ui.py`, and finds nothing in ripgrep, fd, bat,
  hyperfine, just, axum, nushell, delta, eza, tokei, starship and cargo. Its errors, and rustc's
  `unused_mut` warning, are rust-analyzer diagnostics and `ra-jit borrowck` prints them ([docs/borrowck.md](docs/borrowck.md)).
- Item well-formedness, the crate `hir-ty-wfcheck` next to the borrow checker: rustc's E0309
  and E0310 outside function bodies, for struct, enum and union fields (`&'static T` without
  `T: 'static`), a trait impl's associated types, an impl of a trait bounded by `'static`,
  and the where clauses of ADTs, impls, traits and functions. Its errors are refused as the borrow checker's are.
- The daemon refuses to build an edit with a borrow-check error, as it does one with a type
  error; `RA_JIT_BORROWCK_REFUSE=0` turns that off. The checker still misses about a third
  of rustc's borrow-check errors, so `cargo check` stays the final word.
- `ra-jit borrowck`: the borrow-check errors of single files, or of a cargo workspace with
  `--project`.
- Inlining across crates, on by default in a program built whole with LLVM at `-O2` or
  `-O3`: it has what it calls in every crate built into it or shown to LLVM, the standard
  library's generic and `#[inline]` functions as rustc does across crates.
  `RA_JIT_INLINE=crate` keeps that to the function's own crate, `RA_JIT_INLINE=all` does it
  in other builds too. On Linux x86-64 (AWS c6i.16xlarge, at 13ee5a5339) the geometric mean
  of 14 runtime benchmarks was 1.15x the time of `cargo build --release` at `-O3` and 1.16x
  at `-O2`, against 3.89x with each crate alone; the programs other than the kernels took
  2.3 to 3.7 times as long to build as with each crate alone (ripgrep 30.1 s against 8.5 s).
  The README's table, measured at the merge into main, has 1.21x over 15 benchmarks
  ([README](README.md#how-fast)): numeric's ra-jit build took 57 ms in three of its five
  runs and 32 ms in the other two, where release's took 31 ms in all. A whole program's
  build from cold against `cargo build --release`'s from cold: ripgrep 31.7 s against
  8.4 s, tokei 50.7 s against 13.2 s, bat 125 s against 53.3 s, eza 38.9 s against 34.6 s.
  Commit 99cbb902ad's message
  says the levels alone turned it on; until b460bbe327 they did not, and it took
  `RA_JIT_INLINE=all`.

### Changed

- `unused_mut` a source denies fails the build, with the seven deny lints before it (#84).
  `harness/lints_ui.py --deny unused_mut` scores it on every rustc UI test: 51 right, 0 wrong,
  5 missed against the nightly, 49, 0, 5 against 1.99.0. `RA_JIT_BORROWCK_REFUSE=0` turns it
  off with the borrow checker's errors. `fn f(mut self)` gets it too, where none was given.
- Each debug type is described once, by itself, and the types made of each other are kept
  together as one group that every set of debug types reaching them shares
  ([#29](https://github.com/indexable-inc/ra-jit/issues/29)); the DWARF is the same. On Linux
  x86-64 (16 cores, `memsplit.py`, one run each) the daemon after its own base and edits holds
  7.36 GB instead of 7.63 on nushell, 5.87 instead of 6.55 on helix and 14.47 instead of
  15.97 on bun, at most 7.9, 6.1 and 15.8 GB at an edit instead of 11.7, 9.8 and 22.9, and the
  first edit after the base takes 0.37 s instead of 2.06 on nushell, 0.19 instead of 1.71 on
  helix and 1.51 instead of 7.23 on bun. zed's own base, which passed 64 GB, finishes with no
  bound of its own at 62.6 GB in 662 s; at the default bound, a quarter of memory, it is still
  given up (#188).
- A whole program's compiles under LLVM import what they call, as ThinLTO's backends do,
  where each built its callees again from MIR and optimized them from scratch. Each function
  a compile calls is lowered and run through LLVM's pre-link pipeline once in the process,
  and a compile links in the bodies small enough to be built in (100 instructions, 300 for
  `#[inline]`, the whole budget of 40 000 for `#[inline(always)]`), and those of what they
  call with 0.7 of the room each call deeper, 8 deep, as `available_externally`; what LLVM
  does not build in is dropped. Bun on 16 cores (c8a.24xlarge) builds in 118 s where it took
  564 s, against 135 s for cargo's release build, and runs 1.244x cargo's where it ran 1.267x.
  `RA_JIT_IMPORT=0` builds the callees in as before.
- The daemon keeps what each debug type is told as once, rather than in every set of debug
  types it is in ([#55](https://github.com/indexable-inc/ra-jit/issues/55)); the DWARF it
  writes is the same. On Linux x86-64 (16 cores, `memsplit.py`, one run each) the daemon
  after its own base and edits holds 7.5 GB instead of 9.7 on nushell and 6.4 instead of 11.3
  on helix, and bun's own base, which was given up at a quarter of a 124 GiB host's memory,
  is built at 18.9 GB, so its signature edit is a 6.3 s patch instead of a 32.8 s full link.
  The own base asks the allocator to give back what it freed before it gives up for memory.
- ra-jit allocates with jemalloc on Linux and macOS, and with mimalloc on Windows only
  ([#39](https://github.com/indexable-inc/ra-jit/issues/39)). mimalloc 3 kept more of what a
  build freed, transparent huge pages included: on Linux x86-64 (16 cores, `bench.py`, two
  interleaved rounds) the daemon after its edits holds 1.57 GB instead of 2.03 on ripgrep, 9.52
  instead of 10.80 on nushell, 11.07 instead of 12.13 on helix and 24.3 instead of 27.0 on bun,
  with the same edit times. `RA_JIT_MEMORY` reports the live heap without the `heap-profile`
  feature, which now only adds jemalloc's profiler.
- The daemon no longer keeps a spare worker loaded for the next project
  ([#56](https://github.com/indexable-inc/ra-jit/issues/56)). It starts one as it starts, for
  the build that started it, and one each time a build of a new workspace begins, which loads
  the workspace beside cargo as before; none waits once a workspace is loaded. On Linux x86-64
  (16 cores, 3 runs), the daemon after the edits of `bench.py`: ripgrep 3.63 to 2.31 GB, fd
  3.42 to 2.04 GB, nushell 12.70 to 11.36 GB, with the same edit times; an idle daemon 2.64
  to 1.28 GB. A second project's cold start is as before (ripgrep 4.74 and 4.86 s, nushell 30.5
  and 30.7 s); a warm one, which cargo had built, loads the library first: ripgrep 1.29 to
  2.62 s, fd 2.19 to 3.48 s, nushell 8.62 to 9.51 s (`harness/spare.py`, 3 interleaved rounds).
  `RA_JIT_SPARE=ready` keeps one loaded as before; `RA_JIT_SPARE=0` still starts none.
- With LLVM, `memcmp` is called by its own name in the function's module rather than as one
  of the functions asked for, so LLVM writes a comparison of a few bytes it knows the number of
  as loads, and one only for equality as `bcmp`, as it does for rustc. On Linux x86-64 (AMD
  EPYC 9R45) tokei calls the C library's comparisons 155.9 million times instead of 278.6
  million (cargo's release build: 155.1 million) and runs at 1.34 times the release build
  instead of 1.51. words' machine code is the same, 48 bytes further on.
- With LLVM at `-O3` the bodies of the functions a function calls are built into it before
  the default pipeline, and the branches on what they return threaded and the values
  numbered again, as is done for what Cranelift builds in: a loop over `i + 1..n` inside one
  over `0..n` counted with `phi + 1` before, which alias analysis can't see past. On
  Graviton4 (Linux aarch64) nbody runs at 1.28 times cargo's release build instead of 1.97,
  and words at 1.27 instead of 1.36; on Linux x86-64 (c8a.metal) words at 1.28 instead of
  1.37. Whole-program builds take 1-4% longer. x86-64's nbody is still 1.27: LLVM 20 weighs
  the five-body nest above its full-unroll threshold, and rustc's LLVM 23 below it.
- On macOS a patch whose unwind frames are larger than the program's unwind table is spliced
  rather than linked in full: both of the unwinder's tables move into the room the link leaves
  after its load commands, the program's frames where the table has them and the patch's
  behind. ripgrep's first edit (244 KB of frames, a table of 218 KB) is a patch there too
  (`harness/rustcbase.py`'s `moved-panic` and `cached-reached`).

- After a fresh build through cargo the daemon is ready for edits once it has loaded the
  workspace and read the symbols of the executable rustc linked, instead of after building
  every program once more itself ([#11](https://github.com/indexable-inc/ra-jit/issues/11)):
  the first edits are patches spliced into rustc's executable, and the daemon builds its own
  base in the background once nothing has waited for a second. On Linux x86-64 (16 cores),
  ready after a cold build from an empty store: just 10.4 to 8.9 s, ripgrep 7.8 to 6.6 s,
  nushell 58.8 to 47.0 s, against 7.4, 5.0 and 35.1 s for plain `cargo build`; from a filled
  store 10.7 to 8.7 s, 8.2 to 6.6 s and 64.1 to 46.3 s. That is 0.25 to 0.5 s after the
  command returns on just and ripgrep, and 3 to 4 s on nushell, which reads the symbols of
  233 000 functions. On macOS, nushell 43 to 36 s against 25 s for cargo. The command itself
  is as slow as before: the spare worker that loads the workspace beside cargo still costs
  1.0 s on just, 1.3 s on ripgrep and 8 to 9 s on nushell.
  The first edit over rustc's executable is slower than over the daemon's own build, since
  it reads the edited file's functions and resolves its names for the first time: just 0.08
  to 0.4 s, nushell 0.3–0.45 to 1.6–1.9 s. An edit that is more than function bodies, of a
  generic, `#[inline]` or default trait function or one that moves such a function's lines
  (rustc copies those into their callers, line numbers and all), of a static, or of a file
  whose functions were not read before is built in full, and the log says why
  (`no patch: ...`): ripgrep's `main` sits above a generic function, so its first edit takes
  1.4 to 1.7 s.
  The own base is done some 2.5 s after ready on just and ripgrep and 11 s on nushell.
  `RA_JIT_RUSTC_BASE=0` keeps the old behaviour. On macOS rustc's link of a program is given
  `-Wl,-headerpad,0x400000` and `-Wl,-bind_at_load`, which the daemon's own links leave out.
  `harness/rustcbase.py` checks every kind of first edit against cargo, and
  `harness/coldprof.py --max-wait S` fails when the daemon is ready more than S seconds after
  the command.
- `cargo ra-jit build` from cold is closer to cargo's, and readiness follows it sooner
  ([#11](https://github.com/indexable-inc/ra-jit/issues/11)). The daemon tells the spare
  worker what cargo built from a thread of its own and the spare reads its input from the
  start: before, a spare still loading the standard library let the pipe fill, which held up
  the daemon and with it cargo, whose output went unread. While cargo builds, the spare
  resolves the standard library and compiles the functions of each crate cargo is done with on
  a quarter of the cores, leaving the generic instances they reach to the edits, reads the
  symbols of each library as cargo writes it, and once cargo is done compiles nothing more
  before it is ready. The first edit reads the edited file's functions on every core. An edit
  of a generic, `#[inline]` or default trait function, or one that moves such a function's
  lines, is now a patch too when every copy rustc made of it is found in the executable: the
  patch replaces the copies, and ripgrep's first edit is a patch. Thread-locals of a patch
  over rustc's executable are reached as rustc's code reaches them (on Linux x86-64 the
  general-dynamic sequence becomes local-exec, on macOS through the variable's descriptor).
  On Linux x86-64 (16 vCPUs, 8 cores), interleaved runs from an empty store, command and
  ready: just 8.5 to 8.2 s and 9.0 to 8.6 s, ripgrep 6.6 to 5.7 s and 6.9 to 5.9 s, nushell
  43.5 to 37.6 s and 46.7 to 39.2 s, against 7.6, 5.4 and 35.7 s for plain `cargo build`. The
  first edit: just 0.36 to 0.5 s, ripgrep 1.7 s built in full to 1.5 s patched, nushell 2.0 s
  as before. The command's remaining cost is the spare's own: what it resolves of the
  standard library and the workspace takes cores from cargo (docs/cargo.md).
- An edit in a package whose build script cargo runs again for it (one that names no
  `rerun-if-changed`, as ripgrep's, fd's and hyperfine's) no longer waits for a
  `cargo check` that runs every build script of the workspace: the daemon runs that one
  script again as cargo ran it and compares what it prints and writes with what it loaded,
  as `cargo ra-jit` already did, and asks cargo only when the two differ, the script is older
  than its source, or it was compiled reading a variable cargo did not set. A script
  `cargo ra-jit` ran again for the same build, with nothing it reads written since, is not
  run a second time. On Linux x86-64 an edit of ripgrep's `search` goes from 72 to 48 ms,
  fd's `run` from 78 to 42 ms and hyperfine's `run` from 80 to 35 ms.
- ra-jit is built one way, whether downloaded or built by nix: the new `dist` profile (thin
  LTO), optimized with a profile of ra-jit's own work (PGO) trained on edits of fd, bat, just
  and hyperfine. `.github/scripts/build_release.py` is the recipe; the release workflow runs it
  for every target and the flake's `daemon` (so `nix run`, `nix profile install` and the demo)
  runs it inside its derivation. The profiles are trained by the new `pgo` workflow, which
  runs again whenever Cargo.lock, a manifest or the toolchain changes, and pinned by sha256 in
  `pgo/profiles.json`, so every build uses the same file; the build says how much of the code
  changed since its profile was trained, and a release refuses a profile of other crates.
  `cargo install --locked --profile dist ra-jit` builds the same profile without PGO, which
  starts about 10% and edits about 3% slower than a release's binaries: cargo takes no compiler
  flags from a crate, and crates.io's crates are not the ones a profile can be trained on.
  Against the plain `release` build, on Linux: the
  cold start 14% shorter on nushell and on ripgrep, an edit of a function 8% shorter on
  nushell, the same peak memory. mimalloc v3 stays the allocator: jemalloc, mimalloc v2 and
  snmalloc start colder and edit no faster (an edit spends 1.8% of its time allocating, 73%
  revalidating salsa's memos). `release` itself is unchanged and stays the build to work with,
  so rebuilding ra-jit after a change takes as long as before
  ([docs/reports/RELEASE-BUILD.md](docs/reports/RELEASE-BUILD.md)).
- Under `RA_JIT_BACKEND=llvm`, a body of more than 10 000 Cranelift instructions is compiled
  by Cranelift. Since LLVM lowers a handler that takes more than the exception (tiers), it
  compiled every function, and an edit of nushell's `main` (35 778 instructions) waited
  229 ms for LLVM: on Linux x86-64 the edit went from 343 ms to 472 ms, and is 280 ms now.
  65 of nushell's 160 177 functions are over the limit, none of ripgrep's or of
  `demo/lifebench`'s. A tier still compiles such a body with LLVM.
- `vendor/rust-analyzer` and `vendor/rustc_abi` are git subtrees instead of submodules: a
  plain `git clone` builds, and the flake builds from its own tree with no second pin of the
  forks.
- The first build of a workspace through cargo has the daemon load it and compile ahead
  while cargo builds ([#11](https://github.com/indexable-inc/ra-jit/issues/11)): a spare
  worker reads the workspace and compiles what it can while cargo runs, hears what the
  build scripts said and where the proc macros are, and is the worker the build is handed
  to. On nushell, Linux (32 cores): ready for edits 58 s after a cold build from an empty
  store instead of 71 s; cargo's own build takes about 3 s longer, 40 s instead of 37 s,
  while the worker loads beside it.
- A build through cargo asks the toolchain's `rustc` beside `cargo` for its version and
  sysroot, not the rustup proxy: 16.6 ms to 7.3 ms each of its three calls on Linux.
- An edit to function bodies only lists no functions again and finds the program's tests
  as they were: 5.5 ms to 0.5 ms of an edit of nushell's `main`.
- `RA_JIT_STATS` shows where a splice and the refusals before a link spend their time
  (`splice-us`, `link-us`, `refuse-us`).
- `harness/realbench.py --pause S` waits once the daemon is ready, as a person would before
  the first edit.
- `vendor/subtree.py` replaces `vendor/subtree.sh`. `push` sends what main changed under
  `vendor/` to the fork, one fork commit per commit of main, and only fast-forwards the fork;
  the `fork sync` workflow runs it on every push to main, so the forks no longer fall behind
  (rust-analyzer's had: "warm edits" never reached it). `upstream` merges an upstream
  rust-analyzer release into `vendor/rust-analyzer` in one command, with conflicts left in
  the tree as `git merge` leaves them; before, that took a clone of the fork and a pull.

- Code ra-jit gives LLVM is faster: SIMD intrinsics on 128-bit lanes of integers (add,
  subtract, bitwise, saturating, compare, `simd_bitmask`) are vector instructions instead of a
  loop over lanes, and LLVM's own; a slot whose address is rounded up to an alignment above 16
  (`__m256i`) is an alloca of that alignment, which LLVM can take apart; `memcpy`, `memmove`
  and `memset` are LLVM's intrinsics rather than calls it knows nothing of, so a copy of a
  known size is a few loads and stores.
- A program built whole runs faster with LLVM: a landing pad whose cleanup only resumes is
  left out, so LLVM sees a call that cannot unwind into anything; what a check that is off
  (`ub_checks()` and its kind) guards is not compiled, so its panics do not use up what may be
  built in; a `match` is a chain of
  comparisons of up to 256 that LLVM makes a switch of; bodies are built in and shown up to
  16 calls deep (an iterator's adapters reach `Vec::push` that deep); `offset` is a
  `getelementptr inbounds`, also once Cranelift has inlined it; and a body built to be
  measured and then not used gives back what its build counted of its call's share, so that
  `vec![false; n]`'s `from_elem` is shown to LLVM, which then knows the vector's length and
  drops the sieve kernel's bounds checks. A body shown to LLVM carries its `#[inline]`,
  `#[inline(always)]` or `#[inline(never)]` as rustc emits it. An address the code computed
  as an integer (from `ptrtoint`s, through adds, phis and selects) is rebuilt after the passes
  as pointers and `getelementptr`s on the pointers it came from, so that LLVM can follow what
  is stored and loaded through it: nbody's array of deltas leaves the stack. On Linux x86-64 the geometric mean at `-O3` goes
  from 1.20x (ra-jit f3eeb5295e) to 1.15x of release's time at 13ee5a5339.
- On macOS arm64, ripgrep, fd, bat and jsonrt build whole: NEON's pairwise maximum, minimum
  and add, its table lookups (`tbl1`, `tbx1`), `sqdmulh`, `uaddlp` and `saddlp`, the CRC-32
  instructions, and the generic `simd_reduce_max`, `simd_reduce_min` and `simd_ctpop` are
  compiled, lane by lane or as one instruction. On a MacBook Pro (Apple M5 Max, macOS 27.0.1),
  median of three runs: ripgrep 1.15x, fd 1.01x, bat 1.16x and jsonrt 1.26x of release's
  time, every output the same as release's.
- x86-64's `simd_select_bitmask` (AVX-512's masked blends), `simd_fma`, the AVX-512 two-table
  permutes (`vpermi2var` of bytes, words, dwords and qwords), SSE4.1's `phminposuw` and
  `vzeroupper` are compiled, lane by lane, which nushell's dependencies reach for
  (corpus `simd_x86`).
- AArch64's SHA-1, SHA-256, SHA-512 and AES instructions (`vsha256hq_u32`, `vsha512hq_u64`,
  `vaeseq_u8` and the rest, as sha2 calls them) are compiled, each as a function of the one
  instruction, and NEON's sums of every lane made wider (`vaddlvq_u8`) and doubled products
  made wider (`vqdmull_s16`) lane by lane. `simd_reduce_add_ordered` and `_unordered` (and
  `mul`) are compiled, which `vaddv_u8` and AVX-512's `_mm512_reduce_add_epi32` are, and
  `simd_masked_load` and `simd_masked_store` (AVX-512's `_mm512_mask_loadu_epi8`, in fff_search
  under nushell): a lane the mask leaves out is not read or written (corpus `crypto_lanes`).

### Fixed

- The borrow checker no longer panics on rustc's UI tests (`lints_ui.py --deny unused_mut`
  over all 20,450 files: 4 panics before, none now, and ra-jit's time from 67 s to 14 s). The
  causes were in the trait solver and the lowering it reads: a trait object's projections
  that become the same after substitution were deduplicated before being related, so
  `dyn A<X = T> + B<X = T>` failed to relate to itself (rustc relates them all, #136458); the
  `for<'b>` of an associated type's bound in a `for<'a>` was lowered as a binder of its own
  instead of concatenated onto the outer one, as rustc does; and upcasting a higher-ranked
  trait object built its supertraits outside the source's binder. Each has a regression test
  in ide-diagnostics, which panicked as rustc's test did before the fix.
- A 5,204-arm `else if` chain (rustc's `expr-stack-overflow.rs`, #206) is borrow checked in
  4.5 s, where it took over 300 s and then overflowed the stack: finding each expression's
  syntax walked down from the body's root, quadratic in the chain, and lowering, inference
  and MIR building recursed once an arm. The walk now goes to the child that contains the
  range, and the recursions grow their stack as rustc's `ensure_sufficient_stack` does
  (`stdx::ensure_sufficient_stack`). 1,000 arms take 2.2 s (5.35 s before), 2,000 arms 2.5 s
  (26 s); ide-diagnostics tests the 5,204-arm chain under 60 s.
- A destructuring assignment that moves out of a reference (`S { b, .. } = *r`) is an error
  at `*r`, as rustc reports it, and one that copies is not an error.
- ide-diagnostics's 24 failing tests pass (756 pass, 1 ignored), and CI's fast tier runs them
  in the fork job.

- A function the daemon compiles ahead, while it reads the symbols rustc built, is given up on
  once they are read, where the worker used to finish it before it was ready. bevy's second
  warm-store run began its derived `release_state` of a read-only query struct, which the
  trait solver takes about two minutes over, and was ready 148 s after the build in place of
  26 s; it is now ready in 26.5 s. A worker's own cancellation was not enough: salsa ignores it
  inside a query that recovers from cycles, as inference and MIR building do, so the threads are
  stopped as a write stops them, and the trait solver checks for that at each goal. A bin over
  bevy_ecs with that struct stopped compiling ahead 9 to 20 s after its symbols were read, and now
  within a second. `harness/aheadready.py` holds each function compiled ahead for ten minutes
  and checks that the daemon is ready within two.
- An expression of type `()` that computes nothing (an assignment, a block without a tail, a
  `break` or `return` without a value, an `if` without an `else`) gives its place the `()`, as
  rustc's MIR does: `let z: () = x = 1; let w = z;` was E0381, a build failed on code rustc
  accepts. A comment-only `asm!` writes its `inout` registers back, so the variable is
  mutated.
- A daemon's link that fails on names nothing defines, used only by code rustc's own link
  takes out, is run again as rustc's line has it, with `--gc-sections` (or `-dead_strip`,
  `/OPT:REF`). The daemon leaves those out so a patch can call what the executable's code no
  longer reaches, and zed's webrtc then needs X11's `libXtst`, which nothing links: its own
  base failed to link after 662 s ([#29](https://github.com/indexable-inc/ra-jit/issues/29)).
  An executable linked so takes edits that call into what it left out as builds in full.
- The borrow checker holds a struct or enum to what its fields need of its lifetimes,
  rustc's inferred outlives bounds, where it is named or built: `B(x)` with
  `struct B<'a, 'b>(&'b mut F<'a, 'b>)` needs `'a: 'b`. A closure passed for a bound with
  `for<'r>`, `impl FnOnce(&mut T)`, takes the bound's other lifetimes, so what it needs of
  them reaches its creator. A function item's return type is well-formed but for what names
  a late-bound lifetime, as rustc's: `foo` returning `(&'c (), PhantomData<&'b &'a ()>)`
  needs `'a: 'b` where it is a method's receiver. A trait object's associated type is held
  to the bounds its trait declares for it: `dyn Trait<Assoc = &'a ()>` with
  `type Assoc: 'static` needs `'a: 'static`. 1523 of 1580 of rustc's tests now (was 1520).
- A match arm with a guard binds what it binds by value once the guard holds, as rustc does,
  so `(mut long, ref short) if g => long.0 = &short` is accepted: `short` is declared first
  and dropped last. In a `const` or `static` a mutable borrow is promoted only of `[]` and a
  `const fn` call only where the item certainly runs it, so `helper(&mut 42)` gives E0716, as
  does `&(ALL[i] + 1)` in a function. An `impl Trait` inside the return type,
  `(impl Iterator, &'b str)`, gets the member constraints a returned one does, and the
  lifetime chosen for its hidden type is applied, so what that needs of a type parameter is
  checked (E0309). `borrowck_ui.py` reads each unit with its revision's `--cfg`, so
  `#[cfg($rev)]` in a macro is set. 1532 of 1581 of rustc's tests now (was 1523).
- Assigning `x` in `x[1][{ x = y; 2 }]`, where `x[1]` indexes a slice, is rustc's E0510: the
  bounds check already done must hold. A returned
  `impl IntoIterator<Item = impl Cap<'a> + Cap<'b>>` gets its member constraints, so a
  lifetime it does not capture gives E0700 with early-bound lifetimes as with late-bound
  ones. A closure assigning `bx.0` of a `Box` it captured from a `bx` declared without `mut`
  is rustc's E0594, which names the capture by the place it captured, `*bx.0`, as ra-jit now
  does. 1535 of 1581 of rustc's tests now (was 1532).
- A returned `impl Trait` holds the borrows of the lifetimes elided in its function's
  parameters, so mutating `self` while the iterator of
  `fn evens(&self) -> impl Iterator<Item = &i32> + '_` is live gives rustc's E0502; it was
  missed. Edition 2024 captures every elided lifetime, earlier editions the one `'_` or an
  elided lifetime in the bounds stands for (the lifetime of `&self`, else the parameters'
  only one), and `use<'_>` that one too. Type lowering in rust-analyzer gives the
  `impl Trait` a lifetime parameter for each, as rustc does (corpus `elided_captures`).
  1536 of 1581 of rustc's tests now (was 1535).
- The borrow checker proves `for<'a>` bounds for placeholders, as rustc does, and those an
  impl's where clauses need on the way: `&'static str: DeserializeOwned`, which needs
  `&'static str: for<'de> Deserialize<'de>` where only `'de: 'static` is implemented, gives
  rustc's "higher-ranked lifetime error", as does a forwarding impl that needs
  `T: for<'a> Bar<&'a isize>` of a `T` that is only `Bar<&'b isize>`. They were missed
  (corpus `higher_ranked`). 1539 of 1581 of rustc's tests now (was 1536).
- A returned `impl Trait` hiding a type parameter is checked when the function's lifetimes
  are late-bound or elided: `fn f<T>(d: T, p: &mut ()) -> impl Sized + '_ { (d, p) }` gives
  rustc's E0311, and with `p: &'b mut ()` its E0309. The borrow checker relates the hidden
  type to an elided `+ '_` bound, so `dest: &mut T` returned behind `impl FnOnce() + '_ + 'a`
  gives E0621. 1520 of 1580 of rustc's tests now (was 1517).
- The borrow checker reports what normalizing a signature's associated type needs of its
  lifetimes (`-> <&'a () as Visitor<'d>>::Value` proven by `impl<'a, 'd: 'a>`), and relates
  an associated type through a where clause's projection (`T: Fun<F<'a> = [u8]>` borrowing for
  `'a`); item checking reports what the impl of an associated type in a field needs
  (`<T as Dummy<'a>>::Out` that `T: 'a`). 1517 of 1580 of rustc's tests now (was 1511).
- A body imported through another was called with the importer's type for it: an export
  declares what it calls with the types it calls it through, and one has as an integer what
  another has as a pointer. LLVM takes a call of a body through a type it does not have to be
  undefined, and of `ptr::write::<ForOf>` imported through a function that had the address as
  an integer it kept nothing of what was written, so bun's parser crashed. Such a call is now
  made through the callee's own type, the arguments converted (a unit test of `llvm.rs`).
- On Linux AArch64, an `asm!` of a feature LLVM's assembler has no `.arch_extension` for was
  refused: constant_time_eq's `mrs {}, dit` under `#[target_feature(enable = "dit")]`, which
  failed bun's build there. Such a feature (`dit`, `bti`, `jsconv` and the others of a later
  Armv8 or Armv9, and `v8.4a` and the like themselves) now starts the block with `.arch` of
  the version that has it.
- `runbench.py` runs cargo's release build from beside ra-jit's, in the workspace, where it ran
  it from cargo's target directory. helix looks for its `runtime` next to itself, so only
  cargo's build warned that there was none, and helix was reported as running differently.
- The borrow checker panicked on the corpus's `release_state`, whose impl's where clauses
  name associated types of concrete types (`Option<&'static C>: QueryData<ReadOnly: ..>`), so
  every error in the corpus's main file went unreported. As rustc, it now normalizes those
  where clauses before using them, and relates two associated types by what they normalize to
  (a GAT's `Self::Item<'w, '_>` and `Self::Item<'w, 'static>` are the same `&'w T`, where it
  said "lifetime may not live long enough"). A panic now fails the check where it was
  skipped: `ra-jit borrowck --project` exits with an error and counts panics,
  `borrowck_ui.py` fails on a panic or on any error in a test rustc accepts, and CI checks the
  corpus and `mixfutures` crates.
- The borrow checker no longer reports "temporary value dropped while borrowed" on every
  field of a `tracing::trace!` with tracing 0.1.44, which names fields with a `const` in a
  block inside its callsite's `static`: a `const fn` method call there is promoted, as rustc
  promotes it, so cargo's edit build goes through again
  ([#109](https://github.com/indexable-inc/ra-jit/issues/109)). A trait object given for a
  type parameter bounded `T: 'a` lives for `'a`, as rustc's object lifetime default, where
  rust-analyzer made it `'static` and the checker said "lifetime may not live long enough"
  ([#84](https://github.com/indexable-inc/ra-jit/issues/84)). All of cargo 0.99 now checks
  clean, where five false errors were left: rusqlite's `params![..]` of locals (an impl
  header's `&dyn ToSql` is for any lifetime, not only `'static`), `unwrap_or(&|_| true)` (a
  closure capturing nothing is promoted), git2 callbacks borrowing a counter declared after
  them (a panic's path drops nothing for a value with no drop glue), and an `async fn`
  returning `impl Iterator + 'b` of `&'a self` with `'a: 'b`.
- A program whose `main` is C's, as bun's, is built as a whole program: LLVM gets every body
  and inlines across crates, where each function was compiled alone before. `derive(PartialEq)`
  of an enum compares the discriminants first, as rustc's does. bun's whole program, every
  function ra-jit's, runs at 1.28x cargo's release build on Linux x86-64 (was 5.80x) and 1.25x
  on macOS arm64, where it now links: `runbench.py` no longer passes `-C link-dead-code`
  (which dropped `-dead_strip`), and the names of C in ra-jit's Mach-O objects stay
  exportable, as bun's `napi_*` must be
  ([#35](https://github.com/indexable-inc/ra-jit/issues/35)). The build takes longer, 565 s
  against 87 s on 16 cores: every compile carries the bodies it may inline.
- Adding a function to bun took 19.8 s, more than cargo's 14.5 s; it is a patch now, 0.64 s on
  Linux x86-64 and 0.56 s on macOS arm64. An added function in a program whose every function
  is an entry no longer forces a full build, and a crate the daemon listed before the files
  its `include!`s name were loaded is listed again once they are (`harness/latelisted.py`), so
  bun's 2,834 generated C-ABI functions are entries from the start. The edit's peak memory
  falls from 33.5 to 27.3 GB.
- helix's own build threw its first round away (about 30 s) because std's `Context::with`
  closure was refused for "a writable static without a rustc symbol": the static is declared by
  `thread_local!` in a body, which rust-analyzer expands twice, and ra-jit looked for the
  static's place in the other expansion. It finds rustc's symbol now, and the build takes one
  round: the daemon's own build in the background is done at 114.5 s instead of 130.3 s, a
  second daemon's at 85.9 s instead of 109.4 s, and with `RA_JIT_RUSTC_BASE=0` the daemon is
  ready at 116.2 s instead of 142.3 s (Linux x86-64, 16 cores, medians of three). Nothing is
  left to rustc for it, and `harness/norustc` counts the same functions left to rustc on
  ripgrep (53) and nushell (534) (#37, no-rustc step E).
- A `#[global_allocator]` in an inline `#[cfg(test)] mod test { .. }` of a library (ureq's)
  gave the library's stand-in an allocator outside the test build, whose functions nothing
  defined: every allocation of the program reached the stand-in's trap, so nushell, which links
  ureq, stopped at its first allocation. The `#[cfg]`s of the inline modules an allocator is in
  are now the forwarding allocator's.
- A constant that read a static holding `&dyn` of another static failed to evaluate
  (`InvalidVTableId`): the vtable was the id the static's own evaluation gave the type, which
  meant nothing to the reading evaluation, and a static the reference pointed into was not
  looked through for it. rustls's `ALL_CIPHER_SUITES` is one, which kept nushell from linking
  (corpus `dyn_static_suites`).
- A tuple struct with fields seen in other modules than the struct, both `pub(in path)` (ring's
  `aes::Counter`, `pub(in super::super)` with a `pub(super)` field), had no constructor:
  the least of those visibilities was folded from none, which is none. Its construction and
  patterns were refused, and with them ring's AES-GCM under nushell (corpus
  `narrow_constructors`).
- An `async fn` or block that awaited the `impl Future` of a provided method of a trait,
  which the impl leaves as the trait has it, had no layout: the trait's opaque type was left
  for the solver, which reveals nothing of a trait's, and came out an error. It is the hidden
  type of the trait's body now. rmcp's `ServiceExt::serve` is one, in nushell's MCP server
  (corpus `provided_futures`).
- `-C force-frame-pointers` in a build's flags keeps frame pointers in ra-jit's code too: LLVM
  gave the register to values, so code that walks frames by it read garbage. bun, built with
  every function ra-jit's, faulted in `bun_core::return_address` on its first `-e`; it now
  gives cargo's output on each of its runs. `harness/apps/apps.json` gives bun its own build's
  `-Cforce-frame-pointers=yes`. `harness/bun-repros/frame_pointers`.
- On ELF, a build that no edits follow puts each function and datum of its object in a
  section of its own, as rustc does, so the link takes out what nothing reaches. A program
  whose `main` is the C runtime's has every function compiled, and bun's link was missing
  what only such code calls and nothing defines on Linux: `GetLastError` of
  `bun_windows_sys`, and `ShellInterpreter__getConstructor` of a binding bun's C++ leaves
  out. The daemon's builds keep one section, as their links keep everything.
  `harness/bun-repros/unreached_externs`.
- A crate a build names after the program is one the program links, also when a binary of
  another package has its name: a build of bun with every crate ra-jit's took `--crate
  bun_sema` for the library and for `bun_sema_standalone`'s `bun-sema` too, whose code calls a
  library bun does not link, and the link failed on `bun_sema_standalone::parse`.
  `harness/bun-repros/same_name`.
- A `#[no_mangle]` or `#[export_name]` static of a program whose every crate is ra-jit's is
  defined under that name, also when it is private and when no Rust code uses it. Our copy had
  a name of its own, as rustc's was the one code of another language found, and one only that
  code reads was not compiled at all: bun's link was missing `Bun__version`,
  `Bun__Node__ZeroFillBuffers` and the other statics its C++ reads.
  `harness/bun-repros/exported_statics`.
- An `$n:expr` passed on to another macro is one operand there, as rustc has it: in
  `tags!(@count (0u8 + 1u8) + 1u8,)` the rule `(@ $n:expr,)` took `count (0u8 + 1u8)` for a
  call and matched, so bun's `task_tag::COUNT` expanded to nothing and `TaskTag::name`, the one
  function of bun's left to rustc for a reason other than having no instance, failed on
  `[&str; {const error}]`. The parser now stops an expression where such a fragment starts.
  `harness/bun-repros/macro_fragments`.
- A `macro_rules!` that a macro of another crate defines, called by a name a `use` imports
  (`use cfg_if::cfg_if; cfg_if! { .. macro_rules! trace { .. } }`), is in the textual scope of
  what follows. The call waited for imports to be resolved, by when `#[macro_use]` had taken
  the module's macros to its parent without it, and every `trace!` of lol_html's parser was a
  missing expression: 43 of bun's functions (the `StateMachine` impls of `Lexer` and
  `TagScanner`, `TransformStream::write`) were left to rustc. Such a call is now expanded where
  it is written, as one by a path to the other crate was. `harness/bun-repros/cfg_if_macros`.
- `--crate NAME` names no crate of the standard library's when a crate of the program has the
  name. std vendors foldhash 0.2.0, libc and hashbrown, and its foldhash, which rustc does not
  build (hashbrown has it only with its default hasher), sees no `core` in rust-analyzer's
  graph: bun's whole-program build compiled it beside bun's two foldhash, and its 27 functions
  that did not lower were counted as left to rustc. `harness/bun-repros/std_copies` checks it.
- A dependency's impl that a macro writes in a function body is found: dtoa's `write` calls
  `dtoa!`, which calls `diyfp!`, which writes `impl DiyFp` there, and `normalize_boundary`
  went unresolved because no `impl` is in `write`'s own text. A dependency's function is now
  looked into for impls when it calls a `macro_rules!` of its crate or of a dependency that
  writes one, in its text or through another macro. The dtoa functions bun left to rustc
  compile; `harness/bun-repros/macro_impls` builds dtoa.
- On Linux AArch64, an `asm!` of an instruction that the function's `#[target_feature]`
  enables and the target's baseline lacks (zlib-rs's `crc32w`, in bat) was refused by the
  assembler: the block is assembled apart from its function, so it now starts with the
  `.arch_extension` of each such feature. The runtime benchmark's whole-program builds of
  tokei and eza no longer fail to link there: they kept rustc's object of the names a
  program keeps out of the line, which is how GNU ld takes the panic runtime (#78).
- A checkout that an edit broke could be answered ok, linked with rustc's copy of the function
  from before the edit: a program whose build was interrupted, or panicked, or whose checkout
  was loaded again with the same link, forgot what rustc's copies are copies of, and its next
  build took the source as it was then for what rustc compiled. It keeps that now (#71).
- An edit of code two programs share ran wrong with the LLVM backend when the daemon compiled a
  program's `main` itself (on macOS and Windows, whose temporary paths go through a link): it
  took that build for one of a whole program and inlined across crates, and a caller holding
  the old body of an edited function was kept, as what is kept is compared by each function's
  own code. A daemon or `--watch` no longer inlines across crates; `programs.py` with
  `RA_JIT_ALONE=1` builds both programs that way in CI (#63).
- Under plain cargo, a test that ran the workspace's program could find none: the build the
  daemon loads afterwards ran beside the tests, and linking the program into place again
  deletes it first. It now waits for the user's cargo to exit (#61).
- `cargo ra-jit stop` returns once the daemon has exited, with its workers stopped and its log
  written, rather than once its socket is gone; nothing starts a spare worker while the daemon
  ends, where one could outlive it. A client that replaces a daemon of another version waits
  the same way (#65).
- A daemon's socket is free as soon as the daemon lets go of it, also while another thread of
  the process starts a program, which shares the lock's file until it runs (#58).
- The daemon's own base, built once nothing is asked after a cold build, started over every
  second and was never done when a crate it loads includes a file that is not there: zed's
  `livekit_api` reads `OUT_DIR`, which only a dev-dependency's feature brings into the
  daemon's crates, and whose build script cargo never ran. A file an `include!`,
  `include_str!` or `include_bytes!` names and that cannot be read is now read again once it
  is there, rather than taken for one still to load before each request
  (`harness/rustcbase.py`'s `unbuilt-member`). And since a base nobody waits for must not
  take the machine, a worker that holds more than a quarter of its memory while it builds one
  gives it up and says so; edits go on patching rustc's executable (`RA_JIT_OWN_BASE_MEMORY`,
  `own-base-budget`). zed's base, which the loop had hidden, passes it: 51 GB of 74 GB live are
  debuginfo types
  ([#55](https://github.com/indexable-inc/ra-jit/issues/55)).
- bat builds whole: an `impl` inside a function body of a dependency (bincode's `EnumAccess`
  for `&mut Deserializer`, inside `deserialize_enum`) was not found, as rust-analyzer only
  looked into the bodies of workspace crates; a dependency's file is now lexed for an `impl`
  inside a function's braces, and only the functions holding one are lowered.
- A whole program's build no longer builds a body it found too large again at each call
  that allows no more: with what such a body counted given back, ripgrep's build across
  crates took 416 s on Linux x86-64 and numeric's 151 s; 30.1 s and 8.7 s now.
- LLVM's `instcombine`, as ra-jit's pipelines name it, runs without verifying that it reached
  its fixpoint, as the one in `default<O3>` does: an LLVM built with that check ended the
  process on a function of jsonrt, and of nushell at `-O3` across crates.
- fd built with inlining across crates crashed the LLVM backend on Linux and macOS: an address
  rebuilt as a `getelementptr` may be a sum that is a constant (a global's address plus an
  offset), which is in no block, and LLVM read freed memory when the builder was put before it.
- A `const { .. }` block in a function is a constant evaluated for each instance of the
  function, as rustc's inline consts are, and no longer code the function runs: what only
  compile time may run in one (`#[rustc_comptime]`, `type_info`'s `field`, bun's
  `MultiArrayList::check`) was refused with "intrinsic `type_id_field_representing_type`",
  and every function reaching it with it (591 of bun's 680 refusals on Linux). A block met
  while evaluating a constant runs in that evaluation, so its `TypeId`s compare. `n += i` on an
  integer variable whose operator was not looked up while its type was unknown is lowered as
  the built-in it is, where it was "adjustment of binary op was missing" (`bun_repros.py`
  `type_info`).
- A constant of a struct, enum, tuple or array type given as a const generic argument
  (`adt_const_params`: `check::<{ Requirement::Within { low: 3, high: 9 } }>`,
  `filtered::<{ Filter { mask: 0xf0, strict: true } }>`) compiles and evaluates, and
  `#[derive(ConstParamTy)]` expands. Every function reaching one was refused ("constant of type
  JSType", bun's `alloc`, `test_correct_state` and `completions`) (`bun_repros.py`
  `adt_const_params`).
- A path through a type alias of an inherent impl (`ThreadPool::Worker::get(..)`, `Self::Slot`,
  `Slots<u32>::Slot`, nightly's `inherent_associated_types`) is the alias's type: the
  functions that name one had no type for it and were left to rustc, and rustc's copy of
  bun's `run_nested_rename_task` sent every edit of its crate to cargo (`bun_repros.py`
  `inherent_associated_types`).
- A build in full no longer keeps rustc's copy of an instance that did not compile once an
  edit changed it, as `located::<u8>` of a generic function with a `Location::caller()`
  constant, which `main` calls: the program printed what it did before the edit. Such an edit
  goes to cargo, which names the function (`soakrows.py`, "an instance rustc's copy stands in
  for, edited").
- A static declared in a function's body, as the `FLAG` of a method of bun's
  `impl<const SSL: bool, ..> NewServer<SSL, ..>`, is rustc's in a crate with a function left
  to rustc that reaches what ra-jit can't tell: rustc's copy of that function reaches the
  static through rustc's copy of the other. A build in full had its own copy beside rustc's,
  and the two counted apart, in the base the daemon builds after the first patch
  (`soakrows.py`, "statics one codegen unit uses").
- A borrowed constant with a cast in it, `&[b"node" as &[u8]][..]` returned for `'static`, is
  promoted as rustc promotes it, where it was built on the stack and the reference outlived
  it: every cast is promotable but of a pointer to an integer. bun's `new Bun.Transpiler()`
  crashed on it (breakers, `promoted_index_of_casts`).
- A `match` arm's guard with `let` (`if let` guards and their chains) dropped what it bound
  and its temporaries at the end of the guard, before the arm ran, in editions before 2024;
  rustc keeps them until the arm ends or the next arm is tried
  ([#46](https://github.com/indexable-inc/ra-jit/issues/46)). The four rustc tests that
  caught it run on every pull request now (`harness/ui-pinned.txt`).
- The harnesses set `RUST_BACKTRACE=0` for what they run rather than unsetting it: cargo's
  `[env]` (as in `~/.cargo/config.toml`) fills in only what is unset, and a backtrace changed
  what a panicking program printed.
- Large workspaces are ready in a fraction of the time and memory
  ([#29](https://github.com/indexable-inc/ra-jit/issues/29)): a closure's type holds its inner
  closure twice, so the store's name for a type of nested closures doubled with each level
  (one of helix's was 665 MB). A type is now spelled once per daemon, and one spelled longer
  than 1 KB is named by the SHA-256 of its spelling; shorter names, and the store entries
  under them, are unchanged. helix is ready in 109 s at 18.4 GB of memory instead of 631 s at
  100.7 GB, its store is 1.6 GB instead of 14 GB, and its first `cargo ra-jit run` takes
  46.9 s instead of 640.8 s (`harness/deepnames.py`).
- A `Box` beside an allocator that has a size (`Box<[T], &Arena>`, as bun's arenas) is
  dereferenced and dropped through its pointer wherever its layout put it: a deref of one was
  left to rustc, and its drop read the pointer from the start of the `Box`. `breakers.py`
  runs the crates of `NIGHTLY_CRATES` with RUSTC_BOOTSTRAP=1 (`allocated`, `allocator_api`).
- `simd_reduce_max` and `simd_reduce_min` of integer lanes are compiled (AArch64's `vmaxvq_u8`
  and the other `vmaxv`/`vminv`, AVX-512's `_mm512_reduce_max_*`): a function that reached
  them was left to rustc (bun's ASCII checks; `simd_reduce_max` breaker).
- `RA_JIT_OPT=3`'s last instruction combining no longer aborts the process when one run of it
  leaves something to combine ("Instruction Combining … did not reach a fixpoint after 1
  iterations", bun's whole-program build on AArch64): the default pipelines never ask LLVM to
  check that, and a pass named in a pipeline did.
- `apps.py`'s and `runbench.py`'s whole-program build of a program whose C calls its Rust
  by an unmangled name (a `#[no_mangle]` function) goes on when cargo's link of the stand-ins
  fails for want of those functions: what is taken from it is the link line, and ra-jit's
  objects define them. It ended at "cargo failed" (bun; `nativecalls` in `apps.json`).
- What stands in for a crate in cargo's build (the source with no code that cargo links when
  ra-jit compiles the crate: `apps.py`'s and `runbench.py`'s whole-program builds, and the
  crates the daemon builds while cargo builds the rest) builds with a nightly cargo that names
  each dependency twice, its `.rlib` and its `.rmeta`. It had an `extern crate` for each
  (`E0259`, bun 1.4.3 on `nightly-2026-09-15`).
- A `macro_rules!` that a macro of another crate defines (`dep::define_log!(log)`, as bun's
  `bun_core::define_scoped_log!` at the top of a file) is in the textual scope of the modules
  after it, as in rustc. rust-analyzer expanded a call by a path only after the file's modules
  were collected, so their calls of the macro were missing expressions and their functions were
  left to rustc (`IncompleteExpr`); in bun one such function in `bun_runtime` sent every
  signature edit of the crate to cargo (`harness/breakers`, `macro_defined_by_foreign_macro`).
- `harness/apps.py`'s perf stage ends with its timings again: since the rename from rustd it
  stored ra-jit's as `ra-jit_s` and printed `ra_jit_s`, a `KeyError` after every workload.
  wasmrun builds wasmtime 36.0.19 (was 25.0.3, which Dependabot's alerts name); its workload
  is the same as rustc's on both.
- `cargo ra-jit run` from many clients of one checkout at once starts each client's program,
  on macOS too. A package with a `default-run` (or one program of several) went to plain
  `cargo run`, which on macOS removes and copies again the program cargo has under
  `target/debug` before it starts it, so a client that started it meanwhile found no file
  ("No such file or directory") or hung before its first instruction; and rustc built the
  edit again. ra-jit now runs the `default-run` itself, holds cargo's lock on the build
  directory from putting the daemon's programs in place until they have started (a `cargo
  test` executable too), takes it to give cargo back its own, and on macOS puts a program in
  place as a clone, as cargo does: a program started from a hard link just made can be
  killed as it starts ([rust-lang/cargo#7821](https://github.com/rust-lang/cargo/issues/7821)).
  `cargo_ra_jit.py`'s 16 clients on 4 worktrees run 8 rounds of edits, all by the daemon.
- On Linux a program `cargo ra-jit` started just after a full link failed now and then with
  "could not execute" (exit 1; another row of `cargo_ra_jit.py`'s flags each run): the daemon
  writes what a debugger reads of a new executable in place while it waits for the next edit,
  and the kernel refuses to start a file open for writing ("text file busy"). The daemon now
  writes an executable in place under an exclusive lock on it, and a client starts a program
  under a shared one.
- A program or test `cargo ra-jit` starts itself has what the `[env]` of cargo's
  configuration sets, as under `cargo run` and `cargo test`: the nearest `.cargo/config.toml`
  (or `config`) says a variable, then `$CARGO_HOME`'s, with `force` and `relative`.
- `cargo_ra_jit.py`'s life workspace no longer races with itself on macOS: the test that runs
  `cargo build` inside and the one that starts `other` take turns, since cargo removes and
  copies each program again there on every build and the second sometimes found none (plain
  cargo failed it as often as ra-jit).
- `harness/ui-known.txt` names the 15 tests of rustc's tests/ui run-pass that differ on the
  whole suite, each with why (11 assert a bug of rustc's, 4 are ra-jit's to do), so the
  `claims` gate says only what is new; a line can name the backends it is known on.
- `harness/debuginfo.py`'s perf check takes a sample in a naked function without a line as
  rustc has it (its body is assembly, which rustc gives no line either), not as lost debug
  info: one of the corpus's on Linux arm64 failed it now and then. It reads every frame perf
  prints, those of inlined functions and those whose names have spaces (`<T as Trait>::f`)
  too: it skipped them, so where perf looks CFI up a page off the frame (both Linux runners)
  the frame it took for the one sampled could be one unwound to anywhere, a static on Linux
  x86-64 once. rustc's corpus binary has as many frames unwound to data there.
- `breakers.py`'s `frame_of_tables`, `frame_of_results` and `frame_of_array` ask whether
  ra-jit's frames fit where rustc's do, not whether they are as large: a whole program's
  build with LLVM makes them smaller than rustc's, which each case's crate has room for too,
  and they differed. `frame_of_array` is open on Cranelift alone, whose frames keep three
  arrays where rustc's keep two.
- `cargo_ra_jit.py` says what a client that failed or hung said, and what it was doing,
  rather than ending on a timeout with nothing.
- A workspace the daemon loads after a change whose build scripts did not all run is not
  loaded: the crates of a script that failed were read without its `OUT_DIR`, so thiserror's
  `include!` went missing, every derived `Error` was unimplemented and `?` resolved to the
  body-less `FromResidual::from_residual` for the rest of the daemon's life (what hyperfine's
  edits under plain cargo met now and then on Linux before the edit stopped running every
  script through cargo). It says which script failed instead, as an edit that runs the
  scripts again already did.
- `bench.py` without `--reference` built plain cargo into `target-ref` inside the checkout,
  which cargo takes for a source of the package: ripgrep's, fd's and hyperfine's build scripts
  name no `rerun-if-changed`, so cargo ran them again and built the package again at every
  build, and "nothing changed" took cargo 300 to 390 ms instead of 30 to 60. The README run of
  2026-10-20 at `c8f58ac7d9` had it; cargo's target directories are now outside the checkout
  (`WORK/reference` by default).
- `nix profile install github:indexable-inc/ra-jit#ra-jit`, as the README says to run it,
  installs ra-jit and the programs it runs; the flake had no package of that name.
- A release's archive and the flake's build have `ra-jit-rustc-first`, which `cargo ra-jit`
  runs in the first build of a workspace; they had only ra-jit, cargo-ra-jit and ra-jit-rustc.
- A module whose file is outside its package, by `#[path = "../../shared/outside.rs"]` as
  bun's runtime has, is compiled by ra-jit and its edits are built. The file was in no
  package's directory and so not loaded: the functions that called into it were left to
  rustc, an edit to it was not seen and the program kept running the old code, and in a
  binary crate the daemon could not load the workspace ("unresolved module ... [E0583]").
  The file is now put in the source root of the one that declares it, as a file an
  `include!` names is. `soakrows.py`'s "a module file outside its package" fails without
  this.
- An edit that could change a function ra-jit left to rustc from the start, such as an item
  added elsewhere in its crate, is built by cargo, as docs/cargo.md says. It failed the
  build: when the function was one the frontend turned down, ra-jit took that for an error
  of the source ("has an error ra-jit cannot say more of"), and every build after it failed
  too until cargo built once. A function the frontend turns down only after an edit is still
  reported as an error. Found building bun; `soakrows.py`'s "an item added beside a function
  left to rustc" fails without this.
- A worker that loaded its workspace beside cargo's first build
  ([#11](https://github.com/indexable-inc/ra-jit/issues/11)) ends when its daemon does, as
  every other worker does: once told what cargo built, it waited for a lock on its standard
  input that it held itself, so a daemon that crashed or was killed left the worker running,
  holding its memory (2 GB on fd) and its socket, and the next daemon's first build fell
  back to cargo ("another daemon serves"). `cargo_ra_jit.py`'s "workers of the killed
  daemon are gone" fails without this.
- fd, tokei and eza build whole and run as cargo's builds do: a crate rustc built nothing
  of keeps its `static`s ra-jit's own (fd's `INIT`); a package built only for a build script
  or a proc macro, which `cargo metadata`'s single graph can still show as a dependency of
  the program's, checks overflow as the program does, so rust-analyzer's view of it no
  longer mixes two profiles; and `runbench.py` turns the release profile's `lto` off for the
  whole build, which needs rustc's libraries. bat and nushell do not link yet.
- A program ra-jit builds whole, with its own `main` (`runbench.py`'s builds), ran: on macOS
  the code-free stand-in rustc builds for the program had a weak `main` of its own, and
  ra-jit's object was weakened too, so the linker kept the stand-in's and the program
  exited 0 at once, printing nothing. ra-jit's `main` now stays strong. On macOS and Linux
  a `#[global_allocator]` was ignored in such a program, as the stand-ins declare none and
  rustc's allocator shim went to the default allocator; a stand-in now declares one that
  forwards to functions rust-analyzer's expansion of the attribute defines, under the
  `#[cfg]`s of the static and of the modules it is in, and not for one in a string, as
  heapless's docs have (`wholeprogram.py`).
- `&(1 / 2)`, a division by a constant, and `&mut []` are promoted to constants as rustc
  promotes them, so a reference to one lives as long as the program.
- Before the 2024 edition, a panic in a block's tail drops the tail's temporaries after the
  block's locals, as rustc does; ra-jit dropped them first, so a `Drop` that printed or
  locked saw the other order (`breakers.py unwind_tail_after_local`).
- What leaving a scope early drops (`break`, `continue`, `return`, a future dropped at an
  `await`) is placed on the line where the scope ends, as rustc places it, so a backtrace or
  a debugger's step through those drops shows the same lines under ra-jit as under rustc.
- On macOS, signing a splice reuses the hash of each appended page that is the same as the
  last splice's: 21 ms to 5 ms of each edit of nushell.
- The value of a `match` arm's guard was marked dead before the branch that reads it; with
  storage markers kept, as rustc's optimized MIR keeps them above `-Copt-level=0`, the guard
  could take the wrong arm.
- The code of a function grows in a straight line with the arms of a match that leave
  through `?`, as `#[derive(Serialize, Deserialize)]` writes for an enum: it grew with
  their square, 329 MB for one function of 285 arms, which an AArch64 branch cannot reach
  across ([#5](https://github.com/indexable-inc/ra-jit/issues/5)). `harness/growth.py`
  checks it.
- An edit that arrives while a function is being compiled at a higher level no longer waits
  for LLVM: the compile lets go of the database before LLVM runs.
- A workspace under a directory whose name has a space in it is built against what rustc
  built of it ([#14](https://github.com/indexable-inc/ra-jit/issues/14)). The daemon cut
  the dep-info file that names a crate's sources at every space, found none of the
  workspace's crates among rustc's, and named their functions itself, so a function it
  refused had no copy of rustc's to link to. Dep-info is now read and written as rustc writes
  it, in one place: a space in a path is `\ `, and `$`, `#` and backslashes are themselves;
  the values of the variables in it are unescaped too. `harness/spaces.py` builds a
  workspace under such a directory end to end.
- On macOS a program whose unwind table is larger than 1 MB takes edits as splices, and so
  live in the running program ([#13](https://github.com/indexable-inc/ra-jit/issues/13)).
  The daemon gave Apple's linker a fixed `-headerpad` of 1 MiB, and a splice moves the
  table into that room, so a GUI app of 570 crates with 3 MB of table, or nushell with 3.2
  MiB, linked every edit in full. The link now reads its headerpad from a file beside the
  executable, last on the line: 1 MiB, or what the line's own flags ask for. After each full
  link the daemon reads the header, and when the room is short it links once again with what
  the table needs and a quarter more, which the links after start from, in later daemons
  too. nushell's edit of `main` went from 878 ms to 276 ms on an M5 Max, and a library edit
  from 693 ms to 166 ms. `harness/headerpad.py` checks it.
- The first edit after the daemon is ready is as fast as the ones after it
  ([#12](https://github.com/indexable-inc/ra-jit/issues/12)). Two things made it slow.
  On Linux the first splice after a full link took 2.5 s of nushell's: it copied every
  debug section it added to, about 500 MB, behind the code it appended, and it cut the
  file to size, which on XFS writes out at the close all of the file the linker had not
  yet written. The sections now go where splices write them right after the link, and a
  splice only lengthens the file, so the first splice's link takes 7 ms, as those after it
  do. And code the daemon took from the store does not say where in the checkout it is
  from, so the first edit of a crate it came from compiled all of it and wrote the whole
  object again: 71,661 functions of nushell. The worker now compiles that code again
  by itself after its first build, and the daemon says the checkout is ready once it has.
  nushell's first edit of `main` after a cold build from a warm store went from 9.1 s to
  0.37 s on Linux and from 7-8 s to 0.3-0.4 s on an M5 Max. `harness/storefirst.py`
  checks it.
- On macOS a spliced executable lists its sections in the order of their addresses, and
  each segment a splice adds has a section, as `dyld_info` requires: it read none of the
  spliced executables on macOS 27 (`harness/splices.py`: 11 of 71 right, now 71).
- Tiered code that calls what the executable has no symbol of (`memcmp` where rustc's code
  calls `bcmp`, a `pthread_` function only rustc's inlined code reached) no longer has the
  program linked in full and started over at level 1, after which every later patch of it
  was linked in full too: that function stays where it was, and says why
  ([docs/tiers.md](docs/tiers.md), "Code no patch can hold").
- The thread that compiles at a higher level has a stack of 64 MiB, and the bodies a body
  built in shows are ten deep at most: a deep chain of them overflowed the 2 MiB default and
  took the worker down.
- Tiered code that calls what the executable has no symbol of (`memcmp` where rustc's code
  calls `bcmp`, a `pthread_` function only rustc's inlined code reached) no longer has the
  program linked in full and started over at level 1, after which every later patch of it
  was linked in full too: that function stays where it was, and says why
  ([docs/tiers.md](docs/tiers.md), "Code no patch can hold").
- The thread that compiles at a higher level has a stack of 64 MiB, and the bodies a body
  built in shows are ten deep at most: a deep chain of them overflowed the 2 MiB default and
  took the worker down.

## [0.1.0] - 2026-10-20

The first release.

### Added

- `ra-jit`, a compiler daemon that keeps a workspace loaded in rust-analyzer and generates
  code from its MIR with Cranelift. After the first build, an edit rebuilds only the
  functions that changed.
- `cargo ra-jit build|run|test|check`, with cargo's own flags. Anything the daemon cannot
  build falls back to cargo, and says so.
- `ra-jit enable` and `ra-jit disable`: route plain `cargo build|run|test|check` through
  ra-jit, and put cargo's config back.
- `ra-jit-live` (on crates.io), a crate that applies edits to a running program, keeping
  its state (macOS on Apple silicon).
- A nix flake: `nix run github:indexable-inc/ra-jit` opens a Game of Life you can edit
  while it runs; `#ra-jit` is the cargo wrapper.
- Prebuilt binaries for Linux (x86_64, aarch64) and macOS (aarch64).
- `cargo install ra-jit` from crates.io (Cranelift; `--features llvm` needs LLVM 20). The
  rust-analyzer and rustc_abi forks it builds on are published as `ra-jit-<name>`.

### Known limits

- Rust 1.99.0 only.
- No borrow checker: keep running `cargo check`.
- Changing a struct the running program uses asks for a restart.

[Unreleased]: https://github.com/indexable-inc/ra-jit/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/indexable-inc/ra-jit/releases/tag/v0.1.0
