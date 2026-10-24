#!/usr/bin/env python3
"""macroserver.py: the release's proc-macro server, built by ra-jit, expanding with the
libraries ra-jit links, held against the toolchain's server with rustc's (docs/no-rustc.md,
step J1).

ra-jit builds the server of `vendor/proc-macro-srv` (`--exe`), and each proc-macro crate a
package of serde, thiserror, tokio and clap depends on as a shared library with the list of
its macros in it (`--exe` with `--shared`, step J2). It then builds the package with
`RA_JIT_PROC_MACRO_COMPARE` naming that server (crates/ra-jit-expand/src/compare.rs): each
call the toolchain's server expands with rustc's library, derives with helper attributes,
attributes and function-like macros, is expanded again by ra-jit's server with ra-jit's
library, and every output has to be the toolchain's, spans and all.

    python3 macroserver.py
"""
import glob, json, os, platform, re, shutil, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from allrajit import std_of
from difftest import RA_JIT
from storelib import scratch

SERVER = os.path.join(HERE, "..", "vendor", "proc-macro-srv")
SERVER_PACKAGE = "proc-macro-srv-cli"
SERVER_BIN = "rust-analyzer-proc-macro-srv"

MANIFEST = """[package]
name = "macrouse"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
thiserror = "2"
tokio = { version = "1", features = ["macros", "rt"] }
clap = { version = "4", features = ["derive"] }

[workspace]
"""
SOURCE = r"""use std::collections::BTreeMap;

use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Point {
    x_pos: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(rename = "kind")]
    shape: Shape,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Shape {
    Circle { r: f64 },
    Square { side: u32 },
    #[serde(other)]
    Unknown,
}

#[derive(Serialize, Deserialize)]
pub struct Wrapper<'a, T: Clone> {
    #[serde(borrow)]
    name: &'a str,
    items: Vec<T>,
    #[serde(flatten)]
    extra: BTreeMap<String, u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("io failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad value {value} at {line}")]
    Bad { value: i64, line: usize },
    #[error(transparent)]
    Format(#[from] std::fmt::Error),
}

#[derive(Debug, thiserror::Error)]
#[error("wrapped: {source}")]
pub struct Wrapped {
    #[source]
    source: AppError,
}

/// A fixture's command line.
#[derive(Parser, Debug)]
#[command(name = "macrouse", version, about = "a fixture")]
pub struct Cli {
    /// How many.
    #[arg(short, long, default_value_t = 3)]
    count: u32,
    #[arg(long, value_enum)]
    mode: Option<Mode>,
    #[command(flatten)]
    shared: Shared,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Runs it.
    Run {
        #[arg(short)]
        fast: bool,
    },
    Stop,
}

#[derive(ValueEnum, Clone, Debug)]
pub enum Mode {
    Quick,
    Slow,
}

#[derive(clap::Args, Debug)]
pub struct Shared {
    #[arg(long)]
    verbose: bool,
}

async fn twice(n: u32) -> u32 {
    n * 2
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse_from(["macrouse", "--count", "2", "run", "-f"]);
    let doubled = tokio::select! {
        biased;
        n = twice(cli.count) => n,
        _ = tokio::task::yield_now() => 0,
    };
    let err = AppError::Bad { value: -1, line: doubled as usize };
    println!("{cli:?} {doubled} {err}");
    println!("{}", Wrapped { source: err });
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn doubles() {
        assert_eq!(super::twice(2).await, 4);
    }
}
"""
# The proc-macro crates the package's build expands with, by package.
MACRO_CRATES = {"serde_derive", "thiserror-impl", "tokio-macros", "clap_derive"}


def run(cmd, cwd, what, env=None):
    start = time.monotonic()
    ran = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True,
                         env={**os.environ, "RUST_BACKTRACE": "0", **(env or {})})
    took = time.monotonic() - start
    if ran.returncode != 0:
        raise SystemExit(f"FAIL: {what} exited {ran.returncode}:\n{ran.stdout[-4000:]}"
                         f"{ran.stderr[-8000:]}")
    return ran, took


def platform_triple():
    return f"{platform.machine()}-unknown-linux-gnu"


def metadata(root):
    ran, _ = run(["cargo", "metadata", "--format-version", "1", "--filter-platform",
                  platform_triple()], root, f"cargo metadata of {root}")
    return json.loads(ran.stdout)


def lib_name(package):
    for target in package["targets"]:
        if {"lib", "rlib", "proc-macro"} & set(target["kind"]):
            return target["name"].replace("-", "_")
    return None


def closure(meta, name):
    """The package called `name` and the libraries it links: its normal dependencies, all the
    way down, but proc-macro crates, which run while it is built. Each by its crate's name."""
    packages = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    start = next(p["id"] for p in meta["packages"] if p["name"] == name)
    seen, todo, crates = {start}, [start], []
    while todo:
        node = nodes[todo.pop()]
        for dep in node["deps"]:
            if not any(kind["kind"] is None for kind in dep["dep_kinds"]):
                continue
            package = packages[dep["pkg"]]
            if any("proc-macro" in t["kind"] for t in package["targets"]):
                continue
            if dep["pkg"] not in seen:
                seen.add(dep["pkg"])
                todo.append(dep["pkg"])
                crates.append(lib_name(package))
    return crates


