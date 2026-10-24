//! The daemon of a user: one process that every `cargo ra-jit` of the user finds through one
//! socket. It loads nothing itself. It owns the workers, each a `ra-jit <project> --socket`
//! process with one database, starts them when a build asks, passes the builds on to them,
//! and stops them and itself when nobody has asked for a while.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::OpenOptions,
    io::{BufRead as _, BufReader, Write as _},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
    },
    time::{Duration, Instant, SystemTime},
};

use anyhow::Context as _;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(windows)]
use uds_windows::UnixStream;

use super::{plan::Plan, units::stamp_of};
use crate::socket::{self, NotServed, Served, private_dir, private_file, read_request};

/// How long the daemon and each worker live without being asked, unless
/// `RA_JIT_IDLE_SECS` says otherwise.
const IDLE: Duration = Duration::from_secs(1800);

/// How often the daemon looks for what is idle.
const TICK: Duration = Duration::from_secs(1);

/// How many checkers the daemon keeps loaded, each of another build: a check for several
/// targets, or checks of several workspaces in turn.
const CHECKERS: usize = 4;

/// The share of the machine's memory the workers may hold when `RA_JIT_MEMORY_MAX` does not
/// say: one part in this many.
const MEMORY_SHARE: u64 = 2;

/// The most the workers may hold together, from `RA_JIT_MEMORY_MAX`: a size, or `off` for no
/// limit. Half of the machine's memory when it does not say, and no limit where the system
/// does not tell how much that is.
fn memory_max() -> Result<Option<u64>, String> {
    match std::env::var("RA_JIT_MEMORY_MAX") {
        Err(_) => Ok(crate::memory::of_the_machine().map(|it| it / MEMORY_SHARE)),
        Ok(text) if text == "off" => Ok(None),
        Ok(text) => ra_jit_store::parse_size(&text)
            .map(Some)
            .ok_or_else(|| format!("RA_JIT_MEMORY_MAX=`{text}` is neither a size nor `off`")),
    }
}

/// When the daemon has a spare worker, from `RA_JIT_SPARE`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Spares {
    /// `0`: none. A workspace is loaded once cargo has built it.
    Never,
    /// Without it: one as the daemon starts, for the build that started it, and one for each
    /// build after it that the daemon is told of as cargo begins it. Each loads its workspace
    /// beside cargo; none waits for a build nobody has asked for.
    Asked,
    /// `ready`: as well, one is started again once a workspace is loaded, and waits with the
    /// library loaded for the next. The daemon holds it, 1.3 GB on Linux, for as long as it runs.
    Ready,
}

fn spares() -> Result<Spares, String> {
    match std::env::var("RA_JIT_SPARE") {
        Err(_) => Ok(Spares::Asked),
        Ok(text) if text == "0" => Ok(Spares::Never),
        Ok(text) if text == "ready" => Ok(Spares::Ready),
        Ok(text) => Err(format!("RA_JIT_SPARE=`{text}` is neither `0` nor `ready`")),
    }
}

/// What a worker holds and how much it is in use.
struct Held {
    bytes: u64,
    asked: Instant,
    /// Answering a request or loading a checkout.
    busy: bool,
}

/// The workers to stop for the rest to hold no more than `most`: the ones asked longest ago
/// first. One that is busy is left, and so is the one asked last, which would only be
/// started again.
fn over(holding: &[(String, Held)], most: u64) -> Vec<String> {
    let mut all: u64 = holding.iter().map(|it| it.1.bytes).sum();
    let last = holding.iter().map(|it| it.1.asked).max();
    let mut idle: Vec<&(String, Held)> =
        holding.iter().filter(|it| !it.1.busy && Some(it.1.asked) != last).collect();
    idle.sort_by_key(|it| it.1.asked);
    let mut stopped = Vec::new();
    for (id, held) in idle {
        if all <= most {
            break;
        }
        all -= held.bytes;
        stopped.push(id.clone());
    }
    stopped
}

/// How often a worker that is loading is looked at.
const POLL: Duration = Duration::from_millis(20);

/// How long a worker that hung up without an answer is given to be seen to have exited: its
/// socket closes as it exits, a moment before it can be waited for.
const EXITING: Duration = Duration::from_secs(2);

/// How many of the last lines of a worker's log the daemon's log shows when it ends.
const LAST_LINES: usize = 12;

/// The variable that has a worker end when its standard input does, which is when the
/// daemon that started it is gone.
pub(crate) const ENDS_WITH_INPUT: &str = "RA_JIT_ENDS_WITH_INPUT";

/// Where a worker that listens at `socket` leaves the errors it found in the source as it
/// loaded the workspace, in JSON, before it ends.
pub(crate) fn errors_beside(socket: &Path) -> PathBuf {
    socket.with_file_name("errors")
}

/// A request with the version of the client that sends it.
#[derive(Serialize, Deserialize)]
pub(crate) struct Envelope {
    pub(crate) version: String,
    /// Whether the client starts a daemon of its own version when this one is of another.
    pub(crate) replaces: bool,
    pub(crate) request: Request,
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Request {
    /// Bring the programs of a checkout up to date with its source, if the daemon is warm
    /// for it.
    Sync(Checkout),
    /// Load a checkout that cargo has just built, for the builds to come.
    Warm(Checkout),
    /// Load a checkout cargo has just built what stands in for the crates of, and answer
    /// when its programs are built.
    Build(Checkout),
    /// A workspace cargo is about to build what stands in for the crates of: a worker
    /// loads it meanwhile, for the [`Request::Build`] that follows.
    Ahead(Ahead),
    /// A line cargo printed of a build script it ran or a proc macro it built, for the
    /// worker of the [`Request::Ahead`] of this [`WorkerConfig::id`].
    Said {
        id: String,
        line: String,
    },
    /// No build follows the [`Request::Ahead`] of this [`WorkerConfig::id`].
    Forget(String),
    /// `cargo ra-jit check` of a build the daemon has no programs of: what a checker that
    /// loaded the workspace as it would finds in its crates, with no rustc.
    Check(Ahead),
    Status,
    Stop,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum Reply {
    /// The daemon is of another version. It exits when the client said it replaces it.
    Restart {
        version: String,
    },
    /// The daemon has no part in this build, and why. Cargo builds.
    Cold {
        reason: String,
    },
    /// What the worker said of each program, in the order asked: its `ok` line, or why it
    /// did not build it.
    Synced {
        programs: Vec<Result<String, NotBuilt>>,
        /// The daemon's steps, for the client's trace. The worker's are in its `ok` lines.
        #[serde(default)]
        spans: Vec<crate::trace::Span>,
    },
    Warming,
    /// What the checker of a [`Request::Check`] found: no error, or the errors.
    Checked {
        errors: Option<crate::errors::Said>,
    },
    Status(Status),
    Stopping,
}

/// Why the worker did not build a program.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) enum NotBuilt {
    /// The source has errors: the build fails with them.
    Error(crate::errors::Said),
    /// The daemon failed.
    Failed(String),
    /// ra-jit does not build the source as it is, and cargo does.
    Cargos(String),
}

impl NotBuilt {
    pub(crate) fn why(self) -> String {
        match self {
            NotBuilt::Error(said) => said.to_string(),
            NotBuilt::Failed(why) | NotBuilt::Cargos(why) => why,
        }
    }

