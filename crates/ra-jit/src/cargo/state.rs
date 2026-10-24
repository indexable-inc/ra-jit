//! What a cargo build left for the builds after it: the programs and where cargo has them,
//! what the daemon is asked to build each from, and everything a change to which makes the
//! next build cargo's again.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::OpenOptions,
    io::{BufReader, Write as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::Context as _;
use cargo_metadata::{
    Artifact, BuildScript, CrateType, DependencyKind, Message, Metadata, MetadataCommand, Package,
    Target, TargetKind,
};
use rustc_hash::FxHashSet;
use serde::{Deserialize, Serialize};

use super::{
    Request, Start,
    cli::{Invocation, Subcommand, worker_env},
    client,
    daemon::{self, Checkout, WorkerConfig},
    package,
    plan::Plan,
    script, switch,
    units::{self, Record, stamp_now, stamp_of},
    wrapper,
};

/// The toolchain whose layouts and symbol names this ra-jit has: `harness/toolchain`.
const TOOLCHAIN: &str = include_str!("../../../../harness/toolchain");

/// The names rustup reads the toolchain of a directory from.
const TOOLCHAIN_FILES: [&str; 2] = ["rust-toolchain.toml", "rust-toolchain"];

/// The state of one build of a checkout: one command with one set of flags.
#[derive(Serialize, Deserialize)]
pub(crate) struct State {
    /// The version of the ra-jit that wrote it. Another one reads nothing of it.
    version: String,
    pub(crate) key: String,
    pub(crate) root: PathBuf,
    /// When the cargo build this is the state of started.
    built_at: u128,
    /// The profile, as cargo's `Finished` line names it.
    pub(crate) profile: String,
    pub(crate) programs: Vec<Program>,
    guards: Vec<Guard>,
    /// What the daemon is asked for: the programs that are not unit tests, and the ones
    /// that are.
    pub(crate) checkouts: Vec<Checkout>,
    /// Why every build of this is cargo's, when ra-jit has no part in it.
    pub(crate) refusal: Option<String>,
    /// Whether a library that was built has doc tests.
    pub(crate) doc_tests: bool,
    pub(crate) counts: Counts,
    /// The `[env]` of cargo's configuration, which cargo sets for what it runs.
    #[serde(default)]
    pub(crate) configured: Vec<Configured>,
}

/// A variable of the `[env]` tables of cargo's configuration. Cargo sets it for a program it
/// runs where the environment has none, or always when it is `force`d.
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct Configured {
    pub(crate) name: String,
    pub(crate) value: String,
    pub(crate) force: bool,
}

/// How many builds the daemon served and how many went to cargo, by the reason.
#[derive(Serialize, Deserialize, Default)]
pub(crate) struct Counts {
    pub(crate) served: u64,
    pub(crate) cargo: BTreeMap<String, u64>,
}

/// An executable cargo built.
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct Program {
    pub(crate) name: String,
    pub(crate) kind: Kind,
    pub(crate) package: String,
    /// Whether it is built as a test: `cargo test` runs it.
    pub(crate) test: bool,
    /// The root file of the target.
    pub(crate) source: PathBuf,
    /// The directory of the package, which a test runs in.
    pub(crate) package_dir: PathBuf,
    /// Where cargo has the executable.
    pub(crate) executable: PathBuf,
    /// The other names cargo has for the executable: the file rustc wrote, when
    /// `executable` is cargo's hard link to it. A cargo that finds the two apart links
    /// them again, under whoever is running the program.
    #[serde(default)]
    names: Vec<PathBuf>,
    /// A hard link to the executable rustc linked (a clone on macOS), to give cargo back
    /// before it builds.
    cargos: PathBuf,
    /// The directory of the record of the unit rustc linked it from, when it is known.
    #[serde(default)]
    unit: Option<PathBuf>,
    /// Where it is among the programs of its checkout in `checkouts`.
    pub(crate) asked_as: Option<(usize, usize)>,
    /// Why the daemon is never asked for it, when it is not.
    left_to_cargo: Option<String>,
    /// The source files of the workspace it is compiled from, its libraries' too. While
    /// none is written, the executable cargo built is the program.
    inputs: Vec<PathBuf>,
    /// What cargo sets for the program when it runs it.
    pub(crate) env: Vec<(String, String)>,
    /// Whether it is its package's `default-run`: what `cargo run` runs when it names no
    /// program.
    #[serde(default)]
    pub(crate) default_run: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Kind {
    Lib,
    Bin,
    Example,
    Test,
    Bench,
}

impl Kind {
    fn of(kinds: &[TargetKind]) -> Kind {
        match kinds.first() {
            Some(TargetKind::Bin) => Kind::Bin,
            Some(TargetKind::Example) => Kind::Example,
            Some(TargetKind::Test) => Kind::Test,
            Some(TargetKind::Bench) => Kind::Bench,
            _ => Kind::Lib,
        }
    }
}

/// Something cargo acts on that is not source the daemon compiles. When one does not hold
/// the build is cargo's.
#[derive(Serialize, Deserialize)]
enum Guard {
    /// Files that are to be as they were when cargo built: there or not, and not written
    /// since.
    Files {
        what: String,
        files: Vec<(PathBuf, bool)>,
    },
    /// Files that are to be as they were when cargo built but for their tests: what rustc
    /// built of them without `cfg(test)` holds while the rest is, every token where it was.
    /// Each is there or not, and when it was there, when it was written and its
    /// [`without_tests`].
    /// `cargo` is cargo's dep-info of what rustc built from them, made newer than a file only
    /// its tests changed in, so cargo doesn't build it again either.
    BesideTests {
        what: String,
        files: Vec<(PathBuf, Option<(u128, String)>)>,
        #[serde(default)]
        cargo: Vec<PathBuf>,
    },
    /// A directory with nothing in it written since cargo built.
    Tree {
        what: String,
        dir: PathBuf,
    },
    Env {
        name: String,
        value: Option<String>,
    },
    /// A file cargo writes itself while it builds, `Cargo.lock`: as cargo left it, by when
    /// it was last written, or not there.
    AsLeft {
        file: PathBuf,
        written: Option<u128>,
    },
    /// A build script that cargo runs again when one of `reruns_for` does not hold, and
    /// that the daemon then runs itself: the build is cargo's when the script prints or
    /// writes something else than it did.
    Script {
        package: String,
        out_dir: PathBuf,
        reruns_for: Vec<Guard>,
        checked_at: u128,
    },
}

impl Guard {
    /// What changed, when something did.
    fn broken(&mut self, built_at: u128) -> Option<String> {
        match self {
            Guard::Script { package, out_dir, reruns_for, checked_at } => {
                let since = built_at.max(*checked_at);
                reruns_for.iter_mut().find_map(|it| it.broken(since))?;
                let now = stamp_now();
                match script::rerun_is_same(out_dir) {
                    Ok(()) => {
                        *checked_at = now;
                        None
                    }
                    Err(what) => Some(format!("the build script of `{package}` {what}")),
                }
            }
            Guard::Files { what, files } => {
                let changed = files.iter().find(|(file, was_there)| {
                    match std::fs::metadata(file).and_then(|it| it.modified()) {
                        Ok(written) => !was_there || stamp_of(written) > built_at,
                        Err(_) => *was_there,
                    }
                })?;
                Some(format!("{what} {} changed", changed.0.display()))
            }
            Guard::BesideTests { what, files, cargo } => {
                let mut tests_only = None;
                let changed = files.iter().find(|(file, was)| {
                    let written = written_at(file);
                    match (written, was) {
                        (None, None) => false,
                        (Some(_), None) | (None, Some(_)) => true,
                        // Written while cargo built or after: what it was then is not known.
                        (Some(_), Some((then, _))) if *then > built_at => true,
                        (Some(now), Some(_)) if now <= built_at => false,
                        (Some(now), Some((_, print))) => {
                            tests_only = tests_only.max(Some(now));
                            without_tests(file).as_ref() != Some(print)
                        }
                    }
                });
                if let Some((file, _)) = changed {
                    return Some(format!("{what} {} changed", file.display()));
                }
                let stale = cargo.iter().filter(|it| written_at(it) < tests_only);
                for file in stale {
                    let file = std::fs::File::options().write(true).open(file);
                    _ = file.and_then(|it| it.set_modified(std::time::SystemTime::now()));
                }
                None
            }
            Guard::Tree { what, dir } => {
                let changed = newer_in(dir, built_at)?;
                Some(format!("{what} and {} changed", changed.display()))
            }
            Guard::AsLeft { file, written } => {
                (written_at(file) != *written).then(|| format!("{} changed", file.display()))
            }
            Guard::Env { name, value } => (std::env::var(&*name).ok() != *value)
                .then(|| format!("the environment variable {name} changed")),
        }
    }

    fn files(what: &str, files: impl IntoIterator<Item = PathBuf>) -> Guard {
        let files = files.into_iter().map(|it| {
            let there = it.exists();
            (it, there)
        });
        Guard::Files { what: what.to_owned(), files: files.collect() }
    }

    fn beside_tests(what: &str, files: Vec<PathBuf>, cargo: Vec<PathBuf>) -> Guard {
        let files = files.into_iter().map(|it| {
            let was = written_at(&it).zip(without_tests(&it));
            (it, was)
        });
        Guard::BesideTests { what: what.to_owned(), files: files.collect(), cargo }
    }
}

/// A hash of the Rust source in `file` with every item marked `#[test]` or `#[cfg(test)]`
/// blanked, and the blanks it ends with dropped: the same while only those change and every
/// other token keeps its line and column. A file that isn't Rust, one `include!`d or
/// `include_str!`d, is hashed as it is. `None` when it can't be read.
fn without_tests(file: &Path) -> Option<String> {
    use sha2::Digest as _;
    use syntax::{AstNode as _, ast};
    let text = std::fs::read_to_string(file).ok()?;
    if file.extension().is_none_or(|it| it != "rs") {
        let hash = sha2::Sha256::digest(&text);
        return Some(hash[..16].iter().map(|it| format!("{it:02x}")).collect());
    }
    let parse = syntax::SourceFile::parse(&text, syntax::Edition::CURRENT);
    let mut bytes = text.clone().into_bytes();
    let only_tests = |item: &ast::Item| {
        ast::HasAttrs::attrs(item).any(|attr| {
            let attr: String =
                attr.syntax().text().to_string().chars().filter(|it| !it.is_whitespace()).collect();
            attr == "#[test]" || attr == "#[cfg(test)]"
        })
    };
    for item in parse.tree().syntax().descendants().filter_map(ast::Item::cast) {
        if only_tests(&item) {
            let range = item.syntax().text_range();
            for byte in &mut bytes[usize::from(range.start())..usize::from(range.end())] {
                if *byte != b'\n' {
                    *byte = b' ';
                }
            }
        }
    }
    let blanked = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = blanked.lines().map(str::trim_end).collect();
    let kept = lines.len() - lines.iter().rev().take_while(|it| it.is_empty()).count();
    let hash = sha2::Sha256::digest(lines[..kept].join("\n"));
    Some(hash[..16].iter().map(|it| format!("{it:02x}")).collect())
}

/// When the file was last written, as a stamp. `None` when it is not there.
fn written_at(file: &Path) -> Option<u128> {
    std::fs::metadata(file).and_then(|it| it.modified()).ok().map(stamp_of)
}

/// A file below `dir` that was written after `since`. What cargo and git keep there is not
/// the package's.
fn newer_in(dir: &Path, since: u128) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let (path, name) = (entry.path(), entry.file_name());
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            if name != "target"
                && name != ".git"
                && let Some(found) = newer_in(&path, since)
            {
                return Some(found);
            }
        } else if entry.metadata().and_then(|it| it.modified()).is_ok_and(|it| stamp_of(it) > since)
        {
            return Some(path);
        }
    }
    None
}

