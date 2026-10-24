//! `cargo ra-jit build|run|test|check`: cargo's commands, with the daemon building what
//! changed since cargo last built. `CARGO.md` at the root of the repository is the design.

pub(crate) mod daemon;
pub(crate) mod wrapper;

mod ancestor;
mod cli;
mod client;
mod messages;
pub(crate) mod own_script;
mod package;
pub(crate) mod plan;
pub(crate) mod run;
mod say;
pub(crate) mod script;
mod state;
pub(crate) mod switch;
mod units;

use std::{collections::BTreeMap, path::Path, process::Command, time::Instant};

use anyhow::Context as _;

use cli::{Asked, Invocation, Subcommand};
use client::Start;
use daemon::{Checkout, NotBuilt, Reply, Request};
use state::{Built, Counts, Held, Staged, State};
pub(crate) use units::Record;

/// `ra-jit cargo ...`: runs the command and ends with the status cargo would end with.
pub(crate) fn command(args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let args: Vec<String> = args.collect();
    if let [word, cargo, rest @ ..] = args.as_slice()
        && word == SETTLE
    {
        return settle(cargo.parse().context("`settle` is told the cargo to wait for")?, rest);
    }
    let asked = Asked::parse(args);
    if !matches!(asked, Asked::Build(_) | Asked::Cargo(_)) {
        crate::end_with_reader();
    }
    let code = match asked {
        Asked::Build(asked) => match asked.only_cargo() {
            None if asked.flags.doc => run::only_doc_tests(&asked, &asked.typed)?,
            Some(reason) => {
                say::fallback(&reason, &format!("this is `cargo {}`", asked.subcommand.name()));
                let typed = std::iter::once(asked.subcommand.name().to_owned()).chain(asked.typed);
                plain_cargo(typed.collect())?
            }
            None if asked.subcommand == Subcommand::Check => check(&asked)?,
            None => build(&asked)?,
        },
        Asked::Cargo(args) => plain_cargo(args)?,
        Asked::Cache(args) => cache(args)?,
        Asked::Status => status()?,
        Asked::Stop => stop()?,
        Asked::Enable => switch::enable()?,
        Asked::Disable => switch::disable()?,
        Asked::Help => {
            println!("{HELP}");
            0
        }
        Asked::Version => {
            println!("{}", crate::version());
            0
        }
    };
    std::process::exit(code)
}

const HELP: &str = "\
cargo ra-jit: builds with the ra-jit daemon, with cargo's flags

usage: cargo ra-jit <command> [cargo's flags]

  build, run, test   by the daemon when it has the workspace loaded, by cargo otherwise
  check              what the daemon's build of the same flags says, when it has one loaded;
                     otherwise what the daemon finds loading the workspace as it, with no rustc
  status             whether ra-jit is enabled, what the daemon has loaded, what fell back
  stop               ends the daemon; the next build starts one
  enable, disable    plain `cargo build|run|test|check` through ra-jit, and back
  cache              what the `#[ra_jit::cache]` functions kept, by function
  cache clear        forgets it; `--stale`, all but what each function's code computed

Any other command is cargo's own. CARGO.md is the guide.";

/// What a wrapper runs ra-jit with once rustc has linked a program under plain cargo.
pub(super) const SETTLE: &str = "settle";

/// Has the daemon load the build `typed` is the cargo command of, which the cargo `cargo`
/// is running: once that cargo has exited, the build is asked of cargo again, which has
/// nothing left to compile and says what it built. This process has that cargo's directory
/// and environment. Nobody reads what it says but the log.
fn settle(cargo: u32, typed: &[String]) -> anyhow::Result<()> {
    // Cargo lets go of the target directory once it has built, before it runs the tests or
    // the program, and a build links each program into place again by deleting it first:
    // one of the tests that starts it then finds none.
    while crate::live::alive(cargo) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let settled = (|| {
        let asked = cli::of_cargo(typed).context("`settle` is told a build command")?;
        let cwd = std::env::current_dir()?;
        let key = asked.key(&cwd);
        eprintln!("settling {key}: cargo {} in {}", typed.join(" "), cwd.display());
        if let Ok(state) = by_cargo(&asked, &cwd, &key, Counts::default(), None)? {
            state.write()?;
        }
        anyhow::Ok(())
    })();
    wrapper::settled(cargo);
    settled
}

/// `cargo ra-jit cache`: what the `#[ra_jit::cache]` functions kept, which is under the target
/// directory unless `RA_JIT_FN_CACHE_DIR` says where; `cache clear` forgets it, and `cache clear
/// --stale` all but what each function's code computed. Any other verb is one of `ra-jit cache`,
/// on the disk store.
fn cache(args: Vec<String>) -> anyhow::Result<i32> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let stale = match args[..] {
        [] => return list_kept(&fn_cache_dir()?).map(|()| 0),
        ["clear"] => false,
        ["clear", "--stale"] => true,
        _ => {
            crate::remote::command(args.into_iter().map(str::to_owned))?;
            return Ok(0);
        }
    };
    let dir = fn_cache_dir()?;
    if stale {
        let (results, bytes) = fn_cache::__private::clear_stale(&dir);
        println!("forgot {results} results of older code ({})", size(bytes));
        return Ok(0);
    }
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => println!("cleared the kept results in {}", dir.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("no results are kept"),
        Err(e) => return Err(e).with_context(|| format!("clearing {}", dir.display())),
    }
    Ok(0)
}

