//! Proc-macro expansions kept in the store, so that a daemon that loads a workspace again
//! expands no macro it expanded before.
//!
//! An expansion is a function of what the macro is given, as long as it asks nothing of the
//! database while it runs. Its key is the content of the macro's dylib, the macro's name and
//! kind, the input (and the attribute's input) with each span replaced by its place in a
//! table of the distinct spans of the call (`def_site`, `call_site`, `mixed_site`, then the
//! input's), that table's spans, the environment and the current directory it is given. A
//! span is in the key as its byte range, which is from the item it is anchored to, and which
//! of the call's anchors and syntax contexts it has, numbered as they first appear: the
//! next call with the same key has other anchors and contexts, as another database or
//! another revision numbers them, in the same places. What a macro makes of spans without
//! asking (joins them, parses a string at the call site) is a function of the key, and the
//! value is the output with its spans written the same way. What else a macro learns of a
//! span (its text, file or line) it asks the database for, and an expansion that asked is not
//! kept.
//!
//! Neither is one that failed, nor one whose output has an anchor or a context the input has
//! not, which the next call could not be given back. A macro that reads files or the
//! environment by itself does so without asking. Derives and
//! attributes are kept, as rustc's `-Zcache-proc-macros` keeps derives; function-like macros,
//! where such reading is (a query checked against a database, a directory put in the
//! program), only with `RA_JIT_PROC_MACRO_CACHE=all`. A macro named in
//! `RA_JIT_PROC_MACRO_CACHE_SKIP` is never kept, `RA_JIT_PROC_MACRO_CACHE=check` expands every
//! call anyway and says which macro gave other output than it kept, and
//! `RA_JIT_PROC_MACRO_CACHE=off` keeps nothing and still counts.
//!
//! The store's own key adds the build of ra-jit and the toolchain, whose proc-macro server
//! expands.
//!
//! [`compare`] holds the toolchain's server against ra-jit's, and keeps nothing while it does.

mod compare;

use std::{
    any::Any,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use base_db::{Env, SourceDatabase};
use hir_expand::proc_macro::{ProcMacroExpander, ProcMacroExpansionError, ProcMacroKind};
use paths::{AbsPath, AbsPathBuf};
use proc_macro_api::{
    legacy_protocol::msg::{FlatTree, SpanDataIndexMap},
    version::CURRENT_API_VERSION,
};
use ra_jit_store::{KEY, Store};
use rustc_hash::FxHashMap;
use sha2::{Digest, Sha256};
use span::{Span, SpanAnchor, SyntaxContext, TextRange};

/// What the key of an expansion starts with: another number when the key or the value is
/// made otherwise.
const DOMAIN: &[u8] = b"ra-jit proc-macro expansion 2";

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Off,
    /// Derives and attributes: what a function-like macro is given is the code it reads, and
    /// the ones that read more than that (a query checked against a database, a directory
    /// put in the program) are of that kind.
    On,
    All,
    Check,
}

struct Config {
    mode: Mode,
    skip: Vec<Box<str>>,
    compare: bool,
}

fn config() -> &'static Config {
    static CONFIG: OnceLock<Config> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let mode = match std::env::var("RA_JIT_PROC_MACRO_CACHE").as_deref() {
            Ok("off" | "0") => Mode::Off,
            Ok("all") => Mode::All,
            Ok("check") => Mode::Check,
            _ => Mode::On,
        };
        let skip = std::env::var("RA_JIT_PROC_MACRO_CACHE_SKIP").unwrap_or_default();
        let skip = skip.split(',').map(str::trim).filter(|it| !it.is_empty()).map(Into::into);
        let compare = std::env::var_os("RA_JIT_PROC_MACRO_COMPARE").is_some();
        Config { mode, skip: skip.collect(), compare }
    })
}

/// Puts the cache around every proc macro the workspaces load from here on.
pub fn install() {
    load_cargo::wrap_expanders(wrap);
}