/// What cargo said while it built.
pub(crate) struct Built {
    /// When cargo started.
    started: u128,
    artifacts: Vec<Artifact>,
    scripts: Vec<BuildScript>,
    /// What cargo said of each build script it ran and each proc macro it built, a line
    /// each as it said it, a fresh one too.
    said: Vec<String>,
}

/// The crates of a workspace the first build of it leaves to the daemon.
pub(crate) struct Staged {
    /// The root file of each.
    roots: Vec<PathBuf>,
    /// What the daemon is told of the workspace before cargo starts, so that it loads it
    /// while cargo builds. `None` when the build is of no one project of it.
    pub(crate) ahead: Option<daemon::Ahead>,
}

impl Staged {
    /// `Err` is why rustc builds every crate of the build `asked` for. The crates that
    /// run during the build are rustc's, and so are the ones those depend on: a build
    /// script, a proc macro, and what either is declared to need under any feature.
    pub(crate) fn of(asked: &Invocation) -> anyhow::Result<Result<Staged, String>> {
        if asked.subcommand == Subcommand::Test || asked.flags.selects_targets {
            return Ok(Err("the build picks its targets".to_owned()));
        }
        let metadata = metadata(asked)?;
        let members: Vec<&Package> = metadata.workspace_packages();
        let needs = |package: &Package, kinds: &[DependencyKind]| -> Vec<usize> {
            let needed = package.dependencies.iter().filter(|it| kinds.contains(&it.kind));
            let dirs = needed.filter_map(|it| it.path.as_ref());
            dirs.filter_map(|dir| members.iter().position(|it| package_dir(it) == *dir)).collect()
        };
        let is_macro = |it: &&Package| it.targets.iter().any(|it| it.is_proc_macro());
        let mut at_build: Vec<usize> =
            members.iter().flat_map(|it| needs(it, &[DependencyKind::Build])).collect();
        at_build.extend((0..members.len()).filter(|&at| is_macro(&&members[at])));
        let mut reached = FxHashSet::default();
        while let Some(member) = at_build.pop() {
            if reached.insert(member) {
                let kinds = [DependencyKind::Normal, DependencyKind::Build];
                at_build.extend(needs(members[member], &kinds));
            }
        }
        let mut roots = Vec::new();
        for (at, member) in members.iter().enumerate() {
            if reached.contains(&at) {
                continue;
            }
            let of_build = |it: &&Target| {
                let apart = [
                    TargetKind::CustomBuild,
                    TargetKind::Example,
                    TargetKind::Test,
                    TargetKind::Bench,
                ];
                !it.kind.iter().any(|it| apart.contains(it))
            };
            for target in member.targets.iter().filter(of_build) {
                let alone = matches!(
                    *target.crate_types,
                    [CrateType::Lib | CrateType::RLib | CrateType::Bin]
                );
                if !alone {
                    return Ok(Err(format!(
                        "`{}` is linked into more than a program of the workspace",
                        target.name
                    )));
                }
                roots.push(target.src_path.clone().into_std_path_buf());
            }
        }
        if roots.is_empty() {
            return Ok(Err("every crate of the workspace runs during the build".to_owned()));
        }
        // Before cargo builds what would then be built again.
        if let Some(why) = other_toolchain() {
            return Ok(Err(why));
        }
        let root = metadata.workspace_root.clone().into_std_path_buf();
        let ahead = project(asked, &std::env::current_dir()?, &metadata).ok().map(|project| {
            let config = WorkerConfig {
                repository: repository(&root),
                project: project.strip_prefix(&root).unwrap_or(Path::new("")).to_owned(),
                cfg_test: false,
                selection: asked.selection(),
                env: worker_env(),
            };
            daemon::Ahead { config, crates: roots.clone(), root }
        });
        Ok(Ok(Staged { roots, ahead }))
    }
}

