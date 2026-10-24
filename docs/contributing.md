# Contributing

## AI use

This code was written largely with AI assistance under human direction. The same is true
of the harnesses, the reports under `docs/reports/` and these documents. The reports are
written in the first person by the session that did the work; "I ran" in a report means
that session ran it.

What makes that workable is the testing method. Every change is judged by comparing ra-jit's
output with rustc's on real programs, and a change that makes a harness differ does not
land. If you contribute with AI assistance, the expectation is the same as without: you
understand the change, you ran the gate, and you say what you ran and what you did not.

rust-analyzer has its own AI policy for contributions to rust-analyzer. It applies to what
you send them, including fixes you find here and want to upstream.

## Layout of the tree

    crates/ra-jit/             the daemon, the cargo drop-in, the splice, the remote
    crates/ra-jit/src/bin/     cargo-ra-jit
    crates/hir-ty-codegen/    the code generator: MIR to Cranelift IR, the producers
    crates/ra-jit-store/      the disk store of compiled code, which knows nothing about Rust
    crates/ra-jit-fuzz/       made-up and damaged inputs for the tests of what reads the store
    crates/ra-jit-wasm/        experiment: the compiler running in a web page (not in the workspace)
    harness/                  the test harnesses and their fixtures
    demo/                     small projects the harnesses and the scoreboard build
    docs/                     these documents
    docs/reports/             working reports, one per piece of work, as written at the time
    SECURITY.md               what to know before sharing a store, and how to report a problem
    vendor/rust-analyzer/     rust-analyzer with ra-jit's changes, a git subtree
    vendor/rust-analyzer/crates/hir-ty-borrowck/
                              the borrow checker, a crate next to hir-ty (borrowck.md)
    vendor/rust-analyzer/lib/rowan/
                              rowan 0.17.0 with a flat kind of tree behind its cursor (flat-syntax.md)
    vendor/rustc_abi/         rustc's layout crate, copied from the 1.99.0 tag, a git subtree
    vendor/proc-macro-srv/    the 1.99.0 release's proc-macro server, built by ra-jit to load
                              the proc-macro libraries ra-jit links (no-rustc.md, step J1)
    vendor/subtree.py         moves vendor/ changes to the forks, and the forks and upstream into vendor/
    vendor/source.rev         the commit of the old development branch this tree last took from

`crates/hir-ty-codegen` is the code generator, a crate of its own over `hir-ty`'s MIR: what
it uses of `hir-ty` is public there. Its root is `mod.rs`, the file it was compiled from when
it was a module of `hir-ty`. Nothing in `vendor/` depends on anything in `crates/`, so the
fork builds alone ([architecture-split.md](architecture-split.md)).

## rust-analyzer