fn wrap(
    inner: Arc<dyn ProcMacroExpander>,
    dylib: &AbsPath,
    name: &str,
    kind: ProcMacroKind,
) -> Arc<dyn ProcMacroExpander> {
    let skipped = match config().mode {
        Mode::Off => true,
        Mode::On => kind == ProcMacroKind::Bang,
        Mode::All | Mode::Check => false,
    } || config().compare
        || config().skip.iter().any(|it| **it == *name);
    Arc::new(Kept { inner, dylib: Dylib::at(dylib), name: name.into(), kind, skipped })
}

/// A dylib of proc macros, hashed the first time one of its macros expands.
#[derive(Debug)]
struct Dylib {
    path: AbsPathBuf,
    hash: OnceLock<Option<[u8; KEY]>>,
}

impl Dylib {
    /// The one dylib at `path`: the macros of a dylib share its hash.
    fn at(path: &AbsPath) -> Arc<Dylib> {
        static DYLIBS: Mutex<Option<FxHashMap<AbsPathBuf, Arc<Dylib>>>> = Mutex::new(None);
        let mut dylibs = DYLIBS.lock().unwrap_or_else(|it| it.into_inner());
        let dylibs = dylibs.get_or_insert_default();
        let dylib = dylibs.entry(path.to_path_buf());
        dylib
            .or_insert_with(|| Arc::new(Dylib { path: path.to_path_buf(), hash: OnceLock::new() }))
            .clone()
    }

    fn hash(&self) -> Option<&[u8; KEY]> {
        let hash = || Some(Sha256::digest(std::fs::read(&self.path).ok()?).into());
        self.hash.get_or_init(hash).as_ref()
    }
}

#[derive(Debug)]
struct Kept {
    inner: Arc<dyn ProcMacroExpander>,
    dylib: Arc<Dylib>,
    name: Box<str>,
    kind: ProcMacroKind,
    skipped: bool,
}

/// What the expansion of a call is kept under, and the anchors and contexts of its spans,
/// which the output is written against.
struct Call {
    key: [u8; KEY],
    spans: Spans,
}

/// A span as a key or a value has it: its byte range, and the numbers of its anchor and its
/// context among the call's.
type Shape = [u32; 4];

/// The anchors and syntax contexts of a call's spans, in the order they first appear.
#[derive(Default)]
struct Spans {
    anchors: Vec<SpanAnchor>,
    contexts: Vec<SyntaxContext>,
}

impl Spans {
    fn number<T: PartialEq>(all: &mut Vec<T>, it: T) -> u32 {
        let at = all.iter().position(|known| *known == it).unwrap_or_else(|| {
            all.push(it);
            all.len() - 1
        });
        at as u32
    }

    fn shape_new(&mut self, span: &Span) -> Shape {
        let anchor = Self::number(&mut self.anchors, span.anchor);
        let context = Self::number(&mut self.contexts, span.ctx);
        [span.range.start().into(), span.range.end().into(), anchor, context]
    }

    /// The shape of a span of the output, which has to have the call's anchors and contexts.
    fn shape(&self, span: &Span) -> Option<Shape> {
        let anchor = self.anchors.iter().position(|it| *it == span.anchor)?;
        let context = self.contexts.iter().position(|it| *it == span.ctx)?;
        Some([span.range.start().into(), span.range.end().into(), anchor as u32, context as u32])
    }

    fn span(&self, [start, end, anchor, context]: Shape) -> Option<Span> {
        Some(Span {
            range: TextRange::new(start.into(), end.max(start).into()),
            anchor: *self.anchors.get(anchor as usize)?,
            ctx: *self.contexts.get(context as usize)?,
        })
    }
}

