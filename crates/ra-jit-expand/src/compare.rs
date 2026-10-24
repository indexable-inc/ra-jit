//! The toolchain's proc-macro server held against ra-jit's (docs/no-rustc.md, step J1): with
//! `RA_JIT_PROC_MACRO_COMPARE` naming the server built from `vendor/proc-macro-srv` and
//! `RA_JIT_PROC_MACRO_LIBS` a directory of the libraries ra-jit linked (`lib<crate>` and the
//! platform's suffix, with the list of their macros in them), each call the toolchain's server
//! expands with rustc's library of a crate is expanded again by that server with ra-jit's
//! library of the crate, given the same input, and the outputs, spans and all, are compared.

use std::{
    cell::Cell,
    path::PathBuf,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use base_db::{Env, SourceDatabase};
use hir_expand::proc_macro::{ProcMacro, ProcMacroKind};
use paths::{AbsPath, AbsPathBuf};
use proc_macro_api::ProcMacroClient;
use rustc_hash::FxHashMap;
use span::Span;

thread_local! {
    /// Set while ra-jit's server expands a call again, whose expander is wrapped as every
    /// other is: that expansion is neither kept nor compared.
    static COMPARING: Cell<bool> = const { Cell::new(false) };
}

/// Whether this thread is expanding a call again for a comparison.
pub(crate) fn comparing() -> bool {
    COMPARING.with(Cell::get)
}

struct Compare {
    server: ProcMacroClient,
    libs: PathBuf,
    /// The macros of ra-jit's library of each of rustc's, by the path of rustc's, or why
    /// there are none.
    loaded: Mutex<FxHashMap<AbsPathBuf, Result<Vec<ProcMacro>, String>>>,
}

fn compare() -> Option<&'static Compare> {
    static COMPARE: OnceLock<Option<Compare>> = OnceLock::new();
    COMPARE
        .get_or_init(|| {
            let server = std::env::var_os("RA_JIT_PROC_MACRO_COMPARE")?;
            let libs = std::env::var_os("RA_JIT_PROC_MACRO_LIBS")?;
            let server = AbsPathBuf::assert_utf8(std::path::absolute(server).ok()?);
            let none: [(&str, &Option<&str>); 0] = [];
            let server = match ProcMacroClient::spawn(&server, none, None, 1) {
                Ok(it) => it,
                Err(e) => {
                    eprintln!("proc macros: ra-jit's server {server} did not start: {e}");
                    return None;
                }
            };
            Some(Compare { server, libs: libs.into(), loaded: Mutex::default() })
        })
        .as_ref()
}

/// The crate rustc's library at `path` is of: cargo names it `lib<crate>-<hash>` and the
/// platform's suffix.
fn crate_of(path: &AbsPath) -> Option<&str> {
    let stem = path.file_stem()?;
    let stem = stem.strip_prefix("lib").unwrap_or(stem);
    Some(stem.rsplit_once('-').map_or(stem, |(name, _)| name))
}

impl Compare {
    /// ra-jit's library of the crate of rustc's at `dylib`, loaded into ra-jit's server.
    fn macro_of(&self, dylib: &AbsPath, name: &str) -> Result<ProcMacro, String> {
        let mut loaded = self.loaded.lock().unwrap_or_else(|it| it.into_inner());
        let macros = loaded.entry(dylib.to_path_buf()).or_insert_with(|| -> Result<_, String> {
            let krate = crate_of(dylib).ok_or_else(|| format!("{dylib} names no crate"))?;
            let file = format!("lib{krate}{}", std::env::consts::DLL_SUFFIX);
            let path = std::path::absolute(self.libs.join(file)).map_err(|e| e.to_string())?;
            if !path.exists() {
                return Err(format!("ra-jit linked no {}", path.display()));
            }
            let path = AbsPathBuf::assert_utf8(path);
            load_cargo::load_proc_macro(&self.server, &path, &[]).map_err(|e| e.to_string())
        });
        let macros = macros.as_ref().map_err(Clone::clone)?;
        let found = macros.iter().find(|it| it.name.as_str() == name);
        found.cloned().ok_or_else(|| format!("ra-jit's library of {dylib} lists no `{name}`"))
    }
}