/// What each cached function holds: the results of its code, and of the older codes kept for an
/// edit taken back.
fn list_kept(dir: &std::path::Path) -> anyhow::Result<()> {
    let (held, other) = fn_cache::__private::holdings(dir);
    if held.is_empty() && other.0 == 0 {
        println!("no results are kept");
        return Ok(());
    }
    println!("kept in {}", dir.display());
    for function in &held {
        let Some(&(now, now_bytes)) = function.codes.first() else { continue };
        let older = &function.codes[1..];
        let (old, old_bytes) = older.iter().fold((0, 0), |(n, b), it| (n + it.0, b + it.1));
        print!("  {}: {now} results ({})", function.name, size(now_bytes));
        if old > 0 {
            print!(", and {old} of {} older code ({})", older.len(), size(old_bytes));
        }
        println!();
    }
    if other.0 > 0 {
        println!("  {} results of code no function has now ({})", other.0, size(other.1));
    }
    Ok(())
}

fn size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1048576 => format!("{:.1} KiB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MiB", bytes as f64 / 1048576.0),
    }
}

fn fn_cache_dir() -> anyhow::Result<std::path::PathBuf> {
    Ok(match std::env::var_os("RA_JIT_FN_CACHE_DIR") {
        Some(dir) => std::path::PathBuf::from(dir),
        None => {
            let metadata = cargo_metadata::MetadataCommand::new()
                .cargo_path(state::cargo_path())
                .no_deps()
                .exec()
                .context("running `cargo metadata`")?;
            metadata.target_directory.as_std_path().join("ra-jit").join("fn-cache")
        }
    })
}

/// Runs cargo with `args` and nothing of ra-jit's but its answers to what cargo asks rustc.
fn plain_cargo(args: Vec<String>) -> anyhow::Result<i32> {
    let mut cargo = Command::new(state::cargo_path());
    crate::answers::ready_cargo(&mut cargo);
    let status = cargo.args(args).status().context("running cargo")?;
    Ok(run::exit_code(status))
}