    /// What the worker's reply other than `ok` says.
    fn of(reply: &str) -> NotBuilt {
        if let Some(why) = reply.strip_prefix("cargo: ") {
            return NotBuilt::Cargos(why.to_owned());
        }
        match reply.strip_prefix("error: ") {
            Some(said) => NotBuilt::Error(crate::errors::Said::of_reply(said)),
            None => NotBuilt::Failed(reply.to_owned()),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct Status {
    pub(crate) pid: u32,
    pub(crate) version: String,
    pub(crate) idle_secs: Option<u64>,
    /// The most the workers may hold together, in bytes.
    #[serde(default)]
    pub(crate) memory_max: Option<u64>,
    pub(crate) workers: Vec<WorkerStatus>,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct WorkerStatus {
    pub(crate) pid: u32,
    pub(crate) project: PathBuf,
    pub(crate) log: PathBuf,
    /// The bytes it holds, where the system tells.
    #[serde(default)]
    pub(crate) bytes: Option<u64>,
    /// Each checkout the worker holds and how warm it is.
    pub(crate) checkouts: Vec<(PathBuf, String)>,
}

/// What a worker is loaded with. Two builds with the same one share a worker.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct WorkerConfig {
    /// What the checkouts of one repository have in common: the directory of its `.git`.
    pub(crate) repository: PathBuf,
    /// The directory of the package being built, from the root of the checkout.
    pub(crate) project: PathBuf,
    pub(crate) cfg_test: bool,
    /// Which build the worker stands in for, as the daemon's own plan of it has it.
    pub(crate) selection: crate::plan::Selection,
    /// The variables the worker runs with that decide what it compiles and with what.
    pub(crate) env: BTreeMap<String, String>,
}

impl WorkerConfig {
    /// 16 hex digits: it names the worker's directory, and the path of the socket in it has
    /// to fit the 104 bytes macOS has for one, with the daemon's directory before it.
    pub(crate) fn id(&self) -> String {
        let text = serde_json::to_vec(self).unwrap_or_default();
        Sha256::digest(text).iter().take(8).map(|it| format!("{it:02x}")).collect()
    }
}

impl WorkerConfig {
    /// What a worker's command line says of the workspace under `root` and of `crates` of
    /// it: all that loading it and compiling its functions reads.
    fn args(&self, root: &Path, crates: &[PathBuf]) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec![root.join(&self.project).into(), "--own-target".into()];
        args.extend(self.selection.flags().into_iter().map(Into::into));
        if self.cfg_test {
            args.push("--cfg-test".into());
        }
        if self.selection.dev_deps {
            args.push("--dev-deps".into());
        }
        for krate in crates {
            args.push("--crate".into());
            args.push(krate.strip_prefix(root).unwrap_or(krate).into());
        }
        args
    }
}

/// What is known of a build before cargo has built anything of it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct Ahead {
    pub(crate) config: WorkerConfig,
    /// The root of the checkout.
    pub(crate) root: PathBuf,
    /// Where the source of each crate the daemon compiles starts.
    pub(crate) crates: Vec<PathBuf>,
}

/// A checkout as cargo last built it, and the programs of it the daemon is asked for.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct Checkout {
    /// The build this is: the checkout, the command and its flags.
    pub(crate) key: String,
    /// The newest of rustc's output the programs are linked against. A worker that loaded
    /// the checkout with another one starts again.
    pub(crate) generation: u128,
    /// When cargo started, as a stamp: source newer than this is not what rustc compiled.
    pub(crate) built_at: u128,
    pub(crate) config: WorkerConfig,
    /// The root of the checkout.
    pub(crate) root: PathBuf,
    pub(crate) programs: Vec<Plan>,
    /// The source files rustc read. Only a request to warm names them.
    pub(crate) sources: Vec<PathBuf>,
    /// The variables `env!` reads in the checkout: the ones cargo set to say where its
    /// programs are, and in a request under them the ones of the process that asks.
    #[serde(default)]
    pub(crate) env: BTreeMap<String, String>,
    /// The file of what cargo said of the build scripts it ran and the proc macros it
    /// built, which a worker loads the checkout with.
    #[serde(default)]
    pub(crate) scripts: Option<PathBuf>,
}

impl Checkout {
    /// Whether one of the source files was written after `then`.
    fn written_since(&self, then: SystemTime) -> bool {
        let written = |file: &PathBuf| std::fs::metadata(file).and_then(|it| it.modified());
        self.sources.iter().any(|file| written(file).is_ok_and(|it| it > then))
    }

    fn project(&self) -> PathBuf {
        self.root.join(&self.config.project)
    }

    /// The line that asks a worker for `plan`.
    fn sync_line(&self, plan: &Plan) -> String {
        let named =
            |path: &PathBuf| path.strip_prefix(&self.root).unwrap_or(path).display().to_string();
        // Each with a comma after it: the program is of these crates and no others.
        let crates: String = plan.crates.iter().map(|it| named(it) + ",").collect();
        let mut line = format!(
            "sync\t{}\t{}\t{}\t{}\t{}\t{}",
            self.project().display(),
            crates,
            plan.object.display(),
            plan.patch.display(),
            plan.link_cmd,
            plan.binary.display()
        );
        for file in &plan.symbol_files {
            line.push('\t');
            line.push_str(&file.display().to_string());
        }
        line.push('\n');
        line
    }
}

/// How far a worker is with a checkout.
#[derive(Clone)]
enum Warmth {
    Loading(Instant),
    Ready,
    /// Why the worker has no part in the checkout as cargo last built it.
    Failed(String, SystemTime),
    /// The errors in the source the worker found loading it or building it first: each
    /// program's build fails with them until a source is written.
    Broken(crate::errors::Said, SystemTime),
}

impl Warmth {
    fn describe(&self) -> String {
        match self {
            Warmth::Loading(since) => format!("loading for {:.0?}", since.elapsed()),
            Warmth::Ready => "ready".to_owned(),
            Warmth::Failed(why, _) => format!("failed: {why}"),
            Warmth::Broken(..) => "the source has an error".to_owned(),
        }
    }
}

struct Loaded {
    generation: u128,
    root: PathBuf,
    warmth: Warmth,
}

/// A `ra-jit <project> --socket` process and the checkouts it holds.
struct Worker {
    project: PathBuf,
    socket: PathBuf,
    log: PathBuf,
    /// The variables the process runs with. A request says them with the ones of its
    /// checkout over them.
    env: ide_db::base_db::Env,
    child: Mutex<Child>,
    checkouts: Mutex<FxHashMap<String, Loaded>>,
    asked: Mutex<Instant>,
    /// The requests it is answering now.
    answering: AtomicUsize,
    /// Whether the daemon's log says it ended: it says so once.
    noted: AtomicBool,
}

/// Counts a request among those a worker is answering for as long as it lives, also when the
/// thread that asks unwinds.
struct Answering<'a>(&'a AtomicUsize);