/// What the checker of `cargo ra-jit check` with the flags of `asked` loads: the workspace
/// as a build of those flags has it, with the crates of every package of it. A flag that
/// picks the tests checks them as `cargo check --tests` does, with `cfg(test)`.
pub(crate) fn checked(asked: &Invocation) -> anyhow::Result<daemon::Ahead> {
    if let Some(why) = other_toolchain() {
        anyhow::bail!(why);
    }
    let metadata = metadata(asked)?;
    let root = metadata.workspace_root.clone().into_std_path_buf();
    let project =
        project(asked, &std::env::current_dir()?, &metadata).map_err(anyhow::Error::msg)?;
    let crates: Vec<PathBuf> = metadata
        .workspace_packages()
        .iter()
        .flat_map(|it| &it.targets)
        .filter(|it| !it.is_custom_build())
        .map(|it| it.src_path.clone().into_std_path_buf())
        .collect();
    anyhow::ensure!(!crates.is_empty(), "the workspace has no crate to check");
    let config = WorkerConfig {
        repository: repository(&root),
        project: project.strip_prefix(&root).unwrap_or(Path::new("")).to_owned(),
        cfg_test: asked.flags.dev_targets,
        selection: asked.selection(),
        env: worker_env(),
    };
    Ok(daemon::Ahead { config, crates, root })
}

impl Built {
    /// Runs cargo on what `asked` builds, with ra-jit as the wrapper of the workspace's
    /// crates. Progress and diagnostics are cargo's own. `Err` is cargo's exit status when
    /// it failed.
    /// `ahead` is what the daemon was told of the build before cargo started, for the
    /// worker that loads it meanwhile to hear what the scripts said.
    pub(crate) fn by_cargo(
        asked: &Invocation,
        ahead: Option<&daemon::Ahead>,
    ) -> anyhow::Result<Result<Built, i32>> {
        Built::by(asked, None, ahead)
    }

    /// The same, with rustc building of the crates rooted at `staged` only what stands in
    /// for them: the code of those is the daemon's to compile.
    pub(crate) fn staged(
        asked: &Invocation,
        staged: &Staged,
    ) -> anyhow::Result<Result<Built, i32>> {
        Built::by(asked, Some(staged), staged.ahead.as_ref())
    }

    fn by(
        asked: &Invocation,
        staged: Option<&Staged>,
        ahead: Option<&daemon::Ahead>,
    ) -> anyhow::Result<Result<Built, i32>> {
        let started = stamp_now();
        let mut cargo = Command::new(cargo_path());
        match asked.subcommand {
            Subcommand::Build | Subcommand::Run | Subcommand::Check => cargo.arg("build"),
            Subcommand::Test => cargo.args(["test", "--no-run"]),
        };
        cargo.args(&asked.build_args).arg("--message-format=json-render-diagnostics");
        as_built(&mut cargo)?;
        if let Some(staged) = staged {
            cargo.env(switch::VARIABLE, switch::first_wrapper_path()?);
            cargo.env(wrapper::STAGED, std::env::join_paths(&staged.roots)?);
        }
        cargo.stdout(Stdio::piped());
        let mut cargo = cargo.spawn().context("running cargo")?;
        let said = cargo.stdout.take().context("cargo has no output")?;
        let mut built =
            Built { started, artifacts: Vec::new(), scripts: Vec::new(), said: Vec::new() };
        let ahead = ahead.map(|it| it.config.id());
        // Cargo waits while its output is unread, so the daemon is told on a thread of its own.
        let (tell, told) = std::sync::mpsc::channel::<Request>();
        let telling = std::thread::spawn(move || {
            for said in told {
                let _ = client::ask(said, Start::Never);
            }
        });
        for message in Message::parse_stream(BufReader::new(said)) {
            let message = message?;
            // The worker that loads the workspace meanwhile reads what the scripts said and
            // where the proc macros are from cargo's one build of them, and the symbols of
            // each library as it is built.
            let scripted = match &message {
                Message::BuildScriptExecuted(_) => true,
                Message::CompilerArtifact(it) => it.target.is_proc_macro(),
                _ => false,
            };
            let loads = scripted
                || matches!(&message, Message::CompilerArtifact(it)
                    if it.filenames.iter().any(|it| it.extension() == Some("rlib")));
            if loads && let Some(id) = &ahead {
                let said = Request::Said { id: id.clone(), line: serde_json::to_string(&message)? };
                let _ = tell.send(said);
            }
            if scripted {
                built.said.push(serde_json::to_string(&message)?);
            }
            match message {
                Message::CompilerArtifact(artifact) => built.artifacts.push(artifact),
                Message::BuildScriptExecuted(script) => built.scripts.push(script),
                // Whoever reads this may stop reading before its end.
                Message::TextLine(line) => {
                    let _ = writeln!(std::io::stdout(), "{line}");
                }
                _ => {}
            }
        }
        // What cargo said reaches the worker before it is told the build is over.
        drop(tell);
        let _ = telling.join();
        let status = cargo.wait()?;
        Ok(if status.success() { Ok(built) } else { Err(status.code().unwrap_or(101)) })
    }
}

impl State {
    /// The file in the record of a unit that names the builds its program is in, a key a
    /// line.
    pub(crate) const STATES: &str = "states";

    fn path(key: &str) -> anyhow::Result<PathBuf> {
        Ok(daemon::home()?.join("states").join(format!("{key}.json")))
    }