/// Builds what `asked` names and then runs what the subcommand runs. Cargo builds what
/// the daemon has not been given yet and what only cargo acts on (a manifest, a build
/// script), and so does it a program the daemon was asked for and did not build, unless the
/// source has an error: the build fails with it.
fn build(asked: &Invocation) -> anyhow::Result<i32> {
    use crate::trace::{CLIENT, note};
    note("client start", CLIENT, crate::trace::began());
    let started = Instant::now();
    let cwd = std::env::current_dir()?;
    let key = asked.key(&cwd);
    let finished = |state: &State| {
        let took = started.elapsed().as_secs_f64();
        let finished = format!("`{}` profile [ra-jit] target(s) in {took:.2}s", state.profile);
        say::status(asked.flags.quiet, "Finished", &finished);
    };
    let read = State::read(&key);
    note("client read state", CLIENT, started);
    let mut served = false;
    let mut held = None;
    let state = match read.map(|state| by_daemon(state)) {
        Ok(Ok((state, settled))) => {
            finished(&state);
            served = true;
            held = Some(settled);
            state
        }
        Ok(Err(NotServed::Error(_, written))) => {
            eprintln!("{written}");
            return Ok(FAILED);
        }
        Ok(Err(NotServed::Cargo(ToCargo(state, reason)))) => {
            say::fallback(&reason, "building with cargo");
            let mut counts = state.counts;
            *counts.cargo.entry(reason).or_default() += 1;
            match by_cargo(asked, &cwd, &key, counts, None)? {
                Ok(state) => state,
                Err(code) => return Ok(code),
            }
        }
        Err(unread) => {
            if let Some(why) = unread {
                say::fallback(why, "building with cargo");
            }
            match staged(asked, &cwd, &key)? {
                Ok((state, settled)) => {
                    finished(&state);
                    held = Some(settled);
                    state
                }
                Err(NotStaged::Failed(code)) => return Ok(code),
                Err(NotStaged::Cargos(why, ahead)) => {
                    if let Some(why) = why {
                        say::fallback(&why, "building with cargo");
                    }
                    match by_cargo(asked, &cwd, &key, Counts::default(), ahead)? {
                        Ok(state) => state,
                        Err(code) => return Ok(code),
                    }
                }
            }
        }
    };
    let writing = Instant::now();
    state.write()?;
    if served {
        note("client write state", CLIENT, writing);
        crate::trace::finish();
    }
    after_build(asked, &state, &cwd, held)
}

/// The status of a build that failed, as cargo's of one rustc turned down.
const FAILED: i32 = 101;

/// `cargo ra-jit check`: what the daemon says of the source as it builds the programs of
/// the build with the same flags, when it has that build loaded. Any other check is the
/// daemon's checker's, which loads the workspace as that build would and builds nothing,
/// once for each target asked for. Neither runs rustc on the source. The verdict is written
/// as `--message-format` asks: in cargo's JSON, it is what an editor shows.
fn check(asked: &Invocation) -> anyhow::Result<i32> {
    let format = match messages::Format::of(&asked.flags.message_format) {
        Ok(format) => format,
        Err(why) => {
            eprintln!("error: {why}");
            return Ok(1);
        }
    };
    let started = Instant::now();
    let cwd = std::env::current_dir()?;
    let finished = |profile: &str| {
        let took = started.elapsed().as_secs_f64();
        let finished = format!("`{profile}` profile [ra-jit] target(s) in {took:.2}s");
        say::status(asked.flags.quiet, "Finished", &finished);
        messages::passed(format);
    };
    let turned_down = |said: &[crate::errors::Said]| -> anyhow::Result<i32> {
        messages::turned_down(format, said, &state::metadata(asked)?);
        Ok(FAILED)
    };
    if let Ok(state) = State::read(&asked.key(&cwd))
        && !state.programs.is_empty()
    {
        match by_daemon(state) {
            Ok((state, _held)) => {
                finished(&state.profile);
                state.write()?;
                return Ok(0);
            }
            Err(NotServed::Error(said, _)) => return turned_down(&[said]),
            Err(NotServed::Cargo(_)) => {}
        }
    }
    let ahead = state::checked(asked)?;
    let profile = ahead.config.selection.profile.clone();
    let targets = match &asked.flags.targets[..] {
        [_, _, ..] => asked.flags.targets.iter().cloned().map(Some).collect(),
        _ => vec![ahead.config.selection.target.clone()],
    };
    let mut said = Vec::new();
    for target in targets {
        let mut ahead = ahead.clone();
        ahead.config.selection.target = target;
        match client::ask(Request::Check(ahead), Start::IfMissing)? {
            Some(daemon::Reply::Checked { errors: None }) => {}
            Some(daemon::Reply::Checked { errors: Some(errors) }) => said.push(errors),
            Some(daemon::Reply::Cold { reason }) => {
                eprintln!("error: ra-jit could not check the workspace: {reason}");
                return Ok(FAILED);
            }
            reply => anyhow::bail!("the daemon answered a check with {reply:?}"),
        }
    }
    if said.is_empty() {
        finished(&profile);
        return Ok(0);
    }
    turned_down(&said)
}