/// Expands the call again with ra-jit's server and library when asked to, and counts whether
/// it gave what the toolchain's did.
#[allow(clippy::too_many_arguments)]
pub(crate) fn again(
    dylib: &AbsPath,
    name: &str,
    kind: ProcMacroKind,
    db: &dyn SourceDatabase,
    subtree: &tt::TopSubtree,
    attrs: Option<&tt::TopSubtree>,
    env: &Env,
    def_site: Span,
    call_site: Span,
    mixed_site: Span,
    current_dir: &str,
    toolchain: &Result<tt::TopSubtree, hir_expand::proc_macro::ProcMacroExpansionError>,
) {
    let Some(compare) = compare() else { return };
    let mac = match compare.macro_of(dylib, name) {
        Ok(it) => it,
        Err(why) => {
            COUNTS.unloaded.fetch_add(1, Ordering::Relaxed);
            say_once(name, &format!("not compared: {why}"));
            return;
        }
    };
    COMPARING.with(|it| it.set(true));
    let ours = mac.expander.expand(
        db,
        subtree,
        attrs,
        env,
        def_site,
        call_site,
        mixed_site,
        current_dir.to_owned(),
    );
    COMPARING.with(|it| it.set(false));
    COUNTS.compared[crate::kind_index(kind)].fetch_add(1, Ordering::Relaxed);
    let same = match (toolchain, &ours) {
        (Ok(theirs), Ok(ours)) => theirs == ours,
        (Err(theirs), Err(ours)) => {
            COUNTS.failed.fetch_add(1, Ordering::Relaxed);
            format!("{theirs:?}") == format!("{ours:?}")
        }
        _ => false,
    };
    if !same {
        COUNTS.differed.fetch_add(1, Ordering::Relaxed);
        let said = |it: &Result<tt::TopSubtree, _>| match it {
            Ok(tree) => format!("{tree:?}"),
            Err(e) => format!("error: {e:?}"),
        };
        say_once(
            name,
            &format!(
                "ra-jit's server and library expanded otherwise than the toolchain's:\n\
                 toolchain's: {}\nra-jit's:    {}",
                said(toolchain),
                said(&ours)
            ),
        );
    }
}

/// Says `what` of the macro `name` the first time only.
fn say_once(name: &str, what: &str) {
    static SAID: Mutex<Option<FxHashMap<String, ()>>> = Mutex::new(None);
    let mut said = SAID.lock().unwrap_or_else(|it| it.into_inner());
    if said.get_or_insert_default().insert(name.to_owned(), ()).is_none() {
        eprintln!("proc macros: `{name}`: {what}");
    }
}

struct Counts {
    compared: [AtomicU64; 3],
    differed: AtomicU64,
    /// Calls both servers failed to expand, the same way or not.
    failed: AtomicU64,
    unloaded: AtomicU64,
}

static COUNTS: Counts = Counts {
    compared: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
    differed: AtomicU64::new(0),
    failed: AtomicU64::new(0),
    unloaded: AtomicU64::new(0),
};

/// What the comparison found so far, for the report: `None` when there is none.
pub(crate) fn report() -> Option<String> {
    compare()?;
    let compared: Vec<u64> = COUNTS.compared.iter().map(|it| it.load(Ordering::Relaxed)).collect();
    let kinds = ["derives", "attributes", "function-like"];
    let kinds: Vec<_> =
        kinds.iter().zip(&compared).map(|(what, calls)| format!("{calls} {what}")).collect();
    Some(format!(
        "compared with ra-jit's server: {} calls ({}), {} expanded otherwise, {} failed by both, \
         {} not compared",
        compared.iter().sum::<u64>(),
        kinds.join(", "),
        COUNTS.differed.load(Ordering::Relaxed),
        COUNTS.failed.load(Ordering::Relaxed),
        COUNTS.unloaded.load(Ordering::Relaxed),
    ))
}