    /// The state of the build `key`. `Err` is why what is there is not read, when there
    /// is something.
    pub(crate) fn read(key: &str) -> Result<State, Option<&'static str>> {
        let path = State::path(key).map_err(|_| None)?;
        let text = std::fs::read_to_string(path).map_err(|_| None)?;
        let another = "this build's state was written by another build of ra-jit";
        let state = serde_json::from_str::<State>(&text).map_err(|_| another)?;
        if state.version != daemon::version() {
            return Err(Some(another));
        }
        if !state.programs.iter().all(|it| it.cargos.exists()) {
            return Err(Some("what cargo built is no longer in the target directory"));
        }
        Ok(state)
    }

    /// The states that have the program rustc linked from the unit recorded in `unit`, by
    /// their keys: the builds of the checkout that share the unit.
    pub(crate) fn of_unit(unit: &Path) -> Vec<String> {
        let keys = std::fs::read_to_string(unit.join(State::STATES)).unwrap_or_default();
        keys.lines().map(str::to_owned).collect()
    }

    /// Forgets the builds `keys`: rustc has linked a program of theirs again, and what
    /// they kept of cargo's is not what cargo has now.
    pub(crate) fn forget<'a>(keys: impl IntoIterator<Item = &'a String>) {
        for key in keys {
            for path in [State::path(key), State::kept_path(key)].into_iter().flatten() {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    /// The program linked from the unit recorded in `unit`, the checkout it is asked for
    /// in, and where it is among that checkout's programs. `Err` is why the daemon is not
    /// asked for it.
    pub(crate) fn asked_for(&self, unit: &Path) -> Result<(&Checkout, usize), String> {
        let program = self.of_unit_dir(unit).ok_or("the build no longer has the program")?;
        match (program.asked_as, &program.left_to_cargo) {
            (Some((checkout, at)), _) => Ok((&self.checkouts[checkout], at)),
            (None, why) => Err(why.clone().unwrap_or_default()),
        }
    }

    fn of_unit_dir(&self, unit: &Path) -> Option<&Program> {
        self.programs.iter().find(|it| it.unit.as_deref() == Some(unit))
    }

    /// Has `binary` be what cargo has of the program linked from the unit recorded in
    /// `unit`: cargo was given it where it had rustc's, for the source as it is now.
    pub(crate) fn cargo_has(&self, unit: &Path, binary: &Path) -> std::io::Result<()> {
        match self.of_unit_dir(unit) {
            Some(program) => place(binary, &program.cargos),
            None => Ok(()),
        }
    }

    pub(crate) fn write(&self) -> anyhow::Result<()> {
        let path = State::path(&self.key)?;
        std::fs::create_dir_all(path.parent().context("a state without a directory")?)?;
        let new = path.with_extension(format!("{}.new", std::process::id()));
        std::fs::write(&new, serde_json::to_vec(self)?)?;
        std::fs::rename(&new, &path)?;
        for unit in self.programs.iter().filter_map(|it| it.unit.as_deref()) {
            if !State::of_unit(unit).contains(&self.key) {
                let mut states =
                    OpenOptions::new().create(true).append(true).open(unit.join(State::STATES))?;
                writeln!(states, "{}", self.key)?;
            }
        }
        let kept: Vec<(&Path, &Path)> = self
            .programs
            .iter()
            .flat_map(|program| program.places().map(|it| (it, &*program.cargos)))
            .collect();
        Ok(std::fs::write(State::kept_path(&self.key)?, serde_json::to_vec(&kept)?)?)
    }

    /// Every state this ra-jit wrote for the user.
    pub(crate) fn all() -> Vec<State> {
        let dir = daemon::home().map(|it| it.join("states"));
        let entries = dir.ok().and_then(|it| std::fs::read_dir(it).ok()).into_iter().flatten();
        let keys = entries.flatten().filter_map(|it| {
            let name = it.file_name();
            let key = name.to_str()?.strip_suffix(".json")?;
            (!key.contains('.')).then(|| key.to_owned())
        });
        keys.filter_map(|it| State::read(&it).ok()).collect()
    }

    /// Why the next build is cargo's: something cargo acts on changed since it built.
    pub(crate) fn broken_guard(&mut self) -> Option<String> {
        self.guards.iter_mut().find_map(|it| it.broken(self.built_at))
    }

    /// Why the next build is cargo's: a program the daemon is never asked for has new
    /// source.
    pub(crate) fn stale_cargos(&self) -> Option<String> {
        self.left_to_cargo().find_map(|(program, why)| {
            let file = program.newer_input(self.built_at)?;
            Some(format!("`{}`: {why}, and {} changed", program.name, file.display()))
        })
    }

    /// `said`, the errors the daemon found in the source, ended as cargo ends a build rustc
    /// turned down: with the program the build is of.
    pub(crate) fn turned_down(&self, said: &crate::errors::Said) -> String {
        let Some(program) = self.programs.iter().find(|it| it.asked_as.is_some()) else {
            return said.to_string();
        };
        let name = &program.name;
        let target = match (program.kind, program.test) {
            (Kind::Lib, false) => "lib".to_owned(),
            (Kind::Lib, true) => "lib test".to_owned(),
            (Kind::Bin, false) => format!("bin \"{name}\""),
            (Kind::Bin, true) => format!("bin \"{name}\" test"),
            (Kind::Example, _) => format!("example \"{name}\""),
            (Kind::Test, _) => format!("test \"{name}\""),
            (Kind::Bench, _) => format!("bench \"{name}\""),
        };
        let errors = match said.count() {
            0 | 1 => "1 previous error".to_owned(),
            n => format!("{n} previous errors"),
        };
        let package = &program.package;
        let mut notes = said.notes().join("\n");
        if !notes.is_empty() {
            notes.push('\n');
        }
        format!("{said}\n\n{notes}error: could not compile `{package}` ({target}) due to {errors}")
    }

    /// Counts a build that went to cargo.
    pub(crate) fn count_cargo(&mut self, reason: &str) {
        *self.counts.cargo.entry(reason.to_owned()).or_default() += 1;
    }

    /// The executables of the build `key` and the hard link kept to what rustc linked of
    /// each. A file of its own, in a shape that stays: it is read when the state can't be.
    fn kept_path(key: &str) -> anyhow::Result<PathBuf> {
        Ok(daemon::home()?.join("states").join(format!("{key}.kept.json")))
    }

    /// Puts the executables rustc linked back where cargo has them, for cargo to find what
    /// it built. One whose link is gone is deleted, and cargo links it again.
    pub(crate) fn give_back(key: &str) {
        let kept = State::kept_path(key).ok().and_then(|it| std::fs::read(it).ok());
        let kept: Vec<(PathBuf, PathBuf)> =
            kept.and_then(|it| serde_json::from_slice(&it).ok()).unwrap_or_default();
        let _held = build_locks(kept.iter().map(|(executable, _)| &**executable));
        for (executable, cargos) in kept {
            // Whatever goes wrong here, cargo finds a file that is not its own or none,
            // and builds the program again.
            let _ = if is_placed(&cargos, &executable) {
                Ok(())
            } else if cargos.is_file() {
                place(&cargos, &executable)
            } else {
                std::fs::remove_file(&executable)
            };
        }
    }

    /// Has every program be where cargo has it: the daemon's executable of one it built,
    /// and cargo's own of one it did not, which is the program while none of its source is
    /// written. `answers` is what the daemon said of each program of each checkout. `Ok`
    /// are the programs that stay cargo's and why; `Err` is why cargo builds, when a
    /// program the daemon did not build has new source. Under `held`, which is this build's.
    pub(crate) fn settle(
        &self,
        answers: &[Vec<Result<(), String>>],
        _held: &Held,
    ) -> Result<Vec<(&Program, String)>, String> {
        let mut cargos = Vec::new();
        for program in &self.programs {
            let answer = match program.asked_as {
                Some((checkout, at)) => answers
                    .get(checkout)
                    .and_then(|it| it.get(at))
                    .cloned()
                    .unwrap_or_else(|| Err("the daemon said nothing of it".to_owned())),
                None => Err(program.left_to_cargo.clone().unwrap_or_default()),
            };
            let from = match (&answer, program.asked_as) {
                (Ok(()), Some((checkout, at))) => &self.checkouts[checkout].programs[at].binary,
                _ => &program.cargos,
            };
            // On Linux cargo's program is the daemon's file, which whatever starts it next
            // starts with no word to the daemon, which may still be writing it.
            let _unwritten = answer.is_ok().then(|| crate::splice::unwritten(from));
            let package = match (&answer, program.asked_as) {
                (Ok(()), Some((checkout, at))) => {
                    let plan = &self.checkouts[checkout].programs[at];
                    let rustcs = plan.package.as_deref();
                    rustcs.map(|it| packed(&plan.binary, &plan.object, it)).transpose()?
                }
                _ => None,
            };
            if let Err(why) = answer {
                if let Some(file) = program.newer_input(self.built_at) {
                    let name = &program.name;
                    return Err(format!("`{name}`: {why}, and {} changed", file.display()));
                }
                cargos.push((program, why));
            }
            for to in program.places() {
                install(from, to)?;
                if let Some(package) = &package {
                    install(package, &package::beside(to))?;
                }
            }
        }
        Ok(cargos)
    }

    /// Has each program be, where cargo's own is kept, a copy of the one the daemon
    /// built: the daemon writes an edit into its own.
    pub(crate) fn keep_daemons(&self) -> anyhow::Result<()> {
        for program in &self.programs {
            let Some((checkout, at)) = program.asked_as else { continue };
            let mut new = program.cargos.as_os_str().to_owned();
            new.push(format!(".{}.new", std::process::id()));
            std::fs::copy(&self.checkouts[checkout].programs[at].binary, &new)?;
            std::fs::rename(&new, &program.cargos)?;
        }
        Ok(())
    }

    /// Cargo's locks on the build directories of the programs, held until the programs this
    /// command runs have started.
    pub(crate) fn hold(&self) -> Held {
        Held { _locks: build_locks(self.programs.iter().map(|it| &*it.executable)) }
    }

    /// The programs cargo built that the daemon is never asked for, and why.
    pub(crate) fn left_to_cargo(&self) -> impl Iterator<Item = (&Program, &str)> {
        self.programs.iter().filter_map(|it| Some((it, it.left_to_cargo.as_deref()?)))
    }

    /// The state after cargo built `asked` in `cwd`: what it built, what the daemon can
    /// build of it, and what the next build has to check.
    pub(crate) fn after(
        asked: &Invocation,
        cwd: &Path,
        key: &str,
        built: Built,
        counts: Counts,
    ) -> anyhow::Result<State> {
        let metadata = metadata(asked)?;
        // By its real path, as rustc's working directory and so the sources of the units
        // are: a workspace reached through a link is the one it links to.
        let root = metadata.workspace_root.as_std_path();
        let root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_owned());
        let dir = metadata.target_directory.as_std_path().join("ra-jit").join(key);
        let members: FxHashSet<_> = metadata.workspace_members.iter().collect();
        let artifacts: Vec<&Artifact> =
            built.artifacts.iter().filter(|it| members.contains(&it.package_id)).collect();
        let records = records_of(&artifacts)?;
        let used = used_by(&artifacts, &records);

        let mut state = State {
            version: daemon::version(),
            key: key.to_owned(),
            root: root.clone(),
            built_at: built.started,
            profile: asked.profile().to_owned(),
            programs: Vec::new(),
            guards: Vec::new(),
            checkouts: Vec::new(),
            refusal: None,
            doc_tests: false,
            counts,
            configured: configured_env(cwd),
        };
        let project = match project(asked, cwd, &metadata) {
            Ok(project) => dunce::canonicalize(&project).unwrap_or(project),
            Err(why) => {
                state.refusal = Some(why);
                root.clone()
            }
        };
        if state.refusal.is_none() {
            state.refusal = other_toolchain();
        }
        let env = match units::places_named(&used) {
            Ok(env) => env,
            Err(why) => {
                state.refusal.get_or_insert(why);
                BTreeMap::new()
            }
        };
        let config = |unit_test: bool| WorkerConfig {
            repository: repository(&root),
            project: project.strip_prefix(&root).unwrap_or(Path::new("")).to_owned(),
            cfg_test: unit_test,
            selection: asked.selection(),
            env: worker_env(),
        };
        let sysroot_libraries = sysroot_libraries();
        let scripts = match kept_said(&dir, &built.said) {
            Ok(file) => Some(file),
            Err(why) => {
                eprintln!("ra-jit: what the build scripts said is not kept: {why}");
                None
            }
        };
        let checkout = |unit_test: bool| Checkout {
            key: format!("{key}{}", if unit_test { "-unit" } else { "" }),
            generation: 0,
            built_at: built.started,
            config: config(unit_test),
            root: root.clone(),
            programs: Vec::new(),
            sources: Vec::new(),
            env: env.clone(),
            scripts: scripts.clone(),
        };
        let mut checkouts = [checkout(false), checkout(true)];
        let executables = artifacts.iter().filter(|it| !it.target.is_custom_build());
        for artifact in executables {
            let Some(executable) = &artifact.executable else { continue };
            let package = metadata.packages.iter().find(|it| it.id == artifact.package_id);
            let package = package.context("cargo built a package it does not list")?;
            let kind = Kind::of(&artifact.target.kind);
            let test = artifact.profile.test;
            let unit_test = test && !matches!(kind, Kind::Test | Kind::Bench);
            let name = artifact.target.name.clone();
            let unit = Record::of_artifact(&used, executable.as_std_path());
            // Named as cargo names the unit, the same in every build of it.
            let files = dir.join("programs").join(unit.map_or(&name, |it| &it.id));
            std::fs::create_dir_all(&files)?;
            let cargos = files.join("cargo");
            place(executable.as_std_path(), &cargos)?;
            let names = unit.map(|it| it.outputs()).unwrap_or_default().into_iter();
            let names = names
                .filter(|it| it != executable.as_std_path())
                .filter(|it| same_file::is_same_file(it, executable).unwrap_or(false))
                .collect();
            let plan = match unit {
                Some(unit) => Plan::of(unit, &used, &files, unit_test)?,
                None => Err(format!(
                    "cargo built `{name}` before ra-jit recorded what it is linked from; \
                     `cargo clean -p {}` has it built again",
                    package.name
                )),
            };
            let (asked_as, left_to_cargo) = match plan {
                Ok(plan) => {
                    let checkout = &mut checkouts[usize::from(unit_test)];
                    checkout.generation = checkout.generation.max(plan.stamp);
                    checkout.programs.push(plan);
                    (Some((usize::from(unit_test), checkout.programs.len() - 1)), None)
                }
                Err(why) => (None, Some(why)),
            };
            let compiled = reached_from(unit.into_iter().collect(), &used);
            let inputs: FxHashSet<PathBuf> = compiled
                .iter()
                .flat_map(|it| it.inputs().files)
                .filter(|it| !it.starts_with(metadata.target_directory.as_std_path()))
                .collect();
            let default_run =
                kind == Kind::Bin && !test && package.default_run.as_ref() == Some(&name);
            state.programs.push(Program {
                default_run,
                name,
                kind,
                package: package.name.to_string(),
                test,
                source: artifact.target.src_path.clone().into_std_path_buf(),
                package_dir: package_dir(package),
                executable: executable.clone().into_std_path_buf(),
                names,
                cargos,
                unit: unit.and_then(|it| Some(Record::dir_for(&it.out_dir()?).join(&it.id))),
                asked_as,
                left_to_cargo,
                inputs: inputs.into_iter().collect(),
                env: run_env(package, &built.scripts)
                    .into_iter()
                    .chain(library_path(executable.as_std_path(), sysroot_libraries.as_deref()))
                    .collect(),
            });
        }
        state
            .programs
            .sort_by(|a, b| (&a.package, a.kind, &a.name).cmp(&(&b.package, b.kind, &b.name)));
        for checkout in &mut checkouts {
            let sources = checkout.programs.iter().flat_map(|plan| {
                let compiled = used.iter().filter(|it| plan.crates.contains(&it.source));
                compiled.flat_map(|it| it.inputs().files)
            });
            // What a build script wrote is cargo's, written while it built.
            let target_dir = metadata.target_directory.as_std_path();
            let sources: FxHashSet<PathBuf> =
                sources.filter(|it| !it.starts_with(target_dir)).collect();
            checkout.sources = sources.into_iter().collect();
        }
        state.checkouts = checkouts.into_iter().collect();
        // Cargo runs the doc tests of the packages it runs the tests of.
        let tested: FxHashSet<_> =
            artifacts.iter().filter(|it| it.profile.test).map(|it| &it.package_id).collect();
        // A proc macro is a library with doc tests as much as any other.
        let library = |it: &cargo_metadata::Target| {
            it.is_lib() || it.is_rlib() || it.is_dylib() || it.is_proc_macro()
        };
        let documented = |it: &&Package| it.targets.iter().any(|it| library(it) && it.doctest);
        state.doc_tests =
            metadata.packages.iter().filter(|it| tested.contains(&it.id)).any(|it| documented(&it));
        state.guards = guards(cwd, &metadata, &used, &built.scripts, &state.checkouts);
        let kept: Vec<PathBuf> = State::all()
            .iter()
            .chain([&state])
            .flat_map(|it| it.checkouts.iter().flat_map(|it| &it.programs))
            .flat_map(|it| it.copies.clone())
            .collect();
        for dir in record_dirs(&artifacts) {
            units::collect_garbage(&dir, &kept)?;
        }
        Ok(state)
    }
}

impl Program {
    /// Every path cargo has the executable at.
    fn places(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(&*self.executable).chain(self.names.iter().map(|it| &**it))
    }

    /// The first of its source files that was written after `built_at`.
    fn newer_input(&self, built_at: u128) -> Option<&Path> {
        let newer = |file: &&PathBuf| written_at(file).is_some_and(|it| it > built_at);
        self.inputs.iter().find(newer).map(PathBuf::as_path)
    }
}

/// Cargo's lock on each build directory one of `executables` is in, held: cargo takes it
/// for a build and waits for it as this does. Every build, fresh or not, cargo puts the
/// program in `deps` under its other name again, removing the file there first when the
/// two are not one file, and while a program is installed under its names one by one they
/// are not: a `cargo run` of another client of the checkout that found that moment would
/// find no program. So does a program started while a cargo writes it, and one started
/// after another client gave cargo its own back is not this build's: a client holds the
/// locks from putting its programs in place until they have started. A directory cargo
/// keeps no lock in, on NFS, has none to hold.
fn build_locks<'a>(executables: impl Iterator<Item = &'a Path>) -> Vec<std::fs::File> {
    // `debug/app`, `debug/examples/app` and `debug/deps/app-…` are all of `debug`.
    let lock_of = |executable: &Path| {
        let above = executable.ancestors().skip(1).take(2);
        above.map(|dir| dir.join(".cargo-lock")).find(|it| it.is_file())
    };
    let mut locks: Vec<PathBuf> = executables.filter_map(lock_of).collect();
    // In one order, so that two clients that each want two wait for neither.
    locks.sort();
    locks.dedup();
    let held = |lock: PathBuf| {
        let file = std::fs::File::open(lock).ok()?;
        if file.try_lock().is_err() {
            super::say::status(false, "Blocking", "waiting for file lock on build directory");
            file.lock().ok()?;
        }
        Some(file)
    };
    locks.into_iter().filter_map(held).collect()
}