/// Why the first build of a workspace is not rustc's of what the workspace depends on and
/// the daemon's of the workspace.
enum NotStaged {
    /// Cargo failed, with this status: rustc turned the source down.
    Failed(i32),
    /// Cargo builds all of it, and why when that is worth a line: it has not begun to, or it
    /// built its part and the daemon did not build a program. With what the daemon is told
    /// of the build so that it loads it while cargo builds.
    Cargos(Option<String>, Option<daemon::Ahead>),
}

/// The first build of a workspace: cargo has rustc build what the workspace depends on
/// and check the crates of the workspace, and the daemon compiles those. Every program is
/// the daemon's: cargo linked only what stands in for it.
fn staged(
    asked: &Invocation,
    cwd: &Path,
    key: &str,
) -> anyhow::Result<Result<(State, Held), NotStaged>> {
    let on = std::env::var_os(STAGED).is_some_and(|it| it != "0");
    let crates = match Staged::of(asked) {
        Ok(Ok(crates)) => crates,
        Ok(Err(why)) => return Ok(Err(NotStaged::Cargos(on.then_some(why), None))),
        // Cargo's build of all of it loses only the daemon's head start.
        Err(_) if !on => return Ok(Err(NotStaged::Cargos(None, None))),
        Err(why) => return Err(why),
    };
    if !on {
        return Ok(Err(NotStaged::Cargos(None, crates.ahead)));
    }
    State::give_back(key);
    let ahead = crates.ahead.as_ref().map(|ahead| {
        // Without an answer the daemon loads the workspace once cargo is done.
        let _ = client::ask(Request::Ahead(ahead.clone()), Start::IfMissing);
        ahead.config.id()
    });
    let state = match Built::staged(asked, &crates)? {
        Ok(built) => daemons(asked, cwd, key, built)
            .unwrap_or_else(|why| Err(NotStaged::Cargos(Some(format!("{why:#}")), None))),
        Err(code) => Err(NotStaged::Failed(code)),
    };
    if let (Err(_), Some(id)) = (&state, ahead) {
        let _ = client::ask(Request::Forget(id), Start::Never);
    }
    Ok(state)
}

/// The programs of a build whose workspace crates cargo built what stands in for, as the
/// daemon builds them. `Err` is what is not the daemon's: the source has an error, which is
/// said, or why cargo builds it.
fn daemons(
    asked: &Invocation,
    cwd: &Path,
    key: &str,
    built: Built,
) -> anyhow::Result<Result<(State, Held), NotStaged>> {
    let state = State::after(asked, cwd, key, built, Counts::default())?;
    let cargos = |why| Ok(Err(NotStaged::Cargos(Some(why), None)));
    if let Some(why) = &state.refusal {
        return cargos(why.clone());
    }
    if let Some((program, why)) = state.left_to_cargo().next() {
        return cargos(format!("`{}`: {why}", program.name));
    }
    let answers = match state.checkouts.iter().map(self::built).collect::<Result<Vec<_>, _>>() {
        Ok(answers) => answers,
        Err(NotBuilt::Error(said)) => {
            eprintln!("{}", state.turned_down(&said));
            return Ok(Err(NotStaged::Failed(FAILED)));
        }
        Err(why) => return cargos(why.why()),
    };
    Ok(first_by_daemon(state, &answers).map_err(|why| NotStaged::Cargos(Some(why), None)))
}