impl<'a> Answering<'a> {
    fn new(count: &'a AtomicUsize) -> Answering<'a> {
        count.fetch_add(1, Relaxed);
        Answering(count)
    }
}

impl Drop for Answering<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Relaxed);
    }
}

impl Worker {
    /// Starts a worker on the first program of `checkout`, with its files in `dir`.
    /// `spare` is a process that waits to be told a build, and is told this one.
    fn start(checkout: &Checkout, dir: PathBuf, spare: Option<Spare>) -> anyhow::Result<Worker> {
        let first = checkout.programs.first().context("a checkout without programs")?;
        private_dir(&dir)?;
        let (socket, mut log) = (dir.join("sock"), dir.join("log"));
        // The worker of a daemon that was killed may still be there, on its way out: it
        // is not the one to connect to.
        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_file(errors_beside(&socket));
        let config = &checkout.config;
        let mut args = config.args(&checkout.root, &first.crates);
        let named: [(&str, &Path); 5] = [
            ("--out", &first.object),
            ("--patch", &first.patch),
            ("--link-cmd", Path::new(&first.link_cmd)),
            ("--splice", &first.binary),
            ("--socket", &socket),
        ];
        for (flag, path) in named {
            args.push(flag.into());
            args.push(path.into());
        }
        if let Some(rustcs) = &first.rustc_binary {
            args.push("--rustc-binary".into());
            args.push(rustcs.into());
            // A plan of rustc's executable copies the files of crates that call by symbol
            // alone.
            let by_symbol = dir.join("by-symbol");
            let listed: String =
                first.copies.iter().map(|it| format!("{}\n", it.display())).collect();
            std::fs::write(&by_symbol, listed)?;
            args.push("--by-symbol-in".into());
            args.push(by_symbol.into());
        }
        // In a file: a program of many crates has more of them than fit the command line
        // of a process on Windows.
        let symbols = dir.join("symbols");
        let listed: String =
            first.symbol_files.iter().map(|it| format!("{}\n", it.display())).collect();
        std::fs::write(&symbols, listed)?;
        args.push("--symbols-in".into());
        args.push(symbols.into());
        if let Some(scripts) = &checkout.scripts {
            args.push("--scripts-from".into());
            args.push(scripts.into());
        }
        let mut env = crate::process::own();
        for (name, value) in &config.env {
            env.set(name, value);
        }
        env.set(ENDS_WITH_INPUT, "1");
        let told = spare.and_then(|it| it.told(&args, &checkout.config, &checkout.root, false));
        let child = match told {
            Some(spare) => {
                log = spare.log;
                spare.child
            }
            None => {
                let mut command = Command::new(std::env::current_exe()?);
                command.args(&args);
                match store() {
                    Some(store) => command.env("RA_JIT_CACHE", store),
                    None => command.env_remove("RA_JIT_CACHE"),
                };
                let output =
                    private_file(OpenOptions::new().write(true).create(true).truncate(true), &log)?;
                command.envs(&config.env).env(ENDS_WITH_INPUT, "1");
                command.current_dir(&checkout.root);
                command.stdin(Stdio::piped()).stdout(output.try_clone()?).stderr(output);
                own_group(&mut command);
                command.spawn().context("starting a worker")?
            }
        };
        let loading = Loaded {
            generation: checkout.generation,
            root: checkout.root.clone(),
            warmth: Warmth::Loading(Instant::now()),
        };
        Ok(Worker {
            project: checkout.project(),
            socket,
            log,
            env,
            child: Mutex::new(child),
            checkouts: Mutex::new(FxHashMap::from_iter([(checkout.key.clone(), loading)])),
            asked: Mutex::new(Instant::now()),
            answering: AtomicUsize::new(0),
            noted: AtomicBool::new(false),
        })
    }

    /// Whether the process has ended, and how: its exit status and what it said last.
    fn ended(&self) -> Option<String> {
        let status = exited(&mut lock(&self.child));
        status.map(|status| format!("{status}: {}", last_words(&self.log)))
    }

    /// Whether the process has ended, given `patience` to.
    fn ended_within(&self, patience: Duration) -> Option<String> {
        let asked = Instant::now();
        loop {
            match self.ended() {
                None if asked.elapsed() < patience => std::thread::sleep(POLL),
                ended => return ended,
            }
        }
    }

    /// Says in the daemon's log, once, that the process ended by itself: how, and the end
    /// of its own log, which has what it panicked with.
    fn note_end(&self) {
        let (pid, status) = {
            let mut child = lock(&self.child);
            (child.id(), exited(&mut child))
        };
        let Some(status) = status else { return };
        if self.noted.swap(true, Relaxed) {
            return;
        }
        eprintln!(
            "the worker of pid {pid} for {} ended ({status}); its log {} ends with:\n{}",
            self.project.display(),
            self.log.display(),
            log_tail(&self.log)
        );
    }

    fn stop(&self) {
        // One that ended by itself says so before it goes.
        self.note_end();
        kill(&mut lock(&self.child));
        let _ = std::fs::remove_file(&self.socket);
    }

    fn set(&self, key: &str, warmth: Warmth) {
        if let Some(loaded) = lock(&self.checkouts).get_mut(key) {
            loaded.warmth = warmth;
        }
    }

    /// Waits for the process to listen, then has it build every program of `checkout` once.
    fn warm(&self, checkout: &Checkout) {
        let warmth = match self.warmed(checkout) {
            Ok(None) => Warmth::Ready,
            Ok(Some(said)) => Warmth::Broken(said, SystemTime::now()),
            Err(why) => match std::fs::read(errors_beside(&self.socket)) {
                Ok(said) if self.ended().is_some() => match serde_json::from_slice(&said) {
                    Ok(said) => Warmth::Broken(said, SystemTime::now()),
                    Err(_) => Warmth::Failed(format!("{why:#}"), SystemTime::now()),
                },
                _ => Warmth::Failed(format!("{why:#}"), SystemTime::now()),
            },
        };
        eprintln!("{}: {}", checkout.root.display(), warmth.describe());
        self.set(&checkout.key, warmth);
        // Nobody could ask it for a build until now.
        *lock(&self.asked) = Instant::now();
    }

    /// Whether a checkout is still being loaded.
    fn loads(&self) -> bool {
        lock(&self.checkouts).values().any(|it| matches!(it.warmth, Warmth::Loading(_)))
    }

    /// The errors in the source, when the worker found any.
    fn warmed(&self, checkout: &Checkout) -> anyhow::Result<Option<crate::errors::Said>> {
        self.connected()?;
        // A source written since cargo built is the daemon's like any edit: it compiles
        // every function and static of the workspace from what it reads. The program the
        // worker was started on is built already, and is asked for too: what the worker then
        // compiles again by itself, for the first edit to be a patch, is part of warming up.
        for plan in &checkout.programs {
            match self.ask(checkout, plan)? {
                Ok(_) => {}
                Err(NotBuilt::Error(said)) => return Ok(Some(said)),
                Err(why) => anyhow::bail!(why.why()),
            }
        }
        Ok(None)
    }