/// Cargo's locks on build directories, held while this is.
pub(crate) struct Held {
    _locks: Vec<std::fs::File>,
}

/// Has `to` be the file `from` is, when it is not.
fn install(from: &Path, to: &Path) -> Result<(), String> {
    if is_placed(from, to) {
        return Ok(());
    }
    place(from, to).map_err(|it| format!("installing {}: {it}", to.display()))
}

/// Whether `to` is what `place` made of `from`: the file itself, or on macOS its copy, which
/// has its length and the time it was last written.
fn is_placed(from: &Path, to: &Path) -> bool {
    if same_file::is_same_file(from, to).unwrap_or(false) {
        return true;
    }
    let (Ok(from), Ok(to)) = (from.metadata(), to.metadata()) else { return false };
    let written = |it: &std::fs::Metadata| it.modified().ok();
    cfg!(target_os = "macos")
        && from.len() == to.len()
        && written(&from).is_some()
        && written(&from) == written(&to)
}

/// Writes `said`, what cargo said of the build scripts and proc macros of a build, to the
/// file `scripts` in `dir`, whole or not at all: a worker started after the build loads the
/// workspace with it, and does not build the scripts and proc macros again in a target
/// directory of its own to find out.
fn kept_said(dir: &Path, said: &[String]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let file = dir.join("scripts");
    let mut new = file.as_os_str().to_owned();
    new.push(format!(".{}.new", std::process::id()));
    std::fs::write(&new, said.join("\n"))?;
    std::fs::rename(&new, &file)?;
    Ok(file)
}