fn first_by_daemon(
    mut state: State,
    answers: &[Vec<Result<(), String>>],
) -> Result<(State, Held), String> {
    let held = state.hold();
    let cargos = state.settle(answers, &held)?;
    if let Some((_, why)) = cargos.first() {
        return Err(why.clone());
    }
    // What cargo linked stands in for the program, and is not what it is while its source
    // is unchanged: the daemon's is.
    state.keep_daemons().map_err(|why| format!("{why:#}"))?;
    state.counts.served += 1;
    Ok((state, held))
}

/// The variable that has the first build of a workspace be the daemon's too.
const STAGED: &str = "RA_JIT_FIRST_BUILD";

/// The build is cargo's, with the state so far and the reason: something cargo acts on
/// changed, the build is one the daemon is never asked for, or the daemon did not build a
/// program whose source changed.
struct ToCargo(Box<State>, String);

/// Why the daemon's programs are not the build.
enum NotServed {
    Cargo(ToCargo),
    /// The source has errors: them, and them as rustc and cargo write them.
    Error(crate::errors::Said, String),
}

/// Has the daemon bring the programs of `state` up to date and puts them where cargo has
/// them. Rustc sees none of it: the daemon builds what rust-analyzer passes, and its verdict
/// on the source is the build's.
fn by_daemon(mut state: State) -> Result<(State, Held), NotServed> {
    use crate::trace::{CLIENT, note};
    let to_cargo = |state, why| NotServed::Cargo(ToCargo(Box::new(state), why));
    let checking = Instant::now();
    let cargos = state.refusal.clone().or_else(|| state.broken_guard());
    if let Some(reason) = cargos.or_else(|| state.stale_cargos()) {
        return Err(to_cargo(state, reason));
    }
    note("client checks", CLIENT, checking);
    // What the daemon did not build for an error in the source is not built: an item
    // rust-analyzer does not resolve is ra-jit's to fix, not rustc's to build. What it does
    // not build otherwise, cargo does.
    let answers = match state.checkouts.iter().map(built).collect::<Result<Vec<_>, _>>() {
        Ok(answers) => answers,
        Err(NotBuilt::Error(said)) => {
            let written = state.turned_down(&said);
            return Err(NotServed::Error(said, written));
        }
        Err(why) => return Err(to_cargo(state, why.why())),
    };
    let settling = Instant::now();
    let held = state.hold();
    let cargos = match state.settle(&answers, &held) {
        Ok(cargos) => cargos.len(),
        Err(why) => return Err(to_cargo(state, why)),
    };
    note("client settle", CLIENT, settling);
    let served = cargos < state.programs.len() || state.programs.is_empty();
    state.counts.served += u64::from(served);
    Ok((state, held))
}

/// Has the daemon build the programs of `checkout`, loading it first when it has not.
/// `Err` is why a program is not built.
pub(super) fn built(checkout: &Checkout) -> Result<Vec<Result<(), String>>, NotBuilt> {
    if checkout.programs.is_empty() {
        return Ok(Vec::new());
    }
    let asking_at = Instant::now();
    let reply = client::ask(Request::Build(asking(checkout)), Start::IfMissing)
        .map_err(|it| NotBuilt::Failed(format!("{it:#}")))?;
    crate::trace::note("client↔daemon", crate::trace::CLIENT, asking_at);
    let programs = match reply {
        Some(Reply::Synced { programs, spans }) => {
            crate::trace::note_all(spans);
            for line in programs.iter().flatten() {
                crate::trace::note_all(crate::trace::split(line).1);
            }
            programs
        }
        None => return Err(NotBuilt::Failed(client::why_none())),
        Some(Reply::Cold { reason }) => return Err(NotBuilt::Failed(reason)),
        Some(other) => return Err(NotBuilt::Failed(format!("the daemon answered {other:?}"))),
    };
    std::iter::zip(&checkout.programs, programs)
        .map(|(program, built)| match built {
            Ok(_) => Ok(Ok(())),
            Err(NotBuilt::Failed(why)) => Err(NotBuilt::Failed(format!(
                "ra-jit did not build {}: {why}",
                program.binary.display()
            ))),
            Err(cargos) => Err(cargos),
        })
        .collect()
}

