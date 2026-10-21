# The disk store and the remote

ra-jit keeps compiled functions in memory while a daemon runs. Two more layers keep them
longer. The disk store is a directory on this machine that every daemon of the user can
share. The remote is a directory or an S3 bucket that several machines push to and pull
from. Both are off until you configure them.

The reports behind this file are [reports/DISKCACHE.md](reports/DISKCACHE.md) and
[reports/REMOTE.md](reports/REMOTE.md). The variables and commands below were checked
against `crates/hir-ty-codegen/store.rs` and `crates/ra-jit/src/remote.rs`.

## What is stored and under what key

The store holds compiled functions: machine code with its relocations, unwind information
and line tables. It does not hold rust-analyzer's analysis, which is rebuilt when a daemon
loads a workspace.

A function's key is a SHA-256 over the backend and optimisation level, the target triple,
whether the build has debug info and overflow checks, and the function's stable name. The
stable name holds the function's path, its generic arguments spelled out, and its crate. A
dependency is named by rustc's stable crate id (name, version, and the metadata cargo
passes). A crate of your checkout is named by a hash of its source, so two checkouts with
the same source share entries wherever they are on disk. An edit gives new names to
everything in the edited crate and in the crates of the checkout that depend on it, so
what the store serves across edits is the code of crates the edit does not reach:
dependencies, their generic instances, and the untouched crates of the workspace. Within
one running daemon, unchanged functions of an edited crate are kept by the database
instead.

Before a record is written, the store hashes that key together with the store format, a
hash of the ra-jit executable and the toolchain's commit. An entry is therefore only ever
found by the same ra-jit binary, for the same toolchain. Upgrading ra-jit makes the old
entries unreachable; they are removed over time as least recently used.

`harness/keys.py` tests the keying: it changes, one at a time, each thing code can depend
on that is not the text of its file, and checks that a build with a store filled before the
change still prints what cargo's build prints.

## The disk store

Set `RA_JIT_CACHE` to a directory. The daemon reads the variable when it starts, so under
`cargo ra-jit` it has to be set in the environment of the command that starts the daemon;
run `cargo ra-jit stop` first if one is running.

    export RA_JIT_CACHE=~/.cache/ra-jit-store
    cargo ra-jit stop
    cargo ra-jit build

| Variable | Meaning | Default |
|---|---|---|
| `RA_JIT_CACHE` | the store's directory | not set: no store |
| `RA_JIT_CACHE_MAX` | the most the sealed packs may take: bytes, or a number with `K`, `M` or `G`, or `off` | 8G |
| `RA_JIT_CACHE_SEAL_AT` | size at which an open pack is sealed at the end of a build; exists for the harnesses | 4M |
| `RA_JIT_HYDRATE` | `0` compiles the checkout's own functions in the first build instead of taking them from the store (dependencies still come from it), so the first edit can be spliced rather than rewrite the whole object and restart the program | take them from the store |

### What is on disk

The store is one directory. `version` names the format. `<sha256>.pack` is a sealed pack,
named by the hash of its bytes and never changed afterwards. `<pid>-<nanos>.open` is the
pack one daemon is appending to, held locked. `*.tmp` is a file on its way to another name.
`lock` is held while the directory is emptied or collected.

A daemon that finds an older format, or files with no `version`, empties the directory and
starts over. One that finds a newer format leaves it alone and runs without a store.

### Integrity and crashes

A pack's hash is checked the first time a process reads it, and each record carries a
checksum that is checked every time its value is read. A pack that fails either check is
removed and what was in it is compiled again. A sealed pack appears by a rename after it is
written and synced. An open pack left by a daemon that died is sealed up to its last whole
record by whoever finds it.

Several daemons can share one directory. Reading and adding take no lock; only collection
and a change of format do.

If the disk fills, the daemon prints `store: nothing more is kept of this build: ...`
once, keeps compiling, and tries again on the next build. If the directory cannot be used
at all it prints `store: not used: ...` and runs without a store.

### Size and collection