impl Kept {
    #[allow(clippy::too_many_arguments)]
    fn call(
        &self,
        subtree: &tt::TopSubtree,
        attrs: Option<&tt::TopSubtree>,
        env: &Env,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
        current_dir: &str,
    ) -> Option<Call> {
        let mut table = SpanDataIndexMap::default();
        for span in [def_site, call_site, mixed_site] {
            table.insert(span);
        }
        let input = FlatTree::from_subtree(subtree.view(), CURRENT_API_VERSION, &mut table);
        let attrs =
            attrs.map(|it| FlatTree::from_subtree(it.view(), CURRENT_API_VERSION, &mut table));
        let mut spans = Spans::default();
        let shapes: Vec<Shape> = table.iter().map(|span| spans.shape_new(span)).collect();
        let mut env: Vec<_> = env.iter().collect();
        env.sort_unstable();
        let mut hasher = Sha256::new();
        let mut field = |bytes: &[u8]| {
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };
        field(DOMAIN);
        field(self.dylib.hash()?);
        field(self.name.as_bytes());
        field(&[self.kind as u8]);
        field(&CURRENT_API_VERSION.to_le_bytes());
        field(&postcard::to_allocvec(&input).ok()?);
        field(&postcard::to_allocvec(&attrs).ok()?);
        field(&postcard::to_allocvec(&shapes).ok()?);
        field(&(env.len() as u64).to_le_bytes());
        for (name, value) in env {
            field(name.as_bytes());
            field(value.as_bytes());
        }
        field(current_dir.as_bytes());
        Some(Call { key: hasher.finalize().into(), spans })
    }
}

/// The output with its spans as shapes of the call's, or `None` when it has an anchor or a
/// context the call has not.
fn written(output: &tt::TopSubtree, spans: &Spans) -> Option<Vec<u8>> {
    let mut table = SpanDataIndexMap::default();
    let flat = FlatTree::from_subtree(output.view(), CURRENT_API_VERSION, &mut table);
    let shapes: Option<Vec<Shape>> = table.iter().map(|span| spans.shape(span)).collect();
    postcard::to_allocvec(&(shapes?, flat)).ok()
}

fn read(bytes: &[u8], spans: &Spans) -> Option<tt::TopSubtree> {
    let (shapes, flat): (Vec<Shape>, FlatTree) = postcard::from_bytes(bytes).ok()?;
    let table: Option<SpanDataIndexMap> = shapes.into_iter().map(|it| spans.span(it)).collect();
    Some(flat.to_subtree_resolved(CURRENT_API_VERSION, &table?))
}

/// The store, once the daemon has told it the toolchain: asked before that, it would be
/// opened without one, which is not to have one for as long as the process runs.
fn store() -> Option<&'static Store> {
    ra_jit_store::toolchain()?;
    Store::global()
}

impl ProcMacroExpander for Kept {
    fn expand(
        &self,
        db: &dyn SourceDatabase,
        subtree: &tt::TopSubtree,
        attrs: Option<&tt::TopSubtree>,
        env: &Env,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
        current_dir: String,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        if compare::comparing() {
            return self.inner.expand(
                db,
                subtree,
                attrs,
                env,
                def_site,
                call_site,
                mixed_site,
                current_dir,
            );
        }
        let start = Instant::now();
        let mode = config().mode;
        let store = (!self.skipped).then(store).flatten();
        let call = store.and_then(|_| {
            self.call(subtree, attrs, env, def_site, call_site, mixed_site, &current_dir)
        });
        if call.is_some() {
            COUNTS.keyed.add(start.elapsed());
        }
        let found = store.zip(call.as_ref()).and_then(|(store, call)| store.get(&call.key));
        if let (Some(bytes), Some(call)) = (&found, &call)
            && mode != Mode::Check
            && let Some(output) = read(bytes, &call.spans)
        {
            COUNTS.found.add(start.elapsed());
            return Ok(output);
        }
        // What was found and could not be read is written again.
        let found = found.filter(|_| mode == Mode::Check);
        let asked = load_cargo::asked();
        let expand = Instant::now();
        let expanded = self.inner.expand(
            db,
            subtree,
            attrs,
            env,
            def_site,
            call_site,
            mixed_site,
            current_dir.clone(),
        );
        COUNTS.expanded.add(expand.elapsed());
        COUNTS.kinds[kind_index(self.kind)].add(expand.elapsed());
        if config().compare {
            compare::again(
                &self.dylib.path,
                &self.name,
                self.kind,
                db,
                subtree,
                attrs,
                env,
                def_site,
                call_site,
                mixed_site,
                &current_dir,
                &expanded,
            );
        }
        let (Some(store), Some(call), Ok(output)) = (store, &call, &expanded) else {
            if expanded.is_err() {
                COUNTS.failed.add(Duration::ZERO);
            }
            return expanded;
        };
        if load_cargo::asked() != asked {
            COUNTS.asked.add(Duration::ZERO);
            return expanded;
        }
        let Some(bytes) = written(output, &call.spans) else {
            COUNTS.unspanned.add(Duration::ZERO);
            return expanded;
        };
        match found {
            Some(kept) if kept != bytes => {
                COUNTS.differed.add(Duration::ZERO);
                eprintln!("proc macros: `{}` expanded otherwise than it was kept", self.name);
            }
            Some(_) => COUNTS.found.add(Duration::ZERO),
            None => {
                store.insert(&call.key, &bytes);
                COUNTS.kept.add(Duration::ZERO);
            }
        }
        expanded
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool {
        (other as &dyn Any)
            .downcast_ref::<Kept>()
            .is_some_and(|other| self.name == other.name && self.inner.eq_dyn(&*other.inner))
    }
}

/// How many, and the time they took on the threads that expanded them.
struct Count {
    calls: AtomicU64,
    nanos: AtomicU64,
}

impl Count {
    const fn new() -> Count {
        Count { calls: AtomicU64::new(0), nanos: AtomicU64::new(0) }
    }