/// `checkout` as a request names it: with the variables of this process, which `env!`
/// reads where cargo set none. The ones that make ra-jit the wrapper are cargo's alone.
fn asking(checkout: &Checkout) -> Checkout {
    // Nor are those that say what ra-jit prints of the build: one that changed would be an
    // edit of every `env!`.
    let wraps = |name: &str| {
        name == wrapper::DRIVEN
            || name == switch::VARIABLE
            || name == "RA_JIT_STATS"
            || name == "RA_JIT_TRACE"
    };
    let mut env: BTreeMap<String, String> =
        std::env::vars().filter(|(name, _)| !wraps(name)).collect();
    env.extend(checkout.env.clone());
    Checkout { env, ..checkout.clone() }
}

/// Tells the daemon to load `checkout` as cargo just built it, starting one when there is
/// none. `Err` is why it does not.
fn warmed(checkout: &Checkout) -> Result<(), String> {
    match client::ask(Request::Warm(asking(checkout)), Start::IfMissing) {
        Ok(Some(Reply::Cold { reason })) => Err(reason),
        Ok(_) => Ok(()),
        Err(why) => Err(format!("{why:#}")),
    }
}

/// Builds with cargo, and has the daemon load what cargo built for the builds to come.
/// `Err` is cargo's exit status when it failed. With `ahead` the daemon loads the workspace
/// and compiles what it can while cargo builds, rather than after.
fn by_cargo(
    asked: &Invocation,
    cwd: &Path,
    key: &str,
    counts: Counts,
    ahead: Option<daemon::Ahead>,
) -> anyhow::Result<Result<State, i32>> {
    State::give_back(key);
    if let Some(ahead) = &ahead {
        // Without an answer the daemon loads the workspace once cargo is done.
        let _ = client::ask(Request::Ahead(ahead.clone()), Start::IfMissing);
    }
    let built = loaded_by_cargo(asked, cwd, key, counts, ahead.as_ref());
    // The worker that loaded ahead is the one the build was given to, unless there is none.
    if let Some(ahead) = ahead {
        let _ = client::ask(Request::Forget(ahead.config.id()), Start::Never);
    }
    built
}

fn loaded_by_cargo(
    asked: &Invocation,
    cwd: &Path,
    key: &str,
    counts: Counts,
    ahead: Option<&daemon::Ahead>,
) -> anyhow::Result<Result<State, i32>> {
    let built = match Built::by_cargo(asked, ahead)? {
        Ok(built) => built,
        Err(code) => return Ok(Err(code)),
    };
    let mut state = State::after(asked, cwd, key, built, counts)?;
    if state.refusal.is_none() {
        let asked = state.checkouts.iter().filter(|it| !it.programs.is_empty());
        let cold: Vec<String> = asked.filter_map(|it| warmed(it).err()).collect();
        for why in cold {
            say::fallback(&why, "the daemon has no part in the next build");
            state.count_cargo(&why);
        }
        for (program, why) in state.left_to_cargo() {
            say::fallback(why, &format!("`{}` is cargo's to build", program.name));
        }
    }
    Ok(Ok(state))
}