/// The package of what a debugger reads of the executable at `binary`, linked from `object`
/// and what rustc compiled, whose package is `rustcs`: written again when either is newer.
fn packed(binary: &Path, object: &Path, rustcs: &Path) -> Result<PathBuf, String> {
    let package = package::beside(binary);
    if !package::is_current(&package, &[object, rustcs]) {
        package::pack(binary, rustcs, &package)
            .map_err(|it| format!("packing {}: {it:#}", package.display()))?;
    }
    Ok(package)
}

/// Has `to` be the file `from` is, under a second name: what was at `to` stays as it is
/// for whoever runs it. On macOS a copy, which is a clone and as quick: there a program
/// started from a hard link just made can be killed as it starts, which is why cargo
/// copies there too (rust-lang/cargo#7821).
pub(crate) fn place(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut new = to.as_os_str().to_owned();
    new.push(format!(".{}.new", std::process::id()));
    let _ = std::fs::remove_file(&new);
    // Or another filesystem.
    if cfg!(target_os = "macos") || std::fs::hard_link(from, &new).is_err() {
        std::fs::copy(from, &new)?;
    }
    std::fs::rename(&new, to)
}

/// Gives `command` the environment cargo built in, so that a cargo it runs, or is, has the
/// same units: cargo keys the crates of the workspace by their wrapper, and one run
/// without it builds each program again and puts its own where this build's is, under a
/// program that is running.
pub(crate) fn as_built(command: &mut Command) -> std::io::Result<()> {
    command.env(switch::VARIABLE, switch::wrapper_path()?).env(wrapper::DRIVEN, "1");
    every_crate(command)
}

/// Has cargo run the rustc of every crate through ra-jit's wrapper, for the build scripts of
/// the packages outside the workspace (docs/no-rustc.md, step I), unless the user has a
/// wrapper there. Cargo keys no unit by this wrapper, so the builds of plain cargo are still
/// this one's.
pub(crate) fn every_crate(command: &mut Command) -> std::io::Result<()> {
    let dir =
        command.get_current_dir().map_or_else(std::env::current_dir, |it| Ok(it.to_owned()))?;
    if !switch::user_wraps(&dir) {
        command.env("RUSTC_WRAPPER", switch::wrapper_path()?).env(wrapper::EVERY_CRATE, "1");
    }
    Ok(())
}

/// The cargo to run: the one that ran this command, or the one on the path.
pub(crate) fn cargo_path() -> PathBuf {
    std::env::var_os("CARGO").map_or_else(|| PathBuf::from("cargo"), PathBuf::from)
}

/// The rustc cargo builds with: the one it is told, else the one beside the cargo that ran
/// this command, which is the toolchain's and not the rustup proxy on the path that would
/// pick it again.
fn rustc_path() -> OsString {
    if let Some(rustc) = std::env::var_os("RUSTC") {
        return rustc;
    }
    let rustc = format!("rustc{}", std::env::consts::EXE_SUFFIX);
    let beside = std::env::var_os("CARGO").map(|it| PathBuf::from(it).with_file_name(rustc));
    beside.filter(|it| it.is_file()).map_or_else(|| "rustc".into(), PathBuf::into_os_string)
}

pub(super) fn metadata(asked: &Invocation) -> anyhow::Result<Metadata> {
    let mut command = MetadataCommand::new();
    command.cargo_path(cargo_path()).no_deps();
    if let Some(manifest) = &asked.flags.manifest_path {
        command.manifest_path(manifest);
    }
    command.exec().context("running `cargo metadata`")
}

fn package_dir(package: &Package) -> PathBuf {
    package.manifest_path.parent().map(|it| it.as_std_path().to_owned()).unwrap_or_default()
}

/// The directories the records of what cargo just built are in.
fn record_dirs(artifacts: &[&Artifact]) -> FxHashSet<PathBuf> {
    let files = artifacts.iter().flat_map(|it| &it.filenames);
    files.filter_map(|it| it.parent()).map(|it| Record::dir_for(it.as_std_path())).collect()
}

fn records_of(artifacts: &[&Artifact]) -> anyhow::Result<Vec<Record>> {
    let mut records = Vec::new();
    for dir in record_dirs(artifacts) {
        records.extend(Record::all(&dir)?);
    }
    Ok(records)
}

/// The records of the units of this build: a directory has those of other builds too. Cargo
/// names a program by its hard link to what rustc wrote or by its copy of it, so the files
/// are told by what they are and not by their paths.
fn used_by(artifacts: &[&Artifact], records: &[Record]) -> Vec<Record> {
    let files = artifacts.iter().flat_map(|it| &it.filenames);
    let is_built = |record: &&Record| files.clone().any(|it| record.wrote(it.as_std_path()));
    records.iter().filter(is_built).cloned().collect()
}

/// The directory of the package the build is of, whose features the daemon loads the
/// workspace with. `Err` is why the selection is one it does not load.
fn project(asked: &Invocation, cwd: &Path, metadata: &Metadata) -> Result<PathBuf, String> {
    let root = metadata.workspace_root.as_std_path();
    if let Some(named) = asked.flags.packages.first() {
        let name = named.split('@').next().unwrap_or(named);
        let package = metadata.workspace_packages().into_iter().find(|it| it.name.as_str() == name);
        return package
            .map(package_dir)
            .ok_or(format!("`-p {named}` is not a package of the workspace"));
    }
    if asked.flags.workspace {
        return Ok(root.to_owned());
    }
    let start = match &asked.flags.manifest_path {
        Some(manifest) => cwd.join(manifest).parent().map(Path::to_owned).unwrap_or_default(),
        None => cwd.to_owned(),
    };
    let nearest = start.ancestors().find(|it| it.join("Cargo.toml").exists());
    nearest.map(Path::to_owned).ok_or("no Cargo.toml here".to_owned())
}