`vendor/rust-analyzer` is a squashed git subtree of the `ra-jit` branch of
[indexable-inc/rust-analyzer](https://github.com/indexable-inc/rust-analyzer). That branch is DioxusLabs' `rust-analyzer` branch of rustd (upstream at
`dd5ed69d4b` with rustd's changes on top), merged with upstream rust-analyzer master, with
the MIR passes that make rust-analyzer's MIR like rustc's on top
([`docs/mir-passes.md`](https://github.com/indexable-inc/rust-analyzer/blob/ra-jit/docs/mir-passes.md)
there). The fork stays the public home of those changes and the place upstream is merged
in; ra-jit carries the files themselves, so a plain clone builds:

    git clone https://github.com/indexable-inc/ra-jit
    cargo +1.99.0 build -p ra-jit --release

`vendor/rustc_abi` is a subtree of [indexable-inc/rustc_abi](https://github.com/indexable-inc/rustc_abi)
(`main`) the same way.

### Changing a rust-analyzer file

Edit the file in `vendor/rust-analyzer`, build and test from the root as usual, and commit
in ra-jit like any other file: one commit can change MIR lowering and the code generator
together. A branch that changes vendor/ merges into main like any other branch; there is
nothing to port and no pointer to bump.

The fork follows main by itself: on every push to main that changes `vendor/`, the
`fork sync` workflow runs

    vendor/subtree.py push rust-analyzer
    vendor/subtree.py push rustc_abi

which makes one fork commit for each commit on main's first-parent line that changed the
directory (its tree under `vendor/<name>`, its author, dates and message, and a line naming
it) and pushes them to the fork's branch (`ra-jit` for rust-analyzer, `main` for
rustc_abi). The same commit always makes the same fork commit, so running it by hand
(`--dry-run` makes the commits and pushes nothing) and in CI agree. It only fast-forwards:
it never force-pushes, and if the fork holds a change main does not have, it stops, pushes
nothing and says so. That happens only if someone committed to the fork directly; bring it
in with `pull` below, merge that into main, and the next push goes through. A fork pushed
from a branch before its merge, or a main whose history was rewritten since the last push,
is caught up with one commit that says what it brings, once it is checked that the fork has
nothing main lacks. The `ra-jit` branch only moves forward; do not rebase or force-push it.

`git subtree split` is not used: it cannot follow `vendor/rustc_abi` past the time it was a
submodule, and it makes different commits from a branch than from main, so the fork stopped
being a fast-forward of what main makes.

### Taking a newer fork commit

    vendor/subtree.py pull rust-analyzer
    # git subtree pull --prefix vendor/rust-analyzer https://github.com/indexable-inc/rust-analyzer ra-jit --squash

That is the whole bump: there is no second pin, and the flake builds from the same files.

### Merging upstream rust-analyzer

    vendor/subtree.py upstream rust-analyzer              # its newest weekly release tag
    vendor/subtree.py upstream rust-analyzer 2026-10-26   # a tag, a branch (master) or a commit

merges upstream's commit into `vendor/rust-analyzer` on the current branch, with a clean
tree. The base is the newest upstream commit the fork's history already has (the fork's
branch and every squashed merge on this branch), so the merge brings exactly upstream's
changes since then, as merging upstream in the fork would. A clean merge is committed;
otherwise the conflicts are left in `vendor/rust-analyzer` with the usual markers and
`git status`, and `git add` and `git commit` finish it (or `git merge --abort`). The merge's
second parent is a squashed commit of upstream's tree with `git subtree`'s
`git-subtree-split` line; when main gets the merge, `push` makes upstream's commit a parent
of the fork's commit, so the fork's history records the upstream merge too.

### Where new code goes

New code that is the language's semantics goes in `vendor/rust-analyzer`: building MIR and
the passes over it, borrow checking, const evaluation, layout, and the hashes that say what a
function's code is made from, as salsa queries. These are what rustc keeps in `rustc_middle`,
`rustc_mir_transform` and `rustc_borrowck`; rust-analyzer could take them, and a query lives
with the database it reads. Where rust-analyzer's MIR differs from rustc's, the fix goes in
the fork's lowering or in a pass of its runtime MIR queries, and the code generator relies on
it: codegen does not compensate.

Inside `vendor/rust-analyzer`, prefer a new crate under `crates/` (or, where the code needs
private items of `hir-ty`, a new module file declared with one line) over edits spread through
upstream's files. Every changed line of an upstream file is a line an upstream merge can
conflict on; a new file conflicts with nothing. Fixes that are simply rust-analyzer bugs are
the exception, and should be sent upstream, under their rules.

ra-jit's machinery stays in `crates/`: the code generator's backends, object and debug-info
writing, the splice, the daemon and its socket, the cargo drop-in, the store and the remote,
tiers and sampling, the live runtime, and the harnesses. A change there is one ordinary
commit.

New code never adds a path to rustc ([no-rustc.md](no-rustc.md)). ra-jit compiles
everything itself, so a change does not start rustc, rustdoc or a cargo that compiles, does not
read `RUSTC` or `RUSTDOC`, does not add a reason for a build to go to cargo or a kind of function
to leave to rustc, and does not use rustc's rlibs, objects, symbols or link line for anything
new. Where a feature needs what rustc does, the change builds that piece in ra-jit, or waits for
it. The paths that remain may be fixed where they are wrong, and are otherwise only removed.

### Moving to a newer rust-analyzer

1. On a branch, `vendor/subtree.py upstream rust-analyzer` and resolve the conflicts.
2. Update `Cargo.toml` and `Cargo.lock` at the root for dependencies rust-analyzer changed.
3. Build, and run the whole gate under both backends ([harness.md](harness.md)). A newer
   rust-analyzer changes inference, MIR lowering and layout queries, any of which can
   change generated code. The corpus with `--layouts`, `breakers.py` and
   `fuzz/fuzz.py smoke` are the ones that notice first.
4. Land it on `main` through the merge queue ([Landing a change](#landing-a-change)). The
   `fork sync` workflow sends the merge to the fork.

This was done once, on 2026-10-20, for upstream `0b21bd6596`. The merge was clean; the one
API change ra-jit had to follow was `CrateData::is_proc_macro` becoming `target_kind` (a
`base_db::TargetKind`). Step 3 has not been run on that merge yet.

### Where the history is

`main` began as a tree generated from a development branch that was rust-analyzer's tree
with ra-jit's changes made in place. That branch and its history are kept as
`ra-jit-collate` and `ra-jit-spike`. `publish/split.py` there is what wrote this layout.

The toolchain is a separate pin. ra-jit is built for Rust 1.99.0 and `vendor/rustc_abi` is
rustc's crate from that tag. Moving to another Rust version means taking the new
`rustc_abi`, rechecking everything that mirrors rustc (the Rust ABI in the code generator,
`vtable.rs`, `type_id.rs`, symbol names in `symbols.rs`), and running rustc's own test
suites through `uitest.py` and `libsuite.py` again.

## Rules for code

1. **rustc is the reference.** If ra-jit's program behaves differently from rustc's, ra-jit
   is wrong, even where rustc's behaviour looks like an accident. The harnesses run rustc to
   compare with; ra-jit itself never does ([no-rustc.md](no-rustc.md)).
2. **Never guess.** Input the code generator has no explicit lowering for is refused with a
   reason (`CodegenError`), not lowered on a best-effort basis. A refusal is visible and
   counted. A wrong guess is a miscompile nobody sees.
3. **A refused function is a bug**, to be listed and fixed, not a normal path to design
   around.
4. **General fixes.** A fix that tests for one crate, one function name or one test is not
   a fix. Find what the case is an instance of.

Style follows rust-analyzer's guide
(`vendor/rust-analyzer/docs/book/src/contributing/style.md`): `cargo fmt`, no new
dependencies without a reason, small focused changes. Comments say what the code is for and
why, not what was changed. Messages printed to the user say what happened and what ra-jit
did about it in one line.

## What a change needs

- A reduced case that fails before and passes after: a corpus function for a code
  generator feature, a breaker for a bug found in real code, a step in the relevant edit
  harness for a daemon bug. [harness.md](harness.md) says how to add each.
- The gate, under Cranelift and under LLVM, on the platform you have. Say which platforms
  you did not run. CI's fast tier covers Linux x86-64, Linux AArch64 and macOS on every
  change; Windows and the slow harnesses run nightly ([harness.md](harness.md#ci)).
- The LLVM backend builds in the flake's dev shell, against the LLVM it pins:
  `nix develop -c cargo +1.99.0 build -p ra-jit --release --features llvm`
  ([LLVM for `--features llvm`](#llvm-for---features-llvm)).
- `cargo fmt --all -- --check` and
  `cargo test --release -p hir-ty -p hir-def -p parser -p syntax -p ra-jit`.
- For anything that changes numbers in [results.md](results.md): the command, the commit
  and the machine, so the entry can be replaced and not just appended to.

When you report what you did, separate what you ran from what you inferred. The reports
under `docs/reports/` do this and it is the reason they can be used.

## Working in parallel

Several lanes, people or agents, change ra-jit at once. A lane shares the Mac, `git`'s
metadata and main with all the others, so:

- **Heavy work runs on a dev host.** A release build of a branch, a harness sweep
  (`cargo_ra_jit.py`, `buildmodel.py`, `keys.py`, the soak), a benchmark or anything longer
  than a few minutes runs on an AWS dev host. Claim the host with a CLAIMED row in the host
  ledger (`/Volumes/Projects/nix/scratch/memory/host-ledger.md`; hosts, ssh and the ledger's
  rules are in the `fleet-ops` skill), work in a directory named for the lane, and add a
  RELEASED row when done. Ask for a lease as long as the run (`dev-aws renew N <hours>`): an
  expired lease stops the host mid-run.
- **The Mac does quick checks only:** one harness, one unit test, `cargo check`, hot reload.
  It is shared by every lane at once, and a lane that builds a full release there slows all
  of them.
- **One worktree per lane, named for it** (`ra-jit-<lane>-<date>`), with its own
  `CARGO_TARGET_DIR` or a shared sccache. Two lanes building into one target directory wait on
  each other's lock and overwrite each other's binaries.
- **Never `git stash`.** The stash is one stack for every worktree of the repository, so a pop
  can take another lane's changes. Commit work in progress to the lane's branch.
- **Run harnesses with `RUST_BACKTRACE=0`**, so a test that panics prints its message and
  not a backtrace into what the harness compares.
- **Land small and often, through the merge queue** ([below](#landing-a-change)). A lane that
  waits hours for one large merge meets every other lane's changes at once. For anything the
  fast tier does not cover, run the full tier on the branch first
  (`gh workflow run ra-jit.yaml --ref <branch> -f tier=full`).
- **Clean up when done:** remove the worktree (`git worktree remove`), its target directory,
  and the lane's directories on the dev host; delete the merged branch.

## Landing a change

Main moves only through the merge queue: nobody pushes to it or merges into it by hand. Once the
branch's work is committed and the local gate passed ([What a change needs](#what-a-change-needs)),
run in the lane's worktree

    python3 .github/scripts/land.py "what the branch does"

It pushes the branch, opens its pull request ("Merge <branch>: what the branch does") and queues
it. The queue merges it onto main and whatever is ahead of it, runs CI's fast tier on that
(about ten minutes, [harness.md](harness.md#ci)) and merges it with a merge commit, as
`git merge --no-ff` would. `land.py` waits for that and prints the commit, or says why the pull
request left the queue; then fix the branch and run it again. The waiting runs in a process of
its own that outlives `land.py`, its terminal and a tool that ends its process group, writing to
`.git/land/pr-N.log` in the main checkout; `land.py --wait` (or `--wait N`) follows it again, as
does running `land.py` again on the branch, so nobody polls GitHub by hand. A branch that conflicts with main
gets `git merge origin/main` first. Once it has merged, GitHub deletes the branch on origin;
remove the worktree and the local branch
(`git worktree remove <dir> && git branch -D <branch>`).

## The build that ships

`cargo build --release` is the build to work with: the harnesses, the gate and CI use it, and
it rebuilds quickly after a change. What users get is one other build: the `dist` profile
(thin LTO) optimized with the PGO profile `pgo/profiles.json` pins, built by
`.github/scripts/build_release.py`. The release workflow runs that script, and the flake's
`dist` runs it inside its derivation with the same profile fetched by its sha256.
`cargo install --profile dist` builds the same profile without PGO, from the release after 0.1.0.
[reports/RELEASE-BUILD.md](reports/RELEASE-BUILD.md) has the numbers.

The PGO profiles are trained by the `pgo` workflow, one per target on the runner its release
is built on; it uploads them to the `pgo-profiles` release and commits their names and hashes
to `pgo/profiles.json`. It runs by itself when Cargo.lock, a manifest or `harness/toolchain`
changes on main: a profile finds each function by its symbol, which carries a hash of the
whole graph of crates, so after such a change the old one finds almost nothing (the build
warns, and a release refuses). A profile trained on older code of the same crates still
applies to every function that has not changed since, and `build_release.py` ends by saying
how many have: 24 after a one-line edit, about 1,600 for code 23 commits and 6,700 lines older
(of the 187,000 functions a profile has). Run the workflow by hand when that count passes a
few thousand and before a release after large changes to the daemon.

## LLVM for `--features llvm`

The LLVM backend builds against one LLVM, the 20.1.8 the flake pins (`flake.lock`), on macOS
and on Linux alike, in the flake's dev shell:

    nix develop -c cargo +1.99.0 build -p ra-jit --release --features llvm

CI builds it the same way (`.github/actions/toolchain`). Outside the shell the build stops and
says to enter it: neither Homebrew's LLVM nor a distribution's is a source. A build without the
feature needs no LLVM, and neither does what ships. A binary built with it loads `libLLVM`
from the store by its path, so after a garbage collection `nix develop` brings it back. Windows,
which has no Nix, gets LLVM's release of the same version in CI.

## Credit

A commit or pull request that carries someone else's idea, diagnosis, workaround or patch
(from an issue, a review, a discussion or another pull request) names them with a
`Co-authored-by:` trailer, one line per person, after a blank line at the end of the
message, so GitHub shows them as an author:

```
Co-authored-by: Name <id+login@users.noreply.github.com>
```

Use the address their own commits use, or GitHub's private one built from
`gh api users/<login> -q .id` and their login. Name the issue or comment the idea came
from in the message too. When a squash or a merge rewrites commits, keep every trailer.

## Licence

ra-jit is MIT or Apache-2.0, like rust-analyzer. Contributions are accepted under the same
terms.