    /// A connection to the process, once it has loaded the workspace and listens.
    fn connected(&self) -> anyhow::Result<UnixStream> {
        loop {
            match socket::connect(&self.socket) {
                Ok(stream) => return Ok(stream),
                Err(_) => {
                    if let Some(how) = self.ended() {
                        self.note_end();
                        anyhow::bail!(
                            "the daemon could not load the workspace ({how}); the worker's log is {}",
                            self.log.display()
                        );
                    }
                    std::thread::sleep(POLL);
                }
            }
        }
    }

    /// Asks the process for `plan` of `checkout`. The outer error is the worker's failing,
    /// the inner one its answer that it does not build this.
    fn ask(&self, checkout: &Checkout, plan: &Plan) -> anyhow::Result<Result<String, NotBuilt>> {
        let _answering = Answering::new(&self.answering);
        let failed = || match self.ended_within(EXITING) {
            Some(how) => {
                self.note_end();
                format!("the daemon's worker ended ({how}); its log is {}", self.log.display())
            }
            None => {
                format!("the daemon's worker did not answer; its log is {}", self.log.display())
            }
        };
        let mut stream = self.connected()?;
        let mut env = self.env.clone();
        for (name, value) in &checkout.env {
            env.set(name, value);
        }
        stream.write_all(checkout.sync_line(plan).as_bytes()).with_context(failed)?;
        stream
            .write_all(format!("{}\n", crate::process::encode(&env)).as_bytes())
            .with_context(failed)?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).with_context(failed)?;
        let reply = reply.trim_end();
        match reply {
            _ if reply.is_empty() => Err(anyhow::anyhow!(failed())),
            _ if reply.starts_with("ok") => Ok(Ok(reply.to_owned())),
            _ => Ok(Err(NotBuilt::of(reply))),
        }
    }

    /// Brings the programs of `checkout` up to date, all at once: the worker builds the
    /// requests that wait for it together.
    fn sync(&self, checkout: &Checkout) -> Reply {
        let spans = Mutex::new(Vec::new());
        let asked: Vec<anyhow::Result<Result<String, NotBuilt>>> = std::thread::scope(|scope| {
            let asking: Vec<_> = checkout
                .programs
                .iter()
                .map(|plan| {
                    scope.spawn(|| {
                        let start = Instant::now();
                        let asked = self.ask(checkout, plan);
                        let level = Some(crate::trace::ASKED);
                        let span =
                            crate::trace::span("daemon↔worker", level, start, Instant::now());
                        lock(&spans).push(span);
                        asked
                    })
                })
                .collect();
            let panicked = || Err(anyhow::anyhow!("asking the worker panicked"));
            asking.into_iter().map(|it| it.join().unwrap_or_else(|_| panicked())).collect()
        });
        match asked.into_iter().collect::<anyhow::Result<Vec<_>>>() {
            Ok(programs) => {
                Reply::Synced { programs, spans: spans.into_inner().unwrap_or_default() }
            }
            // The worker is gone or stuck, which says nothing of the checkout: the next
            // build that has nothing to compile has it loaded again.
            Err(why) => {
                lock(&self.checkouts).remove(&checkout.key);
                Reply::Cold { reason: format!("{why:#}") }
            }
        }
    }

    /// Whether it is loading a checkout.
    fn loading(&self) -> bool {
        lock(&self.checkouts).values().any(|it| matches!(it.warmth, Warmth::Loading(_)))
    }

    /// What it holds now, and how much it is in use.
    fn held(&self) -> Held {
        Held {
            bytes: crate::memory::held_by(lock(&self.child).id()).unwrap_or(0),
            asked: *lock(&self.asked),
            busy: self.answering.load(Relaxed) != 0 || self.loads(),
        }
    }

    fn status(&self) -> WorkerStatus {
        let checkouts = lock(&self.checkouts);
        let pid = lock(&self.child).id();
        WorkerStatus {
            pid,
            project: self.project.clone(),
            log: self.log.clone(),
            bytes: crate::memory::held_by(pid),
            checkouts: checkouts
                .values()
                .map(|it| (it.root.clone(), it.warmth.describe()))
                .collect(),
        }
    }
}

/// A worker started before a build asked for one. It has the library of the toolchain
/// loaded and its names resolved, which the first build of a workspace then does not wait
/// for, and reads the build it is the worker of from its standard input.
struct Spare {
    child: Child,
    log: PathBuf,
}

impl Spare {
    /// Starts one with its files in `dir`.
    fn start(dir: &Path) -> anyhow::Result<Spare> {
        private_dir(dir)?;
        let log = dir.join("log");
        let mut command = Command::new(std::env::current_exe()?);
        command.arg("--spare").arg(dir.join("package"));
        match store() {
            Some(store) => command.env("RA_JIT_CACHE", store),
            None => command.env_remove("RA_JIT_CACHE"),
        };
        let output =
            private_file(OpenOptions::new().write(true).create(true).truncate(true), &log)?;
        command.stdin(Stdio::piped()).stdout(output.try_clone()?).stderr(output);
        own_group(&mut command);
        let child = command.spawn().context("starting a spare worker")?;
        Ok(Spare { child, log })
    }

    /// Tells it to be the worker `args` start for `checkout`. `None` when it can't be
    /// told: it has ended, or a path of the build is not text.
    /// One told a build `ahead` has what cargo has not built yet left out of `args`, and
    /// is told them whole afterwards.
    fn told(
        mut self,
        args: &[OsString],
        config: &WorkerConfig,
        cwd: &Path,
        ahead: bool,
    ) -> Option<Spare> {
        let told = || {
            let args: Option<Vec<String>> =
                args.iter().map(|it| it.to_str().map(str::to_owned)).collect();
            let told =
                crate::Told { args: args?, env: config.env.clone(), cwd: cwd.to_owned(), ahead };
            let mut line = serde_json::to_string(&told).ok()?;
            line.push('\n');
            Some(line)
        };
        let sent = match (self.child.try_wait(), told()) {
            (Ok(None), Some(line)) => {
                self.child.stdin.as_mut().is_some_and(|it| it.write_all(line.as_bytes()).is_ok())
            }
            _ => false,
        };
        if !sent {
            self.stop();
            return None;
        }
        Some(self)
    }

    /// Passes on what cargo said of a script or a proc macro. One that has ended is found
    /// out when it is told its build.
    fn said(&mut self, line: String) {
        let Ok(mut line) = serde_json::to_string(&crate::Line::Said { said: line }) else { return };
        line.push('\n');
        if let Some(input) = self.child.stdin.as_mut() {
            let _ = input.write_all(line.as_bytes());
        }
    }

    fn stop(&mut self) {
        kill(&mut self.child);
    }
}

/// The process that loads a workspace to check it, as the worker of a build with its
/// [`WorkerConfig`] would, and says what rust-analyzer finds in its crates when asked.
struct Checker {
    /// The [`WorkerConfig::id`] it was started with.
    id: String,
    child: Child,
    socket: PathBuf,
    log: PathBuf,
    project: PathBuf,
    asked: Instant,
}