/// Why the toolchain cargo builds with is not one ra-jit compiles for, when it is not: it
/// is another release than the one ra-jit has the layouts of, or the source of its standard
/// library, which the daemon compiles from, is not installed.
///
/// `RA_JIT_OTHER_TOOLCHAIN=1` takes another release anyway, for a project pinned to a
/// toolchain near ours (a nightly of the next release): the layouts, symbol names, ABI and
/// `TypeId`s are still 1.99.0's, and nothing checks that the other rustc agrees with them.
fn other_toolchain() -> Option<String> {
    let rustc = rustc_path();
    let said = |flag: &str| {
        let output = crate::answers::answer(Command::new(&rustc).arg(flag), &[]).ok()?;
        Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let version = said("--version")?;
    let version = version.split_whitespace().nth(1).unwrap_or_default();
    let ours = TOOLCHAIN.trim();
    if version != ours && !other_toolchain_allowed() {
        return Some(format!(
            "rustc {version} is not the {ours} this ra-jit compiles for \
             (RA_JIT_OTHER_TOOLCHAIN=1 builds with it anyway)"
        ));
    }
    let library = Path::new(&said("--print=sysroot")?).join("lib/rustlib/src/rust/library");
    (!library.exists()).then(|| {
        "the toolchain has no source of the standard library, \
         `rustup component add rust-src` installs it"
            .to_owned()
    })
}

fn other_toolchain_allowed() -> bool {
    std::env::var_os("RA_JIT_OTHER_TOOLCHAIN").is_some_and(|it| it == "1")
}

/// What the checkouts of one repository share: the repository's own directory, which is
/// `.git` in the first checkout and is named by the `.git` file of a worktree. A directory
/// that is no checkout is its own.
fn repository(root: &Path) -> PathBuf {
    // git names the directory its own way, on Windows with other separators than the
    // checkout's path has: both are spelled as the file system spells them.
    let spelled = |dir: &Path| std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_owned());
    let git = root.join(".git");
    if git.is_dir() {
        return spelled(&git);
    }
    let named = std::fs::read_to_string(&git).ok();
    let common = named.as_deref().and_then(|it| {
        // `gitdir: /repo/.git/worktrees/name`
        let dir = Path::new(it.strip_prefix("gitdir:")?.trim());
        dir.ancestors().find(|it| it.file_name().is_some_and(|it| it == "worktrees"))?.parent()
    });
    common.map_or_else(|| root.to_owned(), spelled)
}

/// The variable the dynamic linker of this platform looks for libraries by.
const LIBRARY_PATH: &str = if cfg!(windows) {
    "PATH"
} else if cfg!(target_os = "macos") {
    "DYLD_FALLBACK_LIBRARY_PATH"
} else {
    "LD_LIBRARY_PATH"
};

/// Where the toolchain cargo builds with has the libraries of the host, `libstd` as a
/// dynamic library among them. `None` when rustc does not say.
fn sysroot_libraries() -> Option<PathBuf> {
    let rustc = rustc_path();
    let output =
        crate::answers::answer(Command::new(rustc).arg("--print=target-libdir"), &[]).ok()?;
    let said = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !said.is_empty()).then(|| PathBuf::from(said))
}

/// Where cargo has the dynamic linker look when it runs `executable`: the dependencies of
/// the profile, the profile's own directory, the toolchain's libraries, and then wherever
/// it looked already. A test of a proc macro is linked with the toolchain's `libstd`.
fn library_path(executable: &Path, sysroot: Option<&Path>) -> Option<(String, String)> {
    let inside = |it: &&Path| it.file_name().is_some_and(|it| it == "deps" || it == "examples");
    let dir = executable.parent()?;
    let profile = if inside(&dir) { dir.parent()? } else { dir };
    let already = std::env::var_os(LIBRARY_PATH);
    let already = already.iter().flat_map(std::env::split_paths);
    let dirs =
        [profile.join("deps"), profile.to_owned()].into_iter().chain(sysroot.map(Path::to_owned));
    let joined = std::env::join_paths(dirs.chain(already)).ok()?;
    Some((LIBRARY_PATH.to_owned(), joined.into_string().ok()?))
}

/// The variables cargo sets for a program of `package` when it runs it.
fn run_env(package: &Package, scripts: &[BuildScript]) -> Vec<(String, String)> {
    let version = &package.version;
    let text = |it: &Option<String>| it.clone().unwrap_or_default();
    let mut env = vec![
        ("CARGO".to_owned(), cargo_path().display().to_string()),
        ("CARGO_MANIFEST_DIR".to_owned(), package_dir(package).display().to_string()),
        ("CARGO_MANIFEST_PATH".to_owned(), package.manifest_path.to_string()),
        ("CARGO_PKG_NAME".to_owned(), package.name.to_string()),
        ("CARGO_PKG_VERSION".to_owned(), version.to_string()),
        ("CARGO_PKG_VERSION_MAJOR".to_owned(), version.major.to_string()),
        ("CARGO_PKG_VERSION_MINOR".to_owned(), version.minor.to_string()),
        ("CARGO_PKG_VERSION_PATCH".to_owned(), version.patch.to_string()),
        ("CARGO_PKG_VERSION_PRE".to_owned(), version.pre.to_string()),
        ("CARGO_PKG_AUTHORS".to_owned(), package.authors.join(":")),
        ("CARGO_PKG_DESCRIPTION".to_owned(), text(&package.description)),
        ("CARGO_PKG_HOMEPAGE".to_owned(), text(&package.homepage)),
        ("CARGO_PKG_REPOSITORY".to_owned(), text(&package.repository)),
        ("CARGO_PKG_LICENSE".to_owned(), text(&package.license)),
        (
            "CARGO_PKG_LICENSE_FILE".to_owned(),
            package.license_file.as_ref().map(|it| it.to_string()).unwrap_or_default(),
        ),
        (
            "CARGO_PKG_RUST_VERSION".to_owned(),
            package.rust_version.as_ref().map(|it| it.to_string()).unwrap_or_default(),
        ),
        (
            "CARGO_PKG_README".to_owned(),
            package.readme.as_ref().map(|it| it.to_string()).unwrap_or_default(),
        ),
    ];
    for script in scripts.iter().filter(|it| it.package_id == package.id) {
        env.push(("OUT_DIR".to_owned(), script.out_dir.to_string()));
        env.extend(script.env.iter().cloned());
    }
    env
}

/// The `[env]` of cargo's configuration as cargo reads it in `cwd`: from the `.cargo`
/// directory of each directory up from it, the nearest first, then from cargo's home. Of two
/// files in one directory cargo reads `config`, not `config.toml`. A `relative` value is a
/// path from the directory its `.cargo` is in.
fn configured_env(cwd: &Path) -> Vec<Configured> {
    let in_dir = |dir: &Path| {
        ["config", "config.toml"].map(|it| dir.join(it)).into_iter().find(|it| it.is_file())
    };
    let mut files: Vec<PathBuf> =
        cwd.ancestors().filter_map(|it| in_dir(&it.join(".cargo"))).collect();
    let home = switch::cargo_home().and_then(|it| in_dir(&it));
    if let Some(home) = home
        && !files.iter().any(|it| same_file::is_same_file(it, &home).unwrap_or(false))
    {
        files.push(home);
    }
    let mut env: Vec<Configured> = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let Ok(table) = text.parse::<toml::Table>() else { continue };
        let Some(toml::Value::Table(variables)) = table.get("env") else { continue };
        let base = file.parent().and_then(Path::parent).unwrap_or(Path::new(""));
        for (name, it) in variables {
            if env.iter().any(|known| known.name == *name) {
                continue;
            }
            let flag = |table: &toml::Table, name: &str| {
                table.get(name).and_then(toml::Value::as_bool).unwrap_or(false)
            };
            let (value, force, relative) = match it {
                toml::Value::String(value) => (value.clone(), false, false),
                toml::Value::Table(it) => {
                    let Some(value) = it.get("value").and_then(toml::Value::as_str) else {
                        continue;
                    };
                    (value.to_owned(), flag(it, "force"), flag(it, "relative"))
                }
                _ => continue,
            };
            let value = if relative { base.join(value).display().to_string() } else { value };
            env.push(Configured { name: name.clone(), value, force });
        }
    }
    env
}