def std_source():
    """The toolchain's `library` directory, whose crates rust-analyzer loads as the standard
    library's."""
    toolchain = open(os.path.join(HERE, "toolchain")).read().strip()
    home = os.environ.get("RUSTUP_HOME", os.path.expanduser("~/.rustup"))
    found = glob.glob(os.path.join(home, "toolchains", f"{toolchain}-*", "lib", "rustlib",
                                   "src", "rust", "library"))
    if not found:
        raise SystemExit(f"FAIL: no source of the standard library of {toolchain} in {home}")
    return os.path.realpath(found[0])


def with_std(crates, panic="unwind"):
    """`crates`, then the standard library's and `proc_macro`. A crate of the library that has the name of one
    of `crates` (libc, hashbrown, memchr, cfg_if) is named by its root file, as a name is the
    program's crate's."""
    vendored = {}
    for root in glob.glob(os.path.join(std_source(), "vendor", "*", "src", "lib.rs")):
        package = os.path.basename(os.path.dirname(os.path.dirname(root)))
        vendored[package.rsplit("-", 1)[0].replace("-", "_")] = root
    named = set(crates)
    # `proc_macro` and what it depends on outside the library's unit.
    library = [*std_of(panic), "proc_macro", "rustc_literal_escaper"]
    std = [vendored[c] if c in named and c in vendored else c for c in library]
    return [*crates, *std]


def ra_jit(root, crates, out, *extra, env=None):
    args = sum((["--crate", c] for c in crates), [])
    return run([RA_JIT, root, *args, "--out", out, *extra], root, f"ra-jit {crates[0]}", env)


def linked(ran, what):
    said = re.search(r"^linked .* in (\S+), (\d+) functions in it ra-jit could not compile$",
                     ran.stderr, re.M)
    if not said:
        raise SystemExit(f"FAIL: {what}: ra-jit did not say it linked:\n{ran.stderr[-8000:]}")
    if said.group(2) != "0":
        raise SystemExit(f"FAIL: {what}: {said.group(2)} functions ra-jit could not compile:\n"
                         f"{ran.stderr[-8000:]}")


def main():
    if not sys.platform.startswith("linux"):
        print("not run: ra-jit links proc-macro libraries and the server on Linux so far")
        return
    work = scratch("ra-jit-macroserver-")
    passed = False
    try:
        lines = []
        server_root = os.path.realpath(SERVER)
        server = os.path.join(work, SERVER_BIN)
        crates = with_std([SERVER_BIN.replace("-", "_"),
                           *closure(metadata(server_root), SERVER_PACKAGE)])
        ran, took = ra_jit(server_root, crates, os.path.join(work, "server.o"), "--exe", server)
        linked(ran, "the server")
        lines.append(f"the server: built and linked by ra-jit in {took:.1f} s, "
                     f"{len(crates)} crates")

        root = os.path.join(work, "macrouse")
        os.makedirs(os.path.join(root, "src"))
        with open(os.path.join(root, "Cargo.toml"), "w") as f:
            f.write(MANIFEST)
        with open(os.path.join(root, "src/main.rs"), "w") as f:
            f.write(SOURCE)
        run(["cargo", "generate-lockfile", "-q"], root, "cargo generate-lockfile")
        meta = metadata(root)
        libs = os.path.join(work, "libs")
        os.makedirs(libs)
        versions = []
        for package in meta["packages"]:
            if package["name"] not in MACRO_CRATES:
                continue
            name = lib_name(package)
            library = os.path.join(libs, f"lib{name}.so")
            crates = with_std([name, *closure(meta, package["name"])])
            ran, took = ra_jit(root, crates, os.path.join(work, f"{name}.o"), "--exe", library,
                               "--shared")
            linked(ran, package["name"])
            versions.append(f"{package['name']} {package['version']}")
            lines.append(f"{package['name']} {package['version']}: linked by ra-jit in "
                         f"{took:.1f} s")
        if len(versions) != len(MACRO_CRATES):
            raise SystemExit(f"FAIL: the package depends on {versions}, not each of "
                             f"{sorted(MACRO_CRATES)}")

        env = {"RA_JIT_PROC_MACRO_COMPARE": server, "RA_JIT_PROC_MACRO_LIBS": libs}
        # With its tests, whose `#[tokio::test]` is another attribute.
        ran, took = ra_jit(root, ["macrouse"], os.path.join(work, "macrouse.o"), "--cfg-test",
                           env=env)
        said = re.findall(r"compared with ra-jit's server: (\d+) calls \((\d+) derives, "
                          r"(\d+) attributes, (\d+) function-like\), (\d+) expanded otherwise, "
                          r"(\d+) failed by both, (\d+) not compared", ran.stderr)
        if not said:
            raise SystemExit(f"FAIL: the build compared nothing:\n{ran.stderr[-8000:]}")
        calls, derives, attrs, bangs, differed, failed, unloaded = map(int, said[-1])
        if differed or failed or unloaded or not (derives and attrs > 1 and bangs):
            raise SystemExit(f"FAIL: {calls} calls compared, {derives} derives, {attrs} "
                             f"attributes, {bangs} function-like, {differed} expanded "
                             f"otherwise, {failed} failed by both, {unloaded} not compared:\n"
                             f"{ran.stderr[-8000:]}")
        lines.append(f"the package: {calls} calls ({derives} derives, {attrs} attributes, "
                     f"{bangs} function-like) expanded by both servers, the same, "
                     f"in {took:.1f} s")
        print("\n".join(lines))
        print(f"PASS: ra-jit's server and libraries expand as the toolchain's, "
              f"{', '.join(versions)}")
        passed = True
    finally:
        if passed:
            shutil.rmtree(work, ignore_errors=True)
        else:
            print(f"kept {work}")


if __name__ == "__main__":
    main()