impl Checker {
    fn start(ahead: &Ahead, id: &str, dir: &Path) -> anyhow::Result<Checker> {
        private_dir(dir)?;
        let (socket, log) = (dir.join("sock"), dir.join("log"));
        let _ = std::fs::remove_file(&socket);
        let mut args: Vec<OsString> = vec!["--check".into()];
        args.extend(ahead.config.args(&ahead.root, &ahead.crates));
        args.extend(["--out".into(), "none".into(), "--socket".into(), socket.clone().into()]);
        let mut command = Command::new(std::env::current_exe()?);
        command.args(&args);
        match store() {
            Some(store) => command.env("RA_JIT_CACHE", store),
            None => command.env_remove("RA_JIT_CACHE"),
        };
        let output =
            private_file(OpenOptions::new().write(true).create(true).truncate(true), &log)?;
        command.envs(&ahead.config.env).env(ENDS_WITH_INPUT, "1");
        command.current_dir(&ahead.root);
        command.stdin(Stdio::piped()).stdout(output.try_clone()?).stderr(output);
        own_group(&mut command);
        let child = command.spawn().context("starting a checker")?;
        Ok(Checker {
            id: id.to_owned(),
            child,
            socket,
            log,
            project: ahead.root.join(&ahead.config.project),
            asked: Instant::now(),
        })
    }

    fn ended(&mut self) -> Option<String> {
        exited(&mut self.child)
    }