/// What the subcommand does once the programs are built. `held` are the locks the build
/// put its programs in place under, when it did.
fn after_build(
    asked: &Invocation,
    state: &State,
    cwd: &Path,
    held: Option<Held>,
) -> anyhow::Result<i32> {
    match asked.subcommand {
        Subcommand::Build | Subcommand::Check => Ok(0),
        Subcommand::Run => match run::program(asked, state, cwd, held)? {
            Some(code) => Ok(code),
            // Cargo says which programs there are to pick from.
            None => {
                plain_cargo(std::iter::once("run".to_owned()).chain(asked.typed.clone()).collect())
            }
        },
        Subcommand::Test if asked.flags.no_run => Ok(0),
        Subcommand::Test => {
            let code = run::tests(asked, state, cwd, held)?;
            let stopped = code != 0 && !asked.flags.no_fail_fast;
            // Cargo 1.99 runs no doc tests when tests are picked by target or by name.
            let picked = asked.flags.selects_targets || !asked.positional.is_empty();
            if stopped || picked || !state.doc_tests {
                return Ok(code);
            }
            Ok(code.max(run::doc_tests(asked)?))
        }
    }
}

/// What the store of compiled code holds and may hold, and where it is.
fn store_status() -> String {
    let Some(dir) = daemon::store() else {
        return "store: none (RA_JIT_CACHE says so)".to_owned();
    };
    let held = ra_jit_store::StoreDir::at(&dir).stats().map_or(0, |it| it.held());
    let most = match ra_jit_store::limit_for(&dir) {
        Ok(Some(most)) => format!("at most {} MB", most >> 20),
        Ok(None) => "no limit".to_owned(),
        Err(why) => why,
    };
    format!("store: {} MB, {most} (`ra-jit cache gc` collects it), {}", held >> 20, dir.display())
}

/// `cargo ra-jit status`: the daemon, its workers, the store, and how the builds of each
/// checkout went.
fn status() -> anyhow::Result<i32> {
    use std::fmt::Write as _;
    let mut said = String::new();
    for line in switch::status(&std::env::current_dir()?).into_iter().chain(wrapper::tallied()) {
        let _ = writeln!(said, "{line}");
    }
    match client::ask(Request::Status, Start::Never)? {
        Some(Reply::Status(daemon)) => {
            let idle =
                daemon.idle_secs.map_or("never".to_owned(), |it| format!("after {it}s idle"));
            let most = daemon.memory_max.map_or("no limit".to_owned(), |it| {
                format!("workers hold at most {} MB", it >> 20)
            });
            let _ = writeln!(
                said,
                "daemon: pid {}, version {}, exits {idle}, {most}",
                daemon.pid, daemon.version
            );
            for worker in daemon.workers {
                let holds =
                    worker.bytes.map_or(String::new(), |it| format!("holds {} MB, ", it >> 20));
                let _ = writeln!(
                    said,
                    "  worker: pid {}, {}, {holds}log {}",
                    worker.pid,
                    worker.project.display(),
                    worker.log.display()
                );
                for (root, warmth) in worker.checkouts {
                    let _ = writeln!(said, "    {}: {warmth}", root.display());
                }
            }
        }
        Some(Reply::Restart { version }) => {
            let _ = writeln!(
                said,
                "daemon: another build of ra-jit, version {version}; the next build replaces it"
            );
        }
        _ => said.push_str("daemon: not running\n"),
    }
    let _ = writeln!(said, "{}", store_status());
    for state in State::all() {
        let to_cargo: u64 = state.counts.cargo.values().sum();
        let _ = writeln!(
            said,
            "{} ({}, {}): {} builds by the daemon, {to_cargo} by cargo",
            state.root.display(),
            state.profile,
            state.key,
            state.counts.served
        );
        for (reason, count) in &state.counts.cargo {
            let _ = writeln!(said, "  {count} x {reason}");
        }
        if let Some(refusal) = &state.refusal {
            let _ = writeln!(said, "  every build is cargo's: {refusal}");
        }
    }
    // Whoever reads this may stop reading before its end.
    let _ = std::io::Write::write_all(&mut std::io::stdout(), said.as_bytes());
    Ok(0)
}

/// `cargo ra-jit stop`: has the daemon stop its workers and exit.
fn stop() -> anyhow::Result<i32> {
    match client::ask(Request::Stop, Start::Never)? {
        Some(_) => {
            client::gone(&daemon::socket_in(&daemon::home()?));
            println!("daemon: stopped");
        }
        None => println!("daemon: not running"),
    }
    Ok(0)
}