/// Everything a change to which makes the next build cargo's.
fn guards(
    cwd: &Path,
    metadata: &Metadata,
    used: &[Record],
    scripts: &[BuildScript],
    checkouts: &[Checkout],
) -> Vec<Guard> {
    let root = metadata.workspace_root.as_std_path();
    let manifests = metadata
        .workspace_packages()
        .into_iter()
        .map(|it| it.manifest_path.clone().into_std_path_buf());
    let mut guards = vec![
        Guard::files("the manifest", manifests.chain([root.join("Cargo.toml")])),
        Guard::AsLeft {
            written: written_at(&root.join("Cargo.lock")),
            file: root.join("Cargo.lock"),
        },
        Guard::files(
            "cargo's configuration",
            named_above(cwd, &switch::CONFIGS).chain(switch::home_configs()),
        ),
        Guard::files("the toolchain file", named_above(cwd, &TOOLCHAIN_FILES)),
    ];
    let in_build = runs_in_build(used);
    let sources = in_build.iter().flat_map(|it| it.inputs().files);
    guards.push(Guard::files("what runs during the build:", sources));
    // What cargo or a build script sets for rustc changes with the manifest or the script.
    let set_for_rustc = |name: &str| {
        name.starts_with("CARGO_")
            || name == "OUT_DIR"
            || scripts.iter().any(|it| it.env.iter().any(|(set, _)| set == name))
    };
    for unit in used {
        let env = unit.inputs().env.into_iter().filter(|(name, _)| !set_for_rustc(name));
        guards.extend(env.map(|(name, value)| Guard::Env { name, value }));
    }
    let members: FxHashSet<_> = metadata.workspace_members.iter().collect();
    for script in scripts.iter().filter(|it| members.contains(&it.package_id)) {
        let package = metadata.packages.iter().find(|it| it.id == script.package_id);
        guards.extend(package.map(|it| rerun_guards(it, script)).into_iter().flatten());
    }
    let left = checkouts.iter().flat_map(|it| &it.programs).flat_map(|it| it.left_to_rustc.clone());
    let mut left: Vec<PathBuf> = left.collect();
    left.sort();
    left.dedup();
    let built = used.iter().filter(|it| !it.is_test());
    let built = built.filter(|it| it.inputs().files.iter().any(|file| left.contains(file)));
    let cargo = built.flat_map(Record::cargo_dep_info).collect();
    let what = "a library its unit tests are linked with as rustc built it:";
    guards.push(Guard::beside_tests(what, left, cargo));
    guards
}

/// The files named one of `names` in `dir` and in every directory above it, there or not.
fn named_above<'a>(dir: &'a Path, names: &'a [&str]) -> impl Iterator<Item = PathBuf> + 'a {
    dir.ancestors().flat_map(move |dir| names.iter().map(move |name| dir.join(name)))
}

/// The units that run during the build, and the libraries of the workspace they are built
/// against, and the ones those are.
fn runs_in_build(used: &[Record]) -> Vec<&Record> {
    reached_from(used.iter().filter(|it| it.runs_in_build()).collect(), used)
}

/// `found` and the units among `used` they are built against, and the ones those are.
fn reached_from<'a>(mut found: Vec<&'a Record>, used: &'a [Record]) -> Vec<&'a Record> {
    let mut at = 0;
    while let Some(unit) = found.get(at).copied() {
        at += 1;
        // A library is named by its metadata or by its rlib.
        let externs: FxHashSet<PathBuf> = unit.externs().map(|it| it.with_extension("")).collect();
        for other in used {
            let named = other.outputs().iter().any(|it| externs.contains(&it.with_extension("")));
            if named && !found.iter().any(|it| it.id == other.id) {
                found.push(other);
            }
        }
    }
    found
}

/// What the build script of `package` said it reruns for. One that said nothing reruns for
/// any change to the package. A variable is the user's to change, and then cargo builds; for
/// a file the daemon runs the script again, when it knows how cargo ran it.
fn rerun_guards(package: &Package, script: &BuildScript) -> Vec<Guard> {
    let dir = package_dir(package);
    let what = format!("the build script of `{}` reruns for", package.name);
    let said = script::printed(script.out_dir.as_std_path());
    let said = said.and_then(|it| std::fs::read_to_string(it).ok()).unwrap_or_default();
    let directive = |line: &str, name: &str| {
        let rest = line.strip_prefix("cargo::").or_else(|| line.strip_prefix("cargo:"))?;
        rest.strip_prefix(name)?.strip_prefix('=').map(str::to_owned)
    };
    let (mut files, mut variables) = (Vec::new(), Vec::new());
    for line in said.lines() {
        if let Some(path) = directive(line, "rerun-if-changed") {
            let path = dir.join(path);
            files.push(match path.is_dir() {
                true => Guard::Tree { what: what.clone(), dir: path },
                false => Guard::files(&what, [path]),
            });
        }
        if let Some(name) = directive(line, "rerun-if-env-changed") {
            let value = std::env::var(&name).ok();
            variables.push(Guard::Env { name, value });
        }
    }
    if files.is_empty() && variables.is_empty() {
        let what = format!("the build script of `{}` reruns for any change,", package.name);
        files.push(Guard::Tree { what, dir });
    }
    let out_dir = script.out_dir.clone().into_std_path_buf();
    if script::can_rerun(&out_dir) {
        let package = package.name.to_string();
        files = vec![Guard::Script { package, out_dir, reruns_for: files, checked_at: 0 }];
    }
    files.extend(variables);
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_is_run_with_the_libraries_cargo_runs_it_with() {
        let sysroot = Path::new("/toolchain/lib/rustlib/host/lib");
        let dirs = |executable: &str| {
            let (name, value) = library_path(Path::new(executable), Some(sysroot)).unwrap();
            assert_eq!(name, LIBRARY_PATH);
            let dirs: Vec<PathBuf> = std::env::split_paths(&value).take(3).collect();
            dirs
        };
        let expected: Vec<PathBuf> =
            ["/t/debug/deps", "/t/debug", "/toolchain/lib/rustlib/host/lib"]
                .map(PathBuf::from)
                .into();
        assert_eq!(dirs("/t/debug/deps/unit-0123"), expected);
        assert_eq!(dirs("/t/debug/app"), expected);
        assert_eq!(dirs("/t/debug/examples/one"), expected);
    }

    #[test]
    fn the_nearest_configuration_says_a_variable() {
        let top = std::env::temp_dir().join(format!("ra-jit-configured-{}", std::process::id()));
        let inner = top.join("ws/app");
        std::fs::create_dir_all(inner.join(".cargo")).unwrap();
        std::fs::create_dir_all(top.join(".cargo")).unwrap();
        std::fs::write(
            top.join(".cargo/config.toml"),
            "[env]\nNEAR = \"far\"\nFAR = { value = \"data\", relative = true, force = true }\n",
        )
        .unwrap();
        std::fs::write(inner.join(".cargo/config.toml"), "[env]\nNEAR = \"near\"\n").unwrap();
        let configured = configured_env(&inner);
        let said = |name: &str| configured.iter().find(|it| it.name == name).cloned();
        let near = said("NEAR").unwrap();
        assert_eq!((near.value.as_str(), near.force), ("near", false));
        let far = said("FAR").unwrap();
        assert_eq!((PathBuf::from(far.value), far.force), (top.join("data"), true));
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn only_tests_changed_keeps_the_text_without_tests() {
        let dir = std::env::temp_dir().join(format!("ra-jit-beside-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("lib.rs");
        let print = |text: &str| {
            std::fs::write(&file, text).unwrap();
            without_tests(&file).unwrap()
        };
        let base = "pub fn one() -> u32 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n";
        let was = print(base);
        assert_eq!(print(&format!("{base}\n#[test]\nfn added() {{}}\n")), was);
        assert_eq!(print(&base.replace("fn t() {}", "fn t() { assert!(true); }")), was);
        assert_ne!(print(&base.replace("    1\n", "    2\n")), was);
        assert_ne!(print(&format!("\n{base}")), was);
        assert_ne!(print(&format!("{base}\n#[cfg(not(test))]\nfn other() {{}}\n")), was);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