    fn add(&self, took: Duration) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.nanos.fetch_add(took.as_nanos() as u64, Ordering::Relaxed);
    }

    fn get(&self) -> (u64, f64) {
        (self.calls.load(Ordering::Relaxed), self.nanos.load(Ordering::Relaxed) as f64 / 1e9)
    }
}

struct Counts {
    /// Calls given a key, with the time that took.
    keyed: Count,
    /// Expanded by the proc-macro server, kept or not.
    expanded: Count,
    /// The same, by [`kind_index`].
    kinds: [Count; 3],
    /// Read from the store instead (and, under `check`, expanded and the same).
    found: Count,
    kept: Count,
    asked: Count,
    unspanned: Count,
    failed: Count,
    differed: Count,
}

static COUNTS: Counts = Counts {
    keyed: Count::new(),
    expanded: Count::new(),
    kinds: [Count::new(), Count::new(), Count::new()],
    found: Count::new(),
    kept: Count::new(),
    asked: Count::new(),
    unspanned: Count::new(),
    failed: Count::new(),
    differed: Count::new(),
};

fn kind_index(kind: ProcMacroKind) -> usize {
    match kind {
        ProcMacroKind::CustomDerive => 0,
        ProcMacroKind::Attr => 1,
        ProcMacroKind::Bang => 2,
    }
}

/// The expansions so far, for the daemon's log: `None` before the first.
pub fn report() -> Option<String> {
    let (expanded, expanding) = COUNTS.expanded.get();
    let (found, finding) = COUNTS.found.get();
    if expanded + found == 0 {
        return None;
    }
    let (_, keying) = COUNTS.keyed.get();
    let mut said = format!(
        "proc macros: {expanded} expanded in {expanding:.2}s, {found} from the store in \
         {finding:.2}s, {keying:.2}s to key them"
    );
    let kinds = ["derives", "attributes", "function-like"].iter().zip(&COUNTS.kinds);
    let kinds: Vec<_> = kinds
        .filter_map(|(what, count)| {
            let (calls, took) = count.get();
            (calls != 0).then(|| format!("{calls} {what} in {took:.2}s"))
        })
        .collect();
    if !kinds.is_empty() {
        said += &format!(" ({})", kinds.join(", "));
    }
    let rest = [
        ("kept", &COUNTS.kept),
        ("not kept, as they asked the database", &COUNTS.asked),
        ("not kept, as their output has spans the call has not", &COUNTS.unspanned),
        ("failed", &COUNTS.failed),
        ("expanded otherwise than kept", &COUNTS.differed),
    ];
    for (what, count) in rest {
        let (calls, _) = count.get();
        if calls != 0 {
            said += &format!("; {calls} {what}");
        }
    }
    if let Some(compared) = compare::report() {
        said += &format!("; {compared}");
    }
    Some(said)
}
