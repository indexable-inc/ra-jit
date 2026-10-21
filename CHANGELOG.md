# Changelog

All notable changes to ra-jit are written here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `RA_JIT_HYDRATE=0` ([docs/cache.md](docs/cache.md)): with a store, the first build compiles the
  checkout's own functions instead of taking them from it, so the first edit of a new daemon
  is spliced, not a full rewrite that restarts the program. Default unchanged.
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
  output differs from cargo's is never published; none is yet, since on macOS a program ra-jit
  builds every crate of, with its own `main`, does not run (docs/research/mir-gap.md).
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
- A borrow checker, `crates/hir-ty-borrowck`, written to give rustc's verdict: Polonius alpha
  by default as nightly rustc does, NLL with `RA_JIT_BORROWCK=nll`. It gives rustc's errors on
  1033 of 1578 of rustc's own borrow-check tests that it can run (245 with none), scored by
  `harness/borrowck_ui.py`, and finds nothing in ripgrep, fd, bat, hyperfine, just and axum.
  Its errors are rust-analyzer diagnostics and `ra-jit borrowck` prints them; the daemon
  does not refuse a build for them yet ([docs/borrowck.md](docs/borrowck.md)).
- `ra-jit borrowck`: the borrow-check errors of single files, or of a cargo workspace with
  `--project`.

### Changed

- An edit in a package whose build script cargo runs again for it (one that names no
  `rerun-if-changed`, as ripgrep's, fd's and hyperfine's) no longer waits for a
  `cargo check` that runs every build script of the workspace: the daemon runs that one
  script again as cargo ran it and compares what it prints and writes with what it loaded,
  as `cargo ra-jit` already did, and asks cargo only when the two differ, the script is older
  than its source, or it was compiled reading a variable cargo did not set. A script
  `cargo ra-jit` ran again for the same build, with nothing it reads written since, is not
  run a second time. On Linux x86-64 an edit of ripgrep's `search` goes from 72 to 48 ms,
  fd's `run` from 78 to 42 ms and hyperfine's `run` from 80 to 35 ms.
- Release binaries are built with thin LTO (the new `dist` profile) and optimized with a
  profile of ra-jit's own work (PGO), trained on edits of fd, bat, just and hyperfine, by
  `.github/scripts/build_release.py`, which the release workflow now runs for every target.
  Against the plain `release` build, on Linux: the cold start 14% shorter on nushell and on
  ripgrep, an edit of a function 8% shorter on nushell, the same peak memory. mimalloc v3
  stays the allocator: jemalloc, mimalloc v2 and snmalloc start colder and edit no faster
  (an edit spends 1.8% of its time allocating, 73% revalidating salsa's memos). `release`
  itself is unchanged, so rebuilding ra-jit after a change takes as long as before
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

### Fixed

- A worker that loaded its workspace beside cargo's first build
  ([#11](https://github.com/indexable-inc/ra-jit/issues/11)) ends when its daemon does, as
  every other worker does: once told what cargo built, it waited for a lock on its standard
  input that it held itself, so a daemon that crashed or was killed left the worker running,
  holding its memory (2 GB on fd) and its socket, and the next daemon's first build fell
  back to cargo ("another daemon serves"). `cargo_ra_jit.py`'s "workers of the killed
  daemon are gone" fails without this.
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