A collection runs when a daemon opens the store, after it seals a pack, and on
`ra-jit cache gc`. It seals open packs left behind, removes old temporaries, merges sealed
packs smaller than 4M into one when there are more than 16 of them, and then removes the
least recently used packs until the rest fit under `RA_JIT_CACHE_MAX`. A pack counts as used
when a daemon read a value from it during a build.

Eviction is by pack, not by record. One recently used record keeps its whole pack, which
can be up to 64M, so a limit smaller than a few packs evicts nearly everything.

### Commands

These act on the store `RA_JIT_CACHE` names.

    ra-jit cache stats                 format, packs, bytes, limit, oldest and newest use
    ra-jit cache verify                read everything; exit 1 if a pack is not what was written
    ra-jit cache gc [--max SIZE]       collect now, to RA_JIT_CACHE_MAX or to SIZE

### Limits

- The store harnesses run in CI on Linux, Windows and macOS. `storefull.py` (a full disk)
  runs on Linux only. On Windows a pack that another process has open cannot be removed,
  so eviction of a pack in use fails and the pack is kept; this is not measured.
- Power loss was not tested. The harness kills the daemon, which does not lose the page
  cache.
- The locks are advisory, and behaviour over NFS is not tested.

## The remote

A remote is a directory or `s3://bucket/prefix`. It holds sealed packs under the hash of
their bytes, a list of the keys in each pack, and one manifest per project that lists the
packs that project's builds were served from.

    packs/<sha256>.pack.zst      a pack, compressed with zstd
    keys/<sha256>                the keys of one pack
    manifests/<project>          one line per pack
    units/<sha256>, deps/<project>   cargo's builds of dependencies, see below

**Real AWS S3 has not been run.** Everything was measured against MinIO and SeaweedFS on
the same machine over loopback. The client is the AWS SDK, so the code is the same, but
virtual-hosted addressing, IAM errors and role credentials are untested.

### By hand

    ra-jit cache push REMOTE [PROJECT]
    ra-jit cache pull REMOTE [PROJECT]

`push` sends the packs the manifest lacks and adds them to it. `pull` fetches the packs the
manifest lists that are not in the local store, as many as fit under `RA_JIT_CACHE_MAX`.
`PROJECT` picks the manifest and defaults to `default`.

### From the daemon

With `RA_JIT_REMOTE` set, the daemon pulls before its first build, pulls again when a build
misses a key and the manifest has changed, and pushes what it compiled after the first
build, then every five minutes, and once more when it is told to stop. No build waits for a
push. A daemon killed with SIGKILL pushes nothing on its way out; the next push sends it.

| Variable | Meaning | Default |
|---|---|---|
| `RA_JIT_REMOTE` | the remote the daemon keeps its store in step with | not set |
| `RA_JIT_PROJECT` | which manifest | `default` |
| `RA_JIT_REMOTE_READONLY` | never write to the remote | off |
| `RA_JIT_REMOTE_LAZY` | fetch only the key lists, and a pack when a build first asks for a key in it | off |
| `RA_JIT_REMOTE_PUSH_EVERY` | how often a running daemon pushes (`90s`, `5m`, `1h`), or `off` | `5m` |
| `RA_JIT_REMOTE_JOBS` | requests in flight at once | 8 |
| `RA_JIT_REMOTE_TIMEOUT` | seconds a request may go without an answer | 30 |
| `RA_JIT_REMOTE_SIGN` | file holding the key this machine signs manifests with | not set |
| `RA_JIT_REMOTE_TRUST` | public keys this machine accepts manifests from, comma separated | not set |

`RA_JIT_REMOTE` needs `RA_JIT_CACHE` as well: the remote fills and reads the local store.

### S3 address and credentials

    s3://bucket/prefix?endpoint=URL&region=NAME&profile=NAME&path-style=true|false

Whatever the address leaves out comes from where the AWS tools look: `AWS_ENDPOINT_URL`,
`AWS_REGION`, `AWS_PROFILE`, `~/.aws/config`. An unknown option is refused by name. With an
endpoint of its own, ra-jit addresses the bucket by path unless told `path-style=false`.