    /// Its answer: `ok`, the errors, or [`crate::check::RELOAD`]. Waits for it to have loaded
    /// the workspace and to listen.
    fn ask(&mut self) -> anyhow::Result<String> {
        let mut stream = loop {
            match socket::connect(&self.socket) {
                Ok(stream) => break stream,
                Err(_) => {
                    if let Some(how) = self.ended() {
                        anyhow::bail!(
                            "the daemon could not load the workspace to check it ({how}: {}); \
                             its log is {}",
                            last_words(&self.log),
                            self.log.display()
                        );
                    }
                    std::thread::sleep(POLL);
                }
            }
        };
        stream.write_all(b"check\n")?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply)?;
        match reply.trim_end() {
            "" => anyhow::bail!("the checker hung up; its log is {}", self.log.display()),
            reply => Ok(reply.to_owned()),
        }
    }

    fn status(&mut self) -> WorkerStatus {
        let pid = self.child.id();
        WorkerStatus {
            pid,
            project: self.project.clone(),
            log: self.log.clone(),
            bytes: crate::memory::held_by(pid),
            checkouts: vec![(self.project.clone(), "checks".to_owned())],
        }
    }

    fn stop(&mut self) {
        kill(&mut self.child);
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Has a worker start in a process group of its own, for `kill` to end what it started with
/// it: the cargo it asks of a workspace, which would go on in the daemon's home.
fn own_group(command: &mut Command) {
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
    #[cfg(not(unix))]
    let _ = command;
}

/// Ends a worker, and on Unix the processes it started.
fn kill(child: &mut Child) {
    // One that has exited already has nothing to kill, and its pid may be another's by now.
    #[cfg(unix)]
    if let (Ok(None), Ok(group)) = (child.try_wait(), libc::pid_t::try_from(child.id())) {
        // SAFETY: it takes no pointer; the group is the worker's, which has not been waited for.
        unsafe { libc::killpg(group, libc::SIGKILL) };
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The daemon: its version, its workers, and when it was last asked.
struct Daemon {
    version: String,
    home: PathBuf,
    idle: Option<Duration>,
    /// The most the workers may hold together, in bytes.
    memory_max: Option<u64>,
    workers: Mutex<FxHashMap<String, Arc<Worker>>>,
    /// The worker the next workspace is loaded by, when there is one.
    spare: Mutex<Option<Spare>>,
    spare_when: Spares,
    /// The workers told a build ahead of cargo's, by [`WorkerConfig::id`].
    ahead: Mutex<FxHashMap<String, Spare>>,
    /// How many spare workers were started: each has a directory of its own.
    spares: AtomicUsize,
    /// The checkers of the last [`Request::Check`]s, each of another build and loaded for
    /// the next check of it: a check for several targets asks one for each.
    checkers: Mutex<Vec<Checker>>,
    asked: Mutex<Instant>,
}

/// Where the daemon of this user keeps its socket, its lock, its log and its workers.
pub(crate) fn home() -> anyhow::Result<PathBuf> {
    if let Some(home) = std::env::var_os("RA_JIT_HOME").filter(|it| !it.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|it| !it.is_empty()) {
        return Ok(Path::new(&runtime).join("ra-jit"));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    Ok(Path::new(&home.context("no RA_JIT_HOME, XDG_RUNTIME_DIR or HOME")?).join(".cache/ra-jit"))
}

/// The disk store of the user's builds: the directory `RA_JIT_CACHE` names, none when it
/// says `off`, and without it `ra-jit/store` in the user's cache directory. Unlike `home`
/// it is not in `XDG_RUNTIME_DIR`, which does not outlive the session.
pub(crate) fn store() -> Option<PathBuf> {
    if let Some(named) = std::env::var_os("RA_JIT_CACHE") {
        return (!ra_jit_store::is_off(&named)).then(|| PathBuf::from(named));
    }
    let caches = std::env::var_os("XDG_CACHE_HOME").filter(|it| !it.is_empty()).map(PathBuf::from);
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let caches = caches.or_else(|| Some(PathBuf::from(home?).join(".cache")))?;
    Some(caches.join("ra-jit/store"))
}

pub(crate) fn socket_in(home: &Path) -> PathBuf {
    home.join("daemon.sock")
}

/// What two processes have to agree on to talk: the version of the crate and the executable
/// itself, so that a daemon of an older build of the same version is replaced too. The
/// executable is told by where it is as well as by its size and time: every build in the nix
/// store has the same time, and two builds of a small change can have the same size, but no
/// two have the same path.
pub(crate) fn version() -> String {
    let executable = std::env::current_exe().and_then(std::fs::canonicalize);
    let place: String = executable.as_ref().map_or_else(
        |_| String::new(),
        |it| {
            let digest = Sha256::digest(it.as_os_str().as_encoded_bytes());
            digest.iter().take(4).map(|it| format!("{it:02x}")).collect()
        },
    );
    let built = executable.and_then(std::fs::metadata);
    let built = built.map(|it| (it.len(), it.modified().map_or(0, stamp_of)));
    let (len, written) = built.unwrap_or_default();
    format!("{}-{place}-{len:x}-{written:x}", env!("CARGO_PKG_VERSION"))
}

/// `ra-jit daemon`: serves the user's builds until it has been idle for long enough or is
/// told to stop. It starts:
///
/// 1. the thread that accepts connections, and a thread per connection that answers its
///    one request;
/// 2. the thread that stops idle workers and ends the daemon when it is idle itself;
/// 3. per worker, the process, and a thread that waits for it to load and has it build
///    its programs once.
pub(crate) fn serve() -> anyhow::Result<()> {
    let home = home()?;
    private_dir(&home).with_context(|| format!("the daemon's directory {}", home.display()))?;
    let memory_max = memory_max().map_err(anyhow::Error::msg)?;
    let spare_when = spares().map_err(anyhow::Error::msg)?;
    let socket = socket_in(&home);
    let served = match Served::bind(&socket) {
        Ok(served) => served,
        // Another daemon is the user's: the client that started this one finds it.
        Err(NotServed::Taken) => return Ok(()),
        Err(NotServed::Failed(error)) => {
            // On one line: a worker's log is read for its last line that starts `Error`.
            anyhow::bail!("listening on {}: {error}", socket.display());
        }
    };
    let idle = match std::env::var("RA_JIT_IDLE_SECS").ok().and_then(|it| it.parse::<u64>().ok()) {
        Some(0) => None,
        Some(secs) => Some(Duration::from_secs(secs)),
        None => Some(IDLE),
    };
    let daemon = Arc::new(Daemon {
        version: version(),
        home,
        idle,
        memory_max,
        workers: Mutex::default(),
        spare: Mutex::default(),
        spare_when,
        ahead: Mutex::default(),
        spares: AtomicUsize::new(0),
        checkers: Mutex::default(),
        asked: Mutex::new(Instant::now()),
    });
    // A build starts the daemon, and is given this one.
    daemon.spare();
    eprintln!(
        "daemon {} ({}) of pid {} listens on {}",
        daemon.version,
        crate::version(),
        std::process::id(),
        socket.display()
    );
    {
        let daemon = daemon.clone();
        std::thread::spawn(move || daemon.reap());
    }
    for (stream, open) in served.incoming() {
        let daemon = daemon.clone();
        std::thread::spawn(move || {
            let _open = open;
            if let Err(why) = daemon.answer(stream) {
                eprintln!("a request is not answered: {why:#}");
            }
        });
    }
    Ok(())
}

impl Daemon {
    /// Reads the one request of a connection and answers it.
    fn answer(&self, stream: UnixStream) -> anyhow::Result<()> {
        let began = Instant::now();
        let line = read_request(&stream)?;
        // An editor's text, on a line of its own that is not JSON: it is long and comes
        // with every key typed. The worker answers as to `cargo ra-jit build`.
        if line.starts_with("edit\t") {
            let reply = self.edit(&line).unwrap_or_else(|why| format!("error: {why:#}"));
            // An editor's client reads the errors as text.
            let reply = crate::errors::Said::text_of_reply(&reply);
            return Ok((&stream).write_all(format!("{}\n", reply.replace('\n', " ")).as_bytes())?);
        }
        let envelope = serde_json::from_str::<Envelope>(&line);
        // A request that can't be read is one of another version too, and of a client
        // that waits for an answer it can read: it gets none, and the daemon stays.
        let replaced = envelope.as_ref().is_ok_and(|it| it.replaces && it.version != self.version);
        let read = Instant::now();
        let mut reply = match envelope {
            Ok(envelope) if envelope.version == self.version => self.handle(envelope.request),
            _ => Reply::Restart { version: self.version.clone() },
        };
        if let Reply::Synced { spans, .. } = &mut reply {
            let level = Some(crate::trace::DAEMON);
            spans.push(crate::trace::span("daemon read request", level, began, read));
            spans.push(crate::trace::span("daemon", level, read, Instant::now()));
        }
        let mut text = serde_json::to_string(&reply)?;
        text.push('\n');
        let sent = (&stream).write_all(text.as_bytes());
        if replaced || matches!(reply, Reply::Stopping) {
            self.end();
        }
        Ok(sent?)
    }

    /// Hands `line`, an `edit` of a source file, to the worker that has the checkout of the
    /// file loaded, and gives back its answer.
    fn edit(&self, line: &str) -> anyhow::Result<String> {
        *lock(&self.asked) = Instant::now();
        let escaped = line["edit\t".len()..].split('\t').next().unwrap_or_default();
        let path = PathBuf::from(ra_jit_live::format::unescape(escaped));
        let path = dunce::canonicalize(&path).unwrap_or(path);
        let worker = lock(&self.workers).values().find_map(|worker| {
            let checkouts = lock(&worker.checkouts);
            let has = checkouts.values().any(|it| {
                matches!(it.warmth, Warmth::Ready)
                    && dunce::canonicalize(&it.root).is_ok_and(|root| path.starts_with(root))
            });
            has.then(|| worker.clone())
        });
        let worker = worker.with_context(|| {
            format!("the daemon has no workspace loaded that has {}", path.display())
        })?;
        *lock(&worker.asked) = Instant::now();
        let mut stream = socket::connect(&worker.socket)?;
        // The worker builds it for the process its last build was for.
        stream.write_all(format!("{line}\n\n").as_bytes())?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply)?;
        match crate::trace::without_spans(reply.trim_end()) {
            "" => anyhow::bail!("the worker hung up; its log is {}", worker.log.display()),
            reply => Ok(reply.to_owned()),
        }
    }

    fn handle(&self, request: Request) -> Reply {
        *lock(&self.asked) = Instant::now();
        match request {
            Request::Sync(checkout) => self.sync(&checkout),
            Request::Warm(checkout) => match self.warm(checkout) {
                Ok(()) => Reply::Warming,
                Err(why) => Reply::Cold { reason: format!("{why:#}") },
            },
            Request::Ahead(ahead) => {
                self.ahead(ahead);
                Reply::Warming
            }
            Request::Said { id, line } => {
                if let Some(spare) = lock(&self.ahead).get_mut(&id) {
                    spare.said(line);
                }
                Reply::Warming
            }
            Request::Forget(id) => {
                if let Some(mut spare) = lock(&self.ahead).remove(&id) {
                    spare.stop();
                }
                Reply::Warming
            }
            Request::Build(checkout) => match self.warm(checkout.clone()) {
                Ok(()) => self.built(&checkout),
                Err(why) => Reply::Cold { reason: format!("{why:#}") },
            },
            Request::Check(ahead) => self.check(&ahead),
            Request::Status => Reply::Status(Status {
                pid: std::process::id(),
                version: self.version.clone(),
                idle_secs: self.idle.map(|it| it.as_secs()),
                memory_max: self.memory_max,
                workers: lock(&self.workers)
                    .values()
                    .map(|it| it.status())
                    .chain(lock(&self.checkers).iter_mut().map(Checker::status))
                    .collect(),
            }),
            Request::Stop => Reply::Stopping,
        }
    }

    fn sync(&self, checkout: &Checkout) -> Reply {
        let worker = lock(&self.workers).get(&checkout.config.id()).cloned();
        let Some(worker) = worker else {
            return Reply::Cold { reason: "the daemon has not loaded this workspace".to_owned() };
        };
        *lock(&worker.asked) = Instant::now();
        let warmth = match lock(&worker.checkouts).get(&checkout.key) {
            Some(loaded) if loaded.generation == checkout.generation => loaded.warmth.clone(),
            _ => Warmth::Failed(
                "the daemon has not loaded this build of the workspace".to_owned(),
                SystemTime::now(),
            ),
        };
        match warmth {
            Warmth::Ready => worker.sync(checkout),
            Warmth::Loading(since) => Reply::Cold {
                reason: format!(
                    "the daemon is still loading the workspace ({:.0?} so far)",
                    since.elapsed()
                ),
            },
            Warmth::Failed(reason, _) => Reply::Cold { reason },
            Warmth::Broken(said, _) => {
                let programs = checkout.programs.iter().map(|_| Err(NotBuilt::Error(said.clone())));
                Reply::Synced { programs: programs.collect(), spans: Vec::new() }
            }
        }
    }

    /// What the worker that loads `checkout` has to say of its programs once it has.
    fn built(&self, checkout: &Checkout) -> Reply {
        loop {
            match self.sync(checkout) {
                Reply::Cold { .. } if self.loads(checkout) => std::thread::sleep(POLL),
                reply => return reply,
            }
        }
    }

    /// Whether a worker is loading this build of `checkout`.
    fn loads(&self, checkout: &Checkout) -> bool {
        let worker = lock(&self.workers).get(&checkout.config.id()).cloned();
        worker.is_some_and(|worker| {
            lock(&worker.checkouts).get(&checkout.key).is_some_and(|it| {
                it.generation == checkout.generation && matches!(it.warmth, Warmth::Loading(_))
            })
        })
    }

    /// Has a worker load `checkout`: the one of its configuration when that has not loaded
    /// another build of the same checkout, and a new one otherwise.
    fn warm(&self, checkout: Checkout) -> anyhow::Result<()> {
        let id = checkout.config.id();
        let mut workers = lock(&self.workers);
        // What a worker could not load once it can't load again, until cargo builds anew or
        // a source is written: the failure may be an error in the source.
        let failed = workers.get(&id).and_then(|it| match lock(&it.checkouts).get(&checkout.key) {
            Some(Loaded { generation, warmth, .. }) if *generation == checkout.generation => {
                match warmth {
                    Warmth::Failed(why, at) => Some((Some(why.clone()), *at)),
                    // Its builds say the errors.
                    Warmth::Broken(_, at) => Some((None, *at)),
                    _ => None,
                }
            }
            _ => None,
        });
        match failed {
            Some((Some(why), at)) if !checkout.written_since(at) => anyhow::bail!(why),
            Some((None, at)) if !checkout.written_since(at) => return Ok(()),
            Some(_) => {
                if let Some(old) = workers.remove(&id) {
                    old.stop();
                }
            }
            None => {}
        }
        let running = workers.get(&id).filter(|it| it.ended().is_none()).cloned();
        if let Some(worker) = &running {
            let mut checkouts = lock(&worker.checkouts);
            match checkouts.get(&checkout.key) {
                Some(loaded) if loaded.generation == checkout.generation => return Ok(()),
                // What it linked against is not what cargo has now.
                Some(_) => {}
                None => {
                    let loading = Loaded {
                        generation: checkout.generation,
                        root: checkout.root.clone(),
                        warmth: Warmth::Loading(Instant::now()),
                    };
                    checkouts.insert(checkout.key.clone(), loading);
                    let worker = worker.clone();
                    std::thread::spawn(move || worker.warm(&checkout));
                    return Ok(());
                }
            }
        }
        if let Some(old) = workers.remove(&id) {
            old.stop();
        }
        let ahead = lock(&self.ahead).remove(&id);
        let spare = ahead.or_else(|| lock(&self.spare).take());
        let worker =
            Arc::new(Worker::start(&checkout, self.home.join("workers").join(&id), spare)?);
        eprintln!(
            "worker {id} of pid {} for {}",
            lock(&worker.child).id(),
            worker.project.display()
        );
        workers.insert(id, worker.clone());
        std::thread::spawn(move || worker.warm(&checkout));
        Ok(())
    }

    /// Stops the workers asked longest ago while together they hold more than they may, and
    /// those nobody has asked for the idle time; and the daemon when nobody has asked it.
    fn reap(&self) {
        loop {
            std::thread::sleep(self.idle.map_or(TICK, |idle| idle.min(TICK)));
            // A worker that ended by itself is in the daemon's log when it ends, not when a
            // build next asks for it.
            for worker in lock(&self.workers).values() {
                worker.note_end();
            }
            // The next spare starts when the worker that was one has loaded its workspace.
            if self.spare_when == Spares::Ready {
                let loading = lock(&self.workers).values().any(|it| it.loading());
                if !loading && lock(&self.ahead).is_empty() {
                    self.spare();
                }
            }
            // A program that takes edits is being edited, however long since the last save:
            // a worker started again would link it again on the next one.
            let live = crate::live::any_running();
            if let Some(most) = self.memory_max.filter(|_| !live) {
                self.fit(most);
            }
            let Some(idle) = self.idle else { continue };
            if live {
                *lock(&self.asked) = Instant::now();
                for worker in lock(&self.workers).values() {
                    *lock(&worker.asked) = Instant::now();
                }
                continue;
            }
            // Loading is what the last build asked for: the idle time starts after it.
            for worker in lock(&self.workers).values().filter(|it| it.loading()) {
                *lock(&worker.asked) = Instant::now();
                *lock(&self.asked) = Instant::now();
            }
            if lock(&self.asked).elapsed() >= idle {
                eprintln!("idle for {idle:.0?}");
                self.end();
            }
            lock(&self.workers).retain(|_, worker| {
                // Loading can take longer than the idle time, and is what was asked for.
                let keep = worker.loads() || lock(&worker.asked).elapsed() < idle;
                if !keep {
                    worker.stop();
                }
                keep
            });
            // One that is checking holds the lock, and is not idle.
            if let Ok(mut checkers) = self.checkers.try_lock() {
                checkers.retain_mut(|it| {
                    let keep = it.asked.elapsed() < idle;
                    if !keep {
                        it.stop();
                    }
                    keep
                });
            }
        }
    }

    /// Stops the workers asked longest ago until the rest hold no more than `most` bytes. The
    /// next build of a workspace whose worker was stopped is cargo's while a worker loads it
    /// again.
    fn fit(&self, most: u64) {
        let mut workers = lock(&self.workers);
        let holding: Vec<(String, Held)> =
            workers.iter().map(|(id, worker)| (id.clone(), worker.held())).collect();
        let all: u64 = holding.iter().map(|it| it.1.bytes).sum();
        for id in over(&holding, most) {
            let Some(worker) = workers.remove(&id) else { continue };
            eprintln!(
                "worker {id} of {} stopped: the workers hold {} MB, over the {} MB of \
                 RA_JIT_MEMORY_MAX, and it was asked longest ago",
                worker.project.display(),
                all >> 20,
                most >> 20
            );
            worker.stop();
        }
    }

    /// Has the checker of `ahead` say what it finds in the source, started when there is
    /// none of its build or it ended, and started again when it can only take the source in
    /// by loading it again. The one asked longest ago makes way for it when there are
    /// [`CHECKERS`].
    fn check(&self, ahead: &Ahead) -> Reply {
        let id = ahead.config.id();
        let mut held = lock(&self.checkers);
        held.retain_mut(|it| {
            let ended = it.ended().is_some();
            if ended {
                it.stop();
            }
            !ended
        });
        for _ in 0..2 {
            let at = match held.iter().position(|it| it.id == id) {
                Some(at) => at,
                None => {
                    if held.len() >= CHECKERS
                        && let Some(oldest) = (0..held.len()).min_by_key(|&at| held[at].asked)
                    {
                        held.remove(oldest).stop();
                    }
                    let dir = self.home.join("checkers").join(&id);
                    match Checker::start(ahead, &id, &dir) {
                        Ok(checker) => held.push(checker),
                        Err(why) => return Reply::Cold { reason: format!("{why:#}") },
                    }
                    held.len() - 1
                }
            };
            let checker = &mut held[at];
            checker.asked = Instant::now();
            match checker.ask() {
                Ok(reply) if reply == "ok" => return Reply::Checked { errors: None },
                Ok(reply) if reply == crate::check::RELOAD => held.remove(at).stop(),
                Ok(reply) => match NotBuilt::of(&reply) {
                    NotBuilt::Error(said) => return Reply::Checked { errors: Some(said) },
                    why => return Reply::Cold { reason: why.why() },
                },
                Err(why) => {
                    held.remove(at).stop();
                    return Reply::Cold { reason: format!("{why:#}") };
                }
            }
        }
        Reply::Cold {
            reason: "the checker loaded the workspace twice and still had to again".to_owned(),
        }
    }

    /// Has the spare worker load the workspace of a build cargo has only started.
    fn ahead(&self, ahead: Ahead) {
        let id = ahead.config.id();
        if lock(&self.workers).contains_key(&id) {
            return;
        }
        let waiting = lock(&self.spare).take();
        let Some(spare) = waiting.or_else(|| self.new_spare()) else { return };
        let mut args = ahead.config.args(&ahead.root, &ahead.crates);
        args.extend(["--out".into(), "none".into()]);
        if let Some(spare) = spare.told(&args, &ahead.config, &ahead.root, true)
            && let Some(mut other) = lock(&self.ahead).insert(id, spare)
        {
            other.stop();
        }
    }

    /// Starts a spare worker when there is none.
    fn spare(&self) {
        let mut spare = lock(&self.spare);
        if let Some(old) = spare.as_mut() {
            let Some(status) = exited(&mut old.child) else { return };
            eprintln!(
                "the spare worker of pid {} ended ({status}); its log {} ends with:\n{}",
                old.child.id(),
                old.log.display(),
                log_tail(&old.log)
            );
        }
        *spare = self.new_spare();
    }

    /// A spare worker started now, unless `RA_JIT_SPARE=0` says there are none.
    fn new_spare(&self) -> Option<Spare> {
        if self.spare_when == Spares::Never {
            return None;
        }
        let dir = self.home.join("spares").join(self.spares.fetch_add(1, Relaxed).to_string());
        Spare::start(&dir).inspect_err(|why| eprintln!("no spare worker: {why:#}")).ok()
    }

    /// Stops the workers and the daemon. A client told it ends waits for its connection to
    /// close, which it does when this process exits.
    fn end(&self) -> ! {
        // Held until the process exits, so that no other thread starts a worker or a spare once
        // these are stopped: one started then would write in the home and outlive the daemon.
        // In the order a worker that loads a workspace takes them.
        let workers = lock(&self.workers);
        let mut ahead = lock(&self.ahead);
        let mut spare = lock(&self.spare);
        if let Some(spare) = spare.as_mut() {
            spare.stop();
        }
        for checker in lock(&self.checkers).iter_mut() {
            checker.stop();
        }
        for spare in ahead.values_mut() {
            spare.stop();
        }
        for worker in workers.values() {
            worker.stop();
        }
        let _ = std::fs::remove_file(socket_in(&self.home));
        eprintln!("daemon of pid {} ends at {:?}", std::process::id(), SystemTime::now());
        std::process::exit(0)
    }
}

/// The exit status of a process that has ended, or why it can't be told.
fn exited(child: &mut Child) -> Option<String> {
    match child.try_wait() {
        Ok(None) => None,
        Ok(Some(status)) => Some(status.to_string()),
        Err(error) => Some(format!("not known: {error}")),
    }
}

/// The line of a worker's log that says what went wrong: what it panicked with, else the
/// last error, else its last line.
fn last_words(log: &Path) -> String {
    let log = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = log.lines().collect();
    if let Some(at) = lines.iter().rposition(|it| it.contains(" panicked at ")) {
        // `thread '…' panicked at <place>:`, and the message on the line after it.
        let place = lines[at].split_once(" panicked at ").map_or(lines[at], |it| it.1);
        let message = lines.get(at + 1).copied().unwrap_or_default();
        return format!("panicked at {place} {message}");
    }
    let last = lines.iter().rev().find(|it| it.starts_with("Error"));
    last.or(lines.last()).copied().unwrap_or("no output").to_owned()
}

/// The end of a worker's log, for the daemon's: where it panicked and with what, then its
/// last lines without the frames of a backtrace, each indented.
fn log_tail(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let frame = |line: &str| {
        let line = line.trim_start();
        let numbered = line.split_once(':').is_some_and(|(number, _)| {
            !number.is_empty() && number.bytes().all(|it| it.is_ascii_digit())
        });
        numbered || line.starts_with("at ") || line.to_lowercase().starts_with("stack backtrace")
    };
    let mut kept: Vec<&str> = match lines.iter().rposition(|it| it.contains(" panicked at ")) {
        Some(at) => lines[at..].iter().take(2).copied().collect(),
        None => Vec::new(),
    };
    let mut last: Vec<&str> =
        lines.iter().rev().copied().filter(|it| !frame(it)).take(LAST_LINES).collect();
    last.reverse();
    for line in last {
        if !kept.contains(&line) {
            kept.push(line);
        }
    }
    if kept.is_empty() {
        return "  (nothing)".to_owned();
    }
    kept.iter().map(|it| format!("  {it}")).collect::<Vec<_>>().join("\n")
}

/// The lock, also after a thread panicked with it: what it guards is whole between any two
/// statements here.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_workers_asked_longest_ago_are_stopped_for_the_rest_to_fit() {
        let now = Instant::now();
        let held = |bytes: u64, ago: u64, busy: bool| Held {
            bytes,
            asked: now - Duration::from_secs(ago),
            busy,
        };
        let holding = vec![
            ("oldest".to_owned(), held(4, 30, false)),
            ("old".to_owned(), held(4, 20, false)),
            ("answering".to_owned(), held(4, 40, true)),
            ("last".to_owned(), held(4, 0, false)),
        ];
        assert_eq!(over(&holding, 16), Vec::<String>::new());
        assert_eq!(over(&holding, 12), ["oldest"]);
        assert_eq!(over(&holding, 8), ["oldest", "old"]);
        // The busy one and the one asked last stay, whatever they hold.
        assert_eq!(over(&holding, 0), ["oldest", "old"]);
    }
}