ra-jit does not hold credentials; the AWS SDK finds them (environment, shared config and
profiles, web identity, container and instance roles). They are not printed. The harness
searches everything ra-jit printed in a run for the secret key.

### Trust and signing

There are two questions.

Can the storage change the code? No. A pack is named by the SHA-256 of its bytes and is
hashed after the fetch and before it is put in the store. A remote, proxy or disk that
returns other bytes has them thrown away and reported, and the build compiles those
functions itself. Key lists and cargo units are checked the same way.

Can someone with write access make ra-jit use the wrong code? Without signing, yes. The
manifest is the one mutable object. Somebody who can write to the bucket can upload a
well-formed pack with wrong code under real keys and list it. Content addressing does not
catch that.

Signing does. Make a key and note the public half it prints:

    ra-jit cache keygen ~/.config/ra-jit/sign.key

A machine with `RA_JIT_REMOTE_SIGN` naming that file signs each manifest it writes
(Ed25519, over the manifest's name and body). A machine with `RA_JIT_REMOTE_TRUST` listing
public keys refuses any manifest not signed by one of them. A machine that trusts keys but
has none to sign with refuses to push.

So: with `RA_JIT_REMOTE_TRUST` set, nobody without a trusted signing key can make ra-jit emit
code other than what a local build gives, whatever they do to the bucket. Without it,
whoever can write to the bucket is trusted, as with sccache or a registry mirror. A trusted
signer is trusted completely; ra-jit does not compile again to check them. Under all of it
sits one assumption: two machines with the same ra-jit binary compile the same key to
equivalent code.

The harness checks, against both servers: a pack truncated, extended, emptied or with one
byte flipped; a different valid pack under another's name; a listed pack that is missing;
and manifests that are forged, unsigned, or signed by an unlisted key.

There is no `SECURITY.md` in the repository at the commit these docs were written against.

### Collecting a remote

    ra-jit cache gc --remote REMOTE [--unlisted-after AGE] [--older-than AGE] [--dry-run]

An age is a number and `s`, `m`, `h` or `d`. The run first takes lines out of manifests:
with `--older-than`, the lines of packs written longer ago than that, and always the lines
of packs whose every key is also in other packs of the same manifest. It then removes
objects no manifest names that were written longer ago than `--unlisted-after`, which
defaults to a day and may not be less than two hours.

It is safe against a push in progress by the clock, not by a lock: a push relies on an
unlisted object only for an hour after it was written. A single push that takes more than
an hour between sending a pack and listing it, or a machine whose clock is hours off, is
outside what it promises. Nothing runs a remote collection on its own.

### Dependencies built by cargo

    ra-jit deps push REMOTE DIR [PROJECT]
    ra-jit deps pull REMOTE DIR [PROJECT]

Run in the directory of `Cargo.lock`, where `DIR` is the directory cargo builds a profile
in, such as `target/debug`. This sends and fetches what cargo built of registry and git
dependencies (rlibs, build script output, fingerprints) so that a fresh checkout's first
cargo build finds them fresh. It is separate from the function store and is tested by
`harness/deps.py`.

### What it buys, measured

On one machine over loopback, a fresh second "machine" (another home directory, checkout
and store) building ripgrep 14.1.1 took 16.8 s to a running program with no remote, 14.3 s
with ra-jit's packs, and 9.6 s with packs and cargo's units. For cargo 0.92.0 the same three
were 152.4 s, 117.1 s and 94.4 s. In each case with a remote, ra-jit compiled 0 functions.
What is left is cargo running build scripts and proc macros, and ra-jit loading the
workspace and walking every instance to find its key. Lazy fetching was slightly slower
than fetching everything in both. Transfer times over loopback are a lower bound and say
nothing about a real network. The full tables are in [results.md](results.md).

### Limits

- Real AWS has not been run.
- Windows and macOS were not run for the remote; the harness is Linux only.
- When one fetch runs out of retries the remote is taken to be down and that process's
  other transfers stop. The next run resumes them.
- A merged pack is held twice on the remote until someone runs `cache gc --remote`.
