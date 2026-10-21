//! ra-jit spike: lower RA MIR to native code with Cranelift.
//!
//! Every local lives in memory. Calls follow rustc's (unstable) Rust ABI so the
//! output can be linked against rustc-compiled objects.

use std::{borrow::Cow, fmt, sync::LazyLock};

use base_db::Crate;
use std::cell::Cell;

use cranelift_codegen::{
    ir::{
        self, AbiParam, ArgumentPurpose, AtomicRmwOp, InstBuilder, MemFlagsData, Signature,
        StackSlotData, StackSlotKind, TrapCode, Type, Value,
        condcodes::{FloatCC, IntCC},
        types,
    },
    isa,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Switch};
use cranelift_module::{
    DataDescription, DataId, FuncId, Linkage, Module, ModuleReloc, ModuleRelocTarget,
    default_libcall_names,
};
use cranelift_object::{ObjectBuilder, ObjectModule};
use hir_def::{
    AdtId, DefWithBodyId, FunctionId, HasModule, ItemContainerId, Lookup, StaticId, TraitId,
    VariantId,
    attrs::AttrFlags,
    expr_store::{
        Body,
        scope::{ExprScopes, ScopeId},
    },
    hir::{BindingId, ExprId},
    layout::{BackendRepr, Primitive, Scalar, TagEncoding, Variants},
    signatures::{FunctionSignature, StaticFlags, StaticSignature, StructFlags, StructSignature},
    src::HasSource,
};
use hir_expand::{
    FileRange, HirFileId, InFile, MacroDefKind,
    builtin::{BuiltinFnLikeExpander, EagerExpander},
};
use la_arena::ArenaMap;
use rustc_abi::{ExternAbi, Integer, ReprFlags, Size};
use rustc_ast_ir::Mutability;
use rustc_hash::{FxHashMap, FxHashSet};
use rustc_index::Idx;
use rustc_type_ir::UintTy;
use rustc_type_ir::inherent::{
    AdtDef as _, BoundExistentialPredicates as _, GenericArgs as _, IntoKind, SliceLike, Ty as _,
};
use salsa::plumbing::{AsId, FromId};
use syntax::{AstNode, AstToken, SyntaxElement, SyntaxKind, SyntaxNode, TextRange, TextSize, ast};
use triomphe::Arc;
use unicode_width::UnicodeWidthChar;

use crate::mir::is_by_move_coroutine;
use crate::{
    CallableDefId, InferBodyId, InferenceResult, MemoryMap, ParamEnvAndCrate,
    consteval::try_const_usize,
    db::HirDatabase,
    db::{GeneralConstId, InternedClosureId, InternedCoroutineId},
    drop::{DropGlue, destructor, has_drop_glue},
    infer::PointerCast,
    layout::{CoroutineFrame, Layout, RustcEnumVariantIdx, RustcFieldIdx},
    method_resolution::{is_dyn_method, lookup_impl_const},
    next_solver::{
        Allocation, AllocationData, ConstKind, DbInterner, GenericArgs, Region, StoredGenericArgs,
        StoredTy, TraitRef, Ty, TyKind, TypingMode, UnevaluatedConst, ValTree,
        infer::{DbInternerInferExt, InferCtxt},
    },
    traits::StoredParamEnvAndCrate,
    utils::detect_variant_from_bytes,
};
use hir_def::lang_item::LangItems;

use self::debuginfo::{DebugContext, FunctionDebug, LineRow, SlotPlaces, VariableDebug};
use self::fragment::{
    Code, CompiledFunction, CompiledStatic, DataRequest, FuncRequest, FuncTarget, Named, Recorder,
    Requests, TextPart, Variable,
};
use self::unwind::{FunctionUnwind, UnwindContext};
use self::vtable::{Slot, Upcast, VtableLayout};
use super::{
    AggregateKind, AssertKind, BasicBlockId, BinOp, CastKind, LocalId, MirBody, MirOptLevel,
    MirPhase, MirSpan, Operand, OperandKind, PlaceTy, ProjectionElem, Rvalue, StatementKind,
    StoredPlace, TerminatorKind, UnOp, eval::interpret_mir, lower::is_builtin_index,
    monomorphization::monomorphized, monomorphized_promoted_mir_body_query, return_slot,
};

mod asm;
mod backend;
pub use backend::Backend;
use backend::Step;
mod cached;
mod debuginfo;
pub use debuginfo::Sidecar;
mod float_pair;
mod fncache;
mod fragment;
#[cfg(test)]
mod fuzz;
mod home;
pub use home::body_file;
#[cfg(feature = "llvm")]
mod llvm;
mod patch;
mod places;
use self::places::{row_location, source_location};
mod promote;
mod settled;
mod stable;
pub use stable::{block_place, lower_declared};
mod store;
mod target;
pub use target::{Arch, Calls, ObjectFormat, Target, UnwindFormat, VariableArguments};
pub mod tier;

pub use patch::{Stale, StaleSteps};
/// For `crates/ra-jit-bench` alone.
#[doc(hidden)]
pub use store::bench;
pub use store::{
    Collected, FORMAT as STORE_FORMAT, FetchMissing, Stats, StoreDir, Verified, compiles_against,
    fetch_missing_with, identify_ahead, is_off as store_is_off, is_pack, limit_from_env,
    name_file_whole, pack_format, pack_keys, pack_name, parse_size, seal_open, settled,
    write_file_whole,
};
mod type_id;
mod unwind;
mod vtable;
pub mod wasm;
mod wasm_object;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodegenError {
    /// The function uses something this backend can't compile yet; fall back to rustc.
    Unsupported(String),
    /// MIR lowering or layout failed in RA.
    Frontend(String),
    Backend(String),
    /// rustc exports nothing for the function, so there is nothing to replace: only code in
    /// its own crate can call it.
    NoSymbol,
    /// rustc built the function under a symbol private to its object, so its copy stays the
    /// one that code of that object calls, and goes on using rustc's statics.
    Private,
}

impl CodegenError {
    /// Whether rustc has no symbol of the function that ours could take the place of.
    pub fn no_symbol(&self) -> bool {
        matches!(self, CodegenError::NoSymbol | CodegenError::Private)
    }
}

impl fmt::Display for CodegenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodegenError::Unsupported(it) => write!(f, "unsupported: {it}"),
            CodegenError::Frontend(it) => write!(f, "frontend: {it}"),
            CodegenError::Backend(it) => write!(f, "backend: {it}"),
            CodegenError::NoSymbol | CodegenError::Private => {
                write!(f, "unsupported: no rustc symbol for function")
            }
        }
    }
}

type Result<T> = std::result::Result<T, CodegenError>;

macro_rules! unsupported {
    ($($arg:tt)*) => {
        return Err(CodegenError::Unsupported(format!($($arg)*)))
    };
}

/// What a crate is by what is written in it and what it is built on: a SHA-256, all of it.
/// The names made from it are what another machine's store is asked for code by, so it is
/// as hard to find two crates of one id as two files of one hash.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentId(pub [u8; 32]);

impl fmt::Display for ContentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// Where cargo keeps the sources it fetched of registries and of git, as what is stored names
/// it: the same on every machine, whoever's home the directory is in.
pub const CARGO_HOME: &str = "/cargo";

/// `path` with the directory `to` in place of the directory `from` it is under.
pub fn moved(path: &str, from: &str, to: &str) -> Option<String> {
    let rest = path.strip_prefix(from)?;
    rest.starts_with(['/', '\\']).then(|| format!("{to}{rest}"))
}

/// `path` as rustc spells it in a location or in debug info. For a file of the installed
/// standard library rustc joins `lib/rustlib/src/rust` to the sysroot as one piece, so on
/// Windows it reads `<sysroot>\lib/rustlib/src/rust\library\..` where the file system
/// names it with backslashes throughout. Elsewhere the two are already the same.
fn as_rustc_spells(path: &str) -> String {
    path.replacen(r"\lib\rustlib\src\rust\", r"\lib/rustlib/src/rust\", 1)
}

/// The sources of the standard library: rustc names them where they are on this machine, and
/// what is stored names them where they were when the library was built, as every machine can.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibrarySources {
    /// The directory they are in here.
    pub here: String,
    /// The directory they were built in, `/rustc/<commit>`.
    pub built_in: String,
}

impl LibrarySources {
    fn stored(&self, path: &str) -> Option<String> {
        moved(path, &self.here, &self.built_in)
    }

    fn local(&self, path: &str) -> Option<String> {
        moved(path, &self.built_in, &self.here)
    }
}

/// Symbol names come from the rustc-built artifacts we link against.
pub trait SymbolResolver {
    fn function(&self, func: FunctionId) -> Option<String>;
    fn static_(&self, id: StaticId) -> Option<String>;
    /// Whether rustc built the function under a symbol that only its own object can link to.
    fn private(&self, _func: FunctionId) -> bool {
        false
    }
    /// A non-generic function by path, e.g. `core::panicking::panic_bounds_check`.
    fn path(&self, path: &str) -> Option<String>;
    /// The path of an item, e.g. `core::any::TypeId`.
    fn item(&self, item: hir_def::ModuleDefId) -> Option<String>;
    /// The name rustc gives a source file in panic messages.
    fn source_path(&self, file: base_db::FileId) -> Option<String>;
    /// The directory the names of [`Self::source_path`] are relative to, when the build is
    /// to tell a debugger where its code comes from.
    fn debug_directory(&self) -> Option<String> {
        None
    }
    /// The file a debugger reads the entries of the code from, when the build keeps them
    /// apart from what it links: the object itself, which has them in sections a linker
    /// leaves out.
    fn debug_apart(&self) -> Option<String> {
        None
    }
    /// Whether a debugger is told of the variables of the code too, and not of its lines
    /// alone.
    fn describes_variables(&self) -> bool {
        true
    }
    /// Whether the program's rustc build has the symbol, defined or referred to.
    fn defines(&self, _symbol: &str) -> bool {
        false
    }
    /// The `StableCrateId` of each crate that compiled a copy of an instance of a function
    /// named `function`.
    fn makers(&self, _function: &str) -> Vec<u64> {
        Vec::new()
    }
    /// The copies rustc made of the instance `symbol` is one of, each with the
    /// `StableCrateId` of the crate that compiled it.
    fn copies(&self, _symbol: &str) -> Vec<(String, u64)> {
        Vec::new()
    }
    /// Whether rustc compiled the crate, so that what is skipped of it is in the program.
    fn rustc_built(&self, _krate: Crate) -> bool {
        true
    }
    /// rustc's `StableCrateId` of a crate, which its symbols carry.
    fn stable_crate_id(&self, krate: Crate) -> Option<u64>;
    /// What stands for a crate that is not rustc's alone to build, the same wherever the
    /// same sources are built on the same crates.
    fn content_id(&self, krate: Crate) -> Option<ContentId> {
        _ = krate;
        None
    }
    /// Where cargo keeps the sources it fetched on this machine, when the crates of the
    /// program tell: [`CARGO_HOME`] stands for it in what is stored.
    fn cargo_home(&self) -> Option<String> {
        None
    }
    /// Where the sources of the standard library are, when the toolchain tells where they
    /// were built.
    fn library_sources(&self) -> Option<LibrarySources> {
        None
    }
    /// Whether data has in it where the checkout is, which another checkout's is not.
    fn names_place(&self, bytes: &[u8]) -> bool {
        _ = bytes;
        false
    }
    /// The function a type is declared in the body of, when nothing else is around it there
    /// and no other type of the body has its name.
    fn declared_in(&self, adt: hir_def::AdtId) -> Option<FunctionId> {
        _ = adt;
        None
    }
    /// How many `impl`s of its module rustc numbers before this one.
    fn impl_place(&self, id: hir_def::ImplId) -> Option<u32> {
        _ = id;
        None
    }
    /// Runs `work` for each of `0..count`, on as many threads as there are, each with a
    /// database of its own and this resolver over it. What `work` computes is kept by the
    /// queries it asks.
    fn in_parallel(&self, count: usize, work: &ParallelWork<'_>) {
        self.in_parallel_beside(count, work, &mut || ());
    }
    /// [`Self::in_parallel`], with `beside` run on this thread while the others work.
    fn in_parallel_beside(&self, count: usize, work: &ParallelWork<'_>, beside: &mut dyn FnMut()) {
        _ = (count, work);
        beside();
    }
}

/// What an instance is called in every run of the daemon, if anything.
type StableName = std::sync::Arc<str>;

pub type ParallelWork<'a> = dyn Fn(&dyn HirDatabase, &dyn SymbolResolver, usize) + Sync + 'a;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum OverflowOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Neg,
    Shl,
    Shr,
    DivByZero,
    RemByZero,
}

/// How a value crosses a call boundary in rustc's Rust ABI.
#[derive(Debug, Clone, Copy)]
enum PassMode {
    Ignore,
    Direct(AbiScalar),
    Pair(AbiScalar, AbiScalar, Size),
    /// Small aggregate passed as one integer register.
    Cast(Type),
    /// Aggregate the C ABI splits over two registers, the second holding what lies eight
    /// bytes in.
    CastPair(Type, Type),
    /// Aggregate AAPCS64 hands over in `count` registers of one type, each holding the next
    /// `unit` bytes of it. `skipped` registers of that kind go unused first: the convention
    /// leaves them out when the value has to start at an even register, and takes them all
    /// out of use when the value has to go to the stack, where the units then land. A unit
    /// of 128 bits is the one that is on the stack at a multiple of sixteen bytes.
    Uniform {
        unit: Type,
        count: u8,
        skipped: u8,
    },
    Indirect,
    /// The C ABI's by-value aggregate in memory: the caller copies `size` bytes into its
    /// outgoing arguments, which Cranelift does given a pointer. Cranelift has such a copy
    /// at the next multiple of eight bytes, so one that is aligned to sixteen has eight
    /// bytes of nothing passed the same way ahead of it when it would not be at one:
    /// `padded`.
    OnStack {
        size: u32,
        padded: bool,
    },
    /// A `Pair` with a 128-bit integer half that finds one integer register left by the
    /// System V x86-64 convention: that half is passed as `OnStack` passes sixteen bytes,
    /// `Some(padded)`, and the other as a value (see `wide_ints_on_stack`).
    SpilledPair {
        a: AbiScalar,
        b: AbiScalar,
        offset: Size,
        spilled: [Option<bool>; 2],
    },
}

impl PassMode {
    /// What fills one register of the kind the units of a `Uniform` go to.
    fn register(unit: Type) -> Type {
        if unit == types::I128 { types::I64 } else { unit }
    }
}

/// How every value of a call crosses the boundary.
struct FnAbi {
    ret: PassMode,
    args: Vec<PassMode>,
}

/// The first 128-bit integer parameter that finds one integer register left by the System V
/// x86-64 convention, and how many bytes of the stack the parameters before it take, as
/// Cranelift places them. Cranelift and LLVM both put such an integer on the stack, but
/// LLVM gives the register that is left to the next integer parameter and Cranelift gives
/// it to nobody, so the two place the parameters after it differently.
pub(crate) fn lone_register_i128(params: &[AbiParam]) -> Option<(usize, u32)> {
    const INTS: usize = 6;
    const FLOATS: usize = 8;
    let (mut ints, mut floats, mut stack) = (0, 0, 0u32);
    for (at, param) in params.iter().enumerate() {
        let ty = param.value_type;
        if let ArgumentPurpose::StructArgument(size) = param.purpose {
            stack += size;
        } else if ty == types::I128 {
            if ints == INTS - 1 {
                return Some((at, stack));
            }
            if ints > INTS - 2 {
                stack = stack.next_multiple_of(16) + 16;
            }
            ints += 2;
        } else if ty.is_float() || ty.is_vector() {
            if floats < FLOATS {
                floats += 1;
            } else {
                let size = ty.bytes().max(8);
                stack = stack.next_multiple_of(size) + size;
            }
        } else if ints < INTS {
            ints += 1;
        } else {
            stack += 8;
        }
    }
    None
}

/// How AAPCS64 sorts a value, before the registers that are left decide where it goes.
enum Aapcs64 {
    Ignore,
    Scalar(AbiScalar),
    /// A homogeneous aggregate of `count` floats of the type `unit`.
    Floats {
        unit: Type,
        count: u8,
    },
    /// Any other aggregate of at most sixteen bytes.
    Ints {
        size: u64,
        align: u64,
    },
    /// A larger one, which the caller copies and passes the address of.
    Memory,
}

/// What the System V x86-64 ABI calls the class of an eightbyte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum RegClass {
    Int,
    Sse,
}

#[derive(Debug, Clone, Copy)]
struct AbiScalar {
    ty: Type,
    signed: bool,
}

impl AbiScalar {
    fn param(self) -> AbiParam {
        let param = AbiParam::new(self.ty);
        match (self.ty.is_int() && self.ty.bits() < 32, self.signed) {
            (false, _) => param,
            (true, true) => param.sext(),
            (true, false) => param.uext(),
        }
    }
}

/// The result of compiling a set of root functions into one object file.
pub struct CodegenUnit {
    /// The object, unless the one of the build before still stands and `patch` amends it.
    pub object: Option<Vec<u8>>,
    /// With [`Rejections::patching`], an object to link ahead of the last `object`: its
    /// definitions replace those of the same name there.
    pub patch: Option<Vec<u8>>,
    /// The files that go with `object`: what a debugger reads that is kept apart from it.
    pub apart: Vec<Sidecar>,
    /// Roots that were compiled, with their exported symbol.
    pub compiled: Vec<(FunctionId, String)>,
    /// Roots that have to come from rustc.
    pub skipped: Vec<(FunctionId, CodegenError)>,
    /// Callees without a rustc symbol (generic or inlined) that were compiled as local functions.
    pub instances: usize,
}

/// The functions a build starts from.
#[derive(Clone, Copy)]
pub struct Roots<'a> {
    /// Compiled whether or not anything calls them.
    pub entries: &'a [FunctionId],
    /// Compiled once code in the object refers to them. A function not reached keeps rustc's
    /// copy, which nothing in the program calls.
    pub on_demand: &'a [FunctionId],
    /// The `main` of a program rustc built nothing of: the `main` the C runtime calls is
    /// then ours to write as well.
    pub start: Option<FunctionId>,
    /// The statics a linker or a runtime finds though no code names them, in the crates
    /// rustc built nothing of: `#[used]` ones, and those put in a section by name.
    pub kept: &'a [StaticId],
}

impl CodegenUnit {
    /// Compiles every root it can, together with the instances those roots call.
    ///
    /// A callee that can't be compiled poisons its callers, so codegen restarts with that callee
    /// rejected up front until a round finishes without a new rejection.
    pub fn compile(
        db: &dyn HirDatabase,
        symbols: &dyn SymbolResolver,
        roots: Roots<'_>,
        denied: &FxHashSet<FunctionId>,
        known: &mut Rejections,
    ) -> Result<CodegenUnit> {
        let on_demand: FxHashSet<FunctionId> = roots.on_demand.iter().copied().collect();
        let all = || roots.entries.iter().chain(roots.on_demand).copied();
        let mut declined = None;
        // Code of the checkout comes from the store in the first build alone: what a patch is
        // of has to be compiled here, to tell what an edit changes. That makes the first edit a
        // full build, which a program running the first build has to be started again for;
        // RA_JIT_HYDRATE=0 compiles the checkout's code in the first build instead.
        let hydrate =
            known.base.is_none() && std::env::var_os("RA_JIT_HYDRATE").is_none_or(|it| it != "0");
        if let Some(base) = known.base.take().filter(|it| !it.outdated()) {
            let start = stdx::time::Instant::now();
            let own_statics = OwnStatics::new(
                db,
                all().map(|root| root.krate(db)).collect(),
                known.theirs.clone(),
                known.unknown.clone(),
                !roots.on_demand.is_empty(),
            );
            let statics = start.elapsed();
            let mut stale = std::mem::take(&mut known.stale);
            if stale.is_empty() {
                stale.push(base.stale(db, 0, 1));
            }
            let patch = loop {
                let patch = base.patch(
                    db,
                    symbols,
                    roots,
                    &on_demand,
                    denied,
                    known,
                    own_statics.clone(),
                    stale.clone(),
                );
                let why = match patch {
                    Ok(Ok(patch)) => break Some(patch),
                    Ok(Err(why)) => Some(why),
                    Err(_) => None,
                };
                // Code compiled at a higher level that a patch can't hold goes, not the
                // program the patch is for.
                let told = why.unwrap_or("it could not be compiled");
                if !known.tiers.refuse(&format!("no patch can hold its code: {told}")) {
                    declined = why;
                    break None;
                }
            };
            if let Some(patch) = patch {
                backend::Tally::global().report();
                let compiled_now = known.compiled_since_last_build();
                known.stats = BuildStats {
                    bodies: base.bodies,
                    reused: base.bodies.saturating_sub(compiled_now),
                    compiled: compiled_now,
                    phases: Phases { statics, ..patch.phases },
                    rounds: 1,
                    patched: Some(patch.bodies),
                    declined: None,
                };
                let unit = CodegenUnit {
                    object: None,
                    patch: Some(patch.object),
                    apart: Vec::new(),
                    compiled: base.compiled.clone(),
                    skipped: base.skipped.clone(),
                    instances: base.instances.len(),
                };
                known.base = Some(base);
                return Ok(unit);
            }
        }
        let patching = known.patching;
        let used_by_rustc = known.used_by_rustc;
        // FIXME: a rejection outlives an edit that would lift it. Dropping them here costs one
        // full pass per rejected call chain, and they go away with the rustc fallback.
        let Rejections {
            rejected,
            stats,
            theirs,
            unknown,
            left,
            futures,
            base,
            patching: _,
            counted,
            stale: _,
            frozen: _,
            used_by_rustc: _,
            tiers,
        } = known;
        // A full build is linked again, and the program that runs it starts at level 1.
        tiers.clear();
        let mut own_statics = OwnStatics::new(
            db,
            all().map(|root| root.krate(db)).collect(),
            std::mem::take(theirs),
            std::mem::take(unknown),
            !roots.on_demand.is_empty(),
        );
        let mut rounds = 0;
        let mut discarded = std::time::Duration::ZERO;
        let mut start = roots.start;
        loop {
            rounds += 1;
            let round = stdx::time::Instant::now();
            let mut start_over = |why: &str| {
                discarded += round.elapsed();
                if std::env::var_os("RA_JIT_STATS").is_some() {
                    eprintln!("round {rounds}: {:.1?}, started over: {why}", round.elapsed());
                }
            };
            let mut cg = ObjectCodegen::new(
                db,
                symbols,
                rejected,
                own_statics.clone(),
                &on_demand,
                None,
                hydrate,
            )?;
            cg.patching = patching;
            cg.rustc_futures = futures.clone();
            let mut compiled = Vec::new();
            let mut skipped = Vec::new();
            let mut failed = Vec::new();
            let mut queue = roots.entries.to_vec();
            let mut queued: FxHashSet<FunctionId> = queue.iter().copied().collect();
            let mut next = 0;
            if let Some(main) = start {
                cg.define_start(main)?;
            }
            for &id in roots.kept {
                cg.static_data(id)?;
            }
            loop {
                // Every core compiles the roots found so far, before they are put in the
                // object one by one.
                let ours: Vec<FunctionId> = queue[next..]
                    .iter()
                    .copied()
                    .filter(|root| !denied.contains(root) && !left.contains(root))
                    .collect();
                let start = stdx::time::Instant::now();
                // One without a symbol is not compiled, and is still lowered to be told why.
                let told = cg.debug.is_some().then_some(&cg.told);
                symbols.in_parallel(ours.len(), &|db, symbols, at| {
                    if symbols.function(ours[at]).is_none() {
                        _ = db.mir_body(ours[at].into());
                    } else if let (Ok(compiled), Some(told)) =
                        (compiled_root(db, ours[at]).as_ref(), told)
                    {
                        told.ahead(db, compiled);
                    }
                });
                cg.phases.compile += start.elapsed();
                // The other threads go on to what the roots call while these are put in the
                // object.
                cg.compile_ahead(ours, &mut |cg, ahead| {
                    while let Some(&root) = queue.get(next) {
                        next += 1;
                        let result = if denied.contains(&root) {
                            Err(CodegenError::Unsupported("left to rustc on request".to_owned()))
                        } else if left.contains(&root) {
                            let reason = "shares an `async` block with a function left to rustc";
                            Err(CodegenError::Unsupported(reason.to_owned()))
                        } else {
                            cg.define_function(root)
                        };
                        match result {
                            Ok(symbol) => compiled.push((root, symbol)),
                            Err(e) => {
                                // rustc's copy calls what ours would have.
                                if !on_demand.is_empty()
                                    && !e.no_symbol()
                                    && !cg.want_reachable_from(root)
                                {
                                    let krate = root.krate(db);
                                    let rest =
                                        roots.on_demand.iter().filter(|it| it.krate(db) == krate);
                                    cg.wanted.extend(rest);
                                }
                                skipped.push((root, e));
                            }
                        }
                        queue.extend(cg.wanted.drain(..).filter(|&func| queued.insert(func)));
                    }
                    failed.extend(cg.define_waves(ahead));
                });
                queue.extend(cg.wanted.drain(..).filter(|&func| queued.insert(func)));
                if next == queue.len() {
                    break;
                }
            }
            if cg.misfit {
                start_over("an entry of the store did not fit");
                rejected.extend(failed);
                continue;
            }
            // The `main` the C runtime calls would call a `main` that is not in the object.
            if start.is_some_and(|main| skipped.iter().any(|(root, _)| *root == main)) {
                start = None;
                rejected.extend(failed);
                continue;
            }
            let fallback_start = stdx::time::Instant::now();
            let from_rustc: FxHashSet<FunctionId> = left_to_rustc(db, symbols, &skipped).collect();
            // A function left to rustc keeps using rustc's copy of a private static, so we
            // can't bring our own.
            let mut split = false;
            let found = TheirStatics::of(db, from_rustc.iter().copied());
            split |= found.statics.iter().any(|it| cg.statics.contains_key(it));
            own_statics.theirs.extend(found.statics);
            let before = own_statics.unknown.len();
            own_statics.unknown.extend(found.unknown);
            if own_statics.unknown.len() != before {
                split |= cg.statics.keys().any(|&it| !own_statics.owns(db, it));
            }
            if split {
                start_over("a static is used by a function left to rustc");
                rejected.extend(failed);
                continue;
            }
            // The copy rustc makes of a function without a symbol, where it is used, handles
            // the futures the function does.
            let generic: Vec<FunctionId> = match used_by_rustc {
                true => skipped.iter().filter(|(_, e)| e.no_symbol()).map(|it| it.0).collect(),
                false => Vec::new(),
            };
            // An instance of ours can still poll a future rustc's code made: one of a generic
            // function that awaits a function left to rustc, or that rustc's copy of a
            // forwarded instance hands back.
            if failed.is_empty() && futures_kept_apart() {
                let before = futures.len();
                for root in from_rustc.iter().chain(&generic) {
                    futures.extend(async_blocks(db, *root).iter().flatten().copied());
                }
                for (instance, env) in &cg.envs {
                    if cg.forwarded.contains(instance) {
                        futures.extend(made_futures(db, instance, env));
                    }
                }
                if futures.len() != before {
                    start_over("a future rustc's code makes is polled by code of ours");
                    continue;
                }
            }
            if failed.is_empty()
                && futures_kept_apart()
                && (!from_rustc.is_empty() || !generic.is_empty() || !futures.is_empty())
            {
                let mut theirs = Some(futures.clone());
                for root in from_rustc.iter().chain(&generic) {
                    let blocks = async_blocks(db, *root).clone();
                    theirs = theirs.zip(blocks).map(|(mut all, blocks)| {
                        all.extend(blocks);
                        all
                    });
                }
                let before = left.len();
                left.extend(compiled.iter().map(|it| it.0).filter(|&root| {
                    match (async_blocks(db, root).clone(), &theirs) {
                        (Some(ours), Some(theirs)) => !ours.is_disjoint(theirs),
                        (Some(ours), None) => !ours.is_empty(),
                        (None, _) => true,
                    }
                }));
                if left.len() != before {
                    start_over("an `async` block is shared with a function left to rustc");
                    continue;
                }
            }
            if failed.is_empty() {
                let instances = cg.instances.len();
                if let Some(store) = store::Store::global() {
                    let mut misses: Vec<_> = cg.names.misses.take().into_iter().collect();
                    misses.sort_unstable_by_key(|it| std::cmp::Reverse(it.1));
                    let (mut kept, writer) = (std::mem::take(&mut cg.kept), cg.writer.take());
                    // The object and the link do not wait for the store to have what was
                    // kept: whoever reads the store next does.
                    store.settle_with(move || {
                        if let Some(mut writer) = writer {
                            kept.written(writer.finish());
                        }
                        store.flush();
                        let (found, added) = store.take_counts();
                        if found + added != 0 {
                            eprintln!("store: {found} functions found, {added} compiled and added");
                        }
                        kept.report();
                        if !misses.is_empty() {
                            eprintln!("without a stable name: {misses:?}");
                        }
                    });
                }
                let total = COMPILED.load(std::sync::atomic::Ordering::Relaxed);
                let compiled_now = total - std::mem::replace(counted, total);
                *stats = BuildStats {
                    bodies: cg.bodies,
                    reused: cg.bodies.saturating_sub(compiled_now),
                    compiled: compiled_now,
                    phases: Phases { fallback: fallback_start.elapsed(), discarded, ..cg.phases },
                    rounds,
                    patched: None,
                    declined,
                };
                if patching {
                    *base = patch::Base::record(db, &cg, roots.entries, &compiled, &skipped);
                }
                if let Some(path) = std::env::var_os("RA_JIT_LAYOUTS") {
                    let lines = cg
                        .layouts
                        .iter()
                        .map(|(ty, (size, align))| format!("{ty}\t{size}\t{align}\n"))
                        .collect::<String>();
                    // Appended: isolated builds of single functions go through here too.
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                        .and_then(|mut file| std::io::Write::write_all(&mut file, lines.as_bytes()))
                        .map_err(|e| CodegenError::Backend(format!("layout log: {e}")))?;
                }
                let start = stdx::time::Instant::now();
                let (object, apart) = cg.finish()?;
                stats.phases.object = start.elapsed();
                backend::Tally::global().report();
                (*theirs, *unknown) = (own_statics.theirs, own_statics.unknown);
                let patch = if patching { Some(patch::empty_object()?) } else { None };
                return Ok(CodegenUnit {
                    object: Some(object),
                    patch,
                    apart,
                    compiled,
                    skipped,
                    instances,
                });
            }
            let first = failed.first().map_or("", |it| it.1.why.as_str());
            start_over(&format!("{} instances did not compile, the first: {first}", failed.len()));
            cg.poison_callers(&mut failed);
            rejected.extend(failed);
        }
    }
}

/// Which statics we define ourselves rather than use rustc's. A function left to rustc uses
/// rustc's copy of a static, so the ones it refers to stay rustc's, along with whatever refers
/// to them.
#[derive(Clone)]
struct OwnStatics {
    crates: FxHashSet<Crate>,
    /// Those of `crates` that a crate of the program left to rustc depends on: its code refers
    /// to the statics rustc exports from them.
    shared: FxHashSet<Crate>,
    theirs: FxHashSet<StaticId>,
    /// Crates with a function left to rustc that refers to we don't know what. That can be
    /// any static of the crate but the ones declared in the body of another function.
    // FIXME: a closure that such a function hands out can refer to the statics of its body.
    unknown: FxHashSet<Crate>,
}

impl OwnStatics {
    fn new(
        db: &dyn HirDatabase,
        crates: FxHashSet<Crate>,
        theirs: FxHashSet<StaticId>,
        unknown: FxHashSet<Crate>,
        is_program: bool,
    ) -> OwnStatics {
        // With on-demand roots the build is of a program whose `main` is in `crates`, and only
        // what that depends on is linked. Without, anything may be linked on top.
        let program: FxHashSet<Crate> = if is_program {
            crates.iter().flat_map(|krate| krate.transitive_deps(db)).collect()
        } else {
            base_db::all_crates(db).iter().copied().collect()
        };
        let mut dependents = FxHashMap::<Crate, Vec<Crate>>::default();
        for &krate in &program {
            for dep in &krate.data(db).dependencies {
                dependents.entry(dep.crate_id).or_default().push(krate);
            }
        }
        let is_shared = |krate: &Crate| {
            let mut seen = FxHashSet::default();
            let mut stack = vec![*krate];
            while let Some(krate) = stack.pop() {
                for &dependent in dependents.get(&krate).into_iter().flatten() {
                    if !crates.contains(&dependent) {
                        return true;
                    }
                    if seen.insert(dependent) {
                        stack.push(dependent);
                    }
                }
            }
            false
        };
        let shared = crates.iter().copied().filter(is_shared).collect();
        OwnStatics { crates, shared, theirs, unknown }
    }

    fn owns(&self, db: &dyn HirDatabase, id: StaticId) -> bool {
        let module = id.module(db);
        self.crates.contains(&module.krate(db))
            && !self.theirs.contains(&id)
            && (module.is_block_module(db) || !self.unknown.contains(&module.krate(db)))
    }

    /// Whether our copy of the static stands in for the one rustc exports. rustc's is what the
    /// static was when rustc last built the crate, which an edit to its value leaves behind.
    // FIXME: `#[used]` and `#[link_section]` statics are found by the linker or a runtime
    // through rustc's copy and should stay rustc's too.
    fn replaces(&self, db: &dyn HirDatabase, id: StaticId) -> bool {
        self.owns(db, id)
            && !self.shared.contains(&id.krate(db))
            && !AttrFlags::query(db, id.into()).contains(AttrFlags::NO_MANGLE)
            && AttrFlags::export_name(db, id.into()).is_none()
    }
}

/// Instances already found impossible to compile, kept between builds so a warm build does
/// not rediscover them one restart at a time.
/// An instance that could not be compiled, and what it was compiled for.
#[derive(Clone)]
pub(in crate::mir) struct Rejected {
    env: StoredParamEnvAndCrate,
    why: String,
}

#[derive(Default, Clone)]
pub struct Rejections {
    rejected: FxHashMap<Instance, Rejected>,
    /// The statics that functions left to rustc refer to, and the crates where that is unknown.
    theirs: FxHashSet<StaticId>,
    unknown: FxHashSet<Crate>,
    /// Roots given up on because a function left to rustc handles the same future.
    left: FxHashSet<FunctionId>,
    /// The `async` blocks whose futures rustc's code makes: a poll or a drop of ours would read
    /// them as laid out by the other compiler.
    futures: FxHashSet<salsa::Id>,
    /// Whether to answer an edit with a patch for the object of the last full build.
    pub patching: bool,
    /// Whether rustc compiles a crate that uses the ones built, as it does the examples of a
    /// library: it then has code of its own for every function of theirs without a symbol.
    pub used_by_rustc: bool,
    base: Option<patch::Base>,
    frozen: patch::Frozen,
    stale: Vec<Stale>,
    /// How many functions had been compiled when the last build finished.
    counted: usize,
    pub stats: BuildStats,
    /// The functions at a level above 1, which a patch has at that level.
    pub tiers: tier::Tiers,
}

impl Rejections {
    fn compiled_since_last_build(&mut self) -> usize {
        let total = COMPILED.load(std::sync::atomic::Ordering::Relaxed);
        total - std::mem::replace(&mut self.counted, total)
    }

    /// Looks at one part in `parts` of what the last full build compiled for what an edit
    /// changed, which threads can do a part each. `None` when no patch is to be made.
    pub fn stale(&self, db: &dyn HirDatabase, part: usize, parts: usize) -> Option<Stale> {
        Some(self.base.as_ref()?.stale(db, part, parts))
    }

    /// Whether the next build is a patch of the last one. A build that is interrupted leaves
    /// nothing to patch.
    pub fn patches(&self) -> bool {
        self.base.is_some()
    }

    /// The function of the last full build that is defined under `symbol`, as the object has
    /// it (without the underscore Mach-O puts in front).
    pub fn tier_node(&self, symbol: &str) -> Option<tier::Node> {
        let base = self.base.as_ref()?;
        if let Some((root, _)) = base.compiled.iter().find(|(_, it)| it == symbol) {
            return Some(tier::Node(tier::Kind::Root(*root)));
        }
        if let Some((instance, known)) = base.instances.iter().find(|(_, it)| it.symbol == symbol) {
            return Some(tier::Node(tier::Kind::Instance(instance.clone(), known.env.clone())));
        }
        let (instance, env) = base.found_instances.get(symbol)?;
        Some(tier::Node(tier::Kind::Instance(instance.clone(), env.clone())))
    }

    /// The symbol `node` is defined under, as [`Rejections::tier_node`] takes it.
    pub fn tier_symbol(&self, node: &tier::Node) -> Option<String> {
        let base = self.base.as_ref()?;
        match &node.0 {
            tier::Kind::Root(root) => {
                base.compiled.iter().find(|(it, _)| it == root).map(|it| it.1.clone())
            }
            tier::Kind::Instance(instance, env) => {
                base.instances.get(instance).map(|it| it.symbol.clone()).or_else(|| {
                    let mut found = base.found_instances.iter();
                    found.find(|(_, it)| it.0 == *instance && it.1 == *env).map(|it| it.0.clone())
                })
            }
        }
    }

    /// Makes the next build compile the code of the checkout that the last one took from the
    /// store, which no patch can replace: built while nothing waits, the first edit after it
    /// is a patch. Whether the next build has that to do and no edit had asked for it.
    pub fn dehydrate(&mut self) -> bool {
        self.base.as_mut().is_some_and(|it| it.dehydrate())
    }

    /// Makes the next build a full one. A patch looks at everything the last build compiled,
    /// and what an edit took away can only be looked at where a panic can be caught.
    pub fn forget_base(&mut self) {
        self.base = None;
    }

    /// Notes that rustc compiled `crates` from other source than they are first built from
    /// here, so that what it compiled of them and of those that see them is not to be linked.
    pub fn distrust(&mut self, db: &dyn HirDatabase, crates: &[Crate]) {
        self.frozen.distrust(crates.iter().flat_map(|it| it.transitive_rev_deps(db)));
    }

    /// Notes that a file changed: one of `crates`, or with `None` one we can't place. Every
    /// change since the last build has to be noted before [`Rejections::stale`] looks.
    pub fn touch(&mut self, db: &dyn HirDatabase, crates: Option<&[Crate]>) {
        match crates {
            Some(crates) => {
                self.frozen.edit(crates.iter().flat_map(|it| it.transitive_rev_deps(db)))
            }
            None => self.frozen.edit(base_db::all_crates(db).iter().copied()),
        }
        if let Some(base) = &mut self.base {
            base.touch(db, crates);
        }
        self.forget();
    }

    /// What was found impossible to compile was found so in the source before the edit, and
    /// the edit can be what mends it.
    fn forget(&mut self) {
        self.rejected.clear();
        self.theirs.clear();
        self.unknown.clear();
        self.left.clear();
        self.futures.clear();
    }

    /// Notes that a file changed in the bodies of functions alone, none of which the rest of
    /// the program sees into: not `const` or `async`, not returning `impl Trait`, declaring no
    /// items, and not handed to a macro.
    pub fn touch_bodies(&mut self, db: &dyn HirDatabase, file: base_db::FileId) {
        self.frozen.edit_bodies(file);
        if let Some(base) = &mut self.base {
            base.touch_bodies(db, file);
        }
        self.forget();
    }

    /// A function the last build left to rustc that is not what rustc compiled, and why it was
    /// left. rustc's copies are those of the build the daemon started from: the first call, to
    /// be made after that build, notes what they are copies of.
    pub fn out_of_date(&mut self, db: &dyn HirDatabase) -> Option<(FunctionId, String)> {
        self.frozen.out_of_date(db, self.base.as_ref()?)
    }

    /// Hands the next build what [`Rejections::stale`] found, every part of it and with no
    /// edit in between.
    pub fn set_stale(&mut self, db: &dyn HirDatabase, mut stale: Vec<Stale>) {
        if let Some(base) = &mut self.base {
            base.settle(db, &mut stale);
        }
        self.stale = stale;
    }

    /// The longest any thread took over each step of looking for what is stale.
    pub fn stale_steps(&self) -> StaleSteps {
        let longest = |step: fn(&StaleSteps) -> std::time::Duration| {
            self.stale.iter().map(|it| step(&it.steps)).max().unwrap_or_default()
        };
        StaleSteps {
            roots: longest(|it| it.roots),
            instances: longest(|it| it.instances),
            statics: longest(|it| it.statics),
            left: longest(|it| it.left),
            roots_asked: self.stale.iter().map(|it| it.steps.roots_asked).sum(),
            instances_asked: self.stale.iter().map(|it| it.steps.instances_asked).sum(),
            slowest: longest(|it| it.slowest),
        }
    }
}

/// Measurements of the last build.
#[derive(Default, Clone, Copy)]
pub struct BuildStats {
    /// Compiled functions in the object.
    pub bodies: usize,
    /// How many of them were compiled before this build, and how many during it.
    pub reused: usize,
    pub compiled: usize,
    /// Where the last round of the build spent its time.
    pub phases: Phases,
    /// How many times stitching started over because a callee turned out not to compile.
    pub rounds: usize,
    /// How many functions the patch defines, when the build made one.
    pub patched: Option<usize>,
    /// Why the build wrote the whole object again, when a patch was tried.
    pub declined: Option<&'static str>,
}

/// Time spent in each step of the last round.
#[derive(Default, Clone, Copy)]
pub struct Phases {
    /// Asking for each compiled function: checking that what it was compiled from is unchanged,
    /// and compiling it when not.
    pub compile: std::time::Duration,
    /// Finding what each compiled function refers to in the object.
    pub resolve: std::time::Duration,
    /// Handing the code to the object.
    pub define: std::time::Duration,
    /// Writing the object file.
    pub object: std::time::Duration,
    /// Working out what the functions left to rustc keep the rest from compiling.
    pub fallback: std::time::Duration,
    /// The rounds before the last, whose object was not kept.
    pub discarded: std::time::Duration,
    /// Telling which statics are the program's own (of a patch).
    pub statics: std::time::Duration,
    /// Setting up the object to put the code in (of a patch).
    pub setup: std::time::Duration,
}

/// A function with concrete generic arguments that we compile ourselves.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Instance {
    def: InstanceDef,
    args: StoredGenericArgs,
    /// Whether the crate that compiles it has `-C overflow-checks`: the one that asks for it,
    /// until [`ObjectCodegen::settled`] says which crate rustc had compile it.
    checked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum InstanceDef {
    Fn(FunctionId),
    /// An `InternedClosureId`, stored without its database lifetime.
    Closure(salsa::Id),
    /// An `InternedCoroutineId`: the `Future::poll` of an `async` block. Polled with a null
    /// context, it drops whatever the block holds instead.
    Coroutine(salsa::Id),
    /// The same of the future an `async` closure gives when it is called by value though it
    /// could be called again: that future owns what the closure captured.
    CoroutineByMove(salsa::Id),
    /// `drop_in_place` for the type that is the instance's only generic argument.
    DropGlue,
    /// `FnOnce::call_once` in the vtable of the closure type that is the instance's only
    /// generic argument: calls the closure through the pointer it is handed, then drops it.
    CallOnceShim,
    /// A method that takes `self` by value, in a vtable: the slot is called with a pointer to
    /// the value, which the shim hands to the method as the method takes it.
    VtableShim(FunctionId),
    /// What a closure that captures nothing coerces to as a function pointer: calls the
    /// closure type that is the instance's only generic argument, which borrows a receiver,
    /// with a dangling one.
    FnPointerShim,
    /// A function to point at for the fn item or fn pointer type that is the instance's only
    /// generic argument, where what the type names has no address of its own: a constructor,
    /// or a trait method called on a `dyn`. With `receiver`, the function is what the
    /// `Fn`-trait vtable of the type holds, and so also takes the `self` it has no use for.
    ReifyShim {
        receiver: bool,
    },
}

/// What a call transfers control to.
enum Callee<'db> {
    Instance(InstanceDef, GenericArgs<'db>),
    /// With how many of the arguments are for named parameters, when the rest are for `...`.
    Pointer(Value, ExternAbi, Option<usize>),
}

/// Stitches compiled functions into one object: gives every function and data object they
/// ask for an id in the module, and defines the ones that are ours.
struct ObjectCodegen<'a, 'db> {
    db: &'db dyn HirDatabase,
    symbols: &'a dyn SymbolResolver,
    module: ObjectModule,
    unwind: UnwindContext,
    /// What a debugger is told about the functions, when the build tells one anything.
    debug: Option<debuginfo::Describer>,
    /// What those types are told as, and the ones the threads that compile told of.
    told: ToldTypes,
    /// Writes what is kept to the store, once there is something to keep.
    writer: Option<cached::Writer>,
    imports: FxHashMap<String, FuncId>,
    /// The functions of `asm!` blocks, by their assembly.
    assemblies: FxHashMap<String, FuncId>,
    instances: FxHashMap<Instance, FuncId>,
    pending: Vec<(Instance, StoredParamEnvAndCrate, FuncId)>,
    /// Every instance that went through `pending`.
    envs: Vec<(Instance, StoredParamEnvAndCrate)>,
    homes: home::Homes,
    /// What [`Self::compile_ahead`] has compiled or tried to.
    ahead: FxHashSet<(Instance, StoredParamEnvAndCrate)>,
    /// The instances that call rustc's copy instead of having code of their own.
    forwarded: FxHashSet<Instance>,
    /// [`Rejections::futures`] as the round started.
    rustc_futures: FxHashSet<salsa::Id>,
    /// Whether instances and statics get names another object can refer to.
    patching: bool,
    /// The object this one is a patch for, and what of it is no longer what the source says.
    base: Option<&'a patch::Base>,
    replaced: patch::Unchanged,
    /// The statics given a value here, in the order their values were asked for.
    own_static_order: Vec<StaticId>,
    /// Functions compiled only once something refers to them, and the ones referred to since
    /// the list was last taken.
    on_demand: &'a FxHashSet<FunctionId>,
    wanted: Vec<FunctionId>,
    on_demand_crates: FxHashSet<Crate>,
    /// Instances that failed in an earlier round, with the reason.
    rejected: &'a FxHashMap<Instance, Rejected>,
    /// Size and alignment of every type laid out, collected when `RA_JIT_LAYOUTS` names a file
    /// to write them to. The harness compares them with rustc's.
    layouts: FxHashMap<String, (u64, u64)>,
    phases: Phases,
    /// One line per function for `harness/shapes.py`, when `RA_JIT_SHAPES` is set.
    shapes: Option<std::io::BufWriter<std::fs::File>>,
    /// How many compiled functions went into the object.
    bodies: usize,
    vtables: FxHashMap<(StoredTy, Option<(TraitId, StoredGenericArgs)>), DataId>,
    own_statics: OwnStatics,
    /// The crates of the program, when they do not all check overflow alike.
    mixed: Option<settled::Mixed>,
    /// What `finish` adds to the object of the relocations of the functions defined.
    elf_relocs: Vec<ElfReloc>,
    statics: FxHashMap<StaticId, DataId>,
    /// Statics nothing exports and nothing can write to, which we give a copy of the value.
    frozen_statics: FxHashMap<StaticId, DataId>,
    imported_statics: FxHashMap<StaticId, DataId>,
    consts: FxHashMap<ConstBlob, DataId>,
    source_paths: FxHashMap<base_db::FileId, Option<String>>,
    builder_ctx: FunctionBuilderContext,
    location_counter: u32,
    names: stable::StableNames<'a, 'db>,
    /// Whether the code of the checkout's crates can come from the store.
    hydrate: bool,
    stable_names: FxHashMap<Instance, Option<StableName>>,
    kept: Kept,
    /// The entries of the store to build from in place of compiling, if any.
    entries: Option<std::sync::Arc<cached::Entries>>,
    /// The functions of this object by their stable names.
    named: FxHashMap<StableName, FuncId>,
    /// The ones declared for an entry, and those of them yet to be put in the object.
    found: Vec<(StableName, FuncId)>,
    found_pending: Vec<(StableName, FuncId)>,
    named_vtables: FxHashMap<String, DataId>,
    /// The `TypeId::of` of each hash, and back.
    type_id_shims: FxHashMap<u128, FuncId>,
    type_ids: FxHashMap<FuncId, u128>,
    /// An entry did not fit the object, which now lacks a function it declares.
    misfit: bool,
    /// The functions of a patch that are at a level above 1, whose code it has at that level.
    tiers: Option<&'a tier::Tiers>,
    /// What each function built from an entry of the store is an instance of.
    found_instances: FxHashMap<FuncId, (Instance, StoredParamEnvAndCrate)>,
}

/// Why an entry with a file of its own under what stands for cargo's directory is not kept.
const OF_CARGO: &str = "a file under what stands for cargo's directory";

/// What became of the instances an object was built from, as entries of the store.
#[derive(Default)]
struct Kept {
    added: usize,
    found: usize,
    /// Instances that name something an edit can reach.
    unnamed: usize,
    /// Instances that refer to something an entry has no name for.
    unnamed_requests: usize,
    /// Entries of the store that differ from what was compiled, with `RA_JIT_CACHE_CHECK`.
    differing: usize,
    same: usize,
    /// Why instances with a name were not added.
    unkept: FxHashMap<&'static str, usize>,
    /// Why instances with a name were compiled.
    unfound: FxHashMap<&'static str, usize>,
}

impl Kept {
    fn written(&mut self, written: cached::Written) {
        self.added += written.added;
        if written.of_cargo != 0 {
            *self.unkept.entry(OF_CARGO).or_default() += written.of_cargo;
        }
    }

    fn unfound<T>(&mut self, why: &'static str) -> Option<T> {
        *self.unfound.entry(why).or_default() += 1;
        None
    }

    fn unkept<T>(&mut self, why: &'static str) -> Option<T> {
        *self.unkept.entry(why).or_default() += 1;
        None
    }

    fn report(&self) {
        let Kept { added, found, unnamed, unnamed_requests, differing, same, unkept, unfound } =
            self;
        if added + found + unnamed + unnamed_requests + differing + same != 0 {
            eprintln!(
                "entries: {found} found, {added} added, {same} as stored, {differing} not as stored, \
                 {unnamed} instances and {unnamed_requests} requests without a stable name"
            );
        }
        if !unkept.is_empty() {
            let mut unkept: Vec<_> = unkept.iter().collect();
            unkept.sort_unstable_by_key(|it| std::cmp::Reverse(it.1));
            eprintln!("not kept: {unkept:?}");
        }
        if !unfound.is_empty() {
            let mut unfound: Vec<_> = unfound.iter().collect();
            unfound.sort_unstable_by_key(|it| std::cmp::Reverse(it.1));
            eprintln!("not found: {unfound:?}");
        }
    }
}

/// The ids in a module of what a compiled function or static asked for.
struct Resolved {
    funcs: Vec<FuncId>,
    datas: Vec<DataId>,
}

impl Resolved {
    fn blob(&self, blob: &ConstBlob) -> ConstBlob {
        ConstBlob {
            bytes: blob.bytes.clone(),
            relocs: blob
                .relocs
                .iter()
                .map(|&(offset, target, addend)| {
                    (offset, self.datas[target.as_u32() as usize], addend)
                })
                .collect(),
            fn_relocs: blob
                .fn_relocs
                .iter()
                .map(|&(offset, target)| (offset, self.funcs[target.as_u32() as usize]))
                .collect(),
            align: blob.align,
        }
    }

    fn reloc(&self, reloc: &ModuleReloc, own: FuncId) -> ModuleReloc {
        let name = match reloc.name {
            ModuleRelocTarget::User { namespace: 0, index } => {
                ModuleRelocTarget::user(0, self.funcs[index as usize].as_u32())
            }
            ModuleRelocTarget::User { index, .. } => {
                ModuleRelocTarget::user(1, self.datas[index as usize].as_u32())
            }
            ModuleRelocTarget::FunctionOffset(_, offset) => {
                ModuleRelocTarget::FunctionOffset(own, offset)
            }
            ref other => other.clone(),
        };
        ModuleReloc { name, ..reloc.clone() }
    }
}

/// The code generator of the one [`Target`] the process compiles for.
fn target() -> Result<&'static isa::OwnedTargetIsa> {
    static TARGET: std::sync::OnceLock<std::result::Result<isa::OwnedTargetIsa, String>> =
        std::sync::OnceLock::new();
    TARGET
        .get_or_init(|| Target::selected()?.isa(regalloc(), false))
        .as_ref()
        .map_err(|e| CodegenError::Backend(e.clone()))
}

/// The register allocator of a function that is not over [`SINGLE_PASS`]: `RA_JIT_REGALLOC` names one of
/// Cranelift's, `backtracking` (the default) or `single_pass`. The second allocates in one
/// walk over the code: it took a fifth off Cranelift's time on cold builds of ripgrep and of
/// just, and none off their wall time, and its code was twice the size and up to 1.8 times
/// slower (the demo's Life step). Code in the store is kept whichever made it, so compare the
/// two with a store of their own each.
fn regalloc() -> &'static str {
    static REGALLOC: LazyLock<&'static str> =
        LazyLock::new(|| match std::env::var("RA_JIT_REGALLOC").as_deref() {
            Ok("single_pass") => "single_pass",
            _ => "backtracking",
        });
    *REGALLOC
}

/// The same target for a function of more than [`SINGLE_PASS`] instructions. The usual
/// register allocator takes more than linear time: ten seconds for one of 150 000
/// instructions, which a whole build then waits for.
fn target_for_large() -> Result<&'static isa::OwnedTargetIsa> {
    static TARGET: std::sync::OnceLock<std::result::Result<isa::OwnedTargetIsa, String>> =
        std::sync::OnceLock::new();
    TARGET
        .get_or_init(|| Target::selected()?.isa("single_pass", false))
        .as_ref()
        .map_err(|e| CodegenError::Backend(e.clone()))
}

/// The same target for a function that asks for its frame pointer at its start, as one
/// that takes `...` does to find the arguments on the stack: Cranelift answers only where
/// every function keeps a frame pointer, which the others have no use for.
fn target_with_frame_pointer() -> Result<&'static isa::OwnedTargetIsa> {
    static TARGET: std::sync::OnceLock<std::result::Result<isa::OwnedTargetIsa, String>> =
        std::sync::OnceLock::new();
    TARGET
        .get_or_init(|| Target::selected()?.isa(regalloc(), true))
        .as_ref()
        .map_err(|e| CodegenError::Backend(e.clone()))
}

/// The most instructions a function has that LLVM optimizes.
const LARGE: usize = 50_000;

/// The most instructions a function has that Cranelift gives its usual register allocator,
/// which spends 1 to 2 µs an instruction on one this large and more on larger ones. On
/// dev-aws-6 nushell's `main`, of 35 780 instructions, took 75 ms of Cranelift with it and
/// 47 ms with the single-pass one, on every edit to it; the largest functions of a cold build
/// of nushell took 80 to 220 ms each. The single-pass allocator's code is larger and slower,
/// which a function this large, a `main` or a big `match` run once, rarely pays for.
const SINGLE_PASS: usize = 20_000;

/// A relocation of a function that Cranelift has no kind for, which the object is given
/// once Cranelift has written it: see [`Code::elf_types`].
struct ElfReloc {
    func: FuncId,
    reloc: ModuleReloc,
    r_type: u32,
}

impl ElfReloc {
    fn add(
        &self,
        product: &mut cranelift_object::ObjectProduct,
    ) -> std::result::Result<(), String> {
        use cranelift_object::object::{self, write};
        if product.object.format() != object::BinaryFormat::Elf {
            return Err("a relocation by its ELF type in an object that is not ELF".to_owned());
        }
        let symbol = match self.reloc.name {
            ModuleRelocTarget::User { namespace: 0, index } => {
                product.function_symbol(FuncId::from_u32(index))
            }
            ModuleRelocTarget::User { namespace: 1, index } => {
                product.data_symbol(DataId::from_u32(index))
            }
            ref other => return Err(format!("a relocation by its ELF type against {other}")),
        };
        let own = product.object.symbol(product.function_symbol(self.func));
        let write::SymbolSection::Section(section) = own.section else {
            return Err("a function that is not in a section".to_owned());
        };
        let relocation = write::Relocation {
            offset: own.value + u64::from(self.reloc.offset),
            symbol,
            addend: self.reloc.addend,
            flags: object::RelocationFlags::Elf {
                r_type: object::elf::RelocationType(self.r_type),
            },
        };
        product.object.add_relocation(section, relocation).map_err(|e| e.to_string())
    }
}

fn selected_target() -> Result<&'static Target> {
    Target::selected().map_err(CodegenError::Backend)
}

/// How many functions went through Cranelift since the process started.
static COMPILED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// What tells compiled code from any other: equal for the same code however often an edit
/// went away from it and came back, so that a patch holds what differs from its base alone.
fn print_of(code: &impl std::hash::Hash) -> u128 {
    let mut hasher = rustc_stable_hash::StableSipHasher128::new();
    code.hash(&mut hasher);
    let type_id::Fingerprint(low, high) = hasher.finish();
    u128::from(low) | u128::from(high) << 64
}

/// The print of the function's code. The database runs this again only when compiling gave
/// something else, not each time it compiled.
#[salsa::tracked(returns(copy))]
fn root_serial(db: &dyn HirDatabase, func: FunctionId) -> u128 {
    compiled_root(db, func).as_ref().map_or(0, print_of)
}

#[salsa::tracked(returns(copy))]
fn instance_serial(db: &dyn HirDatabase, instance: Instance, env: StoredParamEnvAndCrate) -> u128 {
    compiled_instance(db, instance, env).as_ref().map_or(0, print_of)
}

#[salsa::tracked(returns(copy))]
fn static_serial(db: &dyn HirDatabase, id: StaticId) -> u128 {
    compiled_static(db, id).as_ref().map_or(0, print_of)
}

/// What the threads of [`compile_unnamed`] have left to compile.
#[derive(Default)]
pub struct Unnamed {
    state: std::sync::Mutex<UnnamedState>,
    told: ToldTypes,
}

/// The types [`compile_unnamed`] told of, for the build that comes next.
// FIXME: a build after an edit must not find these.
static TOLD_UNNAMED: std::sync::Mutex<Option<ToldTypes>> = std::sync::Mutex::new(None);

#[derive(Default)]
struct UnnamedState {
    roots: Vec<FunctionId>,
    queue: Vec<(Instance, StoredParamEnvAndCrate)>,
    seen: FxHashSet<(Instance, StoredParamEnvAndCrate)>,
    working: usize,
    compiled: usize,
}

impl Unnamed {
    pub fn new(roots: Vec<FunctionId>) -> Unnamed {
        let state = UnnamedState { roots, ..Default::default() };
        let told = ToldTypes::default();
        *TOLD_UNNAMED.lock().unwrap_or_else(|e| e.into_inner()) = Some(told.clone());
        Unnamed { state: std::sync::Mutex::new(state), told }
    }

    /// How many functions and instances were compiled.
    pub fn compiled(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).compiled
    }
}

/// Compiles the functions `unnamed` starts from and the generic instances they reach, until
/// `done`, as one of the threads that do. Which symbols rustc built is not known yet: the
/// code of a function is the same whichever they are, and a build that knows them finds it
/// compiled. A call of a function without generic arguments is not followed, since rustc
/// may have built the one called.
pub fn compile_unnamed(db: &dyn HirDatabase, unnamed: &Unnamed, done: &dyn Fn() -> bool) {
    let mut homes = home::Homes::default();
    loop {
        let job = {
            let mut state = unnamed.state.lock().unwrap_or_else(|e| e.into_inner());
            let job = match state.roots.pop() {
                Some(root) => Some(either::Either::Left(root)),
                None => state.queue.pop().map(either::Either::Right),
            };
            if job.is_none() && state.working == 0 {
                return;
            }
            state.working += usize::from(job.is_some());
            job
        };
        if done() {
            return;
        }
        let Some(job) = job else {
            std::thread::sleep(std::time::Duration::from_millis(1));
            continue;
        };
        let compiled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match &job {
            either::Either::Left(root) => {
                root_rows(db, *root);
                compiled_root(db, *root).as_ref().ok()
            }
            either::Either::Right((instance, env)) => {
                instance_rows(db, instance.clone(), env.clone());
                compiled_instance(db, instance.clone(), env.clone()).as_ref().ok()
            }
        }));
        let mut found = Vec::new();
        if let Ok(Some(compiled)) = compiled {
            unnamed.told.ahead(db, compiled);
        }
        for request in compiled.iter().flatten().flat_map(|it| &it.requests.funcs) {
            let FuncTarget::Instance(callee, asked) = &request.target else { continue };
            let named = matches!(callee.def, InstanceDef::Fn(_))
                && callee.args.as_ref().iter().all(|it| it.region().is_some());
            if !named {
                found.push((callee.clone(), homes.home(db, callee, asked)));
            }
        }
        let mut state = unnamed.state.lock().unwrap_or_else(|e| e.into_inner());
        state.working -= 1;
        state.compiled += 1;
        for it in found {
            if state.seen.insert(it.clone()) {
                state.queue.push(it);
            }
        }
    }
}

/// Where the code from an offset on is written: the file, the line and the column.
type Row = (u32, base_db::FileId, u32, u32);

/// The rows of the code of a non-generic function, in the order of [`Code::lines`].
#[salsa::tracked(returns(ref))]
fn root_rows(db: &dyn HirDatabase, func: FunctionId) -> Vec<Row> {
    compiled_root(db, func).as_ref().map(|it| rows_of(db, it)).unwrap_or_default()
}

/// The rows of the code of an instance, in the order of [`Code::lines`].
#[salsa::tracked(returns(ref))]
fn instance_rows(
    db: &dyn HirDatabase,
    instance: Instance,
    env: StoredParamEnvAndCrate,
) -> Vec<Row> {
    compiled_instance(db, instance, env).as_ref().map(|it| rows_of(db, it)).unwrap_or_default()
}

fn rows_of(db: &dyn HirDatabase, compiled: &CompiledFunction) -> Vec<Row> {
    let Some((owner, spans)) = &compiled.source else { return Vec::new() };
    let rows = compiled.code.lines.iter().filter_map(|row| {
        let span = *spans.get(row.span as usize)?;
        let (file, line, column) = row_location(db, *owner, compiled.closure, span)?;
        Some((row.offset, file, line, column))
    });
    rows.collect()
}

/// The code of a non-generic function.
#[salsa::tracked(returns(ref))]
fn compiled_root<'db>(db: &'db dyn HirDatabase, func: FunctionId) -> Result<CompiledFunction> {
    let _span = tracing::info_span!(target: "ra_jit", "compiled_root").entered();
    tier::plain(|| root_code(db, func, false)).map(Produced::code)
}

/// The code of a non-generic function: Cranelift's, or, for a compile at a level above 1
/// (`tier`), its IR for LLVM.
fn root_code(db: &dyn HirDatabase, func: FunctionId, tier: bool) -> Result<Produced> {
    let (body, env) = root_body(db, func)?;
    if std::env::var("RA_JIT_DUMP_MIR")
        .is_ok_and(|want| FunctionSignature::of(db, func).name.as_str() == want)
    {
        dump_mir(body);
    }
    let signature = FunctionSignature::of(db, func);
    let (abi, variadic) = (signature.abi, signature.is_varargs());
    let args = GenericArgs::new_from_slice(&[]).store();
    let checked = crate_checks_overflow(db, func.krate(db)).unwrap_or(false);
    compile_as(db, env, Some(body), track_caller(db, func), checked, tier, |fx| {
        fx.abi = abi;
        fx.variadic = variadic;
        fx.args = Some(args);
        fx.codegen()
    })
}

/// Whether the crate is built with `-C overflow-checks`, when cargo builds it. The standard
/// library is built already, without.
fn crate_checks_overflow(db: &dyn HirDatabase, krate: Crate) -> Option<bool> {
    if krate.data(db).origin.is_lang() {
        return None;
    }
    let flag = cfg::CfgAtom::Flag(intern::sym::overflow_checks);
    Some(krate.cfg_options(db).check_atom(&flag))
}

/// Whether the crate `body` is written in keeps the overflow checks in what it keeps of the
/// body: it does when it has `-C overflow-checks`, or when it is not a crate of the build and
/// the body takes the setting of its callers. Whether the checks then run is up to the crate
/// that compiles the body.
fn body_written_checked(db: &dyn HirDatabase, body: &MirBody<'_>) -> bool {
    match crate_checks_overflow(db, body.owner.module(db).krate(db)) {
        Some(written_with) => written_with,
        None => body.inherits_overflow_checks,
    }
}

/// Whether arithmetic of `body` panics when it overflows in a constant, where it is known:
/// it is not for a body that takes the setting of whichever crate uses it.
pub(crate) fn body_checks_overflow(db: &dyn HirDatabase, body: &MirBody<'_>) -> Option<bool> {
    if !body_written_checked(db, body) {
        return Some(false);
    }
    crate_checks_overflow(db, body.owner.module(db).krate(db))
}

/// The `main` the C runtime calls, for a program whose `main` is `main`.
#[salsa::tracked(returns(ref))]
fn compiled_start<'db>(db: &'db dyn HirDatabase, main: FunctionId) -> Result<CompiledFunction> {
    let krate = main.krate(db);
    let env = ParamEnvAndCrate { param_env: crate::next_solver::ParamEnv::empty(), krate };
    let checked = crate_checks_overflow(db, krate).unwrap_or(false);
    compile(db, env, None, false, checked, |fx| fx.start_shim(main))
}

/// The code of a function, closure, `async` block or shim with its generic arguments filled in.
#[salsa::tracked(returns(ref))]
fn compiled_instance<'db>(
    db: &'db dyn HirDatabase,
    instance: Instance,
    env: StoredParamEnvAndCrate,
) -> Result<CompiledFunction> {
    let _span = tracing::info_span!(target: "ra_jit", "compiled_instance").entered();
    tier::plain(|| instance_code(db, instance, env, false)).map(Produced::code)
}

/// The code of an instance: Cranelift's, or, for a compile at a level above 1 (`tier`), its
/// IR for LLVM, which only a function, closure or `async` block of the source is compiled at.
fn instance_code(
    db: &dyn HirDatabase,
    instance: Instance,
    env: StoredParamEnvAndCrate,
    tier: bool,
) -> Result<Produced> {
    let stored_env = env.clone();
    let env = env.as_ref(db);
    let ty = || instance.args.as_ref().type_at(0);
    if tier
        && !matches!(
            instance.def,
            InstanceDef::Fn(_)
                | InstanceDef::Closure(_)
                | InstanceDef::Coroutine(_)
                | InstanceDef::CoroutineByMove(_)
        )
    {
        unsupported!("a shim stays at level 1");
    }
    let body = match instance.def {
        InstanceDef::DropGlue => {
            return compile_as(db, env, None, false, instance.checked, false, |fx| {
                let ty = ty();
                fx.drop_glue(ty)?;
                if std::env::var_os("RA_JIT_DUMP_CLIF").is_some() {
                    eprintln!("drop glue for {ty:?}\n{}", fx.builder.func.display());
                }
                Ok(())
            });
        }
        InstanceDef::CallOnceShim => {
            let args = instance.args.as_ref();
            return compile_as(db, env, None, false, instance.checked, false, |fx| {
                match args.len() {
                    1 => fx.call_once_shim(ty()),
                    _ => fx.call_once_by_pointer(args),
                }
            });
        }
        InstanceDef::VtableShim(func) => {
            let args = instance.args.as_ref();
            return compile_as(db, env, None, false, instance.checked, false, |fx| {
                fx.vtable_shim(func, args)
            });
        }
        InstanceDef::FnPointerShim => {
            return compile_as(db, env, None, false, instance.checked, false, |fx| {
                fx.fn_pointer_shim(ty())
            });
        }
        InstanceDef::ReifyShim { receiver } => {
            return compile_as(db, env, None, false, instance.checked, false, |fx| {
                fx.reify_shim(ty(), receiver)
            });
        }
        InstanceDef::Fn(_)
        | InstanceDef::Closure(_)
        | InstanceDef::Coroutine(_)
        | InstanceDef::CoroutineByMove(_) => instance_body(db, &instance, stored_env)?,
    };
    let body = &*body;
    let dump = std::env::var("RA_JIT_DUMP_MIR").ok();
    let (tracked, abi, variadic) = match instance.def {
        InstanceDef::Fn(func) => {
            let signature = FunctionSignature::of(db, func);
            if dump.as_deref() == Some(signature.name.as_str()) {
                dump_mir(body);
            }
            (track_caller(db, func), signature.abi, signature.is_varargs())
        }
        _ => {
            if dump.as_deref() == Some("{closure}") {
                dump_mir(body);
            }
            (false, ExternAbi::Rust, false)
        }
    };
    let args = instance.args.clone();
    let closure = match instance.def {
        InstanceDef::Closure(id) => Some(InternedClosureId::from_id(id).loc(db).expr),
        InstanceDef::Coroutine(id) | InstanceDef::CoroutineByMove(id) => {
            Some(InternedCoroutineId::from_id(id).loc(db).expr)
        }
        _ => None,
    };
    let compiled = compile_as(db, env, Some(body), tracked, instance.checked, tier, |fx| {
        fx.abi = abi;
        fx.variadic = variadic;
        fx.args = Some(args);
        fx.codegen()
    })?;
    Ok(match compiled {
        Produced::Code(compiled) => Produced::Code(CompiledFunction { closure, ..compiled }),
        Produced::Pending(pending) => Produced::Pending(Pending { closure, ..pending }),
    })
}

/// How many of the argument registers of a C convention the named parameters of a variadic
/// function take. What is left of them holds the first of the variable arguments. The
/// conventions a function that takes `...` is compiled for are x86-64's System V and
/// AAPCS64 as Linux has it.
#[derive(Clone, Copy)]
struct VariadicRegisters {
    arch: Arch,
    ints: u32,
    floats: u32,
    /// How many bytes of the caller's stack the named parameters take, which the variable
    /// arguments there come after.
    stack: u32,
}

impl VariadicRegisters {
    const FLOATS: u32 = 8;
    /// What `va_arg` expects a saved vector register to take.
    const FLOAT_BYTES: u32 = 16;

    /// How many integer registers hold arguments.
    fn int_count(arch: Arch) -> u32 {
        if arch == Arch::Aarch64 { 8 } else { 6 }
    }

    fn named(sig: &Signature, arch: Arch) -> Result<Self> {
        let mut named = VariadicRegisters { arch, ints: 0, floats: 0, stack: 0 };
        if arch == Arch::Aarch64 {
            return named.aapcs64(sig);
        }
        for param in &sig.params {
            match param.value_type {
                // AAPCS64 has a register of its own for where the result goes.
                _ if param.purpose == ArgumentPurpose::StructReturn && arch == Arch::Aarch64 => {}
                _ if param.purpose != ArgumentPurpose::Normal
                    && param.purpose != ArgumentPurpose::StructReturn =>
                {
                    unsupported!("a variadic function with a named parameter on the stack")
                }
                ty if ty.is_int() && ty.bytes() <= 8 => named.ints += 1,
                types::F32 | types::F64 => named.floats += 1,
                ty => unsupported!("a variadic function with a named {ty}"),
            }
        }
        if named.ints > Self::int_count(arch) || named.floats > Self::FLOATS {
            unsupported!("a variadic function with a named parameter on the stack");
        }
        Ok(named)
    }

    /// Where AAPCS64, as Cranelift has it, puts the named parameters: in the next register
    /// of their kind, or on the stack when those are out, in eight bytes each and a 128-bit
    /// integer in sixteen at a multiple of sixteen.
    fn aapcs64(mut self, sig: &Signature) -> Result<Self> {
        let each = Self::int_count(self.arch);
        for param in &sig.params {
            let on_stack = match param.value_type {
                // The address of what is returned has a register of its own.
                _ if param.purpose == ArgumentPurpose::StructReturn => continue,
                _ if param.purpose != ArgumentPurpose::Normal => {
                    unsupported!("a variadic function with a named aggregate on the stack")
                }
                types::I128 => {
                    let first = self.ints + self.ints % 2;
                    if first + 2 <= each {
                        self.ints = first + 2;
                        continue;
                    }
                    16
                }
                ty if ty.is_int() && self.ints < each => {
                    self.ints += 1;
                    continue;
                }
                types::F32 | types::F64 if self.floats < Self::FLOATS => {
                    self.floats += 1;
                    continue;
                }
                ty if ty.is_int() || ty.is_float() && ty.bytes() <= 8 => 8,
                ty => unsupported!("a variadic function with a named {ty}"),
            };
            self.stack = self.stack.next_multiple_of(on_stack) + on_stack;
        }
        Ok(self)
    }

    /// Adds the registers the named parameters left to `sig`, so that the function is given
    /// all of them whatever it was called with, and on x86-64 the stack past them. Cranelift
    /// has no such parameter for AArch64, where the frame pointer finds the stack.
    fn rest(self, sig: &mut Signature) {
        let ints = Self::int_count(self.arch);
        sig.params.extend((self.ints..ints).map(|_| AbiParam::new(types::I64)));
        sig.params.extend((self.floats..Self::FLOATS).map(|_| AbiParam::new(types::F64)));
        if self.arch == Arch::X86_64 {
            sig.params.push(AbiParam::special(ptr(), ArgumentPurpose::StructArgument(8)));
        }
    }
}

/// How many of the four argument slots of Windows x64 the named parameters of a variadic
/// function take. An argument has the slot of its position whatever its type, the caller
/// leaves room above the return address for the four that come in registers, and the ones
/// after them follow that room on the stack: written there, the registers and the stack are
/// the one run of 8-byte slots a `va_list` walks.
#[derive(Clone, Copy)]
struct HomedRegisters {
    named: u32,
}

impl HomedRegisters {
    const SLOTS: u32 = 4;

    fn named(sig: &Signature) -> Result<Self> {
        for param in &sig.params {
            let plain =
                matches!(param.purpose, ArgumentPurpose::Normal | ArgumentPurpose::StructReturn);
            if !plain || param.value_type.bytes() > 8 {
                unsupported!("a variadic function with a named {}", param.value_type);
            }
        }
        match sig.params.len() as u32 {
            // The first slot is where the exception of a guarded call arrives.
            0 => unsupported!("a variadic function without a named parameter"),
            named => Ok(HomedRegisters { named }),
        }
    }

    /// The registers the named parameters left.
    fn left(self) -> u32 {
        Self::SLOTS.saturating_sub(self.named)
    }

    /// Adds the registers the named parameters left, as the integers a caller passes a
    /// variable argument in, and the stack past the room for the four, to `sig`.
    fn rest(self, sig: &mut Signature) {
        sig.params.extend((0..self.left()).map(|_| AbiParam::new(types::I64)));
        sig.params.push(AbiParam::special(ptr(), ArgumentPurpose::StructArgument(8)));
    }
}

/// How the variable arguments of a function defined here reach it.
#[derive(Clone, Copy)]
enum Variadic {
    SystemV(VariadicRegisters),
    Windows(HomedRegisters),
    /// All of them on the stack, after the `named` bytes of it that the named parameters
    /// take.
    Apple {
        named: u32,
    },
}

impl Variadic {
    /// For a function whose named parameters are `sig`. `None` on a target a function that
    /// takes `...` is not compiled for.
    fn of(target: &Target, sig: &Signature) -> Result<Option<Self>> {
        Ok(match target.variable_arguments() {
            Some(VariableArguments::Registers) => {
                Some(Variadic::SystemV(VariadicRegisters::named(sig, target.arch)?))
            }
            Some(VariableArguments::Homed) => Some(Variadic::Windows(HomedRegisters::named(sig)?)),
            Some(VariableArguments::Stack) => {
                Some(Variadic::Apple { named: Self::apple_stack(sig)? })
            }
            None => None,
        })
    }

    fn rest(self, sig: &mut Signature) {
        match self {
            Variadic::SystemV(named) => named.rest(sig),
            Variadic::Windows(named) => named.rest(sig),
            Variadic::Apple { .. } => (),
        }
    }

    /// How many bytes of stack the named parameters of `sig` take under Apple's AArch64
    /// convention: the ones the eight registers of their kind do not hold, each at a
    /// multiple of its own size. The variable arguments start at the next multiple of eight.
    fn apple_stack(sig: &Signature) -> Result<u32> {
        const EACH: u32 = 8;
        let (mut ints, mut floats, mut stack) = (0, 0, 0u32);
        for param in &sig.params {
            let ty = param.value_type;
            let used = match param.purpose {
                // The address of what is returned has a register of its own.
                ArgumentPurpose::StructReturn => continue,
                ArgumentPurpose::Normal if ty.is_int() => &mut ints,
                ArgumentPurpose::Normal if ty == types::F32 || ty == types::F64 => &mut floats,
                _ => unsupported!("a variadic function with a named {ty}"),
            };
            // A 128-bit integer is in two registers or in none.
            let registers = ty.bytes().div_ceil(8);
            if *used + registers <= EACH {
                *used += registers;
            } else {
                // And nothing after it is in a register of that kind.
                *used = EACH;
                stack = stack.next_multiple_of(ty.bytes()) + ty.bytes();
            }
        }
        Ok(stack.next_multiple_of(8))
    }
}

/// The MIR optimization level the environment asks for with `RA_JIT_MIR_OPT`: 0, the default,
/// compiles the runtime body; 1 and 2 that body after rustc's MIR passes at its
/// `-Zmir-opt-level` 1 and 2 (`crate::mir::MirOptLevel`).
pub fn mir_opt_level() -> MirOptLevel {
    static LEVEL: LazyLock<MirOptLevel> = LazyLock::new(|| {
        let Some(asked) = std::env::var_os(MIR_OPT) else { return MirOptLevel::Off };
        let level =
            asked.to_str().and_then(|it| it.parse().ok()).and_then(MirOptLevel::from_number);
        level.unwrap_or_else(|| {
            eprintln!("ra-jit: {MIR_OPT}={}: expected 0, 1 or 2; using 0", asked.display());
            MirOptLevel::Off
        })
    });
    *LEVEL
}

pub const MIR_OPT: &str = "RA_JIT_MIR_OPT";

/// The body the code generator compiles of `def`: the runtime body, optimized at
/// [`mir_opt_level`].
fn compiled_mir_body<'db>(
    db: &'db dyn HirDatabase,
    def: InferBodyId<'db>,
) -> std::result::Result<&'db MirBody<'db>, crate::mir::MirLowerError<'db>> {
    match mir_opt_level() {
        MirOptLevel::Off => db.runtime_mir_body(def),
        level => db.optimized_mir_body(def, level),
    }
}

fn compiled_mir_body_for_closure<'db>(
    db: &'db dyn HirDatabase,
    closure: InternedClosureId<'db>,
) -> std::result::Result<&'db MirBody<'db>, crate::mir::MirLowerError<'db>> {
    match mir_opt_level() {
        MirOptLevel::Off => db.runtime_mir_body_for_closure(closure),
        level => db.optimized_mir_body_for_closure(closure, level),
    }
}

/// The body of an instance that is not a shim, with the instance's types in place of the
/// generic parameters. The database does not keep it unless `RA_JIT_LEAN=0`: nothing but
/// compiling the instance reads it, and what was compiled is kept.
fn instance_body<'db>(
    db: &'db dyn HirDatabase,
    instance: &Instance,
    env: StoredParamEnvAndCrate,
) -> Result<Cow<'db, MirBody<'db>>> {
    static LEAN: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("RA_JIT_LEAN").is_none_or(|it| it != "0"));
    let args = instance.args.clone();
    let tally = backend::Tally::global();
    let filled = |body, args: &StoredGenericArgs, env: &StoredParamEnvAndCrate| {
        tally.step(Step::Monomorphize, || monomorphized(db, body, args, env).map(Cow::Owned))
    };
    // The database keeps a monomorphized body of the runtime body only.
    let lean = *LEAN || mir_opt_level() != MirOptLevel::Off;
    let body = match instance.def {
        InstanceDef::Fn(func) if lean => tally
            .step(Step::Mir, || compiled_mir_body(db, func.into()))
            .and_then(|it| filled(it, &args, &env)),
        InstanceDef::Closure(id) if lean => {
            compiled_mir_body_for_closure(db, InternedClosureId::from_id(id))
                .and_then(|it| filled(it, &args, &env))
        }
        InstanceDef::Coroutine(id) if *LEAN => db
            .runtime_mir_body_for_coroutine(InternedCoroutineId::from_id(id))
            .and_then(|it| filled(it, &args, &env)),
        InstanceDef::CoroutineByMove(id) if *LEAN => db
            .runtime_mir_body_for_coroutine_by_move(InternedCoroutineId::from_id(id))
            .and_then(|it| filled(it, &args, &env)),
        InstanceDef::Fn(func) => {
            db.monomorphized_runtime_mir_body(func.into(), args, env).map(Cow::Borrowed)
        }
        InstanceDef::Closure(id) => db
            .monomorphized_runtime_mir_body_for_closure(InternedClosureId::from_id(id), args, env)
            .map(Cow::Borrowed),
        InstanceDef::Coroutine(id) => db
            .monomorphized_runtime_mir_body_for_coroutine(
                InternedCoroutineId::from_id(id),
                args,
                env,
            )
            .map(Cow::Borrowed),
        InstanceDef::CoroutineByMove(id) => db
            .monomorphized_runtime_mir_body_for_coroutine_by_move(
                InternedCoroutineId::from_id(id),
                args,
                env,
            )
            .map(Cow::Borrowed),
        InstanceDef::DropGlue
        | InstanceDef::CallOnceShim
        | InstanceDef::VtableShim(_)
        | InstanceDef::FnPointerShim
        | InstanceDef::ReifyShim { .. } => unsupported!("the body of a shim"),
    };
    body.map_err(|e| CodegenError::Frontend(format!("mir: {e:?}")))
}

/// The types a body takes and returns, which is all its callers have to know about it. Asked
/// for on its own so that they do not depend on the rest of the body.
#[derive(Clone, PartialEq, Eq)]
struct BodyTypes {
    params: Vec<StoredTy>,
    returned: StoredTy,
    /// For the body of an `async` block, the `Poll` it hands to the caller of `poll`.
    poll: Option<StoredTy>,
}

#[salsa::tracked(returns(ref))]
fn body_types<'db>(
    db: &'db dyn HirDatabase,
    instance: Instance,
    env: StoredParamEnvAndCrate,
) -> Result<BodyTypes> {
    let body = &*instance_body(db, &instance, env)?;
    Ok(BodyTypes {
        params: body.param_locals.iter().map(|&local| body.locals[local].ty.clone()).collect(),
        returned: body.locals[returned_local(body)].ty.clone(),
        poll: body.coroutine.map(|it| body.locals[it.poll].ty.clone()),
    })
}

/// The future of a non-generic `async fn` as `-Zprint-type-sizes` names it, with the offsets
/// of what each variant holds (the captures, and the locals kept there) in increasing order:
/// `{async fn body of f()} @coroutine 0,16;0,8,16`. `harness/layoutdiff.py` compares it with
/// rustc's, which `--layouts` otherwise skips: a future both compilers make and poll has to be
/// laid out alike to the byte.
fn coroutine_layout_line<'db>(
    db: &'db dyn HirDatabase,
    ty: Ty<'db>,
    layout: &Layout,
) -> Option<String> {
    use hir_def::hir::{ClosureKind, CoroutineSource, Expr};
    let TyKind::Coroutine(id, args) = ty.kind() else { return None };
    if !args.as_coroutine().parent_args().is_empty() {
        return None;
    }
    let closure = id.0.loc(db);
    let InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)) = closure.owner else {
        return None;
    };
    let store =
        hir_def::expr_store::ExpressionStore::of(db, closure.owner.expression_store_owner(db));
    let Expr::Closure {
        closure_kind: ClosureKind::Coroutine { source: CoroutineSource::Fn, .. },
        ..
    } = &store[closure.expr]
    else {
        return None;
    };
    let Variants::Multiple { tag_field, variants, .. } = &layout.variants else { return None };
    let upvars: Vec<u64> = (0..layout.fields.count())
        .filter(|&field| field != rustc_index::Idx::index(*tag_field))
        .map(|field| layout.fields.offset(field).bytes())
        .collect();
    let variants: Vec<String> = variants
        .iter()
        .map(|variant| {
            let mut offsets = upvars.clone();
            offsets.extend(variant.field_offsets.iter().map(|it| it.bytes()));
            offsets.sort_unstable();
            offsets.iter().map(u64::to_string).collect::<Vec<_>>().join(",")
        })
        .collect();
    let name = FunctionSignature::of(db, func).name.as_str().to_owned();
    Some(format!("{{async fn body of {name}()}} @coroutine {}", variants.join(";")))
}

/// Whether a future and everything that makes, polls or drops it have to come from one
/// compiler. They need not: ra-jit lays a future out as rustc does, from the same MIR
/// (`docs/mir-passes.md` of the fork). `RA_JIT_SPLIT_FUTURES=1` keeps them apart as before,
/// a temporary way back until mixing is verified (`harness/mixfutures`).
// FIXME: delete with what it guards once mixed futures are verified on den-native.
fn futures_kept_apart() -> bool {
    static APART: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("RA_JIT_SPLIT_FUTURES").is_some_and(|it| it != "0"));
    *APART
}

/// Whether a non-generic function returns a future of an `async` block, by itself or inside
/// another type. rustc lays such a future out differently, so the function that makes one has
/// to come from the compiler that compiles what holds and polls it.
#[salsa::tracked]
fn returns_async_block<'db>(db: &'db dyn HirDatabase, func: FunctionId) -> bool {
    struct Finder {
        opaque: bool,
        coroutine: bool,
    }
    impl<'db> rustc_type_ir::TypeVisitor<DbInterner<'db>> for Finder {
        type Result = ();

        fn visit_ty(&mut self, ty: Ty<'db>) {
            match ty.kind() {
                TyKind::Coroutine(..) => self.coroutine = true,
                TyKind::Alias(..) => self.opaque = true,
                _ => {}
            }
            rustc_type_ir::TypeSuperVisitable::super_visit_with(&ty, self)
        }
    }
    let mut finder = Finder { opaque: false, coroutine: false };
    // The declared type names a future only as an `impl Trait`, which the body's own does not.
    let declared = db.callable_item_signature(func.into()).skip_binder().skip_binder().output();
    rustc_type_ir::TypeVisitable::visit_with(&declared, &mut finder);
    if !finder.opaque && !FunctionSignature::of(db, func).is_async() {
        return false;
    }
    let Ok((body, _)) = root_body(db, func) else { return false };
    let returned = body.locals[returned_local(body)].ty.as_ref();
    rustc_type_ir::TypeVisitable::visit_with(&returned, &mut finder);
    finder.coroutine
}

/// The methods of the `Fn` traits and of the `AsyncFn` traits, each from the one that borrows
/// the callee to the one that takes it.
fn fn_trait_methods(lang_items: &LangItems) -> [[Option<FunctionId>; 3]; 2] {
    [
        [lang_items.Fn_call, lang_items.FnMut_call_mut, lang_items.FnOnce_call_once],
        [
            lang_items.AsyncFn_async_call,
            lang_items.AsyncFnMut_async_call_mut,
            lang_items.AsyncFnOnce_async_call_once,
        ],
    ]
}

/// The instance that is the `poll` of the future `ty`, when that is one of an `async` block.
fn coroutine_instance<'db>(
    db: &'db dyn HirDatabase,
    ty: Ty<'db>,
) -> Option<(InstanceDef, GenericArgs<'db>)> {
    let TyKind::Coroutine(id, args) = ty.kind() else { return None };
    let args = args.as_coroutine();
    let def = match is_by_move_coroutine(db, id.0, args) {
        true => InstanceDef::CoroutineByMove(id.0.as_id()),
        false => InstanceDef::Coroutine(id.0.as_id()),
    };
    Some((def, GenericArgs::new_from_slice(args.parent_args())))
}

/// The `async` blocks whose futures the body of `func` holds, or `None` when the body
/// can't be read. rustc lays a future out differently, so every function that touches one
/// has to come from the same compiler.
#[salsa::tracked(returns(ref))]
fn async_blocks<'db>(db: &'db dyn HirDatabase, func: FunctionId) -> Option<FxHashSet<salsa::Id>> {
    let Ok((body, env)) = root_body(db, func) else {
        return (!infers_a_future(db, func)).then(FxHashSet::default);
    };
    held_blocks(db, body, env)
}

/// The `async` blocks whose futures `body` holds, itself or in the closures and blocks it
/// holds, or `None` when one of those can't be read.
fn held_blocks<'db>(
    db: &'db dyn HirDatabase,
    body: &MirBody<'db>,
    env: ParamEnvAndCrate<'db>,
) -> Option<FxHashSet<salsa::Id>> {
    struct Collector<'db>(Vec<Ty<'db>>);
    impl<'db> rustc_type_ir::TypeVisitor<DbInterner<'db>> for Collector<'db> {
        type Result = ();

        fn visit_ty(&mut self, ty: Ty<'db>) {
            if let TyKind::Coroutine(..) | TyKind::Closure(..) | TyKind::CoroutineClosure(..) =
                ty.kind()
            {
                self.0.push(ty);
            }
            rustc_type_ir::TypeSuperVisitable::super_visit_with(&ty, self)
        }
    }
    let (mut found, mut seen) = (FxHashSet::default(), FxHashSet::default());
    let mut bodies = vec![body];
    while let Some(body) = bodies.pop() {
        let mut collector = Collector(Vec::new());
        for (_, local) in body.locals.iter() {
            rustc_type_ir::TypeVisitable::visit_with(&local.ty.as_ref(), &mut collector);
        }
        for ty in collector.0 {
            if !seen.insert(ty) {
                continue;
            }
            let body = match ty.kind() {
                TyKind::Coroutine(id, args) => {
                    found.insert(id.0.as_id());
                    let args = args.as_coroutine();
                    let parent = GenericArgs::new_from_slice(args.parent_args()).store();
                    match is_by_move_coroutine(db, id.0, args) {
                        true => db.monomorphized_mir_body_for_coroutine_by_move(
                            id.0,
                            parent,
                            env.store(),
                        ),
                        false => db.monomorphized_mir_body_for_coroutine(id.0, parent, env.store()),
                    }
                }
                TyKind::CoroutineClosure(id, args) => {
                    let parent =
                        GenericArgs::new_from_slice(args.as_coroutine_closure().parent_args());
                    let closure = id.0.as_closure(db);
                    db.monomorphized_mir_body_for_closure(closure, parent.store(), env.store())
                }
                TyKind::Closure(id, args) => {
                    let parent = GenericArgs::new_from_slice(args.as_closure().parent_args());
                    db.monomorphized_mir_body_for_closure(id.0, parent.store(), env.store())
                }
                _ => continue,
            };
            bodies.push(body.ok()?);
        }
    }
    Some(found)
}

/// Whether a type inferred in `func`'s body is or holds a future, an `impl Trait` included:
/// for a body without MIR, which then holds no future when this is `false`.
fn infers_a_future(db: &dyn HirDatabase, func: FunctionId) -> bool {
    struct Finder<'db> {
        interner: DbInterner<'db>,
        seen: FxHashSet<Ty<'db>>,
        found: bool,
    }
    impl<'db> rustc_type_ir::TypeVisitor<DbInterner<'db>> for Finder<'db> {
        type Result = ();

        fn visit_ty(&mut self, ty: Ty<'db>) {
            if self.found || !self.seen.insert(ty) {
                return;
            }
            match ty.kind() {
                TyKind::Coroutine(..) | TyKind::CoroutineClosure(..) => self.found = true,
                TyKind::Alias(rustc_type_ir::AliasTy {
                    kind: rustc_type_ir::AliasTyKind::Opaque { def_id },
                    args,
                    ..
                }) => {
                    let hidden = rustc_type_ir::Interner::type_of(self.interner, def_id.into())
                        .instantiate(self.interner, args)
                        .skip_norm_wip();
                    // What its defining body did not tell is polled as a future only when it is
                    // declared one.
                    if matches!(hidden.kind(), TyKind::Error(_)) {
                        let future = self.interner.lang_items().Future;
                        let bounds =
                            rustc_type_ir::Interner::item_bounds(self.interner, def_id.into());
                        self.found = bounds.skip_binder().into_iter().any(|clause| {
                            rustc_type_ir::inherent::Clause::as_trait_clause(clause)
                                .is_some_and(|it| Some(it.skip_binder().def_id().0) == future)
                        });
                    } else {
                        self.visit_ty(hidden);
                    }
                }
                _ => rustc_type_ir::TypeSuperVisitable::super_visit_with(&ty, self),
            }
        }
    }
    let infer = InferenceResult::of(db, DefWithBodyId::from(func));
    let interner = DbInterner::new_with(db, func.krate(db));
    let mut finder = Finder { interner, seen: FxHashSet::default(), found: false };
    for ty in infer.expression_types().map(|it| it.1).chain(infer.pattern_types().map(|it| it.1)) {
        rustc_type_ir::TypeVisitable::visit_with(&ty, &mut finder);
    }
    finder.found
}

/// The `async` blocks whose futures rustc's copy of `instance` holds: those of its body, with
/// its types filled in, or else those its body names before they are.
fn made_futures(
    db: &dyn HirDatabase,
    instance: &Instance,
    env: &StoredParamEnvAndCrate,
) -> FxHashSet<salsa::Id> {
    let InstanceDef::Fn(func) = instance.def else { return FxHashSet::default() };
    instance_body(db, instance, env.clone())
        .ok()
        .and_then(|body| held_blocks(db, &body, env.as_ref(db)))
        .unwrap_or_else(|| named_futures(db, func))
}

/// The `async` blocks of `func`'s body and of the closures and blocks in it, read before its
/// generic parameters are filled in.
fn named_futures(db: &dyn HirDatabase, func: FunctionId) -> FxHashSet<salsa::Id> {
    struct Collector<'db>(Vec<Ty<'db>>);
    impl<'db> rustc_type_ir::TypeVisitor<DbInterner<'db>> for Collector<'db> {
        type Result = ();

        fn visit_ty(&mut self, ty: Ty<'db>) {
            if let TyKind::Coroutine(..) | TyKind::Closure(..) | TyKind::CoroutineClosure(..) =
                ty.kind()
            {
                self.0.push(ty);
            }
            rustc_type_ir::TypeSuperVisitable::super_visit_with(&ty, self)
        }
    }
    let (mut found, mut seen) = (FxHashSet::default(), FxHashSet::default());
    let mut bodies: Vec<&MirBody<'_>> = db.mir_body(func.into()).into_iter().collect();
    while let Some(body) = bodies.pop() {
        let mut collector = Collector(Vec::new());
        for (_, local) in body.locals.iter() {
            rustc_type_ir::TypeVisitable::visit_with(&local.ty.as_ref(), &mut collector);
        }
        for ty in collector.0 {
            if !seen.insert(ty) {
                continue;
            }
            let body = match ty.kind() {
                TyKind::Coroutine(id, _) => {
                    found.insert(id.0.as_id());
                    db.mir_body_for_coroutine(id.0)
                }
                TyKind::Closure(id, _) => db.mir_body_for_closure(id.0),
                TyKind::CoroutineClosure(id, _) => db.mir_body_for_closure(id.0.as_closure(db)),
                _ => continue,
            };
            bodies.extend(body.ok());
        }
    }
    found
}

/// The functions skipped that rustc has a copy of. One of a crate rustc built nothing of has
/// none: it is missing from the program, and no code of rustc's uses a static in its place.
fn left_to_rustc<'a>(
    db: &'a dyn HirDatabase,
    symbols: &'a dyn SymbolResolver,
    skipped: &'a [(FunctionId, CodegenError)],
) -> impl Iterator<Item = FunctionId> + 'a {
    skipped
        .iter()
        .filter(|(_, e)| !e.no_symbol())
        .map(|it| it.0)
        .filter(|&func| symbols.rustc_built(func.krate(db)))
}

/// The statics that functions left to rustc refer to, themselves or through the instances they
/// name.
#[derive(Default, Clone, PartialEq, Eq)]
struct TheirStatics {
    statics: FxHashSet<StaticId>,
    /// The crates with a function for which that is not all of them: what could not be
    /// compiled refers to who knows what.
    unknown: FxHashSet<Crate>,
}

/// The instances the walk for [`TheirStatics`] went through. What it found stays as it is
/// until the code of one of them, or of a static it found, changes.
type Walked = Vec<(Instance, StoredParamEnvAndCrate)>;

impl TheirStatics {
    /// The functions of a crate are walked together: they mostly reach the same instances.
    fn of(db: &dyn HirDatabase, funcs: impl IntoIterator<Item = FunctionId>) -> TheirStatics {
        TheirStatics::walk(db, funcs).0
    }

    fn walk(
        db: &dyn HirDatabase,
        funcs: impl IntoIterator<Item = FunctionId>,
    ) -> (TheirStatics, Walked) {
        let mut by_crate = FxHashMap::<Crate, Vec<FunctionId>>::default();
        for func in funcs {
            by_crate.entry(func.krate(db)).or_default().push(func);
        }
        let mut theirs = TheirStatics::default();
        let mut walked = Walked::new();
        for (krate, funcs) in by_crate {
            let (mut seen, mut statics) = (FxHashSet::default(), FxHashSet::default());
            let mut complete = true;
            let mut stack = Vec::new();
            for func in funcs {
                match compiled_root(db, func).as_ref() {
                    Ok(compiled) => stack.push(&compiled.requests),
                    Err(_) => complete = false,
                }
            }
            while let Some(requests) = stack.pop() {
                for request in &requests.funcs {
                    let FuncTarget::Instance(instance, env) = &request.target else { continue };
                    if matches!(instance.def, InstanceDef::Fn(_))
                        && instance.args.as_ref().is_empty()
                        || !seen.insert(instance)
                    {
                        continue;
                    }
                    walked.push((instance.clone(), env.clone()));
                    match compiled_instance(db, instance.clone(), env.clone()).as_ref() {
                        Ok(compiled) => stack.push(&compiled.requests),
                        Err(_) => complete = false,
                    }
                }
                for request in &requests.datas {
                    if let DataRequest::Static(id) = request
                        && statics.insert(*id)
                    {
                        match compiled_static(db, *id).as_ref() {
                            Ok(compiled) => stack.push(&compiled.requests),
                            Err(_) => complete = false,
                        }
                    }
                }
            }
            theirs.statics.extend(statics);
            if !complete {
                theirs.unknown.insert(krate);
            }
        }
        (theirs, walked)
    }
}

/// The value of a static that rustc exports no symbol for.
#[salsa::tracked(returns(ref))]
fn compiled_static<'db>(db: &'db dyn HirDatabase, id: StaticId) -> Result<CompiledStatic> {
    let checked = crate_checks_overflow(db, id.krate(db)).unwrap_or(false);
    let mut cg = Recorder::new(db, &**target()?, selected_target()?, checked);
    let env =
        ParamEnvAndCrate { param_env: crate::next_solver::ParamEnv::empty(), krate: id.krate(db) };
    let mut value = None;
    build(&mut cg, env, None, false, |fx| {
        value = Some(fx.static_value(id)?);
        Ok(())
    })?;
    let (blob, writable) = value.expect("`build` ran the closure");
    Ok(CompiledStatic { requests: cg.finish().0, blob, writable })
}

/// What the threads that compile ahead have to do and have done.
struct Ahead {
    /// Compiled functions whose calls are still to be looked through.
    roots: Vec<FunctionId>,
    /// Every function that has been one of `roots`.
    rooted: FxHashSet<FunctionId>,
    queue: Vec<(Instance, StoredParamEnvAndCrate)>,
    seen: FxHashSet<(Instance, StoredParamEnvAndCrate)>,
    done: FxHashSet<(Instance, StoredParamEnvAndCrate)>,
    /// Those of `done` that [`Ready::take`] has not seen yet.
    fresh: Vec<(Instance, StoredParamEnvAndCrate)>,
    working: usize,
    /// A thread was stopped in the middle of an instance, as a write to the database
    /// does: the others are not to wait for what it would have found.
    stopped: bool,
}

/// What the threads that compile ahead are through with, for the one that puts the
/// instances in the object: it would wait for one of them on each it asked for too early.
struct Ready<'a> {
    ahead: &'a std::sync::Mutex<Ahead>,
    idle: &'a std::sync::Condvar,
    /// The instances to put in the object that a thread is still to compile.
    waiting: FxHashMap<(Instance, StoredParamEnvAndCrate), Vec<FuncId>>,
}

impl Ready<'_> {
    /// Takes from `pending` and from what waited the instances no thread is still to
    /// compile, waiting until there is one. Nothing is left when it gives none.
    fn take(
        &mut self,
        pending: &mut Vec<(Instance, StoredParamEnvAndCrate, FuncId)>,
    ) -> Vec<(Instance, StoredParamEnvAndCrate, FuncId)> {
        let mut ready = Vec::new();
        let mut state = self.ahead.lock().unwrap_or_else(|e| e.into_inner());
        for (instance, env, id) in pending.drain(..) {
            let it = (instance, env);
            if state.done.contains(&it) || !state.seen.contains(&it) {
                ready.push((it.0, it.1, id));
            } else {
                self.waiting.entry(it).or_default().push(id);
            }
        }
        loop {
            let ended = state.stopped
                || (state.roots.is_empty() && state.queue.is_empty() && state.working == 0);
            let done: Vec<_> = if ended {
                state.fresh.clear();
                self.waiting.drain().collect()
            } else {
                let fresh = std::mem::take(&mut state.fresh);
                fresh.into_iter().filter_map(|it| self.waiting.remove_entry(&it)).collect()
            };
            for ((instance, env), ids) in done {
                ready.extend(ids.into_iter().map(|id| (instance.clone(), env.clone(), id)));
            }
            if !ready.is_empty() || self.waiting.is_empty() {
                return ready;
            }
            state = self.idle.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// What the types of variables are told as, for every thread: the threads that compile
/// find it ahead of the one that puts the functions in the object, which has it to wait
/// for otherwise.
#[derive(Default, Clone)]
struct ToldTypes(
    #[allow(clippy::type_complexity)]
    std::sync::Arc<
        std::sync::Mutex<FxHashMap<(StoredParamEnvAndCrate, StoredTy), Option<debuginfo::TypeSet>>>,
    >,
);

impl ToldTypes {
    fn told(
        &self,
        db: &dyn HirDatabase,
        env: &StoredParamEnvAndCrate,
        ty: &StoredTy,
    ) -> Option<debuginfo::TypeSet> {
        let key = (env.clone(), ty.clone());
        if let Some(told) = self.0.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
            return told.clone();
        }
        let told = debuginfo::described(db, env.clone(), ty.clone());
        // Another thread may have told of it meanwhile, and the functions share one set.
        self.0.lock().unwrap_or_else(|e| e.into_inner()).entry(key).or_insert(told).clone()
    }

    /// Tells of the types of the variables of `compiled`, for [`describe_variables`] to find.
    fn ahead(&self, db: &dyn HirDatabase, compiled: &CompiledFunction) {
        if let (Some(_), Some((env, variables))) = (&compiled.source, &compiled.variables) {
            variables.iter().for_each(|it| _ = self.told(db, env, &it.ty));
        }
    }
}

/// What a debugger is told of `variables`, which are of the body of `owner` and are where
/// `debug` has the places of, each in the scope `scopes` has for its binding. A variable of
/// a type that is not told of is left out.
fn describe_variables<'db>(
    db: &'db dyn HirDatabase,
    owner: DefWithBodyId,
    (told, env): (&ToldTypes, &StoredParamEnvAndCrate),
    variables: &[Variable],
    places: &SlotPlaces,
    scopes: &FxHashMap<BindingId, u32>,
    debug: &mut FunctionDebug,
) {
    let body = Body::of(db, owner);
    // Where the type of each variable told of is among the types of the function.
    let mut firsts = FxHashMap::default();
    for variable in variables {
        let name = &body[variable.binding].name;
        // A name a macro of the compiler's made up is no name of the source.
        if name.as_str().starts_with('<') {
            continue;
        }
        let Some(place) = places.of(variable.slot) else { continue };
        let place = match variable.pointer {
            Some(offset) => place.pointee(offset),
            None => place.clone(),
        };
        let ty = variable.ty.as_ref();
        let ty = match firsts.get(&ty) {
            Some(&first) => first,
            None => {
                let Some(described) = told.told(db, env, &variable.ty) else { continue };
                let first = debug.types.len() as u32;
                debug.types.push(described);
                firsts.insert(ty, first);
                first
            }
        };
        debug.variables.push(VariableDebug {
            name: name.as_str().to_owned(),
            param: variable.param,
            ty,
            place,
            scope: scopes.get(&variable.binding).copied(),
        });
    }
    debug.frame_base = places.frame_base.clone();
}

/// The scopes of the body of `owner` that `variables` are bound in, added to `debug`, and
/// which of them each binding is of. The code of a scope is the code of the expressions
/// that names are looked up from it or from a scope inside it for: `rows` has, in the order
/// of the code, where the code of each expression starts, and `len` is where the last ends.
/// A binding of the outermost scope, which a parameter is, is known in all of the function
/// and is not among what is returned.
fn describe_scopes(
    db: &dyn HirDatabase,
    owner: DefWithBodyId,
    variables: &[Variable],
    rows: &[(u32, MirSpan)],
    len: u32,
    debug: &mut FunctionDebug,
) -> FxHashMap<BindingId, u32> {
    let scopes = ExprScopes::of(db, owner);
    let body = Body::of(db, owner);
    let outermost = scopes.scope_chain(scopes.scope_for(body.root_expr())).last();
    // The scope each binding is an entry of.
    let mut bound = FxHashMap::default();
    let mut seen = FxHashSet::default();
    for (_, &scope) in scopes.scope_by_expr().iter() {
        for scope in scopes.scope_chain(Some(scope)).take_while(|&it| seen.insert(it)) {
            bound.extend(scopes.entries(scope).iter().map(|entry| (entry.binding(), scope)));
        }
    }
    // The scopes told of, outer ones first, and where each is among them.
    let mut told: FxHashMap<ScopeId, u32> = FxHashMap::default();
    let mut of_binding = FxHashMap::default();
    for variable in variables {
        let Some(&scope) = bound.get(&variable.binding).filter(|&&it| Some(it) != outermost) else {
            continue;
        };
        let mut chain: Vec<ScopeId> = scopes
            .scope_chain(Some(scope))
            .take_while(|it| Some(*it) != outermost && !told.contains_key(it))
            .filter(|&it| it == scope || !scopes.entries(it).is_empty())
            .collect();
        chain.reverse();
        for scope in chain {
            let parent =
                scopes.scope_chain(Some(scope)).skip(1).find_map(|it| told.get(&it).copied());
            told.insert(scope, debug.scopes.len() as u32);
            debug.scopes.push(debuginfo::Scope { parent, ranges: Vec::new() });
        }
        of_binding.insert(variable.binding, told[&scope]);
    }
    if told.is_empty() {
        return of_binding;
    }
    // The code that tries the pattern of an arm is of the scope its `match` is in.
    let mut tried = FxHashMap::default();
    for (expr, it) in body.exprs() {
        if let hir_def::hir::Expr::Match { arms, .. } = it {
            for arm in arms.iter() {
                body.walk_pats(arm.pat, &mut |pat| drop(tried.insert(pat, expr)));
            }
        }
    }
    // Other code that is of no expression is of the scope of the code before it.
    let mut scope = None;
    let ends = rows.iter().skip(1).map(|row| row.0).chain([len]);
    for (&(from, span), to) in rows.iter().zip(ends) {
        match span {
            MirSpan::ExprId(expr) | MirSpan::ExprEnd(expr) => scope = scopes.scope_for(expr),
            MirSpan::PatId(pat) => {
                if let Some(&of) = tried.get(&pat) {
                    scope = scopes.scope_for(of);
                }
            }
            _ => {}
        }
        if from >= to {
            continue;
        }
        for index in scopes.scope_chain(scope).filter_map(|it| told.get(&it)) {
            let ranges = &mut debug.scopes[*index as usize].ranges;
            match ranges.last_mut() {
                Some(last) if last.1 == from => last.1 = to,
                _ => ranges.push((from, to)),
            }
        }
    }
    of_binding
}

/// File, line and column of a place in a body, as rustc reports them, found from where the
/// body is in its file: [`places::source_location`] finds them from the body alone.
#[salsa::tracked]
fn source_location_in_file<'db>(
    db: &'db dyn HirDatabase,
    owner: DefWithBodyId,
    span: MirSpan,
) -> Option<(base_db::FileId, u32, u32)> {
    let (file_id, range) = source_range(db, owner, span)?;
    location_of(db, file_id, range)
}

/// File, line and column of a place in a body for a debugger, as rustc gives code its lines.
/// They are those of [`source_location`] but for what a macro writes: that is of where the
/// macro is called only if the macro is another crate's or the compiler's, and of where the
/// macro has it otherwise, as it is of what a macro writes of the code of `closure`, or of
/// the body when there is none, where that is itself what the macro writes.
#[salsa::tracked]
fn row_location_in_file<'db>(
    db: &'db dyn HirDatabase,
    owner: DefWithBodyId,
    closure: Option<ExprId>,
    span: MirSpan,
) -> Option<(base_db::FileId, u32, u32)> {
    let (file_id, range) = source_range(db, owner, span)?;
    if file_id.macro_file().is_none() {
        return *source_location_in_file(db, owner, span);
    }
    let source_map = &Body::with_source_map(db, owner).1;
    let code = closure.unwrap_or_else(|| Body::of(db, owner).root_expr());
    let within = match source_map.expr_syntax(code) {
        Ok(body) => {
            let start = TextRange::empty(body.value.syntax_node_ptr().text_range().start());
            body.file_id.span_map(db).span_for_range(start).ctx
        }
        Err(_) => return *source_location_in_file(db, owner, span),
    };
    written_place(db, file_id, range, Some(within))
}

/// File, line and column of where `range` of `file_id` is for a debugger: where the macro
/// has it while the macro is the crate's own, and where the first macro that is not is
/// called from, up to the expansion `within` or to the source.
fn written_place(
    db: &dyn HirDatabase,
    file_id: HirFileId,
    range: TextRange,
    within: Option<span::SyntaxContext>,
) -> Option<(base_db::FileId, u32, u32)> {
    let mut at = file_id.span_map(db).span_for_range(TextRange::empty(range.start()));
    let mut kept = at;
    while !at.ctx.is_root() && Some(at.ctx) != within {
        let Some(call) = at.ctx.outer_expn(db) else { break };
        let call = hir_expand::MacroCallId::from(call).loc(db);
        let written = call.to_node(db);
        let range = TextRange::empty(written_range(&written.value).start());
        at = written.file_id.span_map(db).span_for_range(range);
        let own = call.def.krate == call.krate && call.def.kind.is_declarative();
        if !own {
            kept = at;
        }
    }
    line_and_column(db, hir_expand::resolve_span(db, kept))
}

/// The file a place in a body is written in, which may be what a macro expands to, and
/// where in it rustc has the place begin.
fn source_range<'db>(
    db: &'db dyn HirDatabase,
    owner: DefWithBodyId,
    span: MirSpan,
) -> Option<(HirFileId, TextRange)> {
    let source_map = &Body::with_source_map(db, owner).1;
    let source = match span {
        MirSpan::ExprId(expr) => source_map.expr_syntax(expr).ok()?,
        // The block an `async fn` is taken to wrap its body in is not written anywhere, and
        // ends where the body does.
        MirSpan::ExprEnd(expr) => {
            match (source_map.expr_syntax(expr), &Body::of(db, owner)[expr]) {
                (Ok(written), _) => written,
                (Err(_), hir_def::hir::Expr::Block { tail: Some(tail), .. }) => {
                    source_map.expr_syntax(*tail).ok()?
                }
                (Err(_), _) => return None,
            }
        }
        MirSpan::PatId(pat) => source_map.pat_syntax(pat).ok()?,
        MirSpan::BindingId(_) | MirSpan::SelfParam | MirSpan::Unknown => return None,
    };
    if let MirSpan::ExprEnd(_) = span {
        // The brace that closes a block, which is where rustc has a scope left.
        let range = source.value.syntax_node_ptr().text_range();
        let last = range.end().checked_sub(TextSize::new(1)).filter(|it| *it >= range.start());
        return Some((source.file_id, TextRange::new(last?, range.end())));
    }
    let range = match span {
        MirSpan::ExprId(expr) => {
            let root = source.file_id.parse_or_expand(db);
            let node = source.value.syntax_node_ptr().to_node(&root);
            reported_range(db, owner, expr, source.file_id, &node)?
        }
        _ => source.value.syntax_node_ptr().text_range(),
    };
    Some((source.file_id, range))
}

/// The part of an expression that rustc's span of it covers where it panics: a method call
/// from the method's name on, an index through `Index` from its bracket on, and any other
/// expression together with the parentheses it is written in.
fn reported_range(
    db: &dyn HirDatabase,
    owner: DefWithBodyId,
    expr: ExprId,
    file_id: HirFileId,
    node: &SyntaxNode,
) -> Option<TextRange> {
    let end = node.text_range().end();
    if let Some(call) = ast::MethodCallExpr::cast(node.clone()) {
        return Some(TextRange::new(call.name_ref()?.syntax().text_range().start(), end));
    }
    if let Some(written) = ast::IndexExpr::cast(node.clone())
        && let hir_def::hir::Expr::Index { base, index } = Body::of(db, owner)[expr]
        && !is_builtin_index(InferenceResult::of(db, owner), base, index)
    {
        return Some(TextRange::new(written.l_brack_token()?.text_range().start(), end));
    }
    Some(parenthesized(db, file_id, node))
}

/// The range of an expression with the parentheses around it. rustc's span of an expression
/// includes them when both are written in one place, and not when a macro put the
/// parentheses around what it was passed.
fn parenthesized(db: &dyn HirDatabase, file_id: HirFileId, node: &SyntaxNode) -> TextRange {
    let passed_in = |range: TextRange| {
        file_id.macro_file().is_none()
            || file_id.span_map(db).span_for_range(TextRange::empty(range.start())).ctx.is_root()
    };
    let inner = passed_in(node.text_range());
    node.ancestors()
        .skip(1)
        .take_while(|it| it.kind() == SyntaxKind::PAREN_EXPR && passed_in(it.text_range()) == inner)
        .last()
        .map_or(node.text_range(), |it| it.text_range())
}

/// File, line and column of where a function is declared, from its visibility or `fn` on:
/// what a pointer to a `#[track_caller]` function reports as its caller, as rustc's shim does.
#[salsa::tracked]
fn declaration_location<'db>(
    db: &'db dyn HirDatabase,
    func: FunctionId,
) -> Option<(base_db::FileId, u32, u32)> {
    let source = func.lookup(db).source(db);
    location_of(db, source.file_id, written_range(source.value.syntax()))
}

/// File, line and column a debugger is told a function is declared on, and its code ahead of
/// the first statement is of. They are those of [`declaration_location`] but for a function
/// a macro writes, which is where the macro has it as [`row_location`] has the statements
/// of the body, and as rustc tells. gdb on aarch64 takes the lines after the first one that
/// are above it as more of the prologue, so a function a macro of the crate writes, declared
/// on the line of the call below the macro, had its whole body taken as prologue: stepping
/// into it put the breakpoint past the calls, where a panic in them never got.
#[salsa::tracked]
fn debug_declaration_location<'db>(
    db: &'db dyn HirDatabase,
    func: FunctionId,
) -> Option<(base_db::FileId, u32, u32)> {
    let source = func.lookup(db).source(db);
    if source.file_id.macro_file().is_none() {
        return *declaration_location(db, func);
    }
    written_place(db, source.file_id, written_range(source.value.syntax()), None)
}

/// File, line and column of where the struct or the variant `def` constructs is written,
/// which is where rustc has the code of a constructor from. `None` for a function.
fn constructor_location(
    db: &dyn HirDatabase,
    def: CallableDefId,
) -> Option<(base_db::FileId, u32, u32)> {
    let (file, node) = match def {
        CallableDefId::FunctionId(_) => return None,
        CallableDefId::StructId(it) => {
            let source = it.lookup(db).source(db);
            (source.file_id, source.value.syntax().clone())
        }
        CallableDefId::EnumVariantId(it) => {
            let source = it.lookup(db).source(db);
            (source.file_id, source.value.syntax().clone())
        }
    };
    location_of(db, file, written_range(&node))
}

/// The range of an item or a macro call without the attributes and comments on top of it,
/// which rustc's span of it leaves out.
fn written_range(node: &SyntaxNode) -> TextRange {
    let start = node
        .children_with_tokens()
        .find(|it| {
            !matches!(it.kind(), SyntaxKind::ATTR | SyntaxKind::DOC_COMMENT)
                && !it.kind().is_trivia()
        })
        .map_or(node.text_range().start(), |it| it.text_range().start());
    TextRange::new(start, node.text_range().end())
}

/// The literals of a body that `line!()`, `column!()` or `file!()` expanded to. The source map
/// changes with every edit above the body, and this keeps code without such literals from
/// changing with it.
#[salsa::tracked(returns(ref))]
fn location_macros<'db>(
    db: &'db dyn HirDatabase,
    owner: DefWithBodyId,
) -> FxHashMap<ExprId, (BuiltinFnLikeExpander, HirFileId)> {
    let (body, source_map) = (Body::of(db, owner), Body::unmoved_source_map(db, owner));
    body.exprs()
        .filter(|(_, expr)| matches!(expr, hir_def::hir::Expr::Literal(_)))
        .filter_map(|(id, _)| {
            let file_id = source_map.expr_syntax(id).ok()?.file_id;
            let MacroDefKind::BuiltIn(_, expander) = file_id.macro_file()?.loc(db).def.kind else {
                return None;
            };
            matches!(
                expander,
                BuiltinFnLikeExpander::Line
                    | BuiltinFnLikeExpander::Column
                    | BuiltinFnLikeExpander::File
            )
            .then_some((id, (expander, file_id)))
        })
        .collect()
}

/// The literals of a body that `concat!` made of arguments with `line!()`, `column!()` or
/// `file!()` among them, each with the expansion it is in.
#[salsa::tracked(returns(ref))]
fn located_concats<'db>(
    db: &'db dyn HirDatabase,
    owner: DefWithBodyId,
) -> FxHashMap<ExprId, HirFileId> {
    let (body, source_map) = (Body::of(db, owner), Body::unmoved_source_map(db, owner));
    body.exprs()
        .filter(|(_, expr)| matches!(expr, hir_def::hir::Expr::Literal(_)))
        .filter_map(|(id, _)| {
            let file_id = source_map.expr_syntax(id).ok()?.file_id;
            let loc = file_id.macro_file()?.loc(db);
            let concat =
                matches!(loc.def.kind, MacroDefKind::BuiltInEager(_, EagerExpander::Concat));
            (concat && names_location(&loc.to_node(db).value)).then_some((id, file_id))
        })
        .collect()
}

/// What `line!()`, `column!()`, `file!()` or a `concat!` of them is where it is written.
pub(crate) enum Placed {
    Number(u32),
    Text(String),
}

type SourcePaths = dyn Fn(&dyn HirDatabase, base_db::FileId) -> Option<String> + Send + Sync;

/// What the build calls each file, for a constant with `file!()` in its value. The evaluator
/// has no build to ask.
static SOURCE_PATHS: std::sync::RwLock<Option<std::sync::Arc<SourcePaths>>> =
    std::sync::RwLock::new(None);

/// Tells the evaluator of constants what the build calls each file: [`Symbols::source_path`].
pub fn name_sources(
    paths: impl Fn(&dyn HirDatabase, base_db::FileId) -> Option<String> + Send + Sync + 'static,
) {
    *SOURCE_PATHS.write().unwrap_or_else(|e| e.into_inner()) = Some(std::sync::Arc::new(paths));
}

/// The value of the literal `expr` of `owner` when it is what `line!()`, `column!()`,
/// `file!()` or a `concat!` of them expanded to: rust-analyzer expands them to placeholders,
/// and a constant is evaluated from what it expanded them to.
pub(crate) fn placed(db: &dyn HirDatabase, owner: DefWithBodyId, expr: ExprId) -> Option<Placed> {
    let path = |file| {
        let paths = SOURCE_PATHS.read().unwrap_or_else(|e| e.into_inner()).clone()?;
        paths(db, file)
    };
    if let Some(file_id) = located_concats(db, owner).get(&expr) {
        let call = file_id.macro_file()?.loc(db).to_node(db);
        let mut text = String::new();
        for part in concat_parts(&call.value)? {
            match part {
                ConcatPart::Text(part) => text.push_str(&part),
                ConcatPart::Location(expander, range) => {
                    let (file, line, col) = location_of(db, call.file_id, range)?;
                    match expander {
                        BuiltinFnLikeExpander::Line => text.push_str(&line.to_string()),
                        BuiltinFnLikeExpander::Column => text.push_str(&col.to_string()),
                        _ => text.push_str(&path(file)?),
                    }
                }
            }
        }
        return Some(Placed::Text(text));
    }
    let &(expander, file_id) = location_macros(db, owner).get(&expr)?;
    let site = written_call(db, file_id)?;
    let range = InFile::new(site.file_id, written_range(&site.value));
    let FileRange { file_id, range } = range.original_node_file_range_rooted(db);
    let (file, line, col) = location_of(db, file_id.into(), range)?;
    Some(match expander {
        BuiltinFnLikeExpander::Line => Placed::Number(line),
        BuiltinFnLikeExpander::Column => Placed::Number(col),
        _ => Placed::Text(path(file)?),
    })
}

/// The macro of `name` that expands to a place in the source.
fn location_expander(name: &str) -> Option<BuiltinFnLikeExpander> {
    match name {
        "line" => Some(BuiltinFnLikeExpander::Line),
        "column" => Some(BuiltinFnLikeExpander::Column),
        "file" => Some(BuiltinFnLikeExpander::File),
        _ => None,
    }
}

/// Whether a macro call is passed a call of `line!`, `column!` or `file!`.
fn names_location(call: &SyntaxNode) -> bool {
    call.descendants_with_tokens().filter_map(|it| it.into_token()).any(|token| {
        token.kind() == SyntaxKind::IDENT
            && location_expander(token.text()).is_some()
            && token
                .next_token()
                .and_then(|next| syntax::algo::skip_trivia_token(next, syntax::Direction::Next))
                .is_some_and(|next| next.kind() == SyntaxKind::BANG)
    })
}

/// An argument of a `concat!`.
enum ConcatPart {
    Text(String),
    /// A call of `line!`, `column!` or `file!`, and where it is.
    Location(BuiltinFnLikeExpander, TextRange),
}

/// The arguments of a `concat!` call, when each is a literal or a call of `line!`, `column!`
/// or `file!`.
fn concat_parts(call: &SyntaxNode) -> Option<Vec<ConcatPart>> {
    let tree = ast::MacroCall::cast(call.clone())?.token_tree()?;
    let mut inside: Vec<SyntaxElement> =
        tree.syntax().children_with_tokens().filter(|it| !it.kind().is_trivia()).collect();
    inside.pop()?;
    let inside = inside.get(1..)?;
    inside
        .split(|it| it.kind() == SyntaxKind::COMMA)
        .filter(|argument| !argument.is_empty())
        .map(concat_part)
        .collect()
}

fn concat_part(argument: &[SyntaxElement]) -> Option<ConcatPart> {
    if let [SyntaxElement::Token(token)] = argument {
        let text = match token.kind() {
            SyntaxKind::STRING => ast::String::cast(token.clone())?.value().ok()?.into_owned(),
            SyntaxKind::INT_NUMBER => {
                ast::IntNumber::cast(token.clone())?.value().ok()?.to_string()
            }
            SyntaxKind::CHAR => ast::Char::cast(token.clone())?.value().ok()?.to_string(),
            SyntaxKind::TRUE_KW | SyntaxKind::FALSE_KW => token.text().to_owned(),
            _ => return None,
        };
        return Some(ConcatPart::Text(text));
    }
    let [.., SyntaxElement::Token(name), bang, SyntaxElement::Node(arguments)] = argument else {
        return None;
    };
    let expander = location_expander(name.text())?;
    let empty = arguments.children_with_tokens().filter(|it| !it.kind().is_trivia()).count() == 2;
    (bang.kind() == SyntaxKind::BANG && empty).then(|| {
        let range = argument[0].text_range().cover(arguments.text_range());
        ConcatPart::Location(expander, range)
    })
}

/// The macro call an expansion comes from that is itself written out, in the source or in
/// what a macro was passed, and not by another macro.
fn written_call(db: &dyn HirDatabase, file_id: HirFileId) -> Option<InFile<SyntaxNode>> {
    let mut call = file_id.macro_file()?.loc(db).to_node(db);
    while let Some(outer) = call.file_id.macro_file() {
        let start = TextRange::empty(call.value.text_range().start());
        if call.file_id.span_map(db).span_for_range(start).ctx.is_root() {
            break;
        }
        call = outer.loc(db).to_node(db);
    }
    Some(call)
}

fn location_of(
    db: &dyn HirDatabase,
    file_id: HirFileId,
    range: TextRange,
) -> Option<(base_db::FileId, u32, u32)> {
    // What a macro writes itself is reported from where the outermost macro is called;
    // what it is passed keeps its own place.
    let own_tokens = file_id.macro_file().is_some()
        && !file_id.span_map(db).span_for_range(TextRange::empty(range.start())).ctx.is_root();
    let FileRange { file_id, range } = match written_call(db, file_id) {
        Some(call) if own_tokens => InFile::new(call.file_id, written_range(&call.value))
            .original_node_file_range_rooted(db),
        _ => InFile::new(file_id, range).original_node_file_range_rooted(db),
    };
    line_and_column(db, FileRange { file_id, range })
}

/// File, line and column of where a range of a file begins.
fn line_and_column(
    db: &dyn HirDatabase,
    FileRange { file_id, range }: FileRange,
) -> Option<(base_db::FileId, u32, u32)> {
    let file_id = file_id.file_id(db);
    let text = db.file_text(file_id);
    let starts = line_starts(db, text);
    let at = u32::from(range.start());
    let line = starts.partition_point(|&start| start <= at);
    let line_start = *starts.get(line.checked_sub(1)?)?;
    let written = text.text(db).get(line_start as usize..at as usize)?;
    Some((file_id, line as u32, written.chars().map(column_width).sum::<usize>() as u32 + 1))
}

/// Where each line of a file begins, the first of them at 0.
#[salsa::tracked(returns(ref))]
fn line_starts(db: &dyn HirDatabase, text: base_db::FileText) -> Vec<u32> {
    let ends = text.text(db).bytes().enumerate().filter(|&(_, byte)| byte == b'\n');
    std::iter::once(0).chain(ends.map(|(at, _)| at as u32 + 1)).collect()
}

/// How many columns rustc counts a character of a source line as (`rustc_span::char_width`):
/// its display width, four for a tab, and one for the control characters rustc prints a
/// visible character for.
fn column_width(ch: char) -> usize {
    match ch {
        '\t' => 4,
        '\0'..='\u{8}'
        | '\u{b}'..='\u{1f}'
        | '\u{7f}'
        | '\u{202a}'..='\u{202e}'
        | '\u{2066}'..='\u{2069}' => 1,
        _ => UnicodeWidthChar::width(ch).unwrap_or(1),
    }
}

fn is_thread_local(db: &dyn HirDatabase, id: StaticId) -> bool {
    AttrFlags::query(db, id.into()).contains(AttrFlags::THREAD_LOCAL)
}

/// The symbol a function is exported under when an attribute names it: the runtime or the
/// program loading the library looks it up by that name, so it is compiled whether or not
/// anything calls it.
pub fn exported_symbol(db: &dyn HirDatabase, func: FunctionId) -> Option<String> {
    if let Some(name) = AttrFlags::export_name(db, func.into()) {
        return Some(name.as_str().to_owned());
    }
    AttrFlags::query(db, func.into())
        .contains(AttrFlags::NO_MANGLE)
        .then(|| FunctionSignature::of(db, func).name.as_str().to_owned())
}

/// The symbol an item of an `extern` block links to.
pub fn link_name(db: &dyn HirDatabase, func: FunctionId) -> Option<String> {
    let ItemContainerId::ExternBlockId(_) = func.lookup(db).container else {
        return None;
    };
    let name = AttrFlags::link_name(db, func.into())
        .unwrap_or_else(|| FunctionSignature::of(db, func).name.symbol().clone());
    Some(name.as_str().to_owned())
}

/// The symbol a static of an `extern` block links to.
fn static_link_name(db: &dyn HirDatabase, id: StaticId) -> Option<String> {
    let signature = StaticSignature::of(db, id);
    if !signature.flags.contains(StaticFlags::EXTERN) {
        return None;
    }
    let name =
        AttrFlags::link_name(db, id.into()).unwrap_or_else(|| signature.name.symbol().clone());
    Some(name.as_str().to_owned())
}

/// Builds the IR of one function and runs Cranelift on it.
fn compile<'db>(
    db: &'db dyn HirDatabase,
    env: ParamEnvAndCrate<'db>,
    body: Option<&'db MirBody<'db>>,
    tracked: bool,
    checked: bool,
    fill: impl for<'cx> FnOnce(&mut FunctionCx<'cx, 'db>) -> Result<()>,
) -> Result<CompiledFunction> {
    compile_as(db, env, body, tracked, checked, false, fill).map(Produced::code)
}

/// Builds the IR of one function and compiles it with Cranelift, or, for a compile at a level
/// above 1 (`tier`), leaves it to LLVM to compile once the database is let go of.
fn compile_as<'db>(
    db: &'db dyn HirDatabase,
    env: ParamEnvAndCrate<'db>,
    body: Option<&'db MirBody<'db>>,
    tracked: bool,
    checked: bool,
    tier: bool,
    fill: impl for<'cx> FnOnce(&mut FunctionCx<'cx, 'db>) -> Result<()>,
) -> Result<Produced> {
    let isa = &**target()?;
    let mut cg = Recorder::new(db, isa, selected_target()?, checked);
    let tally = backend::Tally::global();
    /// `RA_JIT_PROMOTE=0` leaves every local in its slot of the frame.
    static PROMOTE: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("RA_JIT_PROMOTE").is_none_or(|it| it != "0"));
    let mut ctx = tally.step(Step::Translate, || {
        let mut ctx = build(&mut cg, env, body, tracked, fill)?;
        inline(&mut ctx, &cg.inlined)?;
        if *PROMOTE {
            promote::promote(&mut ctx.func, cg.named_slots());
        }
        Result::Ok(ctx)
    })?;
    let code = match cg.naked.take() {
        // The function is its assembly: what was built of it is only its signature.
        Some((text, syms)) => {
            let assembled = asm::assemble(&isa.triple().to_string(), &text)
                .map_err(CodegenError::Unsupported)?;
            let relocs = assembled.relocs.iter().map(|named| {
                let name = syms.get(named.operand as usize).cloned().flatten()?;
                Some(ModuleReloc {
                    offset: named.offset,
                    kind: named.kind,
                    name,
                    addend: named.addend,
                })
            });
            let Some(relocs) = relocs.collect() else {
                unsupported!("inline assembly that names a symbol");
            };
            Code {
                signature: ctx.func.signature.clone(),
                bytes: assembled.code.clone(),
                align: 16,
                relocs,
                elf_types: assembled
                    .relocs
                    .iter()
                    .filter_map(|named| Some((named.offset, named.elf_type?)))
                    .collect(),
                unwind: None,
                lines: Vec::new(),
                slots: SlotPlaces::default(),
            }
        }
        None if tier => {
            let named = cg.named_slots().collect();
            let (source, variables) = described(&mut cg, env, body);
            let (wants, layouts) = cg.detach();
            return Ok(Produced::Pending(Pending {
                func: ctx.func,
                wants,
                named,
                source,
                variables,
                layouts,
                closure: None,
            }));
        }
        None => Backend::emit(&mut cg, &mut ctx)?,
    };
    let (source, variables) = described(&mut cg, env, body);
    let (requests, layouts) = cg.finish();
    // What a build compiled, which a compile at a level above 1 is not part of.
    if !tier {
        COMPILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(Produced::Code(CompiledFunction {
        requests,
        code,
        source,
        closure: None,
        variables,
        layouts,
    }))
}

/// What `compile_as` gives: the code, or, for a compile at a level above 1, what LLVM makes
/// it of once the database is let go of.
pub(crate) enum Produced {
    Code(CompiledFunction),
    Pending(Pending),
}

impl Produced {
    /// The code of a compile at level 1, which leaves nothing for later.
    fn code(self) -> CompiledFunction {
        match self {
            Produced::Code(code) => code,
            Produced::Pending(_) => unreachable!("only a compile at a level above 1 is pending"),
        }
    }
}

/// A function whose IR is built, and all that was asked of the database for it, which LLVM
/// compiles without it: an edit applied meanwhile waits for nothing.
pub struct Pending {
    func: ir::Function,
    wants: fragment::Wants,
    named: Vec<ir::StackSlot>,
    source: Option<(DefWithBodyId, Vec<MirSpan>)>,
    variables: Option<(StoredParamEnvAndCrate, Vec<Variable>)>,
    layouts: Vec<(String, u64, u64)>,
    closure: Option<ExprId>,
}

impl Pending {
    pub(crate) fn func(&self) -> &ir::Function {
        &self.func
    }

    /// The code LLVM makes of the function, at the level `request` asks for.
    pub(crate) fn produce(mut self, request: &tier::Request) -> Result<CompiledFunction> {
        let code = tier::emit(&mut self.wants, &self.named, &mut self.func, request)?;
        Ok(CompiledFunction {
            requests: self.wants.into_requests(),
            code,
            source: self.source,
            closure: self.closure,
            variables: self.variables,
            layouts: self.layouts,
        })
    }
}

/// What tells the code of a function's lines and variables: the statements of `body` its
/// instructions were built for, and its locals with a name.
fn described<'db>(
    cg: &mut Recorder<'db>,
    env: ParamEnvAndCrate<'db>,
    body: Option<&'db MirBody<'db>>,
) -> (Option<(DefWithBodyId, Vec<MirSpan>)>, Option<(StoredParamEnvAndCrate, Vec<Variable>)>) {
    let source = match body.map(|body| body.owner) {
        Some(InferBodyId::DefWithBodyId(owner)) => Some((owner, cg.take_spans())),
        _ => None,
    };
    let variables = std::mem::take(&mut cg.named)
        .into_iter()
        .map(|Named { binding, param, ty, slot, pointer }| Variable {
            binding,
            param,
            ty,
            slot: slot.as_u32(),
            pointer,
        })
        .collect::<Vec<_>>();
    (source, Some((env.store(), variables)).filter(|it| !it.1.is_empty()))
}

fn build<'db>(
    cg: &mut Recorder<'db>,
    env: ParamEnvAndCrate<'db>,
    body: Option<&'db MirBody<'db>>,
    tracked: bool,
    fill: impl for<'cx> FnOnce(&mut FunctionCx<'cx, 'db>) -> Result<()>,
) -> Result<cranelift_codegen::Context> {
    let interner = DbInterner::new_with(cg.db, env.krate);
    let mut ctx = cranelift_codegen::Context::new();
    ctx.func.signature.call_conv = cg.isa.default_call_conv();
    // Source locations are kept as distances from the first one set, and the distance of
    // the one just below it is taken for none at all.
    ctx.func.params.ensure_base_srcloc(ir::SourceLoc::new(0));
    let mut builder_ctx = FunctionBuilderContext::new();
    let mut fx = FunctionCx {
        cg,
        body,
        env,
        infcx: interner.infer_ctxt().build(TypingMode::PostAnalysis),
        interner,
        builder: FunctionBuilder::new(&mut ctx.func, &mut builder_ctx),
        locals: ArenaMap::new(),
        blocks: ArenaMap::new(),
        layouts: Default::default(),
        glue: Default::default(),
        inner_pad: None,
        unwind_to: None,
        pads: Vec::new(),
        exception: None,
        argument_slots: Vec::new(),
        free_argument_slots: Vec::new(),
        given_away: None,
        in_cleanup: false,
        guarded: false,
        ret_mode: PassMode::Ignore,
        tracked,
        variadic: false,
        abi: ExternAbi::Rust,
        location: None,
        span: MirSpan::Unknown,
        constant_locals: Vec::new(),
        registers: FxHashMap::default(),
        args: None,
        suspended: None,
        held_at: Vec::new(),
    };
    fill(&mut fx)?;
    fx.note_variables();
    fx.builder.seal_all_blocks();
    let config = fx.cg.target_config();
    let guarded = fx.guarded;
    fx.builder.finalize(config);
    if guarded && let Some(entry) = ctx.func.layout.entry_block() {
        // The slot a guarded call looks for an exception in starts empty.
        use cranelift_codegen::cursor::{Cursor as _, FuncCursor};
        let mut cursor = FuncCursor::new(&mut ctx.func).at_first_insertion_point(entry);
        let frame = cursor.ins().get_frame_pointer(ptr());
        let none = cursor.ins().iconst(ptr(), 0);
        cursor.ins().store(MEM, none, frame, unwind::seh::ARRIVAL);
    }
    Ok(ctx)
}

/// The body of a small function, built to be put where the function is called.
#[salsa::tracked(returns(ref), no_eq, cycle_result = inlinable_cycle)]
fn inlinable(
    db: &dyn HirDatabase,
    instance: Instance,
    env: StoredParamEnvAndCrate,
) -> Option<fragment::Inlinable> {
    tier::plain(|| small_body(db, instance, env))
}

fn small_body(
    db: &dyn HirDatabase,
    instance: Instance,
    env: StoredParamEnvAndCrate,
) -> Option<fragment::Inlinable> {
    /// The most blocks and statements the body of a function that is inlined has, and
    /// the most instructions it is built to with what it calls inlined into it.
    const BLOCKS: usize = 4;
    const STATEMENTS: usize = 12;
    const INSTRUCTIONS: usize = 64;
    let InstanceDef::Fn(func) = instance.def else { return None };
    let generic = db.mir_body(func.into()).ok()?;
    let statements: usize = generic.basic_blocks.iter().map(|it| it.1.statements.len()).sum();
    if generic.basic_blocks.len() > BLOCKS
        || statements > STATEMENTS
        || !generic.closures.is_empty()
        || generic.coroutine.is_some()
    {
        return None;
    }
    let body = instance_body(db, &instance, env.clone()).ok()?;
    let mut cg = Recorder::new(db, &**target().ok()?, selected_target().ok()?, instance.checked);
    cg.unlocated = true;
    let args = instance.args.clone();
    let mut ctx = build(&mut cg, env.as_ref(db), Some(&*body), false, |fx| {
        fx.args = Some(args);
        fx.codegen()
    })
    .ok()?;
    if cg.naked.is_some() || cg.opaque {
        return None;
    }
    inline(&mut ctx, &cg.inlined).ok()?;
    if ctx.func.dfg.num_insts() > INSTRUCTIONS {
        return None;
    }
    let pointees = std::mem::take(&mut cg.pointees);
    fragment::Inlinable::of(ctx.func, cg.finish().0, pointees, Vec::new())
}

fn inlinable_cycle(
    _: &dyn HirDatabase,
    _: salsa::Id,
    _: Instance,
    _: StoredParamEnvAndCrate,
) -> Option<fragment::Inlinable> {
    None
}

/// Puts the bodies of `inlined` where `ctx` calls them.
fn inline(ctx: &mut cranelift_codegen::Context, inlined: &[ir::Function]) -> Result<()> {
    use cranelift_codegen::inline::{Inline, InlineCommand};
    struct Bodies<'a> {
        bodies: &'a [ir::Function],
        put: Vec<Put>,
    }
    /// A body that was put where it was called.
    struct Put {
        body: usize,
        /// How many global values and instructions the caller had before.
        globals: usize,
        insts: usize,
        /// The statement the call was built for.
        statement: ir::SourceLoc,
    }
    impl Inline for Bodies<'_> {
        fn inline(
            &mut self,
            caller: &ir::Function,
            call: ir::Inst,
            _: ir::Opcode,
            callee: ir::FuncRef,
            _: &[Value],
        ) -> InlineCommand<'_> {
            let ir::ExternalName::User(name) = caller.dfg.ext_funcs[callee].name else {
                return InlineCommand::KeepCall;
            };
            let name = &caller.params.user_named_funcs()[name];
            if name.namespace != fragment::INLINED {
                return InlineCommand::KeepCall;
            }
            let body = name.index as usize;
            self.put.push(Put {
                body,
                globals: caller.global_values.len(),
                insts: caller.dfg.num_insts(),
                statement: caller.srcloc(call),
            });
            InlineCommand::Inline { callee: Cow::Borrowed(&self.bodies[body]), visit_callee: true }
        }
    }
    if inlined.is_empty() {
        return Ok(());
    }
    let mut bodies = Bodies { bodies: inlined, put: Vec::new() };
    ctx.inline(&mut bodies).map_err(|err| CodegenError::Unsupported(err.to_string()))?;
    // Cranelift copies a symbol of the callee with the name it has among the callee's
    // names, which among the caller's is another symbol's.
    let ends = bodies.put.iter().skip(1).map(|it| it.insts).chain([ctx.func.dfg.num_insts()]);
    for (put, end) in bodies.put.iter().zip(ends) {
        // What a body is built to is of the statement that called it.
        for inst in put.insts..end {
            ctx.func.set_srcloc(ir::Inst::from_u32(inst as u32), put.statement);
        }
        let body = &inlined[put.body];
        for (global, data) in body.global_values.iter() {
            let ir::GlobalValueData::Symbol { name: ir::ExternalName::User(name), .. } = data
            else {
                continue;
            };
            let name = body.params.user_named_funcs()[*name].clone();
            let name = ctx.func.declare_imported_user_function(name);
            let copy = ir::GlobalValue::from_u32(put.globals as u32 + global.as_u32());
            let ir::GlobalValueData::Symbol { name: copied, .. } =
                &mut ctx.func.global_values[copy]
            else {
                unreachable!("a symbol is copied as one")
            };
            *copied = ir::ExternalName::User(name);
        }
    }
    Ok(())
}

/// Runs Cranelift on a finished function. Cranelift panics on IR it has no lowering for
/// (i128 corners, for one); that must cost us one function, not the process.
fn emit(ctx: &mut cranelift_codegen::Context) -> Result<Code> {
    if Target::current().is_wasm() {
        return emit_wasm(&ctx.func, Target::current().calls());
    }
    let large = ctx.func.dfg.num_insts() > SINGLE_PASS;
    let mut isa = &**if large { target_for_large()? } else { target()? };
    let func = &ctx.func;
    if !isa.flags().preserve_frame_pointers()
        && func.layout.entry_block().is_some_and(|entry| {
            func.layout
                .block_insts(entry)
                .any(|inst| func.dfg.insts[inst].opcode() == ir::Opcode::GetFramePointer)
        })
    {
        isa = &**target_with_frame_pointer()?;
    }
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match store::Store::global() {
            Some(store) => ctx
                .compile_with_cache(isa, &mut store::Lookup(store), &mut Default::default())
                .map(drop),
            None => ctx.compile(isa, &mut Default::default()).map(drop),
        }
        .map_err(|e| format!("{:?}", e.inner))?;
        backend::Tally::global().passes();
        let Some(compiled) = ctx.compiled_code() else {
            return Err("no compiled code".to_owned());
        };
        // Relocations against the function itself get its id when it is stitched.
        let own = FuncId::from_u32(u32::MAX);
        Ok(Code {
            signature: ctx.func.signature.clone(),
            bytes: compiled.buffer.data().to_vec(),
            align: compiled.buffer.alignment.into(),
            relocs: compiled
                .buffer
                .relocs()
                .iter()
                .map(|reloc| ModuleReloc::from_mach_reloc(reloc, &ctx.func, own))
                .collect(),
            elf_types: Vec::new(),
            unwind: FunctionUnwind::of(isa, ctx)?,
            lines: LineRow::of_ranges(
                compiled.buffer.get_srclocs_sorted().iter().map(|it| (it.start, it.loc)),
            ),
            slots: SlotPlaces::default(),
        })
    }));
    match outcome {
        Ok(Ok(code)) => Ok(code),
        Ok(Err(e)) => Err(CodegenError::Backend(e)),
        Err(panic) => {
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("unknown");
            Err(CodegenError::Backend(format!("backend panic: {message}")))
        }
    }
}

/// Lowers a finished function to WebAssembly. The bytes are a [`wasm::WasmFunction`], which
/// goes through the object like machine code and is made a module of by a linker.
fn emit_wasm(func: &cranelift_codegen::ir::Function, calls: Calls) -> Result<Code> {
    let (function, mut relocs) = wasm::lower(func, calls).map_err(CodegenError::Unsupported)?;
    let (bytes, body) = function.encode().map_err(CodegenError::Backend)?;
    for reloc in &mut relocs {
        reloc.offset += body;
    }
    Ok(Code {
        signature: func.signature.clone(),
        bytes,
        align: 1,
        relocs,
        elf_types: Vec::new(),
        unwind: None,
        lines: Vec::new(),
        slots: SlotPlaces::default(),
    })
}

/// The body of a non-generic function, with the types it only knows as `impl Trait`
/// resolved.
fn root_body<'db>(
    db: &'db dyn HirDatabase,
    func: FunctionId,
) -> Result<(&'db MirBody<'db>, ParamEnvAndCrate<'db>)> {
    let owner: InferBodyId<'db> = func.into();
    let tally = backend::Tally::global();
    tally.step(Step::Infer, || _ = InferenceResult::of(db, DefWithBodyId::from(func)));
    let body = tally.step(Step::Mir, || compiled_mir_body(db, owner));
    let body = body.map_err(|e| CodegenError::Frontend(format!("mir: {e:?}")))?;
    let krate = owner.module(db).krate(db);
    // Not the function's own bounds: with nothing generic they are bounds on concrete
    // types, which hold through impls, and the calls have to resolve to those.
    let env = ParamEnvAndCrate { param_env: crate::next_solver::ParamEnv::empty(), krate };
    // The future an `async fn` returns is `impl Future` to the body that calls it.
    let opaque = body.locals.iter().any(|(_, local)| {
        rustc_type_ir::TypeVisitableExt::has_type_flags(
            &local.ty.as_ref(),
            rustc_type_ir::TypeFlags::HAS_TY_OPAQUE,
        )
    });
    let body = if opaque {
        let args = GenericArgs::new_from_slice(&[]).store();
        tally
            .step(Step::Monomorphize, || match mir_opt_level() {
                MirOptLevel::Off => db.monomorphized_runtime_mir_body(owner, args, env.store()),
                level => db.monomorphized_optimized_mir_body(owner, level, args, env.store()),
            })
            .map_err(|e| CodegenError::Frontend(format!("mir: {e:?}")))?
    } else {
        body
    };
    Ok((body, env))
}

impl<'a, 'db> ObjectCodegen<'a, 'db> {
    fn new(
        db: &'db dyn HirDatabase,
        symbols: &'a dyn SymbolResolver,
        rejected: &'a FxHashMap<Instance, Rejected>,
        own_statics: OwnStatics,
        on_demand: &'a FxHashSet<FunctionId>,
        base: Option<&'a patch::Base>,
        hydrate: bool,
    ) -> Result<Self> {
        let start = stdx::time::Instant::now();
        let builder = ObjectBuilder::new(target()?.clone(), "ra-jit", default_libcall_names())?;
        let mut module = ObjectModule::new(builder);
        let unwind = UnwindContext::new(&mut module).map_err(CodegenError::Backend)?;
        let built = start.elapsed();
        // A patch is added to a program that is linked already, and is read with it.
        let apart = symbols.debug_apart().filter(|_| base.is_none());
        let debug = symbols.debug_directory().and_then(|directory| {
            DebugContext::new(module.isa(), &directory, symbols.describes_variables(), apart)
                .map(debuginfo::Describer::new)
        });
        let debugged = debug.is_some();
        let described = start.elapsed();
        let mixed = settled::Mixed::of(db, symbols, &own_statics.crates);
        let mixed_at = start.elapsed();
        // Without an identity for such a program, nothing of it is kept or found.
        let program = mixed.as_ref().map(|it| it.key);
        let store = store::Store::global().filter(|_| program != Some(None));
        if let Some(store) = store {
            store.settled();
        }
        let settled_at = start.elapsed();
        let names = stable::StableNames::new(db, symbols, own_statics.crates.clone(), hydrate);
        if std::env::var_os("RA_JIT_STATS").is_some() {
            let us = |it: std::time::Duration| it.as_micros();
            eprintln!(
                "setup-us: module={} debug={} mixed={} settled={} names={}",
                us(built),
                us(described - built),
                us(mixed_at - described),
                us(settled_at - mixed_at),
                us(start.elapsed() - settled_at),
            );
        }
        Ok(ObjectCodegen {
            db,
            symbols,
            module,
            unwind,
            debug,
            told: TOLD_UNNAMED.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default(),
            writer: None,
            imports: FxHashMap::default(),
            assemblies: FxHashMap::default(),
            instances: FxHashMap::default(),
            pending: Vec::new(),
            envs: Vec::new(),
            homes: home::Homes::default(),
            ahead: FxHashSet::default(),
            forwarded: FxHashSet::default(),
            rustc_futures: FxHashSet::default(),
            patching: false,
            base,
            replaced: Default::default(),
            own_static_order: Vec::new(),
            on_demand,
            wanted: Vec::new(),
            on_demand_crates: on_demand.iter().map(|func| func.krate(db)).collect(),
            rejected,
            layouts: FxHashMap::default(),
            phases: Phases::default(),
            shapes: std::env::var_os("RA_JIT_SHAPES")
                .and_then(|path| {
                    use std::io::Write;
                    let mut file =
                        std::fs::File::options().append(true).create(true).open(path).ok()?;
                    writeln!(file, "R").ok()?;
                    Some(file)
                })
                .map(std::io::BufWriter::new),
            bodies: 0,
            vtables: FxHashMap::default(),
            statics: FxHashMap::default(),
            frozen_statics: FxHashMap::default(),
            imported_statics: FxHashMap::default(),
            consts: FxHashMap::default(),
            source_paths: FxHashMap::default(),
            builder_ctx: FunctionBuilderContext::new(),
            location_counter: 0,
            names,
            hydrate,
            stable_names: FxHashMap::default(),
            kept: Kept::default(),
            // A patch gets these from its base, and a check compiles them to compare.
            entries: store
                .filter(|_| base.is_none() && std::env::var_os("RA_JIT_CACHE_CHECK").is_none())
                .map(|store| {
                    let (home, library) = (symbols.cargo_home(), symbols.library_sources());
                    std::sync::Arc::new(cached::Entries::new(
                        store,
                        debugged,
                        program.flatten(),
                        home,
                        library,
                    ))
                }),
            named: FxHashMap::default(),
            found: Vec::new(),
            found_pending: Vec::new(),
            named_vtables: FxHashMap::default(),
            type_id_shims: FxHashMap::default(),
            type_ids: FxHashMap::default(),
            misfit: false,
            own_statics,
            mixed,
            elf_relocs: Vec::new(),
            tiers: None,
            found_instances: FxHashMap::default(),
        })
    }

    /// The object, and the files of what a debugger reads that is kept apart from it.
    fn finish(mut self) -> Result<(Vec<u8>, Vec<Sidecar>)> {
        if Target::current().writes_wasm_objects() {
            return Ok((self.finish_wasm()?, Vec::new()));
        }
        self.unwind.name_personality(&mut self.module).map_err(CodegenError::Backend)?;
        let mut product = self.module.finish();
        for reloc in &self.elf_relocs {
            reloc.add(&mut product).map_err(CodegenError::Backend)?;
        }
        self.unwind.emit(&mut product).map_err(CodegenError::Backend)?;
        let apart = match self.debug {
            Some(debug) => backend::Tally::global()
                .step(Step::Describe, || debug.emit(&mut product))
                .map_err(CodegenError::Backend)?,
            None => Vec::new(),
        };
        // Freeing every type told of takes as long as writing the object: another thread does
        // it, where there is one to start.
        let told = std::mem::take(&mut self.told);
        if cfg!(target_family = "wasm") {
            drop(told);
        } else {
            std::thread::spawn(move || drop(told));
        }
        let macho = product.object.format() == cranelift_object::object::BinaryFormat::MachO;
        let mut object = product.emit().map_err(|e| CodegenError::Backend(e.to_string()))?;
        if macho {
            debuginfo::settle_macho(&mut object).map_err(CodegenError::Backend)?;
        }
        Ok((object, apart))
    }

    /// The object as a relocatable wasm object. Nothing unwinds there, so there are no
    /// unwind tables to add.
    fn finish_wasm(self) -> Result<Vec<u8>> {
        let mut signatures = FxHashMap::default();
        for (_, declared) in self.module.declarations().get_functions() {
            let Some(name) = &declared.name else { continue };
            let signature =
                wasm::WasmSignature::of(&declared.signature).map_err(CodegenError::Backend)?;
            signatures.insert(name.clone(), signature);
        }
        let elf = self.module.finish().emit().map_err(|e| CodegenError::Backend(e.to_string()))?;
        wasm_object::from_elf(&elf, &signatures).map_err(CodegenError::Backend)
    }

    fn want(&mut self, func: FunctionId) {
        if self.on_demand.contains(&func) {
            self.wanted.push(func);
        }
    }

    /// Wants the functions that `func` and the instances it calls refer to, for when `func`
    /// itself is left to rustc. `false` when the body can't be compiled to find them.
    fn want_reachable_from(&mut self, func: FunctionId) -> bool {
        let db = self.db;
        let Ok(compiled) = compiled_root(db, func).as_ref() else { return false };
        self.want_requested(&compiled.requests);
        true
    }

    /// Wants the functions the requests and the instances they name refer to, without putting
    /// any of it in the object.
    fn want_requested(&mut self, requests: &'db Requests) {
        let db = self.db;
        let (mut seen, mut seen_statics) = (FxHashSet::default(), FxHashSet::default());
        let mut stack = vec![requests];
        while let Some(requests) = stack.pop() {
            for request in &requests.funcs {
                let FuncTarget::Instance(instance, env) = &request.target else { continue };
                match instance.def {
                    InstanceDef::Fn(func) if instance.args.as_ref().is_empty() => self.want(func),
                    _ if !seen.insert(instance) => {}
                    _ => {
                        let compiled = compiled_instance(db, instance.clone(), env.clone());
                        stack.extend(compiled.as_ref().ok().map(|it| &it.requests));
                    }
                }
            }
            for request in &requests.datas {
                if let DataRequest::Static(id) = request
                    && seen_statics.insert(*id)
                    && let Ok(compiled) = compiled_static(db, *id).as_ref()
                {
                    stack.push(&compiled.requests);
                }
            }
        }
    }

    /// Puts a non-generic function in the object, exported under its rustc symbol name.
    fn define_function(&mut self, func: FunctionId) -> Result<String> {
        let db = self.db;
        db.mir_body(func.into()).map_err(|e| CodegenError::Frontend(format!("mir: {e:?}")))?;
        let Some(symbol) = self.symbols.function(func) else {
            // FIXME: with a `main` of ours only what it reaches is compiled, and a function
            // left to rustc can still call one private to its object.
            let private = self.on_demand_crates.is_empty() && self.symbols.private(func);
            return Err(if private { CodegenError::Private } else { CodegenError::NoSymbol });
        };
        let start = stdx::time::Instant::now();
        let tiers = self.tiers;
        let tiered = tiers.and_then(|it| it.code(db, &tier::Node(tier::Kind::Root(func))));
        let compiled = match tiered {
            Some(code) => code,
            None => compiled_root(db, func).as_ref().map_err(Clone::clone)?,
        };
        let took = start.elapsed();
        self.phases.compile += took;
        let id =
            self.module.declare_function(&symbol, Linkage::Export, &compiled.code.signature)?;
        self.shape_origin(id, InstanceDef::Fn(func), &func, false, took);
        self.stitch(id, compiled)?;
        let name = super::type_name::rustc_debug_function(db, func, &[])
            .unwrap_or_else(|| FunctionSignature::of(db, func).name.as_str().to_owned());
        let declared = self.debug_declaration_at(func);
        // The rows of the Cranelift code are not those of LLVM's.
        let rows = self.debug.as_ref().filter(|_| tiered.is_none()).map(|_| root_rows(db, func));
        self.describe(id, compiled, rows, FunctionDebug::new(name, symbol.clone()), declared);
        Ok(symbol)
    }

    /// Puts the `main` the C runtime calls in the object.
    fn define_start(&mut self, main: FunctionId) -> Result<()> {
        let compiled = compiled_start(self.db, main).as_ref().map_err(Clone::clone)?;
        let id = self.module.declare_function("main", Linkage::Export, &compiled.code.signature)?;
        self.stitch(id, compiled)?;
        Ok(())
    }

    /// Puts the instances requested by calls in the object. Returns the ones that failed.
    fn define_instances(&mut self) -> Vec<(Instance, Rejected)> {
        let mut failed = Vec::new();
        self.compile_ahead(Vec::new(), &mut |cg, ahead| failed.extend(cg.define_waves(ahead)));
        failed
    }

    /// Puts what is pending in the object, and what that calls. With `ahead`, the other
    /// threads are compiling all of it, and an instance is put in the object once they have
    /// it. Without, each wave is compiled on every core before it is put in the object.
    fn define_waves(&mut self, mut ahead: Option<&mut Ready<'_>>) -> Vec<(Instance, Rejected)> {
        let mut failed = Vec::new();
        loop {
            let wave = match &mut ahead {
                Some(ahead) => ahead.take(&mut self.pending),
                None if self.pending.is_empty() => Vec::new(),
                None => {
                    let wave = std::mem::take(&mut self.pending);
                    let start = stdx::time::Instant::now();
                    let told = self.debug.is_some().then_some(&self.told);
                    self.symbols.in_parallel(wave.len(), &|db, _, at| {
                        let (instance, env, _) = &wave[at];
                        let compiled = compiled_instance(db, instance.clone(), env.clone());
                        if let (Ok(compiled), Some(told)) = (compiled.as_ref(), told) {
                            told.ahead(db, compiled);
                            instance_rows(db, instance.clone(), env.clone());
                        }
                    });
                    self.phases.compile += start.elapsed();
                    wave
                }
            };
            if wave.is_empty() && self.found_pending.is_empty() {
                break;
            }
            for (instance, env, id) in wave {
                if let Err(e) = self.define_instance(&instance, &env, id) {
                    if self.forward_to_rustc(&instance, id).is_some() {
                        self.forwarded.insert(instance);
                    } else {
                        failed.push((instance, Rejected { env, why: e.to_string() }));
                    }
                }
            }
            let start = stdx::time::Instant::now();
            while let Some((name, id)) = self.found_pending.pop() {
                if let Err(e) = self.stitch_entry(&name, id) {
                    eprintln!("entry of `{name}` does not fit: {e}");
                    cached::Entries::ban(&name);
                    self.misfit = true;
                }
            }
            self.phases.define += start.elapsed();
        }
        failed
    }

    /// Compiles what is pending, what the compiled `roots` call and everything those call, on
    /// every core. A thread goes on to what an instance calls as soon as that instance is
    /// compiled, where the waves of [`Self::define_instances`] each wait for their slowest
    /// function. What this compiles that the object has no use for is only wasted. `beside`
    /// runs on this thread meanwhile, and is told whether anything is compiled ahead of it.
    fn compile_ahead(
        &mut self,
        roots: Vec<FunctionId>,
        beside: &mut dyn FnMut(&mut Self, Option<&mut Ready<'_>>),
    ) {
        // A patch calls the base's copy of most of what it reaches.
        if self.base.is_some() || (self.pending.is_empty() && roots.is_empty()) {
            return beside(self, None);
        }
        /// Tells the threads that wait when the one compiling is stopped.
        struct Working<'a>(&'a std::sync::Mutex<Ahead>, &'a std::sync::Condvar);
        impl Drop for Working<'_> {
            fn drop(&mut self) {
                if std::thread::panicking() {
                    self.0.lock().unwrap_or_else(|e| e.into_inner()).stopped = true;
                    self.1.notify_all();
                }
            }
        }
        let mut seen = std::mem::take(&mut self.ahead);
        let queue: Vec<_> = self
            .pending
            .iter()
            .map(|(instance, env, _)| (instance.clone(), env.clone()))
            .filter(|it| seen.insert(it.clone()))
            .collect();
        let ahead = std::sync::Mutex::new(Ahead {
            rooted: roots.iter().copied().collect(),
            roots,
            queue,
            seen,
            done: FxHashSet::default(),
            fresh: Vec::new(),
            working: 0,
            stopped: false,
        });
        let idle = std::sync::Condvar::new();
        let (rejected, hydrate) = (self.rejected, self.hydrate);
        let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
        let (entries, owned) = (self.entries.clone(), &self.own_statics.crates.clone());
        let entries = entries.as_deref();
        let told = self.debug.is_some().then(|| self.told.clone());
        let told = told.as_ref();
        let on_demand = self.on_demand;
        let work = |db: &dyn HirDatabase, symbols: &dyn SymbolResolver, _: usize| {
            let names = stable::StableNames::new(db, symbols, owned.clone(), hydrate);
            // What the store has is not compiled, nor looked into for what it calls.
            let in_store = |callee: &Instance| {
                let Some(entries) = entries else { return false };
                names
                    .instance(callee)
                    .is_some_and(|name| entries.usable(&name, &|symbol| symbols.defines(symbol)))
            };
            let mut homes = home::Homes::default();
            let mut state = ahead.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if state.stopped {
                    return;
                }
                let job = match state.roots.pop() {
                    Some(root) => either::Either::Left(root),
                    None => match state.queue.pop() {
                        Some(it) => either::Either::Right(it),
                        None if state.working == 0 => {
                            idle.notify_all();
                            return;
                        }
                        None => {
                            state = idle.wait(state).unwrap_or_else(|e| e.into_inner());
                            continue;
                        }
                    },
                };
                state.working += 1;
                drop(state);
                let working = Working(&ahead, &idle);
                let (mut found, mut called) = (Vec::new(), Vec::new());
                let compiled = match &job {
                    // One without a symbol is left to rustc, and so is what only it calls.
                    either::Either::Left(root) => symbols.function(*root).and_then(|_| {
                        if told.is_some() {
                            root_rows(db, *root);
                        }
                        compiled_root(db, *root).as_ref().ok()
                    }),
                    either::Either::Right((instance, env)) => {
                        let compiled = compiled_instance(db, instance.clone(), env.clone());
                        let compiled = compiled.as_ref().ok();
                        if let (Some(compiled), Some(told)) = (compiled, told) {
                            told.ahead(db, compiled);
                            instance_rows(db, instance.clone(), env.clone());
                        }
                        compiled
                    }
                };
                for request in compiled.iter().flat_map(|it| &it.requests.funcs) {
                    let FuncTarget::Instance(callee, asked) = &request.target else {
                        continue;
                    };
                    // A function without generic arguments is a root, called by its
                    // symbol when rustc compiled it. The thread that puts the caller in
                    // the object asks for the symbol, and finds it here.
                    let root = match callee.def {
                        InstanceDef::Fn(func)
                            if callee.args.as_ref().iter().all(|it| it.region().is_some()) =>
                        {
                            // One compiled only when called is, by the thread that puts
                            // its caller in the object: it is compiled for it here.
                            if on_demand.contains(&func) {
                                called.push(func);
                            }
                            symbols.function(func).is_some()
                        }
                        _ => false,
                    };
                    if !root && !rejected.contains_key(callee) && !in_store(callee) {
                        found.push((callee.clone(), homes.home(db, callee, asked)));
                    }
                }
                drop(working);
                state = ahead.lock().unwrap_or_else(|e| e.into_inner());
                state.working -= 1;
                if let either::Either::Right(it) = job {
                    state.done.insert(it.clone());
                    state.fresh.push(it);
                }
                for it in found {
                    if state.seen.insert(it.clone()) {
                        state.queue.push(it);
                    }
                }
                for root in called {
                    if state.rooted.insert(root) {
                        state.roots.push(root);
                    }
                }
                idle.notify_all();
            }
        };
        let mut ready = Ready { ahead: &ahead, idle: &idle, waiting: FxHashMap::default() };
        self.symbols.in_parallel_beside(threads, &work, &mut || beside(self, Some(&mut ready)));
        self.ahead = ahead.into_inner().unwrap_or_else(|e| e.into_inner()).seen;
    }

    fn define_instance(
        &mut self,
        instance: &Instance,
        env: &StoredParamEnvAndCrate,
        id: FuncId,
    ) -> Result<()> {
        let start = stdx::time::Instant::now();
        let tiers = self.tiers;
        let node = tier::Node(tier::Kind::Instance(instance.clone(), env.clone()));
        let tiered = tiers.and_then(|it| it.code(self.db, &node));
        let compiled = match tiered {
            Some(code) => code,
            None => compiled_instance(self.db, instance.clone(), env.clone())
                .as_ref()
                .map_err(Clone::clone)?,
        };
        let took = start.elapsed();
        self.phases.compile += took;
        let generic = instance.args.as_ref().iter().any(|arg| arg.ty().is_some());
        self.shape_origin(id, instance.def, &(instance, env), generic, took);
        // The call site declared the instance with the signature it computed from its operands.
        let declared = &self.module.declarations().get_function_decl(id).signature;
        let has_body = matches!(
            instance.def,
            InstanceDef::Fn(_)
                | InstanceDef::Closure(_)
                | InstanceDef::Coroutine(_)
                | InstanceDef::CoroutineByMove(_)
        );
        if has_body && *declared != compiled.code.signature {
            unsupported!(
                "call site and body disagree on the signature: {declared} and {}",
                compiled.code.signature
            );
        }
        let ids = self.stitch(id, compiled)?;
        let name = self.backtrace_name(instance);
        let declared = self.written_at(instance, env.krate);
        let known = match instance.def {
            InstanceDef::Fn(func) => super::type_name::rustc_debug_function(
                self.db,
                func,
                instance.args.as_ref().as_slice(),
            ),
            InstanceDef::Closure(id) => {
                let at = InternedClosureId::from_id(id).loc(self.db);
                self.debug_closure(instance, at.owner, at.expr)
            }
            InstanceDef::Coroutine(id) | InstanceDef::CoroutineByMove(id) => {
                let at = InternedCoroutineId::from_id(id).loc(self.db);
                self.debug_closure(instance, at.owner, at.expr)
            }
            _ => None,
        };
        let known = known.unwrap_or_else(|| name.clone());
        let rows = self
            .debug
            .as_ref()
            .filter(|_| tiered.is_none())
            .map(|_| instance_rows(self.db, instance.clone(), env.clone()));
        let debug = self.describe(id, compiled, rows, FunctionDebug::new(known, name), declared);
        // Code of a level above 1 is of a profile, which is no key of the store.
        if tiered.is_none() {
            let tally = backend::Tally::global();
            tally.step(Step::StoreWrite, || self.keep(instance, compiled, &ids, debug));
        }
        Ok(())
    }

    /// Where what `instance` is the code of is written, which is the line of whatever of its
    /// code is of no statement: a function where it is declared, a closure or an `async`
    /// block where it stands, and a shim where the function it stands in for is declared, as
    /// rustc has it.
    fn written_at(&mut self, instance: &Instance, krate: Crate) -> Option<(String, u32, u32)> {
        self.debug.as_ref()?;
        let db = self.db;
        let lang_items = DbInterner::new_with(db, krate).lang_items();
        let ty = || instance.args.as_ref().as_slice().first().and_then(|arg| arg.ty());
        let of_expr = |owner: InferBodyId<'db>, expr| match owner {
            InferBodyId::DefWithBodyId(owner) => Some((owner, MirSpan::ExprId(expr))),
            _ => None,
        };
        let func = match instance.def {
            InstanceDef::Fn(func) => Some(func),
            InstanceDef::Closure(id) => {
                let at = InternedClosureId::from_id(id).loc(db);
                return self.location_at(of_expr(at.owner, at.expr));
            }
            InstanceDef::Coroutine(id) | InstanceDef::CoroutineByMove(id) => {
                let at = InternedCoroutineId::from_id(id).loc(db);
                return self.location_at(of_expr(at.owner, at.expr));
            }
            InstanceDef::DropGlue => lang_items.DropInPlace.or(lang_items.DropGlue),
            InstanceDef::CallOnceShim => lang_items.FnOnce_call_once,
            InstanceDef::VtableShim(func) => Some(func),
            InstanceDef::FnPointerShim => match ty()?.kind() {
                TyKind::Closure(id, _) => {
                    let at = id.0.loc(db);
                    return self.location_at(of_expr(at.owner, at.expr));
                }
                _ => None,
            },
            InstanceDef::ReifyShim { .. } => match ty()?.kind() {
                TyKind::FnDef(def, _) => match def.0 {
                    CallableDefId::FunctionId(func) => Some(func),
                    constructor => {
                        return constructor_location(db, constructor).and_then(
                            |(file, line, col)| Some((self.source_path(file)?, line, col)),
                        );
                    }
                },
                _ => lang_items.FnOnce_call_once,
            },
        };
        self.debug_declaration_at(func?)
    }

    /// Tells the debugger about the compiled function `id`: what it is called, as `debug`
    /// has it, and which line each of its instructions is of, as `rows` have it. `None`
    /// when no debugger is told anything, which is when there are no `rows`.
    fn describe(
        &mut self,
        id: FuncId,
        compiled: &CompiledFunction,
        rows: Option<&Vec<Row>>,
        mut debug: FunctionDebug,
        declared: Option<(String, u32, u32)>,
    ) -> Option<FunctionDebug> {
        let rows = rows?;
        let start = stdx::time::Instant::now();
        if let Some((file, line, column)) = declared {
            debug.declared = Some(debug.place(file, line, column));
        }
        for &(offset, file, line, column) in rows {
            let Some(file) = self.source_path(file) else { continue };
            let place = debug.place(file, line, column);
            debug.row(offset, place);
        }
        // Compiled code of no statement with a place is of where the function is written
        // throughout. A function that is its assembly has no lines, as rustc gives it none.
        if let Some(declared) = debug.declared
            && debug.rows.is_empty()
            && compiled.code.unwind.is_some()
        {
            debug.row(0, declared);
        }
        if let Some((owner, _)) = &compiled.source
            && let Some((env, variables)) = &compiled.variables
        {
            let places = &compiled.code.slots;
            let db = self.db;
            let len = compiled.code.bytes.len() as u32;
            let mut rows: Vec<(u32, MirSpan)> = match &compiled.source {
                Some((_, spans)) => (compiled.code.lines.iter())
                    .filter_map(|row| Some((row.offset, *spans.get(row.span as usize)?)))
                    .collect(),
                None => Vec::new(),
            };
            rows.sort_by_key(|row| row.0);
            let scopes = describe_scopes(db, *owner, variables, &rows, len, &mut debug);
            describe_variables(
                db,
                *owner,
                (&self.told, env),
                variables,
                places,
                &scopes,
                &mut debug,
            );
        }
        let len = compiled.code.bytes.len() as u32;
        self.debug.as_ref()?.add_function(id, len, debug.clone());
        backend::Tally::global().add(Step::Describe, start.elapsed());
        Some(debug)
    }

    fn stable_name(&mut self, instance: &Instance) -> Option<StableName> {
        if let Some(known) = self.stable_names.get(instance) {
            return known.clone();
        }
        let name = self.names.instance(instance).map(StableName::from);
        self.stable_names.insert(instance.clone(), name.clone());
        name
    }

    /// Adds a stitched instance to the store, when everything it is and refers to has a name
    /// that another run of the daemon can find it by.
    fn keep(
        &mut self,
        instance: &Instance,
        compiled: &CompiledFunction,
        ids: &Resolved,
        debug: Option<FunctionDebug>,
    ) {
        // What a patch refers to is in its base under names of that one object.
        let program = self.mixed.as_ref().map(|it| it.key);
        let store = store::Store::global().filter(|_| self.base.is_none() && program != Some(None));
        let Some(store) = store else { return };
        let Some(name) = self.stable_name(instance) else {
            self.kept.unnamed += 1;
            return;
        };
        let key = cached::Entry::key(&name, debug.is_some(), program.flatten().as_ref());
        let Some(key) = key else { return };
        let check = std::env::var_os("RA_JIT_CACHE_CHECK");
        let sets = || debug.iter().flat_map(|it| &it.types).cloned().collect::<Vec<_>>();
        if check.is_none() && store.has(&key) {
            let kept = cached::Kept::Types(sets());
            return self.writer(store).keep(kept);
        }
        let Some(entry) = self.entry(instance, compiled, ids, debug) else {
            self.kept.unnamed_requests += 1;
            return;
        };
        let mut entry = Box::new(entry);
        let Some(check) = check else {
            return self.writer(store).keep(cached::Kept::Entry { key, entry });
        };
        // The entry as it would be written is set against the one that is there.
        let stored = store.get(&key);
        let mut alone =
            cached::Writer::alone(store, self.symbols.cargo_home(), self.symbols.library_sources());
        let Some(bytes) = alone.encoded(&mut entry) else {
            let _: Option<()> = self.kept.unkept(OF_CARGO);
            return;
        };
        match stored {
            None => {
                store.insert(&key, &bytes);
                self.kept.added += 1;
            }
            Some(stored) if stored == bytes => self.kept.same += 1,
            Some(stored) => {
                self.kept.differing += 1;
                eprintln!("entry of `{name}` is not what was compiled");
                if check != "1" {
                    let at = std::path::Path::new(&check).join(self.kept.differing.to_string());
                    let was = cached::Entry::decode(&stored);
                    _ = std::fs::write(at, format!("{name}\n{was:#?}\n{entry:#?}"));
                }
            }
        }
    }

    fn writer(&mut self, store: &'static store::Store) -> &mut cached::Writer {
        let symbols = self.symbols;
        self.writer.get_or_insert_with(|| {
            cached::Writer::new(store, symbols.cargo_home(), symbols.library_sources())
        })
    }

    /// Whether the instance is compiled with the flags of the crate its function is written
    /// in, as the copy under the symbol of the function is. A crate that uses an `#[inline]`
    /// function of another compiles a copy with its own.
    fn as_its_crate_compiles(&self, instance: &Instance) -> bool {
        let InstanceDef::Fn(func) = instance.def else { return false };
        crate_checks_overflow(self.db, func.krate(self.db)).is_none_or(|it| it == instance.checked)
    }

    /// The instance as the crate rustc had compile it does: `instance` is as the crate that
    /// asks for it would. See [`settled`].
    fn settled(&self, instance: &Instance) -> Result<Instance> {
        let Some(mixed) = &self.mixed else { return Ok(instance.clone()) };
        let (db, asking) = (self.db, instance.checked);
        let nameless = || {
            // Only the crates that made a copy of a function of its name, or of a closure
            // written in one, can have.
            let makers = home::written_in(db, instance).map(|func| {
                let name = FunctionSignature::of(db, func).name.as_str();
                let makers = self.symbols.makers(name).into_iter();
                makers.filter_map(|id| mixed.holder(db, id)).collect::<Vec<_>>()
            });
            let mentions = home::mentioned_crates(db, instance);
            let reachable = mixed.reachable(asking, mentions.as_ref(), makers.as_deref());
            let Some(checked) = reachable else {
                unsupported!(
                    "an instance with no name to find rustc's copies by, which a crate with \
                     other overflow checks may have compiled"
                );
            };
            Ok(checked)
        };
        let checked = match instance.def {
            InstanceDef::Fn(func) => self.settled_function(mixed, func, instance, &nameless)?,
            InstanceDef::Closure(_)
            | InstanceDef::Coroutine(_)
            | InstanceDef::CoroutineByMove(_) => nameless()?,
            // No arithmetic of their own, and what they call is settled when they call it.
            InstanceDef::DropGlue
            | InstanceDef::CallOnceShim
            | InstanceDef::VtableShim(_)
            | InstanceDef::FnPointerShim
            | InstanceDef::ReifyShim { .. } => asking,
        };
        if checked != asking && self.base.is_some() {
            unsupported!("a copy rustc compiled with other overflow checks, in a patch");
        }
        Ok(Instance { checked, ..instance.clone() })
    }

    fn settled_function(
        &self,
        mixed: &settled::Mixed,
        func: FunctionId,
        instance: &Instance,
        nameless: &dyn Fn() -> Result<bool>,
    ) -> Result<bool> {
        let (db, asking) = (self.db, instance.checked);
        let args = instance.args.as_ref();
        let checked = if !args.iter().all(|arg| arg.region().is_some()) {
            let Some(path) = self.instance_path(func, args) else { return nameless() };
            let copies = self.symbols.path(&path).map(|it| self.symbols.copies(&it));
            let holders: Vec<_> =
                copies.iter().flatten().filter_map(|(_, id)| mixed.holder(db, *id)).collect();
            let Some(checked) = mixed.reached(asking, &holders) else {
                unsupported!(
                    "`{path}`, which rustc compiled with and without overflow checks: which \
                     copy a crate without one of its own calls is rustc's to pick"
                );
            };
            checked
        } else if AttrFlags::query(db, func.into()).contains(AttrFlags::IS_INLINE) {
            asking
        } else {
            // One copy under the function's symbol when rustc made one. Without, rustc found
            // the function small enough to compile in each crate that calls it.
            let own = || self.symbols.function(func).is_some_and(|it| self.symbols.defines(&it));
            match crate_checks_overflow(db, func.krate(db)) {
                Some(written_with) => written_with && (asking || own()),
                None => asking,
            }
        };
        Ok(checked)
    }

    /// rustc's copy of the instance `symbol` is one of that is compiled as `checked` says.
    fn rustc_copy(&self, symbol: String, checked: bool) -> Option<String> {
        let Some(mixed) = &self.mixed else { return Some(symbol) };
        let copies = self.symbols.copies(&symbol);
        if copies.is_empty() {
            return Some(symbol);
        }
        let of = |id: u64| mixed.holder(self.db, id).is_some_and(|it| it.1 == checked);
        copies.into_iter().find(|it| of(it.1)).map(|it| it.0)
    }

    /// The function of the program that an instance without arguments is of, and its symbol.
    fn root_named(&self, name: &str) -> Option<(FunctionId, String)> {
        let of = self.names.functions_named(name);
        of.into_iter().find_map(|it| Some((it, self.symbols.function(it)?)))
    }

    fn entry(
        &mut self,
        instance: &Instance,
        compiled: &CompiledFunction,
        ids: &Resolved,
        debug: Option<FunctionDebug>,
    ) -> Option<cached::Entry> {
        use cached::{CachedData, CachedFunc, CachedTarget};
        let mut funcs = Vec::with_capacity(ids.funcs.len());
        for (request, &id) in compiled.requests.funcs.iter().zip(&ids.funcs) {
            let decl = self.module.declarations().get_function_decl(id);
            let target = if let Some(&hash) = self.type_ids.get(&id) {
                CachedTarget::TypeId(hash)
            } else if decl.linkage == Linkage::Import {
                // rustc's copy of a function of the checkout is of the checkout as rustc
                // built it, and uses rustc's copies of the statics.
                let symbol = decl.linkage_name(id).into_owned();
                let checked = self.symbols.defines(&symbol);
                let callee = match &request.target {
                    FuncTarget::Instance(callee, _) => {
                        let Ok(callee) = self.settled(callee) else {
                            return self.kept.unkept("unsettled callee");
                        };
                        Some(self.stable_name(&callee))
                    }
                    _ => None,
                };
                match callee {
                    // Ours to compile, though under a symbol: rustc built nothing of its crate.
                    Some(Some(name)) if !checked && stable::StableNames::of_checkout(&name) => {
                        if self.root_named(&name).is_none() {
                            return self.kept.unkept("unlisted function of the checkout");
                        }
                        CachedTarget::Root(name.as_ref().to_owned())
                    }
                    Some(name)
                        if name.as_ref().is_none_or(|it| stable::StableNames::of_checkout(it)) =>
                    {
                        return self.kept.unkept("rustc's copy of a function of the checkout");
                    }
                    _ => CachedTarget::Import { symbol, checked },
                }
            } else if let FuncTarget::Instance(callee, _) = &request.target {
                let Ok(callee) = &self.settled(callee) else {
                    return self.kept.unkept("unsettled callee");
                };
                let Some(name) = self.stable_name(callee) else {
                    return self.kept.unkept("unnamed callee");
                };
                // A function the program is built from has no entry: it is compiled anew.
                if stable::StableNames::of_checkout(&name)
                    && self.as_its_crate_compiles(callee)
                    && self.root_named(&name).is_some()
                {
                    CachedTarget::Root(name.as_ref().to_owned())
                } else {
                    CachedTarget::Named(name.as_ref().to_owned())
                }
            } else if let FuncTarget::Asm(text) = &request.target {
                CachedTarget::Asm(text.clone())
            } else {
                return self.kept.unkept("call of no instance");
            };
            funcs.push(CachedFunc { target, sig: request.sig.clone() });
        }
        // Only a crate of the checkout is compiled with where the checkout is.
        let placed =
            self.stable_name(instance).is_some_and(|it| stable::StableNames::of_checkout(&it));
        let home = self.symbols.cargo_home();
        let library = self.symbols.library_sources();
        let mut datas = Vec::with_capacity(ids.datas.len());
        for (request, &id) in compiled.requests.datas.iter().zip(&ids.datas) {
            if let DataRequest::Blob { blob, .. } | DataRequest::Vtable { blob, .. } = request {
                if placed && self.symbols.names_place(&blob.bytes) {
                    return self.kept.unkept("data with the place of the checkout");
                }
                if home.as_deref().is_some_and(|home| cached::holds(&blob.bytes, home)) {
                    return self.kept.unkept("data with the place of cargo's directory");
                }
                if library.as_ref().is_some_and(|it| cached::holds(&blob.bytes, &it.here)) {
                    return self.kept.unkept("data with the place of the library's sources");
                }
            }
            datas.push(match request {
                DataRequest::Blob { blob, identity } => {
                    CachedData::Blob { blob: blob.clone(), identity: *identity }
                }
                DataRequest::Vtable { ty, trait_ref, blob } => {
                    let trait_ref = trait_ref.as_ref().map(|(id, args)| (*id, args.as_ref()));
                    let Some(key) = self.names.vtable(ty.as_ref(), trait_ref) else {
                        return self.kept.unkept("unnamed vtable");
                    };
                    CachedData::Vtable { key, blob: blob.clone() }
                }
                DataRequest::Static(of) => {
                    let decl = self.module.declarations().get_data_decl(id);
                    // Whether a static of the checkout is rustc's or ours is up to the build.
                    let krate = of.krate(self.db);
                    let of_checkout = self.own_statics.crates.contains(&krate)
                        || krate.data(self.db).origin.is_local();
                    let foreign = static_link_name(self.db, *of).is_some();
                    if !foreign && (decl.linkage != Linkage::Import || of_checkout) {
                        let Some(name) = self.names.static_(*of) else {
                            return self.kept.unkept("unnamed static");
                        };
                        CachedData::OwnStatic(name)
                    } else {
                        CachedData::Static {
                            symbol: decl.linkage_name(id).into_owned(),
                            thread_local: decl.tls,
                        }
                    }
                }
                DataRequest::Location(at) => CachedData::Location(self.location_at(*at)),
                DataRequest::Declaration(func) => CachedData::Location(self.declaration_at(*func)),
                DataRequest::FilePath(file) => {
                    let Some(path) = self.source_path(*file) else {
                        return self.kept.unkept("file without a path");
                    };
                    CachedData::FilePath(path)
                }
                DataRequest::Text(parts) => {
                    let Some(text) = self.text(parts) else {
                        return self.kept.unkept("file without a path");
                    };
                    CachedData::FilePath(text)
                }
            });
        }
        Some(cached::Entry {
            label: self.callee_name(instance.def).to_owned(),
            funcs,
            datas,
            code: cached::CachedCode::from(&compiled.code),
            debug,
        })
    }

    /// The function of an entry and everything it calls, declared here and to be defined from
    /// the store. `None` when the store does not have all of it, or `body` is given and is
    /// not the signature the entry was compiled with.
    fn found_callee(&mut self, name: &str, body: Option<&Signature>) -> Result<Option<FuncId>> {
        if let Some(&id) = self.named.get(name) {
            return Ok(Some(id));
        }
        let Some(entries) = self.entries.clone() else { return Ok(None) };
        let symbols = self.symbols;
        if !entries.usable(name, &|symbol| symbols.defines(symbol)) {
            let why = match entries.get(name) {
                None => "no entry",
                Some(it) if !it.links(&|symbol| symbols.defines(symbol)) => {
                    "an entry linked against what the program lacks"
                }
                Some(_) => "an entry that calls what cannot be used",
            };
            return Ok(self.kept.unfound(why));
        }
        let Some(entry) = entries.get(name) else { return Ok(None) };
        if body.is_some_and(|it| it != entry.code.signature()) {
            return Ok(self.kept.unfound("an entry of another signature"));
        }
        let symbol = format!("ra_jit::{}::n{}", entry.label, self.found.len());
        let linkage = if self.patching { Linkage::Hidden } else { Linkage::Local };
        let id = self
            .module
            .declare_function(&symbol, linkage, entry.code.signature())
            .map_err(|e| CodegenError::Backend(e.to_string()))?;
        let name = StableName::from(name);
        self.named.insert(name.clone(), id);
        self.found.push((name.clone(), id));
        self.found_pending.push((name, id));
        Ok(Some(id))
    }

    /// Defines `id` as the function of an entry, and declares what the entry refers to.
    fn stitch_entry(&mut self, name: &str, id: FuncId) -> Result<()> {
        use cached::{CachedData, CachedTarget};
        let Some(entry) = self.entries.as_ref().and_then(|it| it.get(name)) else {
            unsupported!("an entry that was in the store is not");
        };
        let mut ids = Resolved {
            funcs: Vec::with_capacity(entry.funcs.len()),
            datas: Vec::with_capacity(entry.datas.len()),
        };
        for func in &entry.funcs {
            ids.funcs.push(match &func.target {
                CachedTarget::Import { symbol, .. } => self.import(symbol, &func.sig)?,
                CachedTarget::Asm(text) => self.assembly(text, &func.sig)?,
                CachedTarget::TypeId(hash) => self.type_id_shim(&func.sig, *hash)?,
                CachedTarget::Root(callee) => {
                    let Some((of, symbol)) = self.root_named(callee) else {
                        unsupported!("an entry calls `{callee}`, which no crate declares");
                    };
                    self.want(of);
                    self.import(&symbol, &func.sig)?
                }
                CachedTarget::Named(callee) => {
                    let Some(id) = self.found_callee(callee, None)? else {
                        unsupported!("an entry calls `{callee}`, which the store lacks");
                    };
                    id
                }
            });
        }
        for data in &entry.datas {
            let id = match data {
                CachedData::Blob { blob, identity } => {
                    self.define_data_as(ids.blob(blob), *identity)?
                }
                CachedData::Vtable { key, blob } => match self.named_vtables.get(key) {
                    Some(&id) => id,
                    None => {
                        let id = self.define_data_as(ids.blob(blob), DataIdentity::Own)?;
                        self.named_vtables.insert(key.clone(), id);
                        id
                    }
                },
                CachedData::Static { symbol, thread_local } => {
                    self.module.declare_data(symbol, Linkage::Import, false, *thread_local)?
                }
                CachedData::OwnStatic(name) => {
                    let Some(of) = self.names.static_named(name) else {
                        unsupported!("an entry refers to `{name}`, which no crate declares");
                    };
                    self.static_data(of)?
                }
                CachedData::Location(at) => self.define_location(at.clone())?,
                CachedData::FilePath(path) => self.define_file_path(path.clone())?,
            };
            ids.datas.push(id);
        }
        let code = entry.code.code();
        let relocs: Vec<_> = code.relocs.iter().map(|it| ids.reloc(it, id)).collect();
        self.define_code(id, &code, &relocs)?;
        if let (Some(debug), Some(of)) = (&self.debug, &entry.debug) {
            let len = code.bytes.len() as u32;
            backend::Tally::global()
                .step(Step::Describe, || debug.add_function(id, len, of.clone()));
        }
        self.bodies += 1;
        self.kept.found += 1;
        Ok(())
    }

    /// Defines `id` as a compiled function, and declares everything the function refers to.
    fn stitch(&mut self, id: FuncId, compiled: &CompiledFunction) -> Result<Resolved> {
        let start = stdx::time::Instant::now();
        let ids = self.resolve(&compiled.requests);
        self.phases.resolve += start.elapsed();
        let ids = ids?;
        let start = stdx::time::Instant::now();
        let relocs: Vec<_> = compiled.code.relocs.iter().map(|it| ids.reloc(it, id)).collect();
        self.shape_code(id, &compiled.code, &relocs);
        self.define_code(id, &compiled.code, &relocs)?;
        for (ty, size, align) in &compiled.layouts {
            self.layouts.entry(ty.clone()).or_insert((*size, *align));
        }
        self.bodies += 1;
        self.phases.define += start.elapsed();
        Ok(ids)
    }

    fn shape_origin(
        &mut self,
        id: FuncId,
        def: InstanceDef,
        // The symbol of an instance changes from one round to the next.
        identity: &impl std::hash::Hash,
        generic: bool,
        took: std::time::Duration,
    ) {
        use std::hash::Hasher;
        use std::io::Write;
        if self.shapes.is_none() {
            return;
        }
        let db = self.db;
        let (name, derive) = match def {
            InstanceDef::Fn(func) => {
                let derive = func.loc(db).id.file_id.macro_file().is_some_and(|call| {
                    matches!(call.loc(db).def.kind, MacroDefKind::BuiltInDerive(..))
                });
                (FunctionSignature::of(db, func).name.as_str().to_owned(), derive)
            }
            _ => (String::new(), false),
        };
        let decls = self.module.declarations();
        let symbol = decls.get_function_decl(id).linkage_name(id).into_owned();
        let kind = format!("{def:?}");
        let kind = kind.split(['(', ' ']).next().unwrap_or_default().to_owned();
        let origin = match def {
            InstanceDef::Fn(func) => format!("{func:?}"),
            InstanceDef::Closure(it)
            | InstanceDef::Coroutine(it)
            | InstanceDef::CoroutineByMove(it) => format!("{it:?}"),
            _ => kind.clone(),
        };
        let mut hasher = rustc_hash::FxHasher::default();
        identity.hash(&mut hasher);
        if let Some(out) = &mut self.shapes {
            let _ = writeln!(
                out,
                "O\t{symbol}\t{kind}\t{origin}\t{name}\t{}\t{}\t{}\t{:x}",
                generic as u8,
                derive as u8,
                took.as_micros(),
                hasher.finish()
            );
        }
    }

    fn shape_code(&mut self, id: FuncId, code: &Code, relocs: &[ModuleReloc]) {
        use std::hash::{Hash, Hasher};
        use std::io::Write;
        if self.shapes.is_none() {
            return;
        }
        let decls = self.module.declarations();
        let mut hasher = rustc_hash::FxHasher::default();
        code.bytes.hash(&mut hasher);
        let mut targets = Vec::new();
        for reloc in relocs {
            (reloc.offset, format!("{:?}", reloc.kind), reloc.addend).hash(&mut hasher);
            targets.push(match reloc.name {
                ModuleRelocTarget::User { namespace: 0, index } => {
                    let id = FuncId::from_u32(index);
                    decls.get_function_decl(id).linkage_name(id).into_owned()
                }
                ModuleRelocTarget::User { index, .. } => {
                    let id = DataId::from_u32(index);
                    format!("data:{}", decls.get_data_decl(id).linkage_name(id))
                }
                ModuleRelocTarget::FunctionOffset(..) => "self".to_owned(),
                ref other => format!("{other:?}"),
            });
        }
        let symbol = decls.get_function_decl(id).linkage_name(id).into_owned();
        if let Some(out) = &mut self.shapes {
            let _ = writeln!(
                out,
                "C\t{symbol}\t{}\t{:x}\t{}",
                code.bytes.len(),
                hasher.finish(),
                targets.join(" ")
            );
        }
    }

    fn define_code(&mut self, id: FuncId, code: &Code, relocs: &[ModuleReloc]) -> Result<()> {
        if code.elf_types.is_empty() {
            self.module.define_function_bytes(id, code.align, &code.bytes, relocs)?;
        } else {
            // Cranelift is given the relocations it has a kind for, and `finish` adds the
            // others to the object.
            let elf_type = |reloc: &ModuleReloc| {
                code.elf_types.iter().find(|(offset, _)| *offset == reloc.offset).map(|it| it.1)
            };
            let (by_type, plain): (Vec<_>, Vec<_>) =
                relocs.iter().cloned().partition(|reloc| elf_type(reloc).is_some());
            self.module.define_function_bytes(id, code.align, &code.bytes, &plain)?;
            self.elf_relocs.extend(by_type.into_iter().filter_map(|reloc| {
                let r_type = elf_type(&reloc)?;
                Some(ElfReloc { func: id, reloc, r_type })
            }));
        }
        if let Some(unwind) = &code.unwind {
            self.unwind
                .add_function(&mut self.module, id, unwind)
                .map_err(CodegenError::Backend)?;
        }
        Ok(())
    }

    /// Compiles a function built here, whose IR names what it uses by the ids of the module.
    fn define_shim(&mut self, id: FuncId, ctx: &mut cranelift_codegen::Context) -> Result<()> {
        let mut code = emit(ctx)?;
        for reloc in &mut code.relocs {
            if let ModuleRelocTarget::FunctionOffset(_, offset) = reloc.name {
                reloc.name = ModuleRelocTarget::FunctionOffset(id, offset);
            }
        }
        let relocs = std::mem::take(&mut code.relocs);
        self.define_code(id, &code, &relocs)
    }

    fn resolve(&mut self, requests: &Requests) -> Result<Resolved> {
        let mut ids = Resolved {
            funcs: Vec::with_capacity(requests.funcs.len()),
            datas: Vec::with_capacity(requests.datas.len()),
        };
        for request in &requests.funcs {
            ids.funcs.push(self.func(request)?);
        }
        // A data object only points to the ones requested before it.
        for request in &requests.datas {
            let id = self.data(request, &ids)?;
            ids.datas.push(id);
        }
        Ok(ids)
    }

    fn func(&mut self, request: &FuncRequest) -> Result<FuncId> {
        match &request.target {
            FuncTarget::Symbol(symbol) => {
                let own = self.unwind.runtime(&mut self.module, symbol);
                match own.map_err(CodegenError::Backend)? {
                    Some(id) => Ok(id),
                    None => self.import(symbol, &request.sig),
                }
            }
            FuncTarget::Path(path) => {
                let Some(symbol) = self.symbols.path(path) else {
                    unsupported!("call to `{path}`, which rustc has no symbol for");
                };
                self.import(&symbol, &request.sig)
            }
            FuncTarget::Instance(instance, env) => self.callee(instance, env, &request.sig),
            FuncTarget::Asm(text) => self.assembly(text, &request.sig),
        }
    }

    /// The function of an `asm!` block, which is local to the object.
    fn assembly(&mut self, text: &str, sig: &Signature) -> Result<FuncId> {
        if let Some(&id) = self.assemblies.get(text) {
            return Ok(id);
        }
        let triple = self.module.isa().triple().to_string();
        let assembled = asm::assemble(&triple, text).map_err(CodegenError::Unsupported)?;
        let name = format!("ra_jit::asm::{}", self.assemblies.len());
        let id = self
            .module
            .declare_function(&name, Linkage::Local, sig)
            .map_err(|e| CodegenError::Backend(e.to_string()))?;
        self.module.define_function_bytes(id, 16, &assembled.code, &[])?;
        self.assemblies.insert(text.to_owned(), id);
        Ok(id)
    }

    fn data(&mut self, request: &DataRequest, ids: &Resolved) -> Result<DataId> {
        match request {
            DataRequest::Blob { blob, identity } => self.define_data_as(ids.blob(blob), *identity),
            DataRequest::Vtable { ty, trait_ref, blob } => {
                let key = (ty.clone(), trait_ref.clone());
                if let Some(&id) = self.vtables.get(&key) {
                    return Ok(id);
                }
                let id = self.define_data_as(ids.blob(blob), DataIdentity::Own)?;
                self.vtables.insert(key, id);
                Ok(id)
            }
            DataRequest::Static(id) => self.static_data(*id),
            DataRequest::Location(at) => self.location(*at),
            DataRequest::Declaration(func) => {
                let found = self.declaration_at(*func);
                self.define_location(found)
            }
            DataRequest::FilePath(file) => {
                let Some(path) = self.source_path(*file) else {
                    unsupported!("`file!()` in a file without a path");
                };
                self.define_file_path(path)
            }
            DataRequest::Text(parts) => {
                let Some(text) = self.text(parts) else {
                    unsupported!("`file!()` in a file without a path");
                };
                self.define_file_path(text)
            }
        }
    }

    /// The text of the parts, with each file's path for the file.
    fn text(&mut self, parts: &[TextPart]) -> Option<String> {
        parts
            .iter()
            .map(|part| match part {
                TextPart::Text(text) => Some(text.clone()),
                TextPart::FilePath(file) => self.source_path(*file),
            })
            .collect()
    }

    /// The `&str` that `file!()` is, or a `concat!` with it.
    fn define_file_path(&mut self, path: String) -> Result<DataId> {
        let len = path.len();
        let path = self.define_const(ConstBlob {
            bytes: path.into_bytes(),
            relocs: Vec::new(),
            fn_relocs: Vec::new(),
            align: 1,
        })?;
        let mut fat = ConstBlob {
            bytes: vec![0; 2 * word()],
            relocs: Vec::new(),
            fn_relocs: Vec::new(),
            align: word() as u64,
        };
        fat.point_to(0, path, 0);
        fat.bytes[word()..].copy_from_slice(&(len as u64).to_le_bytes()[..word()]);
        self.define_const(fat)
    }

    fn source_path(&mut self, file: base_db::FileId) -> Option<String> {
        let symbols = self.symbols;
        let spelled = || symbols.source_path(file).map(|it| as_rustc_spells(&it));
        self.source_paths.entry(file).or_insert_with(spelled).clone()
    }

    /// The data object of a static: rustc's when it exports one, else our own copy of its value.
    /// The name the linker knows a static by: one of an `extern` block, or one that an
    /// attribute exports and rustc's build defines.
    fn linked_static(&self, id: StaticId) -> Result<Option<intern::Symbol>> {
        let db = self.db;
        let signature = StaticSignature::of(db, id);
        if !signature.flags.contains(StaticFlags::EXTERN) {
            let exported = AttrFlags::export_name(db, id.into()).or_else(|| {
                let unmangled = AttrFlags::query(db, id.into()).contains(AttrFlags::NO_MANGLE);
                unmangled.then(|| signature.name.symbol().clone())
            });
            return Ok(exported.filter(|name| self.symbols.defines(name.as_str())));
        }
        let name = AttrFlags::link_name(db, id.into());
        Ok(Some(name.unwrap_or_else(|| signature.name.symbol().clone())))
    }

    fn static_data(&mut self, id: StaticId) -> Result<DataId> {
        let db = self.db;
        let known = [&self.imported_statics, &self.statics, &self.frozen_statics];
        if let Some(&data) = known.into_iter().find_map(|it| it.get(&id)) {
            return Ok(data);
        }
        // rustc's copy when ours has no value to give.
        let ours = self.own_statics.replaces(db, id) && compiled_static(db, id).is_ok();
        let theirs = || (!ours).then(|| self.symbols.static_(id)).flatten();
        if let Some(symbol) = static_link_name(db, id).or_else(theirs) {
            let data = self.module.declare_data(
                &symbol,
                Linkage::Import,
                false,
                is_thread_local(db, id),
            )?;
            self.imported_statics.insert(id, data);
            // rustc's value of the static points to rustc's copies of functions.
            if self.on_demand_crates.contains(&id.krate(db))
                && let Ok(compiled) = compiled_static(db, id).as_ref()
            {
                self.want_requested(&compiled.requests);
            }
            return Ok(data);
        }
        if let Some(symbol) = self.linked_static(id)? {
            let thread_local = is_thread_local(db, id);
            let data =
                self.module.declare_data(symbol.as_str(), Linkage::Import, false, thread_local)?;
            self.imported_statics.insert(id, data);
            return Ok(data);
        }
        let node = patch::Node::Static(id);
        let known = self.base.and_then(|base| base.statics.get(&id));
        // One with another value keeps its name, and what refers to it by that gets the new.
        let again = known.filter(|_| self.replaced.changed.contains(&node));
        if let Some(known) = known.filter(|_| again.is_none()) {
            if self.replaced.broken.contains(&node) {
                unsupported!("a static of the base changed");
            }
            let data =
                self.module.declare_data(&known.symbol, Linkage::Import, known.writable, false)?;
            self.imported_statics.insert(id, data);
            return Ok(data);
        }
        let compiled = compiled_static(db, id).as_ref().map_err(Clone::clone)?;
        if compiled.writable && !self.own_statics.owns(db, id) {
            let name = hir_def::signatures::StaticSignature::of(db, id).name.as_str();
            unsupported!("writable static `{name}` without a rustc symbol");
        }
        // Not shared with equal data: a static has an address of its own. Known before its
        // value is: what the value points to may point back at the static.
        let thread_local = is_thread_local(db, id);
        let data = if let Some(known) = again {
            self.module.declare_data(&known.symbol, Linkage::Hidden, compiled.writable, false)?
        } else if self.patching || thread_local {
            // Nothing else defines the statics of a program compiled to WebAssembly, and
            // under its own name a static is the same one after a build in full.
            let own = if cfg!(feature = "wasm") { self.symbols.static_(id) } else { None };
            let name = own.unwrap_or_else(|| format!("ra_jit::static::{}", self.location_counter));
            self.location_counter += 1;
            let linkage = if self.patching { Linkage::Hidden } else { Linkage::Local };
            self.module.declare_data(&name, linkage, compiled.writable, thread_local)?
        } else {
            self.declare_local_data(compiled.writable)?
        };
        // One the build after this may replace where it is has to say how large it is.
        let named = (again.is_some() || self.patching) && !thread_local;
        let asked = AttrFlags::link_section(db, id.into());
        let section = asked.as_ref().map(|it| it.as_str()).or_else(|| {
            named.then(|| Target::current().section_of_static(compiled.writable)).flatten()
        });
        let known = if compiled.writable { &mut self.statics } else { &mut self.frozen_statics };
        known.insert(id, data);
        let begun = self.own_static_order.len();
        self.own_static_order.push(id);
        match self.resolve(&compiled.requests) {
            Ok(ids) => {
                self.fill_data(data, ids.blob(&compiled.blob), section)?;
                Ok(data)
            }
            Err(e) => {
                // The statics defined since may point to this one, which has no value.
                for id in self.own_static_order.drain(begun..) {
                    self.statics.remove(&id);
                    self.frozen_statics.remove(&id);
                }
                let nothing = ConstBlob {
                    bytes: Vec::new(),
                    relocs: Vec::new(),
                    fn_relocs: Vec::new(),
                    align: 1,
                };
                self.fill_data(data, nothing, section)?;
                Err(e)
            }
        }
    }

    /// A `core::panic::Location` (`{ file: &str, line: u32, col: u32 }`).
    // FIXME: the layout of `Location` should come from `layout_of_ty`.
    fn location(&mut self, at: Option<(DefWithBodyId, MirSpan)>) -> Result<DataId> {
        let found = self.location_at(at);
        self.define_location(found)
    }

    fn location_at(&mut self, at: Option<(DefWithBodyId, MirSpan)>) -> Option<(String, u32, u32)> {
        at.and_then(|(owner, span)| source_location(self.db, owner, span))
            .and_then(|(file, line, col)| Some((self.source_path(file)?, line, col)))
    }

    fn declaration_at(&mut self, func: FunctionId) -> Option<(String, u32, u32)> {
        declaration_location(self.db, func)
            .and_then(|(file, line, col)| Some((self.source_path(file)?, line, col)))
    }

    /// Where a debugger is told `func` is declared: [`debug_declaration_location`].
    fn debug_declaration_at(&mut self, func: FunctionId) -> Option<(String, u32, u32)> {
        debug_declaration_location(self.db, func)
            .and_then(|(file, line, col)| Some((self.source_path(file)?, line, col)))
    }

    /// A `core::panic::Location`, of no place in particular when the place is not known.
    fn define_location(&mut self, found: Option<(String, u32, u32)>) -> Result<DataId> {
        let (file, line, col) = found.unwrap_or_else(|| ("<ra-jit>".to_owned(), 0u32, 0u32));
        let mut bytes = vec![0u8; 2 * word() + 8];
        bytes[word()..2 * word()].copy_from_slice(&(file.len() as u64).to_le_bytes()[..word()]);
        bytes[2 * word()..2 * word() + 4].copy_from_slice(&line.to_le_bytes());
        bytes[2 * word() + 4..].copy_from_slice(&col.to_le_bytes());
        // The name is followed by a NUL that its length leaves out.
        let mut name = file.into_bytes();
        name.push(0);
        let name = self.define_const(ConstBlob {
            bytes: name,
            relocs: Vec::new(),
            fn_relocs: Vec::new(),
            align: 1,
        })?;
        let mut location =
            ConstBlob { bytes, relocs: Vec::new(), fn_relocs: Vec::new(), align: word() as u64 };
        location.point_to(0, name, 0);
        self.define_const(location)
    }

    /// The function to call for `func` instantiated with `args`: the rustc-compiled one when it
    /// has a symbol we can link against, otherwise an instance compiled into this object.
    fn callee(
        &mut self,
        instance: &Instance,
        env: &StoredParamEnvAndCrate,
        sig: &Signature,
    ) -> Result<FuncId> {
        let instance = &self.settled(instance)?;
        let (def, args) = (instance.def, instance.args.as_ref());
        if let InstanceDef::Fn(func) = def
            && args.iter().all(|arg| arg.region().is_some())
            && let Some(symbol) = self.symbols.function(func)
            && self.as_its_crate_compiles(instance)
            && let Some(symbol) = self.rustc_copy(symbol, instance.checked)
            // rustc's copy of a function of another crate would hand out rustc's future.
            && (self.own_statics.crates.contains(&func.krate(self.db))
                || !futures_kept_apart()
                || !returns_async_block(self.db, func))
        {
            self.want(func);
            return self.import(&symbol, sig);
        }
        let signature = match def {
            InstanceDef::Fn(func) => Some(FunctionSignature::of(self.db, func)),
            InstanceDef::Closure(_)
            | InstanceDef::Coroutine(_)
            | InstanceDef::CoroutineByMove(_)
            | InstanceDef::DropGlue
            | InstanceDef::CallOnceShim
            | InstanceDef::VtableShim(_)
            | InstanceDef::FnPointerShim
            | InstanceDef::ReifyShim { .. } => None,
        };
        let name = self.callee_name(def);
        if let Some(Rejected { why, .. }) = self.rejected.get(instance) {
            unsupported!("callee `{name}`: {why}");
        }
        if self.handles_rustc_future(instance) {
            unsupported!("callee `{name}`: handles a future rustc lays out");
        }
        if let Some(&id) = self.instances.get(instance) {
            return Ok(id);
        }
        if let Some(known) = self.base.and_then(|base| base.instances.get(instance)) {
            let node = patch::Node::Instance(instance.clone());
            if self.replaced.broken.contains(&node) {
                unsupported!("callee `{name}`: an instance of the base gained or lost code");
            }
            // The base's copy is of the source before the edit.
            if self.replaced.changed.contains(&node) {
                let compiled = compiled_instance(self.db, instance.clone(), known.env.clone());
                let compiled = compiled.as_ref().map_err(Clone::clone)?;
                let id = self.module.declare_function(
                    &known.symbol,
                    Linkage::Hidden,
                    &compiled.code.signature,
                )?;
                self.instances.insert(instance.clone(), id);
                self.pending.push((instance.clone(), known.env.clone(), id));
                return Ok(id);
            }
            return self.import(&known.symbol, sig);
        }
        if let InstanceDef::Fn(func) = def
            && let Some(symbol) = self.any_method(func, args)
        {
            return self.import(&symbol, sig);
        }
        // A `TypeId` is a hash only rustc can compute, and the two compilers have to agree on
        // it for `downcast` to work between their code. So take rustc's.
        if let InstanceDef::Fn(func) = def
            && let Some(of) = self.type_id_of(func, args)
        {
            let mut hasher =
                type_id::TypeIdHasher::new(self.db, self.symbols, &self.own_statics.crates);
            if let Some(hash) = hasher.type_id(of) {
                return self.constant_type_id(instance.clone(), sig, hash);
            }
            let stuck = hasher.stuck.unwrap_or_default();
            let Some(ty) = self.symbol_type(of) else {
                unsupported!(
                    "`TypeId` of `{of:?}`, neither hashed (at `{stuck}`) nor named by a rustc symbol"
                );
            };
            if let Some(symbol) = self.symbols.path(&format!("<core::any::TypeId>::of::<{ty}>")) {
                return self.import(&symbol, sig);
            }
            // rustc inlines `TypeId::of` into most of its callers. What is left then is the
            // `Any` method a `dyn Any` of the type calls, which makes no use of its `self`.
            let Some(symbol) = self.symbols.path(&format!("<{ty} as core::any::Any>::type_id"))
            else {
                unsupported!(
                    "`TypeId` of `{ty}`, not hashed (at `{stuck}`) and without a rustc symbol"
                );
            };
            let mut method = sig.clone();
            method.params.push(AbiParam::new(ptr()));
            let method = self.import(&symbol, &method)?;
            let id = self
                .module
                .declare_function(
                    &format!("ra_jit::type_id::{}", self.instances.len()),
                    Linkage::Local,
                    sig,
                )
                .map_err(|e| CodegenError::Backend(e.to_string()))?;
            let mut ctx = self.module.make_context();
            ctx.func.signature = sig.clone();
            let mut builder_ctx = std::mem::take(&mut self.builder_ctx);
            let mut builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
            let block = builder.create_block();
            builder.append_block_params_for_function_params(block);
            builder.switch_to_block(block);
            let mut values = builder.block_params(block).to_vec();
            // A `&self` that points at nothing: any aligned address will do.
            values.push(builder.ins().iconst(ptr(), 16));
            let method = self.module.declare_func_in_func(method, builder.func);
            let call = builder.ins().call(method, &values);
            let results = builder.inst_results(call).to_vec();
            builder.ins().return_(&results);
            builder.seal_all_blocks();
            builder.finalize(self.module.isa().frontend_config());
            self.builder_ctx = builder_ctx;
            self.define_shim(id, &mut ctx)?;
            self.instances.insert(instance.clone(), id);
            return Ok(id);
        }
        if signature.is_some_and(|it| !it.has_body()) {
            // The allocator entry points are declared in `alloc` and defined at link time,
            // under names in a crate of their own.
            if let Some(symbol) = self.symbols.path(&format!("__rustc::{name}")) {
                return self.import(&symbol, sig);
            }
            // FIXME: this passes arguments the way Rust functions take them, which is only
            // what a foreign function expects for scalars and pointers.
            if let InstanceDef::Fn(func) = def
                && let Some(symbol) = link_name(self.db, func)
                && !symbol.starts_with("llvm.")
            {
                return self.import(&symbol, sig);
            }
            unsupported!("call to `{name}`, which has no body and no rustc symbol");
        }
        let from_store = self.entries.is_some() || self.base.is_some_and(|it| !it.found.is_empty());
        let stable = if from_store { self.stable_name(instance) } else { None };
        if let Some(name) = &stable {
            if let Some(symbol) = self.base.and_then(|base| base.found.get(name)) {
                return self.import(symbol, sig);
            }
            let body = signature.is_some().then_some(sig);
            if let Some(id) = self.found_callee(name, body)? {
                self.instances.insert(instance.clone(), id);
                let home = self.homes.home(self.db, instance, env);
                self.found_instances.insert(id, (instance.clone(), home));
                return Ok(id);
            }
        }
        // A patch is linked with its base, whose names these must not take.
        let env = &self.homes.home(self.db, instance, env);
        let prefix = if self.base.is_some() { "ra-jit.patch" } else { "ra-jit" };
        let symbol = format!("{prefix}::{name}::{}", self.instances.len());
        let linkage = if self.patching { Linkage::Hidden } else { Linkage::Local };
        let id = self
            .module
            .declare_function(&symbol, linkage, sig)
            .map_err(|e| CodegenError::Backend(e.to_string()))?;
        self.instances.insert(instance.clone(), id);
        self.pending.push((instance.clone(), env.clone(), id));
        self.envs.push((instance.clone(), env.clone()));
        if let Some(name) = stable {
            self.named.insert(name, id);
        }
        Ok(id)
    }

    /// What a debugger of a rustc build calls the code of a closure of `instance`.
    fn debug_closure(
        &self,
        instance: &Instance,
        owner: InferBodyId<'db>,
        expr: ExprId,
    ) -> Option<String> {
        let InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)) = owner else {
            return None;
        };
        let args = instance.args.as_ref();
        super::type_name::rustc_debug_closure(self.db, func, args.as_slice(), expr)
    }

    /// What a backtrace of a rustc build calls the instance, as far as that is reproduced:
    /// the last name of it otherwise.
    fn backtrace_name(&self, instance: &Instance) -> String {
        let db = self.db;
        let args = instance.args.as_ref();
        let ty = || args.as_slice().first().and_then(|arg| arg.ty());
        let of_closure = |owner: InferBodyId<'db>, expr| match owner {
            InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)) => {
                super::type_name::rustc_closure_name(db, func, args.as_slice(), expr)
            }
            _ => None,
        };
        let name = match instance.def {
            InstanceDef::Fn(func) => {
                super::type_name::rustc_function_name(db, func, args.as_slice())
            }
            InstanceDef::Closure(id) => {
                let at = InternedClosureId::from_id(id).loc(db);
                of_closure(at.owner, at.expr)
            }
            InstanceDef::Coroutine(id) | InstanceDef::CoroutineByMove(id) => {
                let at = InternedCoroutineId::from_id(id).loc(db);
                of_closure(at.owner, at.expr)
            }
            InstanceDef::DropGlue => ty()
                .and_then(|ty| super::type_name::rustc_symbol_type(db, ty))
                .map(|ty| format!("core::ptr::drop_glue::<{ty}>")),
            InstanceDef::CallOnceShim
            | InstanceDef::VtableShim(_)
            | InstanceDef::FnPointerShim
            | InstanceDef::ReifyShim { .. } => None,
        };
        name.unwrap_or_else(|| self.callee_name(instance.def).to_owned())
    }

    fn callee_name(&self, def: InstanceDef) -> &'db str {
        match def {
            InstanceDef::Fn(func) => FunctionSignature::of(self.db, func).name.as_str(),
            InstanceDef::Closure(_) => "{closure}",
            InstanceDef::Coroutine(_) | InstanceDef::CoroutineByMove(_) => "{async}",
            InstanceDef::DropGlue => "drop_in_place",
            InstanceDef::CallOnceShim => "call_once",
            InstanceDef::VtableShim(_) => "{vtable}",
            InstanceDef::FnPointerShim => "{closure}::fn_pointer",
            InstanceDef::ReifyShim { .. } => "{reify}",
        }
    }

    /// Adds to `failed` the instances that call one of them, and so on up: the next round
    /// would reject those, and the round after it their callers. One missed here only costs
    /// that round.
    fn poison_callers(&self, failed: &mut Vec<(Instance, Rejected)>) {
        if failed.is_empty() {
            return;
        }
        let mut callers: FxHashMap<&Instance, Vec<usize>> = FxHashMap::default();
        for (at, (instance, env)) in self.envs.iter().enumerate() {
            let Ok(compiled) = compiled_instance(self.db, instance.clone(), env.clone()).as_ref()
            else {
                continue;
            };
            for request in &compiled.requests.funcs {
                if let FuncTarget::Instance(callee, _) = &request.target {
                    callers.entry(callee).or_default().push(at);
                }
            }
        }
        let mut seen: FxHashSet<Instance> = failed.iter().map(|it| it.0.clone()).collect();
        let mut next = 0;
        while let Some((callee, Rejected { why, .. })) = failed.get(next) {
            next += 1;
            let why = format!("callee `{}`: {why}", self.callee_name(callee.def));
            for &at in callers.get(callee).into_iter().flatten() {
                let (caller, env) = &self.envs[at];
                // rustc's copy stands in for one of these, which its callers then call.
                let forwarded = self.forwarded.contains(caller)
                    || matches!(caller.def, InstanceDef::Fn(func) if self
                        .instance_path(func, caller.args.as_ref())
                        .and_then(|path| self.symbols.path(&path))
                        .is_some_and(|it| self.rustc_copy(it, caller.checked).is_some()));
                if !forwarded && seen.insert(caller.clone()) {
                    failed.push((caller.clone(), Rejected { env: env.clone(), why: why.clone() }));
                }
            }
        }
    }

    /// Whether `instance` is the poll or the drop of a future rustc's code makes. Everything
    /// that reads such a future goes through one of those, so refusing them refuses the rest.
    fn handles_rustc_future(&self, instance: &Instance) -> bool {
        if self.rustc_futures.is_empty() || !futures_kept_apart() {
            return false;
        }
        let block = match instance.def {
            InstanceDef::Coroutine(id) | InstanceDef::CoroutineByMove(id) => Some(id),
            InstanceDef::DropGlue => match instance.args.as_ref().type_at(0).kind() {
                TyKind::Coroutine(id, _) => Some(id.0.as_id()),
                _ => None,
            },
            _ => None,
        };
        block.is_some_and(|id| self.rustc_futures.contains(&id))
    }

    /// Makes `id` a call of the copy of `instance` rustc compiled, when there is one: an
    /// instance we cannot compile is still one we can call.
    fn forward_to_rustc(&mut self, instance: &Instance, id: FuncId) -> Option<()> {
        let InstanceDef::Fn(func) = instance.def else { return None };
        let symbol = self.symbols.path(&self.instance_path(func, instance.args.as_ref())?)?;
        let symbol = self.rustc_copy(symbol, instance.checked)?;
        let sig = self.module.declarations().get_function_decl(id).signature.clone();
        let target = self.import(&symbol, &sig).ok()?;
        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        let mut builder_ctx = std::mem::take(&mut self.builder_ctx);
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
        let block = builder.create_block();
        builder.append_block_params_for_function_params(block);
        builder.switch_to_block(block);
        let values = builder.block_params(block).to_vec();
        let target = self.module.declare_func_in_func(target, builder.func);
        let call = builder.ins().call(target, &values);
        let results = builder.inst_results(call).to_vec();
        builder.ins().return_(&results);
        builder.seal_all_blocks();
        builder.finalize(self.module.isa().frontend_config());
        self.builder_ctx = builder_ctx;
        self.define_shim(id, &mut ctx).ok()
    }

    /// A generic function with its arguments as it reads in a demangled rustc symbol:
    /// `<Vec<u8> as Clone>::clone`, `<Option<u8>>::map::<u8, F>`, `core::mem::swap::<u8>`.
    fn instance_path(&self, func: FunctionId, args: GenericArgs<'db>) -> Option<String> {
        let db = self.db;
        let signature = FunctionSignature::of(db, func);
        // The future of an `async fn` is laid out differently by the two compilers.
        if signature.is_async() {
            return None;
        }
        let name = signature.name.as_str();
        let interner = DbInterner::new_no_crate(db);
        let types = |args: &[crate::next_solver::GenericArg<'db>]| {
            args.iter()
                .filter(|it| it.region().is_none())
                .map(|it| self.symbol_arg(*it))
                .collect::<Option<Vec<_>>>()
        };
        let parent = crate::generics::generics(db, func.into()).len_parent();
        let (parent, own) = args.as_slice().split_at_checked(parent)?;
        let mut path = match func.lookup(db).container {
            ItemContainerId::ModuleId(_) => self.symbols.item(func.into())?,
            ItemContainerId::TraitId(trait_) => {
                let (self_ty, rest) = parent.split_first()?;
                let mut path = format!(
                    "<{} as {}",
                    self.symbol_type(self_ty.ty()?)?,
                    self.symbols.item(trait_.into())?
                );
                let rest = types(rest)?;
                if !rest.is_empty() {
                    path.push_str(&format!("<{}>", rest.join(", ")));
                }
                format!("{path}>::{name}")
            }
            ItemContainerId::ImplId(impl_) => {
                let parent_args = GenericArgs::new_from_slice(parent);
                let self_ty =
                    db.impl_self_ty(impl_).instantiate(interner, parent_args).skip_norm_wip();
                let self_ty = self.symbol_type(self_ty)?;
                match db.impl_trait(impl_) {
                    None => format!("<{self_ty}>::{name}"),
                    Some(trait_ref) => {
                        let trait_ref =
                            trait_ref.instantiate(interner, parent_args).skip_norm_wip();
                        let mut path = format!(
                            "<{self_ty} as {}",
                            self.symbols.item(trait_ref.def_id.0.into())?
                        );
                        let rest = types(&trait_ref.args.as_slice()[1..])?;
                        if !rest.is_empty() {
                            path.push_str(&format!("<{}>", rest.join(", ")));
                        }
                        format!("{path}>::{name}")
                    }
                }
            }
            ItemContainerId::ExternBlockId(_) => return None,
        };
        let own = types(own)?;
        if !own.is_empty() {
            path.push_str(&format!("::<{}>", own.join(", ")));
        }
        Some(path)
    }

    /// A `TypeId::of::<T>` that returns `hash`, the two words of which are its value.
    fn constant_type_id(
        &mut self,
        instance: Instance,
        sig: &Signature,
        hash: u128,
    ) -> Result<FuncId> {
        if let Some(&id) = self.instances.get(&instance) {
            return Ok(id);
        }
        let id = self.type_id_shim(sig, hash)?;
        self.instances.insert(instance, id);
        Ok(id)
    }

    /// The `TypeId::of` that returns `hash`.
    fn type_id_shim(&mut self, sig: &Signature, hash: u128) -> Result<FuncId> {
        if let Some(&id) = self.type_id_shims.get(&hash) {
            return Ok(id);
        }
        let name = format!("ra_jit::type_id::h{}", self.type_id_shims.len());
        let id = self
            .module
            .declare_function(&name, Linkage::Local, sig)
            .map_err(|e| CodegenError::Backend(e.to_string()))?;
        let mut ctx = self.module.make_context();
        ctx.func.signature = sig.clone();
        let mut builder_ctx = std::mem::take(&mut self.builder_ctx);
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
        let block = builder.create_block();
        builder.append_block_params_for_function_params(block);
        builder.switch_to_block(block);
        let words = [hash as u64, (hash >> 64) as u64].map(|it| it as i64);
        let returned = match *sig.returns.as_slice() {
            [] => {
                let &[out] = builder.block_params(block) else {
                    unsupported!("a `TypeId` returned as `{sig}`");
                };
                for (word, offset) in words.into_iter().zip([0, 8]) {
                    let word = builder.ins().iconst(types::I64, word);
                    builder.ins().store(MEM, word, out, offset);
                }
                Vec::new()
            }
            [low, high] if low.value_type == types::I64 && high.value_type == types::I64 => {
                words.map(|it| builder.ins().iconst(types::I64, it)).to_vec()
            }
            [whole] if whole.value_type == types::I128 => {
                let [low, high] = words.map(|it| builder.ins().iconst(types::I64, it));
                vec![builder.ins().iconcat(low, high)]
            }
            _ => unsupported!("a `TypeId` returned as `{sig}`"),
        };
        builder.ins().return_(&returned);
        builder.seal_all_blocks();
        builder.finalize(self.module.isa().frontend_config());
        self.builder_ctx = builder_ctx;
        self.define_shim(id, &mut ctx)?;
        self.type_id_shims.insert(hash, id);
        self.type_ids.insert(id, hash);
        Ok(id)
    }

    /// The `T` of `TypeId::of::<T>`, when `func` is that function.
    fn type_id_of(&self, func: FunctionId, args: GenericArgs<'db>) -> Option<Ty<'db>> {
        let db = self.db;
        if FunctionSignature::of(db, func).name.as_str() != "of" {
            return None;
        }
        let ItemContainerId::ImplId(impl_) = func.lookup(db).container else { return None };
        let TyKind::Adt(adt, _) = db.impl_self_ty(impl_).skip_binder().kind() else { return None };
        if self.symbols.item(adt.def_id().into())? != "core::any::TypeId" {
            return None;
        }
        args.as_slice().first()?.ty()
    }

    /// A type as it reads in a demangled rustc symbol. `None` for the types whose spelling
    /// there this does not know: closures, `dyn`, function pointers, anything with a lifetime
    /// argument.
    /// A type or a `usize` an instance is instantiated with, as rustc's symbols spell it.
    fn symbol_arg(&self, arg: crate::next_solver::GenericArg<'db>) -> Option<String> {
        match (arg.ty(), arg.konst()) {
            (Some(ty), _) => self.symbol_type(ty),
            (None, Some(len)) => Some(try_const_usize(self.db, len)?.to_string()),
            (None, None) => None,
        }
    }

    fn symbol_type(&self, ty: Ty<'db>) -> Option<String> {
        use std::fmt::Write;
        let db = self.db;
        let list = |tys: &mut dyn Iterator<Item = Ty<'db>>| {
            tys.map(|it| self.symbol_type(it)).collect::<Option<Vec<_>>>().map(|it| it.join(", "))
        };
        Some(match ty.kind() {
            TyKind::Bool => "bool".to_owned(),
            TyKind::Char => "char".to_owned(),
            TyKind::Str => "str".to_owned(),
            TyKind::Never => "!".to_owned(),
            TyKind::Int(it) => it.name_str().to_owned(),
            TyKind::Uint(it) => it.name_str().to_owned(),
            TyKind::Float(it) => it.name_str().to_owned(),
            TyKind::Ref(_, pointee, Mutability::Not) => format!("&{}", self.symbol_type(pointee)?),
            TyKind::Ref(_, pointee, Mutability::Mut) => {
                format!("&mut {}", self.symbol_type(pointee)?)
            }
            TyKind::RawPtr(pointee, Mutability::Not) => {
                format!("*const {}", self.symbol_type(pointee)?)
            }
            TyKind::RawPtr(pointee, Mutability::Mut) => {
                format!("*mut {}", self.symbol_type(pointee)?)
            }
            TyKind::Slice(element) => format!("[{}]", self.symbol_type(element)?),
            TyKind::Array(element, len) => {
                format!("[{}; {}]", self.symbol_type(element)?, try_const_usize(db, len)?)
            }
            TyKind::Tuple(tys) if tys.len() == 1 => format!("({},)", list(&mut tys.iter())?),
            TyKind::Tuple(tys) => format!("({})", list(&mut tys.iter())?),
            TyKind::Adt(adt, args) => {
                let mut path = self.symbols.item(adt.def_id().into())?;
                let interner = DbInterner::new_no_crate(db);
                let defaults = db.generic_defaults(adt.def_id().into());
                let mut args = args.as_slice();
                // rustc leaves out the arguments that say what the default says.
                while let Some((last, rest)) = args.split_last()
                    && let Some(default) = defaults.get(rest.len())
                    && default.instantiate(interner, rest).skip_norm_wip() == *last
                {
                    args = rest;
                }
                let args = args.iter().filter(|it| it.region().is_none());
                let args = args.map(|it| self.symbol_arg(*it)).collect::<Option<Vec<_>>>()?;
                if !args.is_empty() {
                    write!(path, "<{}>", args.join(", ")).ok()?;
                }
                path
            }
            TyKind::FnPtr(..) => {
                let sig = ty.fn_sig(DbInterner::new_no_crate(db)).no_bound_vars()?;
                if sig.abi() != ExternAbi::Rust
                    || sig.c_variadic()
                    || sig.safety() != crate::next_solver::abi::Safety::Safe
                {
                    return None;
                }
                let mut path = format!("fn({})", list(&mut sig.inputs().iter().copied())?);
                if !sig.output().is_unit() {
                    write!(path, " -> {}", self.symbol_type(sig.output())?).ok()?;
                }
                path
            }
            TyKind::Dynamic(bounds, _) => {
                let principal = bounds.principal()?.no_bound_vars()?;
                let mut path = format!("dyn {}", self.symbols.item(principal.def_id.0.into())?);
                let args = principal.args.iter().filter(|it| it.region().is_none());
                let mut args = args.map(|it| self.symbol_arg(it)).collect::<Option<Vec<_>>>()?;
                for bound in bounds.projection_bounds() {
                    let bound = bound.no_bound_vars()?;
                    let crate::next_solver::TermId::TypeAliasId(alias) = bound.def_id.0 else {
                        return None;
                    };
                    let name = hir_def::signatures::TypeAliasSignature::of(db, alias).name.clone();
                    let ty = rustc_type_ir::inherent::Term::as_type(&bound.term)?;
                    args.push(format!("{} = {}", name.as_str(), self.symbol_type(ty)?));
                }
                if !args.is_empty() {
                    write!(path, "<{}>", args.join(", ")).ok()?;
                }
                for auto in bounds.auto_traits() {
                    write!(path, " + {}", self.symbols.item(auto.0.into())?).ok()?;
                }
                path
            }
            _ => return None,
        })
    }

    /// rustc's own `<dyn Any>::is::<T>` or `downcast_ref::<T>`, when `func` is such a method
    /// and rustc compiled it for this `T`. It has the `TypeId` of `T` inlined in it, which may
    /// be the one place to get it.
    fn any_method(&self, func: FunctionId, args: GenericArgs<'db>) -> Option<String> {
        let db = self.db;
        let name = FunctionSignature::of(db, func).name.as_str();
        let ItemContainerId::ImplId(impl_) = func.lookup(db).container else { return None };
        let self_ty = db.impl_self_ty(impl_).skip_binder();
        let TyKind::Dynamic(bounds, _) = self_ty.kind() else { return None };
        let principal = bounds.principal_def_id()?.0;
        if self.symbols.item(principal.into())? != "core::any::Any" {
            return None;
        }
        let ty = self.symbol_type(args.as_slice().first()?.ty()?)?;
        self.symbols.path(&format!("<{}>::{name}::<{ty}>", self.symbol_type(self_ty)?))
    }

    /// The symbol a function declared in an `extern` block stands for.
    fn define_const(&mut self, blob: ConstBlob) -> Result<DataId> {
        self.define_data(blob, false)
    }

    fn define_data(&mut self, blob: ConstBlob, writable: bool) -> Result<DataId> {
        let identity = if writable { DataIdentity::Writable } else { DataIdentity::Shared };
        self.define_data_as(blob, identity)
    }

    fn define_data_as(&mut self, blob: ConstBlob, identity: DataIdentity) -> Result<DataId> {
        let writable = identity == DataIdentity::Writable;
        let shared = (identity == DataIdentity::Shared).then(|| blob.clone());
        if let Some(&id) = shared.as_ref().and_then(|blob| self.consts.get(blob)) {
            return Ok(id);
        }
        let id = self.declare_local_data(writable)?;
        self.fill_data(id, blob, None)?;
        if let Some(blob) = shared {
            self.consts.insert(blob, id);
        }
        Ok(id)
    }

    fn declare_local_data(&mut self, writable: bool) -> Result<DataId> {
        let n = self.location_counter;
        self.location_counter += 1;
        self.module
            .declare_data(&format!(".Lra-jit.const.{n}"), Linkage::Local, writable, false)
            .map_err(|e| CodegenError::Backend(e.to_string()))
    }

    /// Defines `id` as `blob`, in a section of its own named `section` when there is one.
    fn fill_data(&mut self, id: DataId, blob: ConstBlob, section: Option<&str>) -> Result<()> {
        let mut data = DataDescription::new();
        if let Some(section) = section {
            data.set_custom_section(section);
        }
        data.define(blob.bytes.into_boxed_slice());
        data.set_align(blob.align);
        for (offset, target, addend) in blob.relocs {
            let target = self.module.declare_data_in_data(target, &mut data);
            data.write_data_addr(offset, target, addend);
        }
        for (offset, target) in blob.fn_relocs {
            let target = self.module.declare_func_in_data(target, &mut data);
            data.write_function_addr(offset, target);
        }
        self.module.define_data(id, &data).map_err(|e| CodegenError::Backend(e.to_string()))
    }

    fn import(&mut self, symbol: &str, sig: &Signature) -> Result<FuncId> {
        if let Some(&id) = self.imports.get(symbol) {
            return Ok(id);
        }
        let id = self
            .module
            .declare_function(symbol, Linkage::Import, sig)
            .map_err(|e| CodegenError::Backend(e.to_string()))?;
        self.imports.insert(symbol.to_owned(), id);
        Ok(id)
    }
}

/// What a call to an intrinsic turned into.
enum Intrinsic {
    /// Lowered inline; continue at the call's target.
    Inline,
    /// Lowered to a trap; the block is already terminated.
    Diverges,
    /// Not known here; call it like a normal function.
    Unknown,
}

/// The pointers of one constant, and the data objects made for what they point to.
struct Relocations<'a, 'db> {
    map: &'a MemoryMap<'db>,
    in_static: bool,
    defined: FxHashMap<usize, DataId>,
}

impl Relocations<'_, '_> {
    /// The alignment to give the data object made for the memory at `start`: what const eval
    /// made it with, or where it does not say, the most a value could need.
    fn align_of(&self, start: usize) -> u64 {
        self.map.align_at(start).map_or(16, |align| align as u64)
    }
}

/// Whether a data object can be the same object as another with the same content.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde_derive::Serialize, serde_derive::Deserialize,
)]
enum DataIdentity {
    /// Read-only and one object with every equal constant, as the same constant is for rustc
    /// wherever it is named.
    Shared,
    /// Read-only with an address of its own, as a static has.
    Own,
    Writable,
}

/// Read-only data whose pointers refer to other data objects.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde_derive::Serialize, serde_derive::Deserialize)]
struct ConstBlob {
    bytes: Vec<u8>,
    relocs: Vec<(u32, DataId, i64)>,
    fn_relocs: Vec<(u32, FuncId)>,
    align: u64,
}

impl ConstBlob {
    /// Whether the bytes are all there is to it, with no address among them.
    #[cfg(feature = "llvm")]
    fn points_nowhere(&self) -> bool {
        self.relocs.is_empty() && self.fn_relocs.is_empty()
    }

    #[cfg(feature = "llvm")]
    fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Whether every pointer in the blob is whole inside it and to one of the first `funcs`
    /// functions or the first `datas` data objects, which a blob read from the store is
    /// not known to be.
    fn sound(&self, funcs: usize, datas: usize) -> bool {
        let inside = |offset: u32| {
            self.bytes.len().checked_sub(word()).is_some_and(|last| offset as usize <= last)
        };
        (self.align == 0 || self.align.is_power_of_two())
            && self.relocs.iter().all(|&(at, to, _)| inside(at) && (to.as_u32() as usize) < datas)
            && self.fn_relocs.iter().all(|&(at, to)| inside(at) && (to.as_u32() as usize) < funcs)
    }

    /// Makes the pointer at `offset` the address `at` bytes into `target`.
    fn point_to(&mut self, offset: usize, target: DataId, at: usize) {
        self.bytes[offset..offset + word()].fill(0);
        self.relocs.push((offset as u32, target, at as i64));
    }

    fn usize_at(&self, offset: usize) -> Option<usize> {
        let bytes = self.bytes.get(offset..offset + word())?;
        Some(bytes.iter().rev().fold(0, |word, &byte| word << 8 | usize::from(byte)))
    }
}

/// A place in memory. `meta` is the slice length (or vtable) when the place came from a fat
/// pointer.
#[derive(Clone, Copy)]
struct PlaceRef<'db> {
    addr: Value,
    meta: Option<Value>,
    ty: PlaceTy<'db>,
}

/// A field that some statement moves out of a local, as the field indices leading to it.
/// The integer type drop glue reads an enum's discriminant as, to pick the variant.
#[derive(Clone, Copy)]
enum DiscriminantWidth {
    Word,
    /// `#[repr(i128)]` and `#[repr(u128)]` enums.
    Wide,
}

impl DiscriminantWidth {
    fn of(tag: &rustc_abi::Scalar) -> Self {
        match tag.primitive() {
            Primitive::Int(Integer::I128, _) => Self::Wide,
            _ => Self::Word,
        }
    }

    fn scalar(self) -> AbiScalar {
        match self {
            Self::Word => AbiScalar { ty: types::I64, signed: true },
            Self::Wide => AbiScalar { ty: types::I128, signed: true },
        }
    }

    /// The switch entry for a variant whose discriminant is `value`.
    fn entry(self, value: i128) -> u128 {
        match self {
            Self::Word => value as i64 as u64 as u128,
            Self::Wide => value as u128,
        }
    }
}

/// The locals a call's operands move whole, whose memory the callee can be handed as it is.
struct GivenAway {
    /// The local the call's result is written to.
    result: LocalId,
    whole: FxHashSet<Value>,
    /// The address of every place the call's operands read.
    read: Vec<Value>,
}

impl GivenAway {
    /// Whether the memory at `addr` is a local moved whole that no other operand reads: a
    /// field at the start of a local has the local's address.
    fn gives(&self, addr: Value) -> bool {
        self.whole.contains(&addr) && self.read.iter().filter(|&&it| it == addr).count() == 1
    }
}

/// A local kept in a register.
#[derive(Clone, Copy)]
struct Register {
    var: cranelift_frontend::Variable,
    ty: Type,
}

#[derive(Clone, Copy)]
struct LocalSlot {
    addr: Value,
    /// The local's type is unsized, and the slot holds a fat pointer to the value where it
    /// was moved from in place of the value.
    borrowed: bool,
}

struct FunctionCx<'cx, 'db> {
    cg: &'cx mut Recorder<'db>,
    /// `None` while generating a shim that has no MIR.
    body: Option<&'db MirBody<'db>>,
    env: ParamEnvAndCrate<'db>,
    infcx: InferCtxt<'db>,
    interner: DbInterner<'db>,
    builder: FunctionBuilder<'cx>,
    locals: ArenaMap<LocalId, LocalSlot>,
    blocks: ArenaMap<BasicBlockId, ir::Block>,
    /// The layouts asked for so far: a body asks for that of the same few types many times.
    layouts: std::cell::RefCell<FxHashMap<Ty<'db>, Arc<Layout>>>,
    /// Whether dropping each type asked about so far does anything.
    glue: std::cell::RefCell<FxHashMap<Ty<'db>, DropGlue>>,
    /// A landing pad that takes over from the cleanup block of `unwind_to` for the calls
    /// made while it is set: drop glue unwinding from one field's drop to the rest.
    inner_pad: Option<ir::Block>,
    /// The cleanup block that what is being generated unwinds to. The body is in
    /// `MirPhase::Runtime`: its drops are elaborated and it names its cleanup blocks.
    unwind_to: Option<BasicBlockId>,
    /// The landing pad of each cleanup block something unwinds to.
    pads: Vec<(BasicBlockId, ir::Block)>,
    /// Where the exception is kept while the cleanup blocks run.
    exception: Option<ir::StackSlot>,
    /// The slots the arguments of the calls so far were copied to since the last were
    /// freed, each with its size and the power of two it is aligned to.
    argument_slots: Vec<(ir::StackSlot, (u32, u8))>,
    /// The ones whose call has returned.
    free_argument_slots: Vec<(ir::StackSlot, (u32, u8))>,
    /// While a call is generated: the local its result goes to, and where the locals moved
    /// whole into it are. Nothing reads those again, so the callee can be handed them.
    given_away: Option<GivenAway>,
    in_cleanup: bool,
    /// Whether a call was made by [`Self::guarded_call`].
    guarded: bool,
    ret_mode: PassMode,
    /// Whether the function is `#[track_caller]` and so takes its caller's location as a last,
    /// hidden argument.
    tracked: bool,
    /// The function takes `...`, and its last parameter is the list of what came for that.
    variadic: bool,
    /// The calling convention of the function being built.
    abi: ExternAbi,
    location: Option<Value>,
    /// Where the statement being compiled comes from.
    span: MirSpan,
    /// Promoted locals that are read-only data instead of a slot the body writes to.
    constant_locals: Vec<LocalId>,
    /// The locals kept in a register in place of their slot: scalars nothing takes the
    /// address of. The slot is still there, written when something asks where the local is.
    registers: FxHashMap<LocalId, Register>,
    /// What the body's generic parameters stand for, when it is the body of an instance.
    args: Option<StoredGenericArgs>,
    /// While compiling an `async` body: where it says where it left off.
    suspended: Option<SuspendedState>,
    /// The locals an `async` body keeps in its future, and how far into it each is.
    held_at: Vec<(LocalId, u32)>,
}

/// Where an `async` body says where it left off, and what that says of its locals.
struct SuspendedState {
    addr: Value,
    ty: Type,
    held: crate::mir::HeldLocals,
}

/// Whether calls to `func` pass a `&Location` after the declared arguments. A method of a trait
/// impl inherits the attribute from the trait's declaration.
fn track_caller(db: &dyn HirDatabase, func: FunctionId) -> bool {
    let has =
        |func: FunctionId| AttrFlags::query(db, func.into()).contains(AttrFlags::TRACK_CALLER);
    if has(func) {
        return true;
    }
    let ItemContainerId::ImplId(impl_id) = func.lookup(db).container else {
        return false;
    };
    let Some(trait_ref) = db.impl_trait(impl_id) else {
        return false;
    };
    let name = &FunctionSignature::of(db, func).name;
    trait_ref.skip_binder().def_id.0.trait_items(db).method_by_name(name).is_some_and(has)
}

/// The integer an address is on the target.
fn ptr() -> Type {
    Target::current().pointer()
}

/// How many bytes an address takes on the target.
fn word() -> usize {
    Target::current().word()
}

/// Where the second word of a wide pointer is.
fn meta_at() -> i32 {
    word() as i32
}

/// Where an `async` body left off: before its first statement, after its last, part of the way
/// through unwinding, or at the await with the index that is added to `SUSPENDED`.
const UNRESUMED: i64 = 0;
const RETURNED: i64 = 1;
const PANICKED: i64 = 2;
const SUSPENDED: i64 = 3;

/// The local a body's caller gets the value of. An `async` body computes the value of its
/// block in the return slot, and hands back a `Poll` of it.
fn returned_local(body: &MirBody<'_>) -> LocalId {
    body.coroutine.map_or(return_slot(), |it| it.poll)
}
/// Field accesses can be unaligned (packed structs, chunked copies).
const MEM: MemFlagsData = MemFlagsData::new().with_notrap();

impl<'cx, 'db> FunctionCx<'cx, 'db> {
    /// Whether pointers to `ty` carry metadata: slices, `str`, trait objects and structs
    /// ending in one.
    fn is_unsized(&self, ty: Ty<'db>) -> bool {
        match ty.kind() {
            TyKind::Slice(_) | TyKind::Str | TyKind::Dynamic(..) => true,
            TyKind::Adt(..) | TyKind::Tuple(_) => {
                self.layout(ty).is_ok_and(|layout| layout.is_unsized())
            }
            _ => false,
        }
    }

    /// The last field of a struct or tuple, which is the only one that can be unsized.
    fn last_field(&self, ty: Ty<'db>) -> Result<(usize, Ty<'db>)> {
        match ty.kind() {
            TyKind::Tuple(tys) if !tys.is_empty() => {
                Ok((tys.len() - 1, tys.iter().last().expect("non-empty tuple")))
            }
            TyKind::Adt(adt_def, subst) => {
                let AdtId::StructId(id) = adt_def.def_id() else {
                    unsupported!("unsized {ty:?}");
                };
                let field_types = self.cg.db.field_types(id.into());
                let Some((field, field_ty)) = field_types.iter().last() else {
                    unsupported!("unsized {ty:?} without fields");
                };
                let field_ty = field_ty.ty().instantiate(self.interner, subst).skip_norm_wip();
                Ok((u32::from(field.into_raw()) as usize, self.normalize(field_ty)))
            }
            _ => unsupported!("unsized {ty:?}"),
        }
    }

    /// `packed(N)` of a struct, which caps the alignment of its fields.
    fn pack(&self, ty: Ty<'db>) -> Option<u64> {
        let TyKind::Adt(adt_def, _) = ty.kind() else { return None };
        let repr = hir_def::attrs::AttrFlags::repr(self.cg.db, adt_def.def_id())?;
        repr.pack.map(|pack| pack.bytes())
    }

    /// Size and alignment of the value behind a pointer with metadata `meta`, as
    /// `size_and_align_of_dst` in rustc's backends.
    fn size_and_align_of_val(
        &mut self,
        ty: Ty<'db>,
        meta: Option<Value>,
    ) -> Result<(Value, Value)> {
        let layout = self.layout(ty)?;
        let align = layout.align.abi.bytes() as i64;
        let meta = match meta {
            Some(meta) if self.is_unsized(ty) => meta,
            _ => {
                let size = self.builder.ins().iconst(ptr(), layout.size.bytes() as i64);
                return Ok((size, self.builder.ins().iconst(ptr(), align)));
            }
        };
        match ty.kind() {
            TyKind::Str => Ok((meta, self.builder.ins().iconst(ptr(), 1))),
            TyKind::Slice(elem) => {
                let elem = self.layout(elem)?;
                let size = self.builder.ins().imul_imm(meta, elem.size.bytes() as i64);
                Ok((size, self.builder.ins().iconst(ptr(), elem.align.abi.bytes() as i64)))
            }
            // Size and align are the vtable's second and third words.
            TyKind::Dynamic(..) => Ok((
                self.builder.ins().load(ptr(), MEM, meta, meta_at()),
                self.builder.ins().load(ptr(), MEM, meta, 2 * meta_at()),
            )),
            _ => {
                let (field, field_ty) = self.last_field(ty)?;
                let offset = layout.fields.offset(field).bytes() as i64;
                let (tail_size, mut tail_align) =
                    self.size_and_align_of_val(field_ty, Some(meta))?;
                if let Some(pack) = self.pack(ty) {
                    let pack = self.builder.ins().iconst(ptr(), pack as i64);
                    tail_align = self.builder.ins().umin(tail_align, pack);
                }
                let sized_align = self.builder.ins().iconst(ptr(), align);
                let full_align = self.builder.ins().umax(tail_align, sized_align);
                let offset = self.builder.ins().iconst(ptr(), offset);
                let offset = self.align_up(offset, tail_align);
                let size = self.builder.ins().iadd(offset, tail_size);
                Ok((self.align_up(size, full_align), full_align))
            }
        }
    }

    /// Where to drop a field of `container`: destructors take an aligned `&mut self`, so
    /// a field a packed struct leaves misaligned is moved out first, as rustc does.
    fn aligned_for_drop(
        &mut self,
        addr: Value,
        field: Ty<'db>,
        container: Ty<'db>,
    ) -> Result<Value> {
        let Some(pack) = self.pack(container) else { return Ok(addr) };
        let field = self.normalize(field);
        let layout = self.layout(field)?;
        if layout.align.abi.bytes() <= pack || self.glue_of(field) == DropGlue::None {
            return Ok(addr);
        }
        let slot = self.stack_slot(&layout);
        self.copy(slot, addr, layout.size);
        Ok(slot)
    }

    fn align_up(&mut self, value: Value, align: Value) -> Value {
        let mask = self.builder.ins().iadd_imm(align, -1);
        let value = self.builder.ins().iadd(value, mask);
        self.builder.ins().band_not(value, mask)
    }

    /// Address of a field whose type is unsized. A trait object's alignment, and so the
    /// field's offset, is only known from the vtable.
    fn unsized_field_addr(
        &mut self,
        addr: Value,
        meta: Option<Value>,
        container: Ty<'db>,
        offset: Size,
        field_ty: Ty<'db>,
    ) -> Result<Value> {
        let field_ty = self.normalize(field_ty);
        let mut tail = field_ty;
        while matches!(tail.kind(), TyKind::Adt(..) | TyKind::Tuple(_)) && self.is_unsized(tail) {
            tail = self.last_field(tail)?.1;
        }
        let (Some(meta), TyKind::Dynamic(..)) = (meta, tail.kind()) else {
            return Ok(self.builder.ins().iadd_imm(addr, offset.bytes() as i64));
        };
        let (_, mut align) = self.size_and_align_of_val(field_ty, Some(meta))?;
        if let Some(pack) = self.pack(container) {
            let pack = self.builder.ins().iconst(ptr(), pack as i64);
            align = self.builder.ins().umin(align, pack);
        }
        let offset = self.builder.ins().iconst(ptr(), offset.bytes() as i64);
        let offset = self.align_up(offset, align);
        Ok(self.builder.ins().iadd(addr, offset))
    }

    fn glue_of(&self, ty: Ty<'db>) -> DropGlue {
        if let Some(glue) = self.glue.borrow().get(&ty) {
            return *glue;
        }
        let glue = has_drop_glue(&self.infcx, ty, self.env.param_env);
        self.glue.borrow_mut().insert(ty, glue);
        glue
    }

    fn layout(&self, ty: Ty<'db>) -> Result<Arc<Layout>> {
        if let Some(layout) = self.layouts.borrow().get(&ty) {
            return Ok(layout.clone());
        }
        let layout = self
            .cg
            .db
            .layout_of_ty(ty.store(), self.env.store())
            .map_err(|e| CodegenError::Frontend(format!("layout of {ty:?}: {e:?}")))?;
        if let Some(log) = &self.cg.layouts {
            log.borrow_mut().entry(ty).or_insert((layout.size.bytes(), layout.align.abi.bytes()));
            if let Some(line) = coroutine_layout_line(self.cg.db, ty, &layout) {
                let (size, align) = (layout.size.bytes(), layout.align.abi.bytes());
                self.cg.coroutine_layouts.borrow_mut().push((line, size, align));
            }
        }
        self.layouts.borrow_mut().insert(ty, layout.clone());
        Ok(layout)
    }

    fn scalar(&self, scalar: Scalar) -> Result<AbiScalar> {
        Ok(match scalar.primitive() {
            Primitive::Int(int, signed) => AbiScalar {
                ty: match int {
                    Integer::I8 => types::I8,
                    Integer::I16 => types::I16,
                    Integer::I32 => types::I32,
                    Integer::I64 => types::I64,
                    Integer::I128 => types::I128,
                },
                signed,
            },
            Primitive::Pointer(_) => AbiScalar { ty: ptr(), signed: false },
            Primitive::Float(rustc_abi::Float::F32) => AbiScalar { ty: types::F32, signed: true },
            Primitive::Float(rustc_abi::Float::F64) => AbiScalar { ty: types::F64, signed: true },
            Primitive::Float(_) => unsupported!("f16 and f128"),
        })
    }

    fn scalar_of(&self, ty: Ty<'db>) -> Result<AbiScalar> {
        match self.layout(ty)?.backend_repr {
            BackendRepr::Scalar(scalar) => self.scalar(scalar),
            _ => unsupported!("expected scalar, got {ty:?}"),
        }
    }

    fn pass_mode(&self, layout: &Layout, is_ret: bool) -> Result<PassMode> {
        if layout.is_zst() {
            return Ok(PassMode::Ignore);
        }
        // rustc returns in memory what is more than two registers hold.
        let wide = is_ret && layout.size.bytes() > 2 * word() as u64;
        Ok(match layout.backend_repr {
            BackendRepr::Scalar(_) if wide => PassMode::Indirect,
            BackendRepr::Scalar(scalar) => PassMode::Direct(self.scalar(scalar)?),
            BackendRepr::ScalarPair { a, b, b_offset: offset } => {
                let (a, b) = (self.scalar(a)?, self.scalar(b)?);
                // LLVM has a WebAssembly function return one value: of a pair it makes a
                // pointer to where the pair goes, the first parameter.
                let one_result = cfg!(feature = "wasm");
                if is_ret && (wide || one_result || a.ty == types::I128 || b.ty == types::I128) {
                    // More than the two return registers hold, so LLVM returns it in memory.
                    PassMode::Indirect
                } else {
                    PassMode::Pair(a, b, offset)
                }
            }
            BackendRepr::Memory { sized: true } => {
                let max = word() as u64;
                match layout.size.bytes() {
                    size if size > max => PassMode::Indirect,
                    1 => PassMode::Cast(types::I8),
                    2 => PassMode::Cast(types::I16),
                    // An integer of exactly the aggregate's size; the bits above it are undefined.
                    3 | 4 => PassMode::Cast(types::I32),
                    _ => PassMode::Cast(types::I64),
                }
            }
            // rustc passes vectors in memory so that callers and callees built with different
            // target features agree.
            BackendRepr::SimdVector { .. } => PassMode::Indirect,
            _ => unsupported!("abi of {:?}", layout.backend_repr),
        })
    }

    /// How a call with these types passes its values. Mirrors rustc: `adjust_for_rust_abi`
    /// for Rust's own conventions, and `callconv/x86_64.rs` or `callconv/aarch64.rs` for C's.
    fn fn_abi(
        &self,
        abi: ExternAbi,
        ret: Ty<'db>,
        args: impl IntoIterator<Item = Ty<'db>>,
    ) -> Result<FnAbi> {
        if abi.is_rustic_abi() {
            let ret = self.pass_mode(&*self.layout(ret)?, true)?;
            let args = args
                .into_iter()
                .map(|ty| self.pass_mode(&*self.layout(ty)?, false))
                .collect::<Result<_>>()?;
            let mut abi = FnAbi { ret, args };
            self.wide_ints_on_stack(&mut abi)?;
            return Ok(abi);
        }
        if self.cg.target.call_conv == isa::CallConv::WindowsFastcall {
            if matches!(abi, ExternAbi::SysV64 { .. }) {
                unsupported!("calling convention {abi:?}");
            }
            let ret = self.win64_pass_mode(ret, true)?;
            let args = args
                .into_iter()
                .map(|ty| self.win64_pass_mode(ty, false))
                .collect::<Result<_>>()?;
            return Ok(FnAbi { ret, args });
        }
        if Target::current().is_wasm() {
            return self.wasm_c_abi(ret, args);
        }
        let arch = self.cg.target.arch;
        match (abi, arch) {
            (ExternAbi::C { .. } | ExternAbi::System { .. }, Arch::X86_64)
            | (ExternAbi::SysV64 { .. }, Arch::X86_64) => {
                let mut abi = self.sysv_x86_64_abi(ret, args)?;
                self.wide_ints_on_stack(&mut abi)?;
                Ok(abi)
            }
            (ExternAbi::C { .. } | ExternAbi::System { .. }, Arch::Aarch64) => {
                self.aapcs64_abi(ret, args)
            }
            _ => unsupported!("calling convention {abi:?}"),
        }
    }

    /// Passes a 128-bit integer that finds one integer register left as memory on the stack.
    ///
    /// By the System V x86-64 convention such an integer goes on the stack, and LLVM (so
    /// rustc) gives the register that is left to the next integer argument, where Cranelift
    /// gives it to nobody. As memory on the stack the integer takes no register in either,
    /// so Cranelift places every argument where rustc does, and where LLVM does.
    fn wide_ints_on_stack(&self, abi: &mut FnAbi) -> Result<()> {
        let target = &self.cg.target;
        if target.arch != Arch::X86_64 || target.call_conv != isa::CallConv::SystemV {
            return Ok(());
        }
        while let Some((at, stack)) = lone_register_i128(&self.signature(&abi.args, abi.ret).params)
        {
            // The argument the parameter is of, and which of the argument's parameters it is.
            let mut first = usize::from(matches!(abi.ret, PassMode::Indirect));
            let arg = abi
                .args
                .iter()
                .position(|&mode| {
                    let mut these = Vec::new();
                    self.push_abi_params(mode, &mut these);
                    if first + these.len() > at {
                        return true;
                    }
                    first += these.len();
                    false
                })
                .expect("every parameter comes from an argument");
            let padded = stack % 16 != 0;
            abi.args[arg] = match abi.args[arg] {
                PassMode::Direct(_) => PassMode::OnStack { size: 16, padded },
                PassMode::Pair(a, b, offset) => {
                    let mut spilled = [None; 2];
                    spilled[usize::from(at > first)] = Some(padded);
                    PassMode::SpilledPair { a, b, offset, spilled }
                }
                // Both halves are 128-bit integers, and the second finds the register the
                // first left.
                PassMode::SpilledPair { a, b, offset, spilled: [half @ Some(_), None] } => {
                    PassMode::SpilledPair { a, b, offset, spilled: [half, Some(padded)] }
                }
                mode => unreachable!("a 128-bit integer passed as {mode:?}"),
            };
        }
        Ok(())
    }

    /// The C convention of the System V x86-64 psABI, as rustc's `callconv/x86_64.rs` has it.
    fn sysv_x86_64_abi(
        &self,
        ret: Ty<'db>,
        args: impl IntoIterator<Item = Ty<'db>>,
    ) -> Result<FnAbi> {
        let (mut int_regs, mut sse_regs) = (6usize, 8usize);
        let ret = match self.c_pass_mode(ret)? {
            PassMode::OnStack { .. } => {
                // The pointer to the return value takes the first integer register.
                int_regs -= 1;
                PassMode::Indirect
            }
            mode => mode,
        };
        // How many bytes of the stack the arguments before this one take, as Cranelift
        // places them.
        let mut stack = 0u32;
        let mut modes = Vec::new();
        for ty in args {
            if self.decays_in_c(ty)? {
                int_regs = int_regs.saturating_sub(1);
                modes.push(PassMode::Indirect);
                continue;
            }
            let mode = self.c_pass_mode(ty)?;
            let (ints, sses) = match mode {
                PassMode::Ignore | PassMode::Indirect | PassMode::OnStack { .. } => (0, 0),
                PassMode::Direct(s) => self.count_regs(&[s.ty]),
                PassMode::Pair(a, b, _) => self.count_regs(&[a.ty, b.ty]),
                PassMode::Cast(ty) => self.count_regs(&[ty]),
                PassMode::CastPair(a, b) => self.count_regs(&[a, b]),
                PassMode::Uniform { .. } => unreachable!("x86-64 has no uniform aggregate"),
                PassMode::SpilledPair { .. } => unreachable!("only the stack spills a pair"),
            };
            let aggregate = matches!(mode, PassMode::Cast(_) | PassMode::CastPair(..));
            let in_registers = match (int_regs.checked_sub(ints), sse_regs.checked_sub(sses)) {
                (Some(int_left), Some(sse_left)) if !matches!(mode, PassMode::OnStack { .. }) => {
                    (int_regs, sse_regs) = (int_left, sse_left);
                    true
                }
                _ => false,
            };
            modes.push(match mode {
                _ if in_registers => mode,
                // Out of registers, or too large for them: an aggregate goes to memory
                // whole, at a multiple of its alignment.
                _ if aggregate || matches!(mode, PassMode::OnStack { .. }) => {
                    let layout = self.layout(ty)?;
                    let size = layout.size.bytes().next_multiple_of(8) as u32;
                    let padded = match layout.align.abi.bytes() {
                        ..=8 => false,
                        16 => stack % 16 != 0,
                        // FIXME: the stack is at a multiple of sixteen and no more when a
                        // function is called.
                        _ => unsupported!("an argument in memory aligned to more than 16 bytes"),
                    };
                    stack += size + if padded { 8 } else { 0 };
                    PassMode::OnStack { size, padded }
                }
                // Cranelift moves anything else to the stack by itself: eight bytes for
                // each value, and a 128-bit integer whole at a multiple of sixteen.
                PassMode::Direct(AbiScalar { ty: types::I128, .. }) => {
                    stack = stack.next_multiple_of(16) + 16;
                    mode
                }
                PassMode::Pair(..) => {
                    stack += 16;
                    mode
                }
                _ => {
                    stack += 8;
                    mode
                }
            });
        }
        Ok(FnAbi { ret, args: modes })
    }

    /// The C convention of AArch64 (AAPCS64), as rustc's `callconv/aarch64.rs` has it.
    ///
    /// Cranelift places scalars the way the convention does, so what is decided here is
    /// what an aggregate turns into, and which registers it leaves unused: Cranelift hands
    /// out registers one value at a time and knows nothing of a value that takes several
    /// or none.
    fn aapcs64_abi(&self, ret: Ty<'db>, args: impl IntoIterator<Item = Ty<'db>>) -> Result<FnAbi> {
        const REGS: u8 = 8;
        let ret = match self.aapcs64_class(ret)? {
            Aapcs64::Ignore => PassMode::Ignore,
            Aapcs64::Scalar(scalar) => PassMode::Direct(scalar),
            Aapcs64::Floats { unit, count } => PassMode::Uniform { unit, count, skipped: 0 },
            Aapcs64::Ints { size, .. } => Self::aapcs64_ints(size, 0),
            // Its address comes in a register of its own, which is not an argument's.
            Aapcs64::Memory => PassMode::Indirect,
        };
        // What a value aligned to sixteen bytes starts at a multiple of, counted in
        // registers: Apple's convention starts it at whichever register is next.
        let even = match self.cg.target.call_conv {
            isa::CallConv::AppleAarch64 => 1,
            _ => 2,
        };
        let (mut ints, mut floats) = (0u8, 0u8);
        let mut modes = Vec::new();
        for ty in args {
            let class = match self.decays_in_c(ty)? {
                true => Aapcs64::Memory,
                false => self.aapcs64_class(ty)?,
            };
            modes.push(match class {
                Aapcs64::Ignore => PassMode::Ignore,
                Aapcs64::Scalar(scalar) => {
                    let (used, wide) = match scalar.ty {
                        ty if ty.is_float() => (&mut floats, false),
                        ty => (&mut ints, ty == types::I128),
                    };
                    if wide {
                        // Cranelift starts it at an even register, or puts it on the stack
                        // whole. The registers it did not fit in stay free there, where
                        // the convention has them out of use.
                        let skipped = REGS - *used;
                        if *used + *used % even + 2 <= REGS {
                            *used += *used % even + 2;
                            PassMode::Direct(scalar)
                        } else {
                            *used = REGS;
                            PassMode::Uniform { unit: types::I128, count: 1, skipped }
                        }
                    } else {
                        *used = (*used + 1).min(REGS);
                        PassMode::Direct(scalar)
                    }
                }
                Aapcs64::Floats { unit, count } if floats + count <= REGS => {
                    floats += count;
                    PassMode::Uniform { unit, count, skipped: 0 }
                }
                Aapcs64::Floats { unit, count } => {
                    let skipped = REGS - floats;
                    floats = REGS;
                    let size = u64::from(unit.bytes()) * u64::from(count);
                    PassMode::Uniform { unit: types::F64, count: size.div_ceil(8) as u8, skipped }
                }
                Aapcs64::Ints { size, align } => {
                    let count = size.div_ceil(8) as u8;
                    let odd = if align > 8 { ints % even } else { 0 };
                    if ints + odd + count <= REGS {
                        ints += odd + count;
                        Self::aapcs64_ints(size, odd)
                    } else {
                        let skipped = REGS - ints;
                        ints = REGS;
                        // On the stack, one aligned to sixteen bytes is at a multiple of
                        // sixteen, as a 128-bit integer is.
                        match align > 8 {
                            true => PassMode::Uniform { unit: types::I128, count: 1, skipped },
                            false => PassMode::Uniform { unit: types::I64, count, skipped },
                        }
                    }
                }
                Aapcs64::Memory => {
                    ints = (ints + 1).min(REGS);
                    PassMode::Indirect
                }
            });
        }
        Ok(FnAbi { ret, args: modes })
    }

    /// An aggregate of at most sixteen bytes in integer registers, `skipped` of them unused.
    fn aapcs64_ints(size: u64, skipped: u8) -> PassMode {
        match size {
            // An integer of exactly the aggregate's size; the bits above it are undefined.
            1 if skipped == 0 => PassMode::Cast(types::I8),
            2 if skipped == 0 => PassMode::Cast(types::I16),
            3 | 4 if skipped == 0 => PassMode::Cast(types::I32),
            5..=8 if skipped == 0 => PassMode::Cast(types::I64),
            _ => PassMode::Uniform { unit: types::I64, count: size.div_ceil(8) as u8, skipped },
        }
    }

    /// What AAPCS64 makes of a value before it looks for registers.
    fn aapcs64_class(&self, ty: Ty<'db>) -> Result<Aapcs64> {
        let layout = self.layout(ty)?;
        if layout.is_zst() {
            return Ok(Aapcs64::Ignore);
        }
        match layout.backend_repr {
            BackendRepr::Scalar(scalar) => return Ok(Aapcs64::Scalar(self.scalar(scalar)?)),
            BackendRepr::SimdVector { .. } => unsupported!("vector in the C ABI"),
            _ => (),
        }
        let size = layout.size.bytes();
        let mut leaves = Vec::new();
        self.scalar_leaves(ty, 0, &mut leaves)?;
        // The members of a union lie over each other.
        leaves.sort_by_key(|&(offset, _)| offset);
        leaves.dedup();
        // A homogeneous floating-point aggregate: one to four floats of one type and
        // nothing else, which go in as many vector registers.
        if let Some(&(_, first)) = leaves.first()
            && let Primitive::Float(_) = first.primitive()
            && leaves.len() <= 4
        {
            let unit = self.scalar(first)?.ty;
            let step = u64::from(unit.bytes());
            let packed = leaves.iter().zip(0..).all(|(&(offset, scalar), index)| {
                offset == index * step && scalar.primitive() == first.primitive()
            });
            if packed && leaves.len() as u64 * step == size {
                return Ok(Aapcs64::Floats { unit, count: leaves.len() as u8 });
            }
        }
        if size > 16 {
            return Ok(Aapcs64::Memory);
        }
        // rustc has one aligned to sixteen bytes in units of 128 bits, which start at an
        // even register and at a multiple of sixteen on the stack. AAPCS64 goes by the
        // alignment the fields ask for, and Apple by the one `repr(align)` may have raised.
        let align = match self.cg.target.call_conv {
            isa::CallConv::AppleAarch64 => layout.align.abi,
            _ => layout.unadjusted_abi_align,
        };
        Ok(Aapcs64::Ints { size, align: align.bytes() })
    }

    /// Every scalar a value of the C ABI is made of, with where in the value it is.
    fn scalar_leaves(
        &self,
        ty: Ty<'db>,
        offset: u64,
        leaves: &mut Vec<(u64, Scalar)>,
    ) -> Result<()> {
        let layout = self.layout(ty)?;
        if layout.is_zst() {
            return Ok(());
        }
        match layout.backend_repr {
            BackendRepr::Scalar(a) => leaves.push((offset, a)),
            BackendRepr::ScalarPair { a, b, b_offset } => {
                leaves.extend([(offset, a), (offset + b_offset.bytes(), b)]);
            }
            BackendRepr::Memory { sized: true } => match ty.kind() {
                TyKind::Tuple(tys) => {
                    for (i, ty) in tys.iter().enumerate() {
                        self.scalar_leaves(ty, offset + layout.fields.offset(i).bytes(), leaves)?;
                    }
                }
                TyKind::Array(element, _) => {
                    let stride = self.layout(element)?.size.bytes();
                    for i in 0..layout.fields.count() as u64 {
                        // More than a register class holds: nothing asks what the rest is.
                        if leaves.len() > 16 {
                            break;
                        }
                        self.scalar_leaves(element, offset + i * stride, leaves)?;
                    }
                }
                TyKind::Adt(def, subst) => {
                    let variant: VariantId = match def.def_id() {
                        AdtId::StructId(id) => id.into(),
                        AdtId::UnionId(id) => id.into(),
                        AdtId::EnumId(_) => unsupported!("enum {ty:?} in the C ABI"),
                    };
                    for (field, ty) in self.cg.db.field_types(variant).iter() {
                        let ty = ty.ty().instantiate(self.interner, subst).skip_norm_wip();
                        let field = layout.fields.offset(u32::from(field.into_raw()) as usize);
                        self.scalar_leaves(ty, offset + field.bytes(), leaves)?;
                    }
                }
                _ => unsupported!("{ty:?} in the C ABI"),
            },
            _ => unsupported!("{ty:?} in the C ABI"),
        }
        Ok(())
    }

    /// The C ABI of wasm, as rustc's `callconv/wasm.rs`: a scalar is a value, and anything
    /// else is in memory, an argument as the address of a copy.
    // FIXME: an aggregate that is one scalar and is not laid out as one, `[f32; 1]`, is
    // passed as that scalar.
    fn wasm_c_abi(&self, ret: Ty<'db>, args: impl IntoIterator<Item = Ty<'db>>) -> Result<FnAbi> {
        let mode = |ty: Ty<'db>| -> Result<PassMode> {
            let layout = self.layout(ty)?;
            Ok(match layout.backend_repr {
                _ if layout.is_zst() => PassMode::Ignore,
                BackendRepr::Scalar(scalar) => PassMode::Direct(self.scalar(scalar)?),
                _ => PassMode::Indirect,
            })
        };
        let ret = mode(ret)?;
        let args = args.into_iter().map(mode).collect::<Result<_>>()?;
        Ok(FnAbi { ret, args })
    }

    fn count_regs(&self, types: &[Type]) -> (usize, usize) {
        let floats = types.iter().filter(|ty| ty.is_float()).count();
        let ints = types.iter().filter(|ty| !ty.is_float()).map(|ty| ty.bytes().div_ceil(8));
        (ints.sum::<u32>() as usize, floats)
    }

    /// How the C ABI of 64-bit Windows passes one value. Mirrors rustc's
    /// `callconv/x86_win64.rs`: what is not the size of a register goes by reference.
    fn win64_pass_mode(&self, ty: Ty<'db>, is_ret: bool) -> Result<PassMode> {
        let layout = self.layout(ty)?;
        if layout.is_zst() {
            // MSVC passes an argument of no size by reference.
            return Ok(if is_ret { PassMode::Ignore } else { PassMode::Indirect });
        }
        if !is_ret && self.decays_in_c(ty)? {
            return Ok(PassMode::Indirect);
        }
        Ok(match (layout.backend_repr, layout.size.bytes()) {
            (BackendRepr::SimdVector { .. }, _) => unsupported!("vector in the C ABI"),
            (BackendRepr::Scalar(scalar), size) if size <= 8 => {
                PassMode::Direct(self.scalar(scalar)?)
            }
            // A 128-bit integer is returned in `xmm0`.
            (BackendRepr::Scalar(_), 16) if is_ret => PassMode::Cast(types::I64X2),
            (BackendRepr::Scalar(_), _) => PassMode::Indirect,
            (_, 1) => PassMode::Cast(types::I8),
            (_, 2) => PassMode::Cast(types::I16),
            (_, 4) => PassMode::Cast(types::I32),
            (_, 8) => PassMode::Cast(types::I64),
            _ => PassMode::Indirect,
        })
    }

    /// Whether a C function is handed the address of a `ty` where Rust hands over the value,
    /// as C does with a `va_list` that is an array. Mirrors rustc's
    /// `TyAndLayout::pass_indirectly_in_non_rustic_abis`, which looks through transparent
    /// wrappers for a struct marked `#[rustc_pass_indirectly_in_non_rustic_abis]`.
    fn decays_in_c(&self, ty: Ty<'db>) -> Result<bool> {
        let TyKind::Adt(def, subst) = ty.kind() else { return Ok(false) };
        let AdtId::StructId(id) = def.def_id() else { return Ok(false) };
        let db = self.cg.db;
        if AttrFlags::query(db, id.into())
            .contains(AttrFlags::RUSTC_PASS_INDIRECTLY_IN_NON_RUSTIC_ABIS)
        {
            return Ok(true);
        }
        let transparent = AttrFlags::repr(db, id.into())
            .is_some_and(|repr| repr.flags.contains(ReprFlags::IS_TRANSPARENT));
        if !transparent {
            return Ok(false);
        }
        for (_, field) in db.field_types(id.into()).iter() {
            let field = field.ty().instantiate(self.interner, subst).skip_norm_wip();
            if !self.layout(field)?.is_1zst() {
                return self.decays_in_c(field);
            }
        }
        Ok(false)
    }

    /// How the C ABI passes one value when registers are left for it.
    fn c_pass_mode(&self, ty: Ty<'db>) -> Result<PassMode> {
        let layout = self.layout(ty)?;
        if layout.is_zst() {
            return Ok(PassMode::Ignore);
        }
        let size = layout.size.bytes();
        let on_stack = PassMode::OnStack { size: size.next_multiple_of(8) as u32, padded: false };
        match layout.backend_repr {
            BackendRepr::Scalar(scalar) => return Ok(PassMode::Direct(self.scalar(scalar)?)),
            BackendRepr::SimdVector { .. } => unsupported!("vector in the C ABI"),
            _ => (),
        }
        if size > 16 {
            return Ok(on_stack);
        }
        let mut classes = [None; 2];
        if !self.classify(ty, 0, &mut classes)? {
            return Ok(on_stack);
        }
        let reg = |class: Option<RegClass>, bytes: u64| match (class, bytes) {
            (Some(RegClass::Sse), 4) => types::F32,
            (Some(RegClass::Sse), _) => types::F64,
            (_, 1) => types::I8,
            (_, 2) => types::I16,
            (_, 3 | 4) => types::I32,
            _ => types::I64,
        };
        Ok(if size <= 8 {
            PassMode::Cast(reg(classes[0], size))
        } else {
            PassMode::CastPair(reg(classes[0], 8), reg(classes[1], size - 8))
        })
    }

    /// Classifies the eightbytes of a value of at most sixteen bytes at `offset`; false if it
    /// has to go in memory.
    fn classify(
        &self,
        ty: Ty<'db>,
        offset: u64,
        classes: &mut [Option<RegClass>; 2],
    ) -> Result<bool> {
        let layout = self.layout(ty)?;
        if layout.is_zst() {
            return Ok(true);
        }
        if offset % layout.align.abi.bytes() != 0 {
            return Ok(false);
        }
        let mut scalar = |scalar: Scalar, offset: u64| {
            let class = match scalar.primitive() {
                Primitive::Float(_) => RegClass::Sse,
                Primitive::Int(..) | Primitive::Pointer(_) => RegClass::Int,
            };
            let last = (offset + scalar.size(self.dl()).bytes() - 1) / 8;
            for slot in &mut classes[(offset / 8) as usize..=last as usize] {
                *slot = Some(slot.map_or(class, |old: RegClass| old.min(class)));
            }
        };
        match layout.backend_repr {
            BackendRepr::Scalar(a) => scalar(a, offset),
            BackendRepr::ScalarPair { a, b, b_offset } => {
                scalar(a, offset);
                scalar(b, offset + b_offset.bytes());
            }
            BackendRepr::Memory { sized: true } => match ty.kind() {
                TyKind::Tuple(tys) => {
                    for (i, ty) in tys.iter().enumerate() {
                        if !self.classify(ty, offset + layout.fields.offset(i).bytes(), classes)? {
                            return Ok(false);
                        }
                    }
                }
                TyKind::Array(element, _) => {
                    let stride = self.layout(element)?.size.bytes();
                    for i in 0..layout.fields.count() as u64 {
                        if !self.classify(element, offset + i * stride, classes)? {
                            return Ok(false);
                        }
                    }
                }
                TyKind::Adt(def, subst) => {
                    let variant: VariantId = match def.def_id() {
                        AdtId::StructId(id) => id.into(),
                        AdtId::UnionId(id) => id.into(),
                        AdtId::EnumId(_) => unsupported!("enum {ty:?} in the C ABI"),
                    };
                    for (field, ty) in self.cg.db.field_types(variant).iter() {
                        let ty = ty.ty().instantiate(self.interner, subst).skip_norm_wip();
                        let field = layout.fields.offset(u32::from(field.into_raw()) as usize);
                        if !self.classify(ty, offset + field.bytes(), classes)? {
                            return Ok(false);
                        }
                    }
                }
                _ => unsupported!("{ty:?} in the C ABI"),
            },
            _ => unsupported!("{ty:?} in the C ABI"),
        }
        Ok(true)
    }

    fn dl(&self) -> &'db rustc_abi::TargetDataLayout {
        self.cg.db.target_data_layout_or_default(self.env.krate)
    }

    fn push_abi_params(&self, mode: PassMode, params: &mut Vec<AbiParam>) {
        let copied = |size| AbiParam::special(ptr(), ArgumentPurpose::StructArgument(size));
        match mode {
            PassMode::Ignore => (),
            PassMode::Direct(s) => params.push(s.param()),
            PassMode::Pair(a, b, _) => params.extend([a.param(), b.param()]),
            PassMode::Cast(ty) => params.push(AbiParam::new(ty)),
            PassMode::CastPair(a, b) => params.extend([AbiParam::new(a), AbiParam::new(b)]),
            PassMode::Uniform { unit, count, skipped } => {
                params.extend((0..skipped).map(|_| AbiParam::new(PassMode::register(unit))));
                params.extend((0..count).map(|_| AbiParam::new(unit)));
            }
            PassMode::Indirect => params.push(AbiParam::new(ptr())),
            PassMode::OnStack { size, padded } => {
                params.extend(padded.then(|| copied(8)));
                params.push(copied(size));
            }
            PassMode::SpilledPair { a, b, spilled, .. } => {
                for (half, spilled) in [a, b].into_iter().zip(spilled) {
                    match spilled {
                        None => params.push(half.param()),
                        Some(padded) => {
                            params.extend(padded.then(|| copied(8)));
                            params.push(copied(16));
                        }
                    }
                }
            }
        }
    }

    fn signature(&self, args: &[PassMode], ret: PassMode) -> Signature {
        let mut sig = self.cg.make_signature();
        match ret {
            PassMode::Indirect => {
                sig.params.push(AbiParam::special(ptr(), ArgumentPurpose::StructReturn))
            }
            mode => self.push_abi_params(mode, &mut sig.returns),
        }
        for &arg in args {
            self.push_abi_params(arg, &mut sig.params);
        }
        sig
    }

    /// Loads the register image of a `PassMode::Cast` aggregate of `size` bytes.
    fn load_cast(&mut self, ty: Type, addr: Value, size: Size) -> Value {
        if u64::from(ty.bytes()) == size.bytes() {
            return self.builder.ins().load(ty, MEM, addr, 0);
        }
        let tmp = self.scratch();
        self.copy(tmp, addr, size);
        self.builder.ins().load(ty, MEM, tmp, 0)
    }

    fn store_cast(&mut self, value: Value, addr: Value, size: Size) {
        let ty = self.builder.func.dfg.value_type(value);
        if u64::from(ty.bytes()) == size.bytes() {
            self.builder.ins().store(MEM, value, addr, 0);
            return;
        }
        let tmp = self.scratch();
        self.builder.ins().store(MEM, value, tmp, 0);
        self.copy(addr, tmp, size);
    }

    /// The values that carry the argument at `addr` to a callee.
    fn pass(&mut self, mode: PassMode, addr: Value, layout: &Layout, values: &mut Vec<Value>) {
        match mode {
            PassMode::Ignore => (),
            PassMode::Direct(s) => values.push(self.builder.ins().load(s.ty, MEM, addr, 0)),
            PassMode::Pair(a, b, offset) => {
                values.push(self.builder.ins().load(a.ty, MEM, addr, 0));
                values.push(self.builder.ins().load(b.ty, MEM, addr, offset.bytes() as i32));
            }
            PassMode::Cast(ty) => values.push(self.load_cast(ty, addr, layout.size)),
            PassMode::CastPair(a, b) => {
                values.push(self.builder.ins().load(a, MEM, addr, 0));
                let rest = self.builder.ins().iadd_imm(addr, 8);
                values.push(self.load_cast(b, rest, layout.size - Size::from_bytes(8)));
            }
            PassMode::Uniform { unit, count, skipped } => {
                for _ in 0..skipped {
                    values.push(self.zero(PassMode::register(unit)));
                }
                let step = u64::from(unit.bytes());
                for index in 0..u64::from(count) {
                    let part = self.builder.ins().iadd_imm(addr, (index * step) as i64);
                    let left = layout.size - Size::from_bytes(index * step);
                    values.push(self.load_cast(unit, part, left.min(Size::from_bytes(step))));
                }
            }
            PassMode::Indirect => {
                // The callee owns the memory it is handed.
                if self.given_away.as_ref().is_some_and(|it| it.gives(addr)) {
                    values.push(addr);
                    return;
                }
                let copy = self.argument_slot(layout);
                self.copy(copy, addr, layout.size);
                values.push(copy);
            }
            PassMode::OnStack { padded, .. } => {
                if padded {
                    values.push(self.scratch());
                }
                values.push(addr);
            }
            PassMode::SpilledPair { a, b, offset, spilled } => {
                for (half, at, spilled) in [(a, 0, spilled[0]), (b, offset.bytes(), spilled[1])] {
                    match spilled {
                        None => values.push(self.builder.ins().load(half.ty, MEM, addr, at as i32)),
                        Some(padded) => {
                            if padded {
                                values.push(self.scratch());
                            }
                            values.push(self.builder.ins().iadd_imm(addr, at as i64));
                        }
                    }
                }
            }
        }
    }

    /// Stores what `pass` or a return sent, taking the values off the front of `values`.
    fn receive(
        &mut self,
        mode: PassMode,
        values: &mut impl Iterator<Item = Value>,
        addr: Value,
        layout: &Layout,
    ) {
        let mut next = || values.next().expect("a value for every abi parameter");
        match mode {
            PassMode::Ignore | PassMode::Indirect | PassMode::OnStack { .. } => (),
            PassMode::Direct(_) => {
                let value = next();
                self.builder.ins().store(MEM, value, addr, 0);
            }
            PassMode::Pair(_, _, offset) => {
                let (a, b) = (next(), next());
                self.builder.ins().store(MEM, a, addr, 0);
                self.builder.ins().store(MEM, b, addr, offset.bytes() as i32);
            }
            PassMode::Cast(_) => {
                let value = next();
                self.store_cast(value, addr, layout.size);
            }
            PassMode::CastPair(..) => {
                let (a, b) = (next(), next());
                self.builder.ins().store(MEM, a, addr, 0);
                let rest = self.builder.ins().iadd_imm(addr, 8);
                self.store_cast(b, rest, layout.size - Size::from_bytes(8));
            }
            PassMode::Uniform { unit, count, skipped } => {
                for _ in 0..skipped {
                    next();
                }
                let step = u64::from(unit.bytes());
                for index in 0..u64::from(count) {
                    let value = next();
                    let part = self.builder.ins().iadd_imm(addr, (index * step) as i64);
                    let left = layout.size - Size::from_bytes(index * step);
                    self.store_cast(value, part, left.min(Size::from_bytes(step)));
                }
            }
            PassMode::SpilledPair { a, b, offset, spilled } => {
                for (half, at, spilled) in [(a, 0, spilled[0]), (b, offset.bytes(), spilled[1])] {
                    let value = match spilled {
                        None => next(),
                        Some(padded) => {
                            if padded {
                                next();
                            }
                            let copy = next();
                            self.builder.ins().load(half.ty, MEM, copy, 0)
                        }
                    };
                    self.builder.ins().store(MEM, value, addr, at as i32);
                }
            }
        }
    }

    /// The value of `ty` that is all zero bits.
    fn zero(&mut self, ty: Type) -> Value {
        match ty {
            types::F32 => self.builder.ins().f32const(0.0),
            types::F64 => self.builder.ins().f64const(0.0),
            _ => self.builder.ins().iconst(ty, 0),
        }
    }

    /// Eight bytes of stack.
    fn scratch(&mut self) -> Value {
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            3,
        ));
        self.builder.ins().stack_addr(ptr(), slot, 0)
    }

    /// Storage for an integer of 128 bits.
    fn scratch_wide(&mut self) -> Value {
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            16,
            4,
        ));
        self.builder.ins().stack_addr(ptr(), slot, 0)
    }

    /// Storage for a fat pointer.
    fn scratch_pair(&mut self) -> Value {
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            16,
            3,
        ));
        self.builder.ins().stack_addr(ptr(), slot, 0)
    }

    /// Storage for a temporary that is borrowed for `'static`. The function writes the same
    /// constant there every time it runs.
    // FIXME: evaluate the constant and make this read-only data.
    fn promoted_slot(&mut self, layout: &Layout) -> Result<Value> {
        let align = layout.align.abi.bytes();
        if layout.size.bytes() == 0 {
            return Ok(self.builder.ins().iconst(ptr(), align as i64));
        }
        let blob = ConstBlob {
            bytes: vec![0; layout.size.bytes_usize()],
            relocs: Vec::new(),
            fn_relocs: Vec::new(),
            align,
        };
        let data = self.cg.define_data(blob, true)?;
        let data = self.cg.declare_data_in_func(data, self.builder.func);
        Ok(self.builder.ins().symbol_value(ptr(), data))
    }

    /// The read-only data a promoted `&expr` points to, when `expr` can be evaluated now.
    /// Equal constants share an address, as they do for rustc. A type that is not `plain` is
    /// promoted when the value is: a `None` of an `Option<Box<T>>` has nothing to drop.
    fn promoted_constant(
        &mut self,
        body: &'db MirBody<'db>,
        expr: ExprId,
        layout: &Layout,
        plain: bool,
    ) -> Option<Value> {
        let db = self.cg.db;
        if layout.size.bytes() == 0 || self.mentions_location(body, expr) {
            return None;
        }
        let constant = monomorphized_promoted_mir_body_query(
            db,
            body.owner,
            expr,
            self.args.clone()?,
            self.env.store(),
        )
        .ok()?;
        let allocation = interpret_mir(db, constant, false, Some(self.env)).ok()?.0.ok()?;
        let ty = allocation.ty;
        let blob = self.allocation_blob(allocation, false).ok()?;
        if blob.bytes.len() != layout.size.bytes_usize()
            || !plain && !self.is_plain_value(&blob.bytes, 0, ty)
        {
            return None;
        }
        self.blob_addr(blob).ok()
    }

    /// Whether a value of `ty` can be read-only data that is never dropped.
    fn is_plain(&self, ty: Ty<'db>) -> bool {
        self.is_freeze(ty) && has_drop_glue(&self.infcx, ty, self.env.param_env) == DropGlue::None
    }

    /// Whether the value of `ty` at `offset` in `bytes` has nothing to drop and no cell in
    /// it, which a value of a type that is not plain can be: an enum by its variant.
    fn is_plain_value(&self, bytes: &[u8], offset: usize, ty: Ty<'db>) -> bool {
        if self.is_plain(ty) {
            return true;
        }
        let db = self.cg.db;
        let Ok(layout) = self.layout(ty) else { return false };
        match ty.kind() {
            TyKind::Tuple(fields) => fields.iter().enumerate().all(|(i, field)| {
                let at = offset + layout.fields.offset(i).bytes_usize();
                self.is_plain_value(bytes, at, field)
            }),
            TyKind::Array(element, len) => {
                let (Some(len), Ok(element_layout)) =
                    (try_const_usize(db, len), self.layout(element))
                else {
                    return false;
                };
                let size = element_layout.size.bytes_usize();
                (0..len as usize).all(|i| self.is_plain_value(bytes, offset + i * size, element))
            }
            TyKind::Adt(def, args) => {
                let adt = def.def_id();
                if destructor(db, adt).is_some() {
                    return false;
                }
                let (variant, layout): (VariantId, Cow<'_, Layout>) = match adt {
                    AdtId::StructId(id) if Some(id) == self.interner.lang_items().UnsafeCell => {
                        return false;
                    }
                    AdtId::StructId(id) => (id.into(), Cow::Borrowed(&*layout)),
                    AdtId::UnionId(_) => return false,
                    AdtId::EnumId(id) => {
                        let Some(bytes) = bytes.get(offset..offset + layout.size.bytes_usize())
                        else {
                            return false;
                        };
                        match detect_variant_from_bytes(&layout, db, self.dl(), bytes, id) {
                            Some((variant, layout)) => (variant.into(), layout),
                            None => return false,
                        }
                    }
                };
                db.field_types(variant).iter().enumerate().all(|(i, (_, field))| {
                    let field = field.ty().instantiate(self.interner, args).skip_norm_wip();
                    let at = offset + layout.fields.offset(i).bytes_usize();
                    self.is_plain_value(bytes, at, field)
                })
            }
            _ => false,
        }
    }

    /// Storage for the copy of an argument a callee is handed. The copy is the callee's
    /// until it returns and nobody's after, so a later call has its own in the same place:
    /// a function that calls a hundred others has the frame of its widest call and not
    /// of all of them.
    fn argument_slot(&mut self, layout: &Layout) -> Value {
        let (size, align) = (layout.size.bytes() as u32, layout.align.abi.bytes());
        if align > 16 {
            return self.stack_slot(layout);
        }
        let align_shift = align.trailing_zeros() as u8;
        let free = self.free_argument_slots.iter().position(|it| it.1 == (size, align_shift));
        let slot = match free {
            Some(free) => self.free_argument_slots.swap_remove(free).0,
            None => self.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                size,
                align_shift,
            )),
        };
        self.argument_slots.push((slot, (size, align_shift)));
        self.builder.ins().stack_addr(ptr(), slot, 0)
    }

    /// The address `addr` is, taken anew when it is of a slot of the frame. One taken where
    /// the function starts is kept in a register or spilled for as long as the function
    /// runs, and a function has more locals than there are registers.
    fn slot_addr(&mut self, addr: Value) -> Value {
        let dfg = &self.builder.func.dfg;
        if let ir::ValueDef::Result(inst, _) = dfg.value_def(addr)
            && let ir::InstructionData::StackAddr {
                opcode: ir::Opcode::StackAddr,
                stack_slot,
                offset,
            } = dfg.insts[inst]
        {
            return self.builder.ins().stack_addr(ptr(), stack_slot, offset);
        }
        addr
    }

    fn stack_slot(&mut self, layout: &Layout) -> Value {
        let (size, align) = (layout.size.bytes() as u32, layout.align.abi.bytes());
        if align <= 16 {
            let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                size,
                align.trailing_zeros() as u8,
            ));
            return self.builder.ins().stack_addr(ptr(), slot, 0);
        }
        // Cranelift keeps the frame 16-byte aligned and no more, so over-allocate and round up.
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            size + align as u32,
            4,
        ));
        let base = self.builder.ins().stack_addr(ptr(), slot, 0);
        let bumped = self.builder.ins().iadd_imm(base, align as i64 - 1);
        self.builder.ins().band_imm(bumped, -(align as i64))
    }

    /// The signature a MIR body is compiled with.
    fn body_abi(&self, body: &MirBody<'db>, abi: ExternAbi) -> Result<FnAbi> {
        self.fn_abi(
            abi,
            body.locals[returned_local(body)].ty.as_ref(),
            body.param_locals.iter().map(|&local| body.locals[local].ty.as_ref()),
        )
    }

    fn body_types(&self, def: InstanceDef, args: GenericArgs<'db>) -> Result<&'db BodyTypes> {
        // The types of a body are the same however it is compiled.
        let instance = Instance { def, args: args.store(), checked: false };
        body_types(self.cg.db, instance, self.env.store()).as_ref().map_err(Clone::clone)
    }

    /// The signature the body of an instance is compiled with.
    fn body_signature(&self, body: &BodyTypes) -> Result<Signature> {
        let abi = self.fn_abi(
            ExternAbi::Rust,
            body.returned.as_ref(),
            body.params.iter().map(|ty| ty.as_ref()),
        )?;
        Ok(self.signature(&abi.args, abi.ret))
    }

    fn codegen(&mut self) -> Result<()> {
        let body = self.body.expect("MIR body");
        let FnAbi { ret, args: mut param_modes } = self.body_abi(body, self.abi)?;
        self.ret_mode = ret;
        if self.variadic {
            // The list is made here of what came, and is not itself something that came.
            param_modes.pop();
        }
        let param_layouts = body
            .param_locals
            .iter()
            .map(|&l| self.layout(body.locals[l].ty.as_ref()))
            .collect::<Result<Vec<_>>>()?;
        let mut sig = self.signature(&param_modes, ret);
        if self.tracked {
            sig.params.push(AbiParam::new(ptr()));
        }
        let variadic = match self.variadic {
            true => {
                let native = match self.cg.target.variable_arguments() {
                    Some(VariableArguments::Registers) => {
                        matches!(self.abi, ExternAbi::C { .. })
                            || matches!(self.abi, ExternAbi::SysV64 { .. })
                                && self.cg.target.arch == Arch::X86_64
                    }
                    Some(VariableArguments::Homed) => {
                        matches!(self.abi, ExternAbi::C { .. } | ExternAbi::Win64 { .. })
                    }
                    Some(VariableArguments::Stack) => matches!(self.abi, ExternAbi::C { .. }),
                    None => false,
                };
                let Some(variadic) = Variadic::of(self.cg.target, &sig)?.filter(|_| native) else {
                    unsupported!("a function that takes `...` for this target");
                };
                variadic.rest(&mut sig);
                Some(variadic)
            }
            false => None,
        };
        self.builder.func.signature = sig;
        self.cg.pointees = self.pointees(body, ret, &param_modes)?;

        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);
        let mut incoming = self.builder.block_params(entry).to_vec().into_iter();

        if let PassMode::Indirect = self.ret_mode {
            let addr = incoming.next().expect("sret param");
            self.locals.insert(returned_local(body), LocalSlot { addr, borrowed: false });
        }
        for ((&local, layout), mode) in
            body.param_locals.iter().zip(&param_layouts).zip(param_modes)
        {
            let addr = match mode {
                PassMode::OnStack { padded: true, .. } => {
                    incoming.nth(1).expect("a parameter in memory after its padding")
                }
                PassMode::Indirect | PassMode::OnStack { .. } => {
                    incoming.next().expect("indirect param")
                }
                _ => self.stack_slot(layout),
            };
            self.receive(mode, &mut incoming, addr, layout);
            self.locals.insert(local, LocalSlot { addr, borrowed: false });
            if matches!(mode, PassMode::Indirect | PassMode::OnStack { .. }) {
                self.note_passed_in_memory(body, local, addr);
            }
        }
        if self.tracked {
            self.location = incoming.next();
        }
        if let Some(Variadic::Windows(named)) = variadic {
            let list = *body.param_locals.last().expect("the list of a variadic function");
            let layout = self.layout(body.locals[list].ty.as_ref())?;
            if layout.size.bytes() != 8 {
                unsupported!("a `VaList` that is not the one of this target");
            }
            let addr = self.stack_slot(&layout);
            let homed: Vec<Value> = (0..named.left())
                .map(|_| incoming.next().expect("a register of the variable arguments"))
                .collect();
            let stack = incoming.next().expect("the variable arguments on the stack");
            let first = -i64::from(named.left() * 8);
            let first = self.builder.ins().iadd_imm(stack, first);
            for (at, value) in homed.into_iter().enumerate() {
                self.builder.ins().store(MEM, value, first, at as i32 * 8);
            }
            self.builder.ins().store(MEM, first, addr, 0);
            self.locals.insert(list, LocalSlot { addr, borrowed: false });
        }
        if let Some(Variadic::SystemV(named)) = variadic {
            let list = *body.param_locals.last().expect("the list of a variadic function");
            let layout = self.layout(body.locals[list].ty.as_ref())?;
            let (int_count, floats_bytes) = (
                VariadicRegisters::int_count(named.arch),
                VariadicRegisters::FLOATS * VariadicRegisters::FLOAT_BYTES,
            );
            if layout.size.bytes() != if named.arch == Arch::Aarch64 { 32 } else { 24 } {
                unsupported!("a `VaList` that is not the one of this target");
            }
            let addr = self.stack_slot(&layout);
            // Where `va_arg` expects the registers saved: the integer ones, then the vector
            // ones, each kind at the place it has among all of its kind.
            let saved = self.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                int_count * 8 + floats_bytes,
                4,
            ));
            let saved = self.builder.ins().stack_addr(ptr(), saved, 0);
            for offset in (named.ints..int_count).map(|at| at * 8).chain(
                (named.floats..VariadicRegisters::FLOATS)
                    .map(|at| int_count * 8 + at * VariadicRegisters::FLOAT_BYTES),
            ) {
                let value = incoming.next().expect("a register of the variable arguments");
                self.builder.ins().store(MEM, value, saved, offset as i32);
            }
            let floats_at = int_count * 8 + named.floats * VariadicRegisters::FLOAT_BYTES;
            if named.arch == Arch::Aarch64 {
                // The list has where each kind of saved register ends, and how far before
                // that the next one is. The arguments on the stack are above the frame
                // pointer and the return address, the named ones first. Asking for the
                // frame pointer here is what has `emit` keep one.
                let frame = self.builder.ins().get_frame_pointer(ptr());
                let stack = self.builder.ins().iadd_imm(frame, 16 + i64::from(named.stack));
                let ints_end = self.builder.ins().iadd_imm(saved, i64::from(int_count * 8));
                let floats_end =
                    self.builder.ins().iadd_imm(saved, i64::from(int_count * 8 + floats_bytes));
                let ints = i64::from(named.ints * 8) - i64::from(int_count * 8);
                let ints = self.builder.ins().iconst(types::I32, ints);
                let floats = i64::from(floats_at) - i64::from(int_count * 8 + floats_bytes);
                let floats = self.builder.ins().iconst(types::I32, floats);
                self.builder.ins().store(MEM, stack, addr, 0);
                self.builder.ins().store(MEM, ints_end, addr, 8);
                self.builder.ins().store(MEM, floats_end, addr, 16);
                self.builder.ins().store(MEM, ints, addr, 24);
                self.builder.ins().store(MEM, floats, addr, 28);
            } else {
                // The list has how far into what was saved the next of each kind is.
                let stack = incoming.next().expect("the variable arguments on the stack");
                let ints = self.builder.ins().iconst(types::I32, i64::from(named.ints * 8));
                let floats = self.builder.ins().iconst(types::I32, i64::from(floats_at));
                self.builder.ins().store(MEM, ints, addr, 0);
                self.builder.ins().store(MEM, floats, addr, 4);
                self.builder.ins().store(MEM, stack, addr, 8);
                self.builder.ins().store(MEM, saved, addr, 16);
            }
            self.locals.insert(list, LocalSlot { addr, borrowed: false });
        }
        if let Some(Variadic::Apple { named }) = variadic {
            let list = *body.param_locals.last().expect("the list of a variadic function");
            let layout = self.layout(body.locals[list].ty.as_ref())?;
            if layout.size.bytes() != 8 {
                unsupported!("a `VaList` that is not the one of this target");
            }
            let addr = self.stack_slot(&layout);
            // What the caller left on the stack is right above the frame record, which is
            // the frame pointer and the return address: the named parameters that are
            // there, then the variable ones.
            let frame = self.builder.ins().get_frame_pointer(ptr());
            let stack = self.builder.ins().iadd_imm(frame, 16 + i64::from(named));
            self.builder.ins().store(MEM, stack, addr, 0);
            self.locals.insert(list, LocalSlot { addr, borrowed: false });
        }
        if body.phase != MirPhase::Runtime {
            unsupported!("a body whose drops are not elaborated");
        }
        // An `async` body keeps its locals in the future, where they survive a return.
        let frame = match body.coroutine {
            Some(_) => {
                let receiver = body.param_locals[0];
                let Some(ty) = body.locals[receiver].ty.as_ref().builtin_deref(true) else {
                    unsupported!("async body without a reference to its future");
                };
                let TyKind::Coroutine(_, args) = ty.kind() else {
                    unsupported!("async body of {ty:?}");
                };
                let upvars = args.as_coroutine().tupled_upvars_ty().tuple_fields().len();
                let held = body.locals_held_across_yields();
                let frame = CoroutineFrame { upvars };
                let addr = self.builder.ins().load(ptr(), MEM, self.locals[receiver].addr, 0);
                Some((frame, self.layout(ty)?, addr, ty, held))
            }
            None => None,
        };
        let in_frame = |this: &mut Self, field: usize| {
            let (_, layout, addr, ..) = frame.as_ref().expect("frame");
            this.builder.ins().iadd_imm(*addr, layout.fields.offset(field).bytes() as i64)
        };
        // Where a local is among the ones the future keeps, when it is one.
        let held_at = |local: LocalId| {
            let (.., held) = frame.as_ref()?;
            held.locals.binary_search(&local).ok()
        };
        // In an `async` body a temporary may be kept in the future where its binding is not.
        let forwarded = if frame.is_none() { forwarded_temps(body) } else { FxHashMap::default() };
        // A local kept in a register gets a slot only when it needs memory: see `local_slot`.
        let mut registers = Vec::new();
        if frame.is_none() {
            for local in register_locals(body, &forwarded) {
                if !self.locals.contains_idx(local)
                    && let Some(ty) = self.register_ty(body.locals[local].ty.as_ref())
                {
                    registers.push((local, ty));
                }
            }
        }
        for &(local, ty) in &registers {
            let var = self.builder.declare_var(ty);
            self.registers.insert(local, Register { var, ty });
        }
        for (local, data) in body.locals.iter() {
            if self.locals.contains_idx(local)
                || forwarded.contains_key(&local)
                || self.registers.contains_key(&local)
            {
                continue;
            }
            let layout = self.layout(data.ty.as_ref())?;
            // A capture an `async` body uses in place is the future's field.
            if let Some((_, frame_layout, frame_addr, ..)) = &frame
                && let Some(&(_, field)) = body.upvars_in_place.iter().find(|it| it.0 == local)
            {
                let offset = frame_layout.fields.offset(field as usize).bytes();
                let addr = self.builder.ins().iadd_imm(*frame_addr, offset as i64);
                self.locals.insert(local, LocalSlot { addr, borrowed: false });
                continue;
            }
            if let Some((_, frame_layout, frame_addr, _, held)) = &frame
                && let Some(at) = held_at(local)
            {
                let Some(offset) = CoroutineFrame::local_offset(frame_layout, held, at) else {
                    unsupported!("a held local that is kept nowhere");
                };
                let addr = self.builder.ins().iadd_imm(*frame_addr, offset.bytes() as i64);
                self.locals.insert(local, LocalSlot { addr, borrowed: false });
                self.held_at.push((local, offset.bytes() as u32));
                continue;
            }
            let promoted = body.promoted_locals.iter().find(|it| it.0 == local);
            let addr = if let Some(&(_, expr)) = promoted {
                let plain = self.is_plain(data.ty.as_ref());
                match self.promoted_constant(body, expr, &layout, plain) {
                    Some(addr) => {
                        self.constant_locals.push(local);
                        addr
                    }
                    None if plain => self.promoted_slot(&layout)?,
                    None => self.stack_slot(&layout),
                }
            } else {
                self.stack_slot(&layout)
            };
            if layout.is_unsized() {
                let addr = self.scratch_pair();
                self.locals.insert(local, LocalSlot { addr, borrowed: true });
                continue;
            }
            self.locals.insert(local, LocalSlot { addr, borrowed: false });
        }
        for (temp, to) in forwarded {
            self.locals.insert(temp, self.locals[to]);
        }
        for (id, _) in body.basic_blocks.iter() {
            let block = self.builder.create_block();
            self.blocks.insert(id, block);
        }

        // Only blocks that can run are compiled: `if cfg!(miri) { .. }` calls functions that do
        // not exist.
        let mut reachable = rustc_hash::FxHashSet::default();
        // Switches on a value known now, and the one way they go.
        let mut decided = FxHashMap::default();
        let mut worklist = vec![body.start_block];
        while let Some(id) = worklist.pop() {
            if !reachable.insert(id) {
                continue;
            }
            match body.basic_blocks[id].terminator.as_ref().map(|it| &it.kind) {
                Some(TerminatorKind::SwitchInt { discr, targets }) => {
                    match self.switch_bits(&body.basic_blocks[id], discr) {
                        Some(bits) => {
                            let target = targets.target_for_value(bits);
                            decided.insert(id, target);
                            worklist.push(target)
                        }
                        None => worklist.extend_from_slice(targets.all_targets()),
                    }
                }
                Some(
                    TerminatorKind::Goto { target }
                    | TerminatorKind::Drop { target, .. }
                    | TerminatorKind::DropAndReplace { target, .. }
                    | TerminatorKind::Assert { target, .. }
                    | TerminatorKind::FalseEdge { real_target: target, .. }
                    | TerminatorKind::FalseUnwind { real_target: target, .. },
                ) => worklist.push(*target),
                Some(TerminatorKind::Call { target, .. }) => worklist.extend(*target),
                Some(TerminatorKind::Yield { resume, drop, .. }) => {
                    worklist.push(*resume);
                    worklist.extend(*drop);
                }
                _ => (),
            }
            let block = &body.basic_blocks[id];
            worklist.extend(block.terminator.as_ref().and_then(|it| it.kind.unwind()));
        }
        let state = frame.as_ref().map(|(frame, ..)| frame.state()).map(|it| in_frame(self, it));
        match (frame, state) {
            (Some((_, layout, addr, ty, held)), Some(state)) => {
                let Some(tag) = CoroutineFrame::state_of(&layout) else {
                    unsupported!("an async block without a state");
                };
                let state = SuspendedState { addr: state, ty: self.scalar(tag)?.ty, held };
                self.enter_coroutine(body, &state, addr, ty, &layout)?;
                self.suspended = Some(state);
            }
            _ => {
                self.builder.ins().jump(self.blocks[body.start_block], &[]);
            }
        }
        for (id, block) in body.basic_blocks.iter() {
            if !reachable.contains(&id) {
                continue;
            }
            self.builder.switch_to_block(self.blocks[id]);
            self.in_cleanup = block.is_cleanup;
            if block.is_cleanup {
                self.builder.set_cold_block(self.blocks[id]);
            }
            for statement in &block.statements {
                self.at(statement.span);
                // No statement unwinds: what panics is an `Assert`.
                self.unwind_to = None;
                match &statement.kind {
                    // A promoted constant already holds its value.
                    StatementKind::Assign(place, _)
                        if self.constant_locals.contains(&place.local) => {}
                    StatementKind::Assign(place, rvalue) => self.assign(place, rvalue)?,
                    StatementKind::Deinit(_) => unsupported!("deinit"),
                    StatementKind::InlineAsm(asm) => self.inline_asm(asm)?,
                    StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::FakeRead(_)
                    | StatementKind::Nop => (),
                }
            }
            let Some(terminator) = &block.terminator else {
                unsupported!("block without terminator");
            };
            self.at(terminator.span);
            self.unwind_to = terminator.kind.unwind();
            let target = match &terminator.kind {
                // A promoted call already holds its value, and that value is read-only.
                TerminatorKind::Call { destination, target: Some(target), .. }
                    if self.constant_locals.contains(&destination.local) =>
                {
                    Some(*target)
                }
                _ => decided.get(&id).copied(),
            };
            match target {
                Some(target) => {
                    self.builder.ins().jump(self.blocks[target], &[]);
                }
                None => self.terminator(&terminator.kind)?,
            }
        }
        self.in_cleanup = false;
        self.unwind_to = None;
        self.emit_pads();
        Ok(())
    }

    /// Field types can still be projections such as `<str as ToOwned>::Owned`.
    fn normalize(&self, ty: Ty<'db>) -> Ty<'db> {
        crate::next_solver::obligation_ctxt::ObligationCtxt::new(&self.infcx)
            .structurally_normalize_ty(
                &crate::next_solver::infer::traits::ObligationCause::dummy(),
                self.env.param_env,
                ty,
            )
            .unwrap_or(ty)
    }

    /// The value of an integer, `bool` or `char` literal operand.
    fn const_bits(&self, operand: &Operand) -> Option<u128> {
        let OperandKind::Constant { konst, ty } = &operand.kind else { return None };
        let ty = ty.as_ref();
        if !matches!(ty.kind(), TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Uint(_)) {
            return None;
        }
        let ConstKind::Value(value) = konst.as_ref().kind() else { return None };
        let layout = self.layout(ty).ok()?;
        Some(value.value.inner().to_leaf().to_bits(layout.size))
    }

    /// The value `block` switches on, when it is a constant: a literal, or an associated
    /// constant such as `T::IS_ZST` that the block has just read. rustc does not compile the
    /// arms such a switch never takes either, and they may not even type check for this `T`.
    fn switch_bits(&mut self, block: &crate::mir::BasicBlock, discr: &Operand) -> Option<u128> {
        if let Some(bits) = self.const_bits(discr) {
            return Some(bits);
        }
        let (OperandKind::Copy(place) | OperandKind::Move(place)) = &discr.kind else {
            return None;
        };
        if !place.projection.as_slice().is_empty() {
            return None;
        }
        let assigned = block.statements.iter().rev().find_map(|it| match &it.kind {
            StatementKind::Assign(to, rvalue) => Some((to, rvalue)),
            _ => None,
        });
        let Some((to, Rvalue::Use(Operand { kind: OperandKind::Constant { konst, ty }, .. }))) =
            assigned
        else {
            return None;
        };
        if to != place || !matches!(ty.as_ref().kind(), TyKind::Bool) {
            return None;
        }
        let ConstKind::Unevaluated(unevaluated) = konst.as_ref().kind() else { return None };
        let allocation = self.eval_const(unevaluated).ok()?;
        Some(*allocation.memory.first()? as u128)
    }

    fn place(&mut self, place: &StoredPlace) -> Result<(Value, PlaceTy<'db>)> {
        let place = self.place_ref(place)?;
        Ok((place.addr, place.ty))
    }

    /// The type of the register a local of this type fits, when it is one a place of has
    /// no part of.
    fn register_ty(&self, ty: Ty<'db>) -> Option<Type> {
        let ty = self.normalize(ty);
        let plain = match ty.kind() {
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::RawPtr(..)
            | TyKind::Ref(..)
            | TyKind::FnPtr(..) => true,
            _ => false,
        };
        if !plain {
            return None;
        }
        match self.layout(ty).ok()?.backend_repr {
            BackendRepr::Scalar(scalar) => Some(self.scalar(scalar).ok()?.ty),
            _ => None,
        }
    }

    /// The slot of a local. One kept in a register gets its slot the first time something
    /// needs it in memory, and most never do.
    fn local_slot(&mut self, local: LocalId) -> LocalSlot {
        if let Some(&slot) = self.locals.get(local) {
            return slot;
        }
        let ty = self.body.expect("MIR body").locals[local].ty.as_ref();
        let layout = self.layout(ty).expect("the layout of a local kept in a register");
        let slot = LocalSlot { addr: self.stack_slot(&layout), borrowed: false };
        self.locals.insert(local, slot);
        slot
    }

    /// Where a local is, with what its register holds written there first.
    fn local_addr(&mut self, local: LocalId) -> Value {
        let addr = self.local_slot(local).addr;
        let addr = self.slot_addr(addr);
        if let Some(register) = self.registers.get(&local) {
            let value = self.builder.use_var(register.var);
            self.builder.ins().store(MEM, value, addr, 0);
        }
        addr
    }

    /// The register an operand is the whole of.
    fn register_of(&self, operand: &Operand) -> Option<Register> {
        let (OperandKind::Copy(place) | OperandKind::Move(place)) = &operand.kind else {
            return None;
        };
        self.whole_register(place)
    }

    fn whole_register(&self, place: &StoredPlace) -> Option<Register> {
        if self.registers.is_empty() || !place.projection.as_slice().is_empty() {
            return None;
        }
        self.registers.get(&place.local).copied()
    }

    fn place_ref(&mut self, place: &StoredPlace) -> Result<PlaceRef<'db>> {
        let borrowed = self.locals.get(place.local).is_some_and(|it| it.borrowed);
        // A pointer in a register is where it points with no load.
        let mut pointer = match place.projection.as_slice().first() {
            Some(ProjectionElem::Deref) => self.registers.get(&place.local).copied(),
            _ => None,
        };
        let mut addr = match pointer {
            Some(register) => self.builder.use_var(register.var),
            None => self.local_addr(place.local),
        };
        let mut meta = None;
        if borrowed {
            meta = Some(self.builder.ins().load(ptr(), MEM, addr, meta_at()));
            addr = self.builder.ins().load(ptr(), MEM, addr, 0);
        }
        let mut ty = PlaceTy::from_ty(self.body.expect("MIR body").locals[place.local].ty.as_ref());
        ty.ty = self.normalize(ty.ty);
        for proj in place.projection.as_slice() {
            let prev = ty;
            ty = ty.projection_ty(&self.infcx, proj, self.env.param_env);
            // The pointee or field can be `Self::Buffer`, which has to be an array to index.
            ty.ty = self.normalize(ty.ty);
            match proj {
                ProjectionElem::Deref => match self.layout(prev.ty)?.backend_repr {
                    BackendRepr::Scalar(_) if pointer.take().is_some() => (),
                    BackendRepr::Scalar(_) => {
                        addr = self.builder.ins().load(ptr(), MEM, addr, 0);
                        meta = None;
                    }
                    BackendRepr::ScalarPair { .. } => {
                        meta = Some(self.builder.ins().load(ptr(), MEM, addr, meta_at()));
                        addr = self.builder.ins().load(ptr(), MEM, addr, 0);
                    }
                    _ => unsupported!("deref of {:?}", prev.ty),
                },
                ProjectionElem::Field(field) => {
                    let layout = self.layout(prev.ty)?;
                    let offset = match &layout.variants {
                        // A variant without values has no layout of its own, and code
                        // reading its fields cannot be reached.
                        Variants::Single { .. } | Variants::Empty
                            if field.0 as usize >= layout.fields.count() =>
                        {
                            Size::ZERO
                        }
                        Variants::Single { .. } | Variants::Empty => {
                            layout.fields.offset(field.0 as usize)
                        }
                        // What an `async` block captured: its variants are its own affair.
                        Variants::Multiple { .. }
                            if matches!(prev.ty.kind(), TyKind::Coroutine(..)) =>
                        {
                            layout.fields.offset(field.0 as usize)
                        }
                        Variants::Multiple { variants, .. } => {
                            let Some(VariantId::EnumVariantId(variant)) = prev.variant_id else {
                                unsupported!("field of enum without downcast");
                            };
                            variants[RustcEnumVariantIdx(variant.index(self.cg.db))].field_offsets
                                [RustcFieldIdx::new(field.0 as usize)]
                        }
                    };
                    addr = self.unsized_field_addr(addr, meta, prev.ty, offset, ty.ty)?;
                }
                ProjectionElem::Downcast(_) => (),
                ProjectionElem::Index(local) => {
                    let index = match self.registers.get(local) {
                        Some(register) => self.builder.use_var(register.var),
                        None => {
                            let index = self.slot_addr(self.locals[*local].addr);
                            self.builder.ins().load(ptr(), MEM, index, 0)
                        }
                    };
                    addr = self.element(addr, prev.ty, index)?;
                    meta = None;
                }
                ProjectionElem::ConstantIndex { offset, from_end } => {
                    let index = if *from_end {
                        let len = self.len(prev.ty, meta)?;
                        self.builder.ins().iadd_imm(len, -(*offset as i64))
                    } else {
                        self.builder.ins().iconst(ptr(), *offset as i64)
                    };
                    addr = self.element(addr, prev.ty, index)?;
                    meta = None;
                }
                ProjectionElem::Subslice { from, to } => {
                    let size = self.element_size(prev.ty)?;
                    // FIXME: rustc checks `from + to <= len` when matching the pattern.
                    addr = self.builder.ins().iadd_imm(addr, (*from * size) as i64);
                    meta = match meta {
                        Some(len) => {
                            Some(self.builder.ins().iadd_imm(len, -((*from + *to) as i64)))
                        }
                        None => None,
                    };
                }
            }
        }
        Ok(PlaceRef { addr, meta, ty })
    }

    fn element_size(&self, container: Ty<'db>) -> Result<u64> {
        match container.kind() {
            TyKind::Array(elem, _) | TyKind::Slice(elem) => Ok(self.layout(elem)?.size.bytes()),
            _ => unsupported!("indexing into {container:?}"),
        }
    }

    /// Number of elements of an array, slice or `str` place.
    fn len(&mut self, container: Ty<'db>, meta: Option<Value>) -> Result<Value> {
        match (container.kind(), meta) {
            (TyKind::Array(_, len), _) => {
                let Some(len) = try_const_usize(self.cg.db, len) else {
                    unsupported!("array with unevaluated length");
                };
                Ok(self.builder.ins().iconst(ptr(), len as i64))
            }
            (TyKind::Slice(_) | TyKind::Str, Some(len)) => Ok(len),
            _ => unsupported!("length of {container:?}"),
        }
    }

    /// Address of `container[index]`: the `Assert` before what reads it has checked the index.
    fn element(&mut self, addr: Value, container: Ty<'db>, index: Value) -> Result<Value> {
        let size = self.element_size(container)?;
        let offset = self.builder.ins().imul_imm(index, size as i64);
        Ok(self.builder.ins().iadd(addr, offset))
    }

    /// Branches to `core::panicking::panic_bounds_check(index, len, location)` when `cond` is set.
    fn bounds_panic_if(&mut self, cond: Value, index: Value, len: Value) -> Result<()> {
        let panic_block = self.builder.create_block();
        let ok_block = self.builder.create_block();
        self.builder.set_cold_block(panic_block);
        self.builder.ins().brif(cond, panic_block, &[], ok_block, &[]);

        self.builder.switch_to_block(panic_block);
        let location = self.caller_location()?;
        let mut sig = self.cg.make_signature();
        sig.params.extend([AbiParam::new(ptr()); 3]);
        let callee = self.cg.import_path("core::panicking::panic_bounds_check", &sig);
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        self.invoke(callee, &[index, len, location]);
        self.builder.ins().trap(TrapCode::user(1).expect("non-zero trap code"));

        self.builder.switch_to_block(ok_block);
        Ok(())
    }

    fn static_ty(&self, id: StaticId) -> Ty<'db> {
        let db = self.cg.db;
        let ty = InferenceResult::of(db, DefWithBodyId::from(id))
            .expr_ty(Body::of(db, id.into()).root_expr());
        Ty::new_ref(self.interner, Region::new_erased(self.interner), ty, Mutability::Not)
    }

    fn operand_ty(&mut self, operand: &Operand) -> Result<Ty<'db>> {
        Ok(match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place)
                if place.projection.as_slice().is_empty() =>
            {
                let ty = self.body.expect("MIR body").locals[place.local].ty.as_ref();
                self.normalize(ty)
            }
            OperandKind::Copy(place) | OperandKind::Move(place) => self.place(place)?.1.ty,
            OperandKind::Constant { ty, .. } => ty.as_ref(),
            OperandKind::Allocation { allocation } => allocation.as_ref().ty,
            OperandKind::Static(id) => self.static_ty(*id),
        })
    }

    /// Address of memory holding the operand's value. Constants live in read-only data, so the
    /// memory must not be written through.
    fn operand(&mut self, operand: &Operand) -> Result<(Value, Ty<'db>)> {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => {
                let (addr, ty) = self.place(place)?;
                if let Some(given) = &mut self.given_away {
                    given.read.push(addr);
                }
                if let OperandKind::Move(_) = operand.kind
                    && place.projection.as_slice().is_empty()
                    && self.glue_of(ty.ty) != DropGlue::None
                    && let Some(given) = &mut self.given_away
                    && given.result != place.local
                {
                    given.whole.insert(addr);
                }
                Ok((addr, ty.ty))
            }
            OperandKind::Constant { konst, ty } => {
                let ty = ty.as_ref();
                if let Some(MirSpan::ExprId(expr)) = operand.span
                    && let Some(addr) = self.builtin_location(expr, ty)?
                {
                    return Ok((addr, ty));
                }
                let blob = match konst.as_ref().kind() {
                    ConstKind::Value(value) => {
                        let is_int = matches!(
                            ty.kind(),
                            TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Uint(_)
                        );
                        if is_int {
                            let layout = self.layout(ty)?;
                            let scalar = self.scalar_of(ty)?;
                            let bits = value.value.inner().to_leaf().to_bits(layout.size);
                            let addr = self.stack_slot(&layout);
                            let value = self.iconst(scalar.ty, bits);
                            self.builder.ins().store(MEM, value, addr, 0);
                            return Ok((addr, ty));
                        }
                        self.valtree_blob(ty, value.value)?
                    }
                    ConstKind::Unevaluated(unevaluated) => {
                        let allocation = self.eval_const(unevaluated)?;
                        self.allocation_blob(allocation, false)?
                    }
                    kind => unsupported!("constant {kind:?}"),
                };
                Ok((self.blob_addr(blob)?, ty))
            }
            OperandKind::Allocation { allocation } => {
                let allocation = allocation.as_ref();
                let ty = allocation.ty;
                if let Some(MirSpan::ExprId(expr)) = operand.span
                    && let Some(addr) = self.builtin_location(expr, ty)?
                {
                    return Ok((addr, ty));
                }
                let blob = self.allocation_blob(allocation, false)?;
                Ok((self.blob_addr(blob)?, ty))
            }
            OperandKind::Static(id) => {
                let data = self.cg.static_data(*id);
                let data = self.cg.declare_data_in_func(data, self.builder.func);
                let value = if is_thread_local(self.cg.db, *id) {
                    self.builder.ins().tls_value(ptr(), data)
                } else {
                    self.builder.ins().symbol_value(ptr(), data)
                };
                let ty = self.static_ty(*id);
                let layout = self.layout(ty)?;
                let addr = self.stack_slot(&layout);
                self.builder.ins().store(MEM, value, addr, 0);
                Ok((addr, ty))
            }
        }
    }

    /// The value of a static that rustc exports no symbol for, and whether it can be written to.
    fn static_value(&mut self, id: StaticId) -> Result<(ConstBlob, bool)> {
        let db = self.cg.db;
        let flags = StaticSignature::of(db, id).flags;
        if flags.contains(StaticFlags::EXTERN) {
            unsupported!("extern static without a rustc symbol");
        }
        let allocation = db
            .const_eval_static(id)
            .map_err(|e| CodegenError::Frontend(format!("const eval: {e:?}")))?;
        // Nothing can be written to a static of no size.
        let writable = !allocation.memory.is_empty()
            && (flags.contains(StaticFlags::MUTABLE)
                || is_thread_local(db, id)
                || !self.is_freeze(allocation.ty));
        Ok((self.allocation_blob(allocation, true)?, writable))
    }

    /// Whether `ty` is known to have no `UnsafeCell` outside of indirections.
    fn is_freeze(&self, ty: Ty<'db>) -> bool {
        crate::drop::is_freeze(self.cg.db, self.interner, ty)
    }

    /// The parameters of the function being built that are addresses, with what their types
    /// say of the memory there, by their place among the parameters of its IR. As rustc's
    /// `arg_attrs_for_rust_scalar`: a reference is aligned and not null, and a `&mut T` of a
    /// `T` that is `Unpin` and a `&T` of one without an `UnsafeCell` are the only way to their
    /// memory, which may be read at any time; an aggregate passed or returned in memory is at
    /// an address of its own of its size.
    fn pointees(
        &self,
        body: &MirBody<'db>,
        ret: PassMode,
        modes: &[PassMode],
    ) -> Result<Vec<(u32, fragment::Pointee)>> {
        use fragment::Pointee;
        let mut found = Vec::new();
        // The body of a coroutine is resumed with a null context too, which it tells apart.
        if body.coroutine.is_some() {
            return Ok(found);
        }
        let mut at = 0u32;
        let in_memory = |layout: &Layout| Pointee {
            unique: false,
            frozen: false,
            size: layout.size.bytes(),
            align: layout.align.abi.bytes(),
        };
        if let PassMode::Indirect = ret {
            let ty = body.locals[returned_local(body)].ty.as_ref();
            found.push((at, in_memory(&*self.layout(ty)?)));
            at += 1;
        }
        for (&local, &mode) in body.param_locals.iter().zip(modes) {
            let ty = body.locals[local].ty.as_ref();
            let pointee = match mode {
                PassMode::Indirect => Some(in_memory(&*self.layout(ty)?)),
                PassMode::Direct(_) | PassMode::Pair(..) => self.reference_pointee(ty)?,
                _ => None,
            };
            if let Some(pointee) = pointee {
                found.push((at, pointee));
            }
            let mut params = Vec::new();
            self.push_abi_params(mode, &mut params);
            at += params.len() as u32;
        }
        Ok(found)
    }

    /// What a reference of type `ty` says of its memory, if `ty` is one that is the only way
    /// to its memory. Only a reference to plain data is looked at: whether a type with a
    /// coroutine, a closure or a trait object in it is `Unpin` takes more than its fields to
    /// know, and a wrong yes is a miscompile.
    fn reference_pointee(&self, ty: Ty<'db>) -> Result<Option<fragment::Pointee>> {
        let TyKind::Ref(_, pointee, mutability) = ty.kind() else { return Ok(None) };
        let element = match pointee.kind() {
            TyKind::Str => None,
            TyKind::Slice(element) => Some(element),
            _ => Some(pointee),
        };
        if !element.is_none_or(|it| self.is_plain_data(it, 0)) {
            return Ok(None);
        }
        let unique = match mutability {
            Mutability::Not => self.is_freeze(pointee),
            Mutability::Mut => self.interner.lang_items().Unpin.is_some_and(|unpin| {
                crate::traits::implements_trait_unique(pointee, self.cg.db, self.env, unpin)
            }),
        };
        if !unique {
            return Ok(None);
        }
        let (size, align) = match pointee.kind() {
            TyKind::Str => (0, 1),
            TyKind::Slice(element) => (0, self.layout(element)?.align.abi.bytes()),
            _ => {
                let layout = self.layout(pointee)?;
                (layout.size.bytes(), layout.align.abi.bytes())
            }
        };
        Ok(Some(fragment::Pointee { unique, frozen: mutability == Mutability::Not, size, align }))
    }

    /// Whether `ty` is made of scalars, pointers and references only, through arrays, tuples
    /// and the fields of structs and enums, `depth` deep in a type that is.
    fn is_plain_data(&self, ty: Ty<'db>, depth: u32) -> bool {
        if depth > 8 {
            return false;
        }
        match self.normalize(ty).kind() {
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Never
            | TyKind::RawPtr(..)
            | TyKind::Ref(..)
            | TyKind::FnPtr(..)
            | TyKind::FnDef(..) => true,
            TyKind::Array(element, _) | TyKind::Pat(element, _) => {
                self.is_plain_data(element, depth + 1)
            }
            TyKind::Tuple(fields) => fields.iter().all(|it| self.is_plain_data(it, depth + 1)),
            TyKind::Adt(def, args) => {
                let variants: Vec<hir_def::VariantId> = match def.def_id() {
                    AdtId::StructId(id) => vec![id.into()],
                    AdtId::EnumId(id) => id
                        .enum_variants(self.cg.db)
                        .variants
                        .iter()
                        .map(|(_, it)| it.0.into())
                        .collect(),
                    AdtId::UnionId(_) => return false,
                };
                variants.into_iter().all(|variant| {
                    self.cg.db.field_types(variant).iter().all(|(_, field)| {
                        let ty = field.ty().instantiate(self.interner, args).skip_norm_wip();
                        self.is_plain_data(ty, depth + 1)
                    })
                })
            }
            _ => false,
        }
    }

    fn eval_const(&mut self, unevaluated: UnevaluatedConst<'db>) -> Result<Allocation<'db>> {
        let db = self.cg.db;
        let UnevaluatedConst { def, args } = unevaluated;
        let frontend = |e| CodegenError::Frontend(format!("const eval: {e:?}"));
        match def.0 {
            GeneralConstId::ConstId(id) => {
                let (id, args) = lookup_impl_const(&self.infcx, self.env.param_env, id, args);
                db.const_eval(id, args, Some(self.env)).map_err(frontend)
            }
            GeneralConstId::StaticId(id) => db.const_eval_static(id).map_err(frontend),
            GeneralConstId::AnonConstId(id) => {
                db.anon_const_eval(id, args, Some(self.env)).map_err(frontend)
            }
        }
    }

    fn blob_addr(&mut self, blob: ConstBlob) -> Result<Value> {
        let data = self.cg.define_const(blob)?;
        let data = self.cg.declare_data_in_func(data, self.builder.func);
        Ok(self.builder.ins().symbol_value(ptr(), data))
    }

    fn valtree_blob(&mut self, ty: Ty<'db>, valtree: ValTree<'db>) -> Result<ConstBlob> {
        let layout = self.layout(ty)?;
        let align = layout.align.abi.bytes();
        let pointee = match ty.kind() {
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::RawPtr(..) => {
                let bits = valtree.inner().to_leaf().to_bits(layout.size);
                let bytes = bits.to_le_bytes()[..layout.size.bytes_usize()].to_vec();
                return Ok(ConstBlob { bytes, relocs: Vec::new(), fn_relocs: Vec::new(), align });
            }
            TyKind::Ref(_, pointee, _) => pointee,
            _ => unsupported!("constant of type {ty:?}"),
        };
        let mut bytes = vec![0; layout.size.bytes_usize()];
        let target = match pointee.kind() {
            TyKind::Str | TyKind::Slice(_) => {
                let mut target = ConstBlob {
                    bytes: Vec::new(),
                    relocs: Vec::new(),
                    fn_relocs: Vec::new(),
                    align: 1,
                };
                let items = valtree.inner().to_branch();
                for item in items.iter() {
                    let ConstKind::Value(item) = item.kind() else {
                        unsupported!("unevaluated const in a slice constant");
                    };
                    let item = self.valtree_blob(item.ty, item.value)?;
                    if !item.relocs.is_empty() {
                        unsupported!("slice constant of pointers");
                    }
                    target.align = target.align.max(item.align);
                    target.bytes.extend(item.bytes);
                }
                bytes[word()..2 * word()]
                    .copy_from_slice(&(items.len() as u64).to_le_bytes()[..word()]);
                target
            }
            TyKind::Dynamic(..) => unsupported!("`dyn` constant"),
            _ => self.valtree_blob(pointee, valtree)?,
        };
        let target = self.cg.define_const(target)?;
        Ok(ConstBlob { bytes, relocs: vec![(0, target, 0)], fn_relocs: Vec::new(), align })
    }

    /// `in_static` says the value is a static's, where memory behind a mutable pointer can be
    /// written to.
    fn allocation_blob(
        &mut self,
        allocation: Allocation<'db>,
        in_static: bool,
    ) -> Result<ConstBlob> {
        let AllocationData { ty, memory, memory_map } = &*allocation;
        let layout = self.layout(*ty)?;
        let size = layout.size.bytes_usize();
        let mut bytes = memory.to_vec();
        match (bytes.len(), size) {
            (len, size) if len == size => (),
            // Const eval stores some scalars as 16 bytes.
            (16, size) if size < 16 => bytes.truncate(size),
            (len, 16) if len < 16 => bytes.resize(16, 0),
            _ => unsupported!("constant whose size differs from its layout"),
        }
        let mut blob = ConstBlob {
            bytes,
            relocs: Vec::new(),
            fn_relocs: Vec::new(),
            align: layout.align.abi.bytes(),
        };
        let mut map = Relocations { map: memory_map, in_static, defined: FxHashMap::default() };
        self.relocate(&mut blob, 0, *ty, &mut map)?;
        Ok(blob)
    }

    /// Const eval gives pointers as keys into `map`. Emits each target as its own data object
    /// and records a relocation for the pointer, walking `ty` to find them.
    fn relocate(
        &mut self,
        blob: &mut ConstBlob,
        offset: usize,
        ty: Ty<'db>,
        map: &mut Relocations<'_, 'db>,
    ) -> Result<()> {
        if let MemoryMap::Empty = map.map {
            return Ok(());
        }
        let db = self.cg.db;
        match ty.kind() {
            TyKind::Pat(base, _) => self.relocate(blob, offset, base, map)?,
            TyKind::Ref(_, pointee, mutability) => {
                let Some(addr) = blob.usize_at(offset) else {
                    unsupported!("constant pointer out of bounds");
                };
                // Const eval gives a vtable as an id into its table of types.
                let pointee = match pointee.kind() {
                    TyKind::Dynamic(..) => {
                        let erased = blob
                            .usize_at(offset + word())
                            .and_then(|id| map.map.vtable_ty(id).ok());
                        let Some(erased) = erased else {
                            unsupported!("`dyn` constant without a known type");
                        };
                        let trait_ref = self.principal(pointee, erased);
                        let vtable = self.vtable(erased, trait_ref)?;
                        blob.point_to(offset + word(), vtable, 0);
                        erased
                    }
                    _ => pointee,
                };
                if let Some((id, at)) = map.map.static_at(addr) {
                    let target = self.cg.static_data(id);
                    blob.point_to(offset, target, at);
                    return Ok(());
                }
                let Some((start, memory)) = map.map.memory_at(addr) else { return Ok(()) };
                let elem = match pointee.kind() {
                    TyKind::Str => None,
                    TyKind::Slice(elem) => Some(elem),
                    _ => Some(pointee),
                };
                let empty = match pointee.kind() {
                    TyKind::Slice(_) => blob.usize_at(offset + word()) == Some(0),
                    _ => false,
                };
                // An empty slice can be at the address of an allocation that is not its own.
                if memory.is_empty() || empty {
                    // Nothing is behind the pointer, and rustc makes it the alignment.
                    let align = match elem {
                        Some(elem) => self.layout(elem)?.align.abi.bytes(),
                        None => 1,
                    };
                    blob.bytes[offset..offset + word()]
                        .copy_from_slice(&align.to_le_bytes()[..word()]);
                    return Ok(());
                }
                if let Some(&target) = map.defined.get(&start) {
                    blob.point_to(offset, target, addr - start);
                    return Ok(());
                }
                let mut target = ConstBlob {
                    bytes: memory.to_vec(),
                    relocs: Vec::new(),
                    fn_relocs: Vec::new(),
                    align: map.align_of(start),
                };
                let at = addr - start;
                match pointee.kind() {
                    TyKind::Str => (),
                    TyKind::Slice(elem) => {
                        let Some(len) = blob.usize_at(offset + word()) else {
                            unsupported!("constant slice length out of bounds");
                        };
                        let size = self.layout(elem)?.size.bytes_usize();
                        for i in 0..len {
                            self.relocate(&mut target, at + i * size, elem, map)?;
                        }
                    }
                    _ => self.relocate(&mut target, at, pointee, map)?,
                }
                let writable = map.in_static && mutability == Mutability::Mut;
                let target = self.cg.define_data(target, writable)?;
                map.defined.insert(start, target);
                blob.point_to(offset, target, at);
            }
            // Const eval gives function pointers as ids into its table of fn item types. A
            // function cast to a raw pointer keeps that id, so a raw pointer holding one is
            // taken to be a function. FIXME: const eval should track provenance instead.
            TyKind::FnPtr(..) | TyKind::RawPtr(..) => {
                let Some(id) = blob.usize_at(offset) else {
                    unsupported!("constant pointer out of bounds");
                };
                let fn_ty = match map.map.vtable_ty(id) {
                    Ok(fn_ty)
                        if matches!(fn_ty.kind(), TyKind::FnDef(..) | TyKind::Closure(..)) =>
                    {
                        fn_ty
                    }
                    _ if let TyKind::RawPtr(_, mutability) = ty.kind() => {
                        self.relocate_data_pointer(blob, offset, id, mutability, map)?;
                        return Ok(());
                    }
                    _ if id == 0 => return Ok(()),
                    _ => unsupported!("fn pointer in a constant"),
                };
                let target = self.fn_item(fn_ty)?;
                blob.bytes[offset..offset + word()].fill(0);
                blob.fn_relocs.push((offset as u32, target));
            }
            TyKind::Adt(def, args) => {
                let layout = self.layout(ty)?;
                let (variant, offsets): (VariantId, Vec<usize>) = match def.def_id() {
                    AdtId::StructId(id) => (
                        id.into(),
                        (0..layout.fields.count())
                            .map(|i| layout.fields.offset(i).bytes_usize())
                            .collect(),
                    ),
                    // Const eval does not say which bytes of a union are a pointer, so every
                    // word that is the address of something it kept is taken to be one.
                    AdtId::UnionId(_) => {
                        let align = layout.align.abi.bytes_usize();
                        let mut may_hold_fn = None;
                        if align.is_multiple_of(word()) {
                            for index in 0..layout.size.bytes_usize() / word() {
                                let at = offset + index * word();
                                let Some(id) = blob.usize_at(at) else { continue };
                                // And every word that is the id of a function is taken to
                                // be that function, in a union that may hold one, as the
                                // one of a `LazyLock` does.
                                if may_hold_fn.get_or_insert_with(|| self.holds_fn_pointer(ty, 8))
                                    == &true
                                    && let Ok(fn_ty) = map.map.vtable_ty(id)
                                    && matches!(
                                        fn_ty.kind(),
                                        TyKind::FnDef(..) | TyKind::Closure(..)
                                    )
                                {
                                    let target = self.fn_item(fn_ty)?;
                                    blob.bytes[at..at + word()].fill(0);
                                    blob.fn_relocs.push((at as u32, target));
                                    continue;
                                }
                                self.relocate_data_pointer(blob, at, id, Mutability::Not, map)?;
                            }
                        }
                        return Ok(());
                    }
                    AdtId::EnumId(id) => {
                        let Some(bytes) =
                            blob.bytes.get(offset..offset + layout.size.bytes_usize())
                        else {
                            unsupported!("constant enum out of bounds");
                        };
                        let Some((variant, layout)) =
                            detect_variant_from_bytes(&layout, db, self.dl(), bytes, id)
                        else {
                            return Ok(());
                        };
                        (
                            variant.into(),
                            (0..layout.fields.count())
                                .map(|i| layout.fields.offset(i).bytes_usize())
                                .collect(),
                        )
                    }
                };
                for ((_, field), field_offset) in db.field_types(variant).iter().zip(offsets) {
                    let field = field.ty().instantiate(self.interner, args).skip_norm_wip();
                    self.relocate(blob, offset + field_offset, field, map)?;
                }
            }
            TyKind::Tuple(tys) => {
                let layout = self.layout(ty)?;
                for (i, field) in tys.iter().enumerate() {
                    let field_offset = layout.fields.offset(i).bytes_usize();
                    self.relocate(blob, offset + field_offset, field, map)?;
                }
            }
            TyKind::Array(elem, len) => {
                let Some(len) = try_const_usize(db, len) else {
                    unsupported!("array with unevaluated length");
                };
                let size = self.layout(elem)?.size.bytes_usize();
                for i in 0..len as usize {
                    self.relocate(blob, offset + i * size, elem, map)?;
                }
            }
            TyKind::Alias(..) => {
                let normalized =
                    crate::next_solver::obligation_ctxt::ObligationCtxt::new(&self.infcx)
                        .structurally_normalize_ty(
                            &crate::next_solver::infer::traits::ObligationCause::dummy(),
                            self.env.param_env,
                            ty,
                        )
                        .unwrap_or(ty);
                if !matches!(normalized.kind(), TyKind::Alias(..)) {
                    self.relocate(blob, offset, normalized, map)?;
                }
            }
            _ => (),
        }
        Ok(())
    }

    /// Whether a value of the type has a function pointer in it, not behind a pointer, looking
    /// `depth` types deep.
    fn holds_fn_pointer(&self, ty: Ty<'db>, depth: u32) -> bool {
        if depth == 0 {
            return false;
        }
        let db = self.cg.db;
        match ty.kind() {
            TyKind::FnPtr(..) => true,
            TyKind::Tuple(tys) => tys.iter().any(|it| self.holds_fn_pointer(it, depth - 1)),
            TyKind::Array(elem, _) => self.holds_fn_pointer(elem, depth - 1),
            TyKind::Adt(def, args) => {
                let variants: Vec<VariantId> = match def.def_id() {
                    AdtId::StructId(id) => vec![id.into()],
                    AdtId::UnionId(id) => vec![id.into()],
                    AdtId::EnumId(id) => {
                        id.enum_variants(db).variants.iter().map(|it| it.1.0.into()).collect()
                    }
                };
                variants.into_iter().any(|variant| {
                    db.field_types(variant).iter().any(|(_, field)| {
                        let field = field.ty().instantiate(self.interner, args).skip_norm_wip();
                        self.holds_fn_pointer(field, depth - 1)
                    })
                })
            }
            _ => false,
        }
    }

    /// Points the word at `offset`, which const eval left holding address `addr`, at the static
    /// or the allocation that address is in.
    fn relocate_data_pointer(
        &mut self,
        blob: &mut ConstBlob,
        offset: usize,
        addr: usize,
        mutability: Mutability,
        map: &mut Relocations<'_, 'db>,
    ) -> Result<()> {
        if let Some((id, at)) = map.map.static_at(addr) {
            let target = self.cg.static_data(id);
            blob.point_to(offset, target, at);
            return Ok(());
        }
        // Const eval keeps the whole allocation a raw pointer points into.
        let Some((start, memory)) = map.map.memory_at(addr) else { return Ok(()) };
        let target = match map.defined.get(&start) {
            Some(&target) => target,
            None => {
                let mut target = ConstBlob {
                    bytes: memory.to_vec(),
                    relocs: Vec::new(),
                    fn_relocs: Vec::new(),
                    align: map.align_of(start),
                };
                // What the allocation is of is not known here, so as in a union, every word
                // of it that is the address of something const eval kept is taken to be one.
                for index in 0..target.bytes.len() / word() {
                    let at = index * word();
                    if let Some(inner) = target.usize_at(at)
                        && inner != 0
                        && map.map.memory_at(inner).is_some_and(|it| it.0 != start)
                    {
                        self.relocate_data_pointer(&mut target, at, inner, mutability, map)?;
                    }
                }
                let writable = map.in_static && mutability == Mutability::Mut;
                let target = self.cg.define_data(target, writable)?;
                map.defined.insert(start, target);
                target
            }
        };
        blob.point_to(offset, target, addr - start);
        Ok(())
    }

    fn iconst(&mut self, ty: Type, bits: u128) -> Value {
        if ty.bits() < 64 {
            // Cranelift wants narrow immediates without stray high bits.
            let bits = bits as u64 & ((1 << ty.bits()) - 1);
            return self.builder.ins().iconst(ty, bits as i64);
        }
        if ty == types::I128 {
            let lo = self.builder.ins().iconst(types::I64, bits as u64 as i64);
            let hi = self.builder.ins().iconst(types::I64, (bits >> 64) as u64 as i64);
            self.builder.ins().iconcat(lo, hi)
        } else {
            self.builder.ins().iconst(ty, bits as u64 as i64)
        }
    }

    /// Calls the function that an `asm!` block is assembled as: see [`asm`].
    fn inline_asm(&mut self, block: &crate::mir::InlineAsm) -> Result<()> {
        use crate::mir::AsmOperand;
        let mut operands = Vec::with_capacity(block.operands.len());
        let mut syms = Vec::with_capacity(block.operands.len());
        for operand in &block.operands {
            syms.push(match operand {
                AsmOperand::Sym(_) if !block.naked => unsupported!("`asm!` with a `sym`"),
                AsmOperand::Sym(Operand { kind: OperandKind::Static(id), .. }) => {
                    if is_thread_local(self.cg.db, *id) {
                        unsupported!("inline assembly with a `sym` that is thread local");
                    }
                    Some(ModuleRelocTarget::user(1, self.cg.static_data(*id).as_u32()))
                }
                AsmOperand::Sym(function) => {
                    let ty = self.operand_ty(function)?;
                    Some(ModuleRelocTarget::user(0, self.fn_item(ty)?.as_u32()))
                }
                AsmOperand::Reg { .. } | AsmOperand::Const(_) | AsmOperand::Label => None,
            });
            operands.push(match operand {
                AsmOperand::Sym(_) => asm::Operand::Sym,
                AsmOperand::Reg { reg, input, output } => {
                    let mut size = None;
                    let tys = [
                        input.as_ref().map(|it| self.operand_ty(it)).transpose()?,
                        output.as_ref().map(|it| self.place(it).map(|it| it.1.ty)).transpose()?,
                    ];
                    for ty in tys.into_iter().flatten() {
                        let bytes = self.layout(ty)?.size.bytes() as u32;
                        if size.is_some_and(|it| it != bytes) {
                            unsupported!(
                                "an `asm!` register that goes in and comes out in two sizes"
                            );
                        }
                        size = Some(bytes);
                    }
                    asm::Operand::Reg {
                        reg,
                        size,
                        input: input.is_some(),
                        output: output.is_some(),
                    }
                }
                AsmOperand::Const(value) => {
                    let value = self.assigned_constant(value).unwrap_or(value);
                    let ty = self.operand_ty(value)?;
                    let bits = match &value.kind {
                        OperandKind::Allocation { allocation }
                            if matches!(ty.kind(), TyKind::Int(_) | TyKind::Uint(_)) =>
                        {
                            let mut bytes = [0; 16];
                            let memory = &allocation.as_ref().memory;
                            bytes[..memory.len()].copy_from_slice(memory);
                            Some(u128::from_le_bytes(bytes))
                        }
                        OperandKind::Constant { konst, .. }
                            if matches!(ty.kind(), TyKind::Int(_) | TyKind::Uint(_))
                                && let ConstKind::Unevaluated(named) = konst.as_ref().kind() =>
                        {
                            let mut bytes = [0; 16];
                            let memory = &self.eval_const(named)?.memory;
                            bytes[..memory.len()].copy_from_slice(memory);
                            Some(u128::from_le_bytes(bytes))
                        }
                        _ => self.const_bits(value),
                    };
                    let Some(bits) = bits else {
                        unsupported!("an `asm!` constant that is not a literal");
                    };
                    let unused = 128 - self.layout(ty)?.size.bits() as u32;
                    asm::Operand::Const(match ty.kind() {
                        TyKind::Int(_) => (bits << unused) as i128 >> unused,
                        _ => bits as i128,
                    })
                }
                AsmOperand::Label => asm::Operand::Label,
            });
        }
        let arch = self.cg.target.arch;
        if block.naked {
            let text = asm::naked(arch, &block.template, block.options, &operands)
                .map_err(CodegenError::Unsupported)?;
            self.cg.naked = Some((text, syms));
            return Ok(());
        }
        let wrapper = asm::wrapper(&self.cg.target, &block.template, block.options, &operands)
            .map_err(CodegenError::Unsupported)?;
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            wrapper.slot_size,
            4,
        ));
        let slot = self.builder.ins().stack_addr(ptr(), slot, 0);
        for (operand, offset) in block.operands.iter().zip(&wrapper.offsets) {
            if let (AsmOperand::Reg { input: Some(input), .. }, Some(offset)) = (operand, offset) {
                let (addr, ty) = self.operand(input)?;
                let size = self.layout(ty)?.size;
                let to = self.builder.ins().iadd_imm(slot, *offset as i64);
                self.copy(to, addr, size);
            }
        }
        let mut sig = self.cg.make_signature();
        sig.params.push(AbiParam::new(ptr()));
        if block.taken.is_some() {
            sig.returns.push(AbiParam::new(types::I64));
        }
        let callee = self.cg.assembly(wrapper.text, &sig);
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        let call = self.builder.ins().call(callee, &[slot]);
        if let Some(taken) = &block.taken {
            let taken = self.place(taken)?.0;
            let which = self.builder.inst_results(call)[0];
            self.builder.ins().store(MEM, which, taken, 0);
        }
        for (operand, offset) in block.operands.iter().zip(&wrapper.offsets) {
            if let (AsmOperand::Reg { output: Some(output), .. }, Some(offset)) = (operand, offset)
            {
                let (addr, ty) = self.place(output)?;
                let size = self.layout(ty.ty)?.size;
                let from = self.builder.ins().iadd_imm(slot, *offset as i64);
                self.copy(addr, from, size);
            }
        }
        Ok(())
    }

    /// The constant a temporary holds, when `operand` reads one that was given a constant
    /// and nothing else: a named constant is lowered to that.
    fn assigned_constant(&self, operand: &Operand) -> Option<&'db Operand> {
        let OperandKind::Copy(place) = &operand.kind else { return None };
        let mut assigned = self.body?.basic_blocks.iter().flat_map(|(_, block)| {
            block.statements.iter().filter_map(|it| match &it.kind {
                StatementKind::Assign(to, rvalue) if to.local == place.local => Some((to, rvalue)),
                _ => None,
            })
        });
        match (assigned.next(), assigned.next()) {
            (Some((to, Rvalue::Use(constant))), None)
                if to == place && matches!(constant.kind, OperandKind::Constant { .. }) =>
            {
                Some(constant)
            }
            _ => None,
        }
    }

    fn load_scalar(&mut self, operand: &Operand) -> Result<(Value, AbiScalar)> {
        if let Some(register) = self.register_of(operand) {
            let ty = self.operand_ty(operand)?;
            return Ok((self.builder.use_var(register.var), self.scalar_of(ty)?));
        }
        if let Some((bits, ty)) = self.literal_bits(operand) {
            let scalar = self.scalar_of(ty)?;
            return Ok((self.iconst(scalar.ty, bits), scalar));
        }
        let (addr, ty) = self.operand(operand)?;
        let scalar = self.scalar_of(ty)?;
        Ok((self.builder.ins().load(scalar.ty, MEM, addr, 0), scalar))
    }

    /// The value of an operand that is an integer, `bool` or `char` whose bytes are known
    /// here, which is read without being put in memory first. `line!()` and `column!()` are
    /// literals whose bytes are placeholders.
    fn literal_bits(&self, operand: &Operand) -> Option<(u128, Ty<'db>)> {
        let ty = match &operand.kind {
            OperandKind::Constant { ty, .. } => ty.as_ref(),
            OperandKind::Allocation { allocation } => allocation.as_ref().ty,
            _ => return None,
        };
        if !matches!(ty.kind(), TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Uint(_)) {
            return None;
        }
        if let Some(MirSpan::ExprId(expr)) = operand.span
            && self.body.is_some_and(|body| self.location_macro(body, expr).is_some())
        {
            return None;
        }
        let OperandKind::Allocation { allocation } = &operand.kind else {
            return Some((self.const_bits(operand)?, ty));
        };
        let memory = &allocation.as_ref().memory;
        let size = self.layout(ty).ok()?.size.bytes_usize();
        // Const eval stores some scalars as 16 bytes.
        if memory.len() != size && memory.len() != 16 {
            return None;
        }
        let mut bytes = [0; 16];
        bytes[..size].copy_from_slice(&memory[..size]);
        Some((u128::from_le_bytes(bytes), ty))
    }

    fn copy(&mut self, dst: Value, src: Value, size: Size) {
        // A temporary and the binding it initializes are one slot.
        if dst == src {
            return;
        }
        // A load and a store per word of a future holding the whole program's state is
        // more IR than fits in memory.
        if size.bytes() > 128 {
            let config = self.cg.target_config();
            let bytes = self.builder.ins().iconst(ptr(), size.bytes() as i64);
            self.builder.call_memcpy(config, dst, src, bytes);
            return;
        }
        let mut offset = 0i32;
        let mut left = size.bytes() as i32;
        for ty in [types::I64, types::I32, types::I16, types::I8] {
            while left >= ty.bytes() as i32 {
                let value = self.builder.ins().load(ty, MEM, src, offset);
                self.builder.ins().store(MEM, value, dst, offset);
                offset += ty.bytes() as i32;
                left -= ty.bytes() as i32;
            }
        }
    }

    fn assign(&mut self, place: &StoredPlace, rvalue: &Rvalue) -> Result<()> {
        let Some(register) = self.whole_register(place) else {
            return self.assign_to_slot(place, rvalue, None, &Cell::new(false));
        };
        if let Rvalue::Use(operand) = rvalue
            && let Ok(scalar) = self.operand_ty(operand).and_then(|ty| self.scalar_of(ty))
            && scalar.ty == register.ty
        {
            let (value, _) = self.load_scalar(operand)?;
            self.builder.def_var(register.var, value);
            return Ok(());
        }
        let registered = Cell::new(false);
        self.assign_to_slot(place, rvalue, Some(register), &registered)?;
        if !registered.get() {
            let addr = self.local_slot(place.local).addr;
            let addr = self.slot_addr(addr);
            let value = self.builder.ins().load(register.ty, MEM, addr, 0);
            self.builder.def_var(register.var, value);
        }
        Ok(())
    }

    /// Assigns through memory, or to the register when the value is one: `registered` is
    /// set then, and the slot of the local is left as it was.
    fn assign_to_slot(
        &mut self,
        place: &StoredPlace,
        rvalue: &Rvalue,
        register: Option<Register>,
        registered: &Cell<bool>,
    ) -> Result<()> {
        if self.locals.get(place.local).is_some_and(|it| it.borrowed)
            && place.projection.as_slice().is_empty()
        {
            let Rvalue::Use(operand) = rvalue else {
                unsupported!("unsized local assigned {rvalue:?}");
            };
            let (OperandKind::Copy(from) | OperandKind::Move(from)) = &operand.kind else {
                unsupported!("unsized local assigned a constant");
            };
            let PlaceRef { addr, meta: Some(meta), .. } = self.place_ref(from)? else {
                unsupported!("unsized local assigned a place without metadata");
            };
            // Marks the place as moved out of.
            self.operand(operand)?;
            let slot = self.slot_addr(self.locals[place.local].addr);
            self.builder.ins().store(MEM, addr, slot, 0);
            self.builder.ins().store(MEM, meta, slot, meta_at());
            return Ok(());
        }
        let (dst, dst_ty) = match register {
            // What the register holds is about to be replaced, and is not written back.
            Some(_) => {
                let ty = self.body.expect("MIR body").locals[place.local].ty.as_ref();
                let addr = self.local_slot(place.local).addr;
                let addr = self.slot_addr(addr);
                (addr, PlaceTy::from_ty(self.normalize(ty)))
            }
            None => self.place(place)?,
        };
        let store = |this: &mut Self, value: Value| match register {
            Some(register) if this.builder.func.dfg.value_type(value) == register.ty => {
                this.builder.def_var(register.var, value);
                registered.set(true);
            }
            _ => {
                this.builder.ins().store(MEM, value, dst, 0);
            }
        };
        match rvalue {
            Rvalue::Use(operand) => {
                let (src, ty) = self.operand(operand)?;
                let size = self.layout(ty)?.size;
                self.copy(dst, src, size);
            }
            Rvalue::CopyForDeref(place) => {
                let (src, ty) = self.place(place)?;
                let size = self.layout(ty.ty)?.size;
                self.copy(dst, src, size);
            }
            Rvalue::Ref(_, place) => {
                let place = self.place_ref(place)?;
                store(self, place.addr);
                if let BackendRepr::ScalarPair { .. } = self.layout(dst_ty.ty)?.backend_repr {
                    let Some(meta) = place.meta else {
                        unsupported!("fat reference to a place without metadata");
                    };
                    self.builder.ins().store(MEM, meta, dst, meta_at());
                }
            }
            Rvalue::Len(place) => {
                let place = self.place_ref(place)?;
                let len = self.len(place.ty.ty, place.meta)?;
                store(self, len);
            }
            Rvalue::Discriminant(place) => {
                let (addr, ty) = self.place(place)?;
                let to = self.scalar_of(dst_ty.ty)?;
                let value = self.discriminant(addr, ty.ty, to)?;
                store(self, value);
            }
            Rvalue::Repeat(operand, len) => {
                let Some(len) = try_const_usize(self.cg.db, len.as_ref()) else {
                    unsupported!("repeat with unevaluated length");
                };
                let (src, ty) = self.operand(operand)?;
                let size = self.layout(ty)?.size;
                if len == 0 || size.bytes() == 0 {
                    // No element, or elements that take no room: nothing is written. A value
                    // `[value; 0]` does not take is dropped by the MIR before.
                } else if len <= 16 {
                    for i in 0..len as u64 {
                        let element = self.builder.ins().iadd_imm(dst, (i * size.bytes()) as i64);
                        self.copy(element, src, size);
                    }
                } else {
                    let header = self.builder.create_block();
                    let body = self.builder.create_block();
                    let done = self.builder.create_block();
                    self.builder.append_block_param(header, ptr());
                    let zero = self.builder.ins().iconst(ptr(), 0);
                    self.builder.ins().jump(header, &[zero.into()]);
                    self.builder.switch_to_block(header);
                    let index = self.builder.block_params(header)[0];
                    let more =
                        self.builder.ins().icmp_imm(IntCC::UnsignedLessThan, index, len as i64);
                    self.builder.ins().brif(more, body, &[], done, &[]);
                    self.builder.switch_to_block(body);
                    let offset = self.builder.ins().imul_imm(index, size.bytes() as i64);
                    let element = self.builder.ins().iadd(dst, offset);
                    self.copy(element, src, size);
                    let next = self.builder.ins().iadd_imm(index, 1);
                    self.builder.ins().jump(header, &[next.into()]);
                    self.builder.switch_to_block(done);
                }
            }
            Rvalue::BinaryOp(
                op @ (BinOp::AddWithOverflow | BinOp::SubWithOverflow | BinOp::MulWithOverflow),
                lhs,
                rhs,
            ) => {
                let (value, overflow) = self.overflowing_op(op, lhs, rhs)?;
                let Some(overflow) = overflow else { unsupported!("{op:?} of floats") };
                let layout = self.layout(dst_ty.ty)?;
                let [value_at, flag_at] =
                    [0, 1].map(|field| layout.fields.offset(field).bytes() as i32);
                self.builder.ins().store(MEM, value, dst, value_at);
                self.builder.ins().store(MEM, overflow, dst, flag_at);
            }
            Rvalue::BinaryOp(op, lhs, rhs) => {
                let value = self.binary_op(op, lhs, rhs)?;
                store(self, value);
            }
            Rvalue::UnaryOp(op, operand) => {
                let (value, _) = self.load_scalar(operand)?;
                let is_bool = matches!(self.operand_ty(operand)?.kind(), TyKind::Bool);
                let value = match op {
                    UnOp::Not if is_bool => self.builder.ins().bxor_imm(value, 1),
                    UnOp::Not => self.builder.ins().bnot(value),
                    UnOp::Neg if self.builder.func.dfg.value_type(value).is_float() => {
                        self.builder.ins().fneg(value)
                    }
                    UnOp::Neg => {
                        let ty = self.builder.func.dfg.value_type(value);
                        let zero = self.builder.ins().iconst(types::I64, 0);
                        let zero = match ty {
                            types::I128 => self.builder.ins().uextend(ty, zero),
                            types::I64 => zero,
                            _ => self.builder.ins().ireduce(ty, zero),
                        };
                        // An `Assert` before it has ruled out `MIN`.
                        self.builder.ins().isub(zero, value)
                    }
                };
                store(self, value);
            }
            Rvalue::Cast(kind, operand, target) => {
                self.cast(kind, operand, target.as_ref(), dst)?
            }
            Rvalue::Aggregate(kind, operands) => match kind {
                AggregateKind::Tuple(ty) => {
                    let layout = self.layout(ty.as_ref())?;
                    self.write_fields(dst, &layout, operands)?;
                }
                AggregateKind::Adt(variant, args) => {
                    if let Some(layout) = self.variant_layout(dst, *variant, args.as_ref())? {
                        self.write_fields(dst, &layout, operands)?;
                    }
                }
                AggregateKind::Array(elem) => {
                    let size = self.layout(elem.as_ref())?.size;
                    for (i, operand) in operands.iter().enumerate() {
                        let (src, _) = self.operand(operand)?;
                        let element =
                            self.builder.ins().iadd_imm(dst, (i as u64 * size.bytes()) as i64);
                        self.copy(element, src, size);
                    }
                }
                AggregateKind::Union(..) => {
                    let [operand] = &**operands else {
                        unsupported!("union aggregate without exactly one field");
                    };
                    let (src, ty) = self.operand(operand)?;
                    let size = self.layout(ty)?.size;
                    self.copy(dst, src, size);
                }
                AggregateKind::Closure(ty) => {
                    let layout = self.layout(ty.as_ref())?;
                    self.write_fields(dst, &layout, operands)?;
                    if let TyKind::Coroutine(..) = ty.as_ref().kind() {
                        self.start_coroutine(dst, &layout)?;
                    }
                }
            },
            _ => unsupported!("rvalue {rvalue:?}"),
        }
        Ok(())
    }

    fn cast(
        &mut self,
        kind: &CastKind,
        operand: &Operand,
        to_ty: Ty<'db>,
        dst: Value,
    ) -> Result<()> {
        let (src, from_ty) = self.operand(operand)?;
        let from = self.layout(from_ty)?;
        let to = self.layout(to_ty)?;
        match kind {
            // RA records an unsizing coercion on both a block and its tail expression; the
            // outer one has nothing left to do.
            CastKind::PointerCoercion(PointerCast::Unsize) if from_ty == to_ty => {
                self.copy(dst, src, to.size)
            }
            CastKind::PointerCoercion(PointerCast::Unsize) => {
                self.unsize(src, from_ty, dst, to_ty)?
            }
            CastKind::PointerCoercion(
                PointerCast::ReifyFnPointer | PointerCast::ClosureFnPointer(_),
            ) => {
                let callee = self.fn_item(from_ty)?;
                let callee = self.cg.declare_func_address_in_func(callee, self.builder.func);
                let pointer = self.builder.ins().func_addr(ptr(), callee);
                self.builder.ins().store(MEM, pointer, dst, 0);
            }
            CastKind::PointerCoercion(
                PointerCast::MutToConstPointer
                | PointerCast::ArrayToPointer
                | PointerCast::UnsafeFnPointer,
            ) => self.copy(dst, src, to.size),
            CastKind::IntToInt
            | CastKind::PtrToPtr
            | CastKind::PointerExposeAddress
            | CastKind::PointerFromExposedAddress
            | CastKind::FnPtrToPtr => match (from.backend_repr, to.backend_repr) {
                (BackendRepr::Scalar(from), BackendRepr::Scalar(to)) => {
                    let (from, to) = (self.scalar(from)?, self.scalar(to)?);
                    let value = self.builder.ins().load(from.ty, MEM, src, 0);
                    let value = self.int_cast(value, from, to);
                    self.builder.ins().store(MEM, value, dst, 0);
                }
                // Fat to fat keeps the metadata, fat to thin drops it.
                (
                    BackendRepr::ScalarPair { .. },
                    BackendRepr::ScalarPair { .. } | BackendRepr::Scalar(_),
                ) => self.copy(dst, src, to.size),
                _ => unsupported!("cast from {from_ty:?} to {to_ty:?}"),
            },
            CastKind::IntToFloat | CastKind::FloatToInt | CastKind::FloatToFloat => {
                let (from, to) = (self.scalar_of(from_ty)?, self.scalar_of(to_ty)?);
                let value = self.builder.ins().load(from.ty, MEM, src, 0);
                let value = self.float_cast(value, from, to)?;
                self.builder.ins().store(MEM, value, dst, 0);
            }
            _ => unsupported!("cast {kind:?}"),
        }
        Ok(())
    }

    /// The function a fn item type names, declared so its address can be taken.
    fn fn_item(&mut self, fn_ty: Ty<'db>) -> Result<FuncId> {
        if let TyKind::Closure(..) = fn_ty.kind() {
            return self.closure_fn_pointer(fn_ty);
        }
        let TyKind::FnDef(def, generic_args) = fn_ty.kind() else {
            unsupported!("fn pointer to {fn_ty:?}");
        };
        let CallableDefId::FunctionId(func) = def.0 else {
            return self.reified(fn_ty, false);
        };
        let db = self.cg.db;
        if FunctionSignature::is_intrinsic(db, func) {
            unsupported!("fn pointer to an intrinsic");
        }
        if is_dyn_method(self.interner, self.env.param_env, func, generic_args).is_some() {
            return self.reified(fn_ty, false);
        }
        let (callee, callee_args) = db.lookup_impl_method(self.env, func, generic_args);
        let either::Either::Left(callee) = callee else {
            unsupported!("fn pointer to a builtin derive method");
        };
        // The function takes its caller's location as well, which the shim supplies.
        if track_caller(db, callee) {
            return self.reified(fn_ty, false);
        }
        let sig = self.interner.instantiate_bound_regions_with_erased(fn_ty.fn_sig(self.interner));
        let abi = self.fn_abi(sig.abi(), sig.output(), sig.inputs().iter().copied())?;
        let mut sig = self.signature(&abi.args, abi.ret);
        self.declare_variadic(callee, &mut sig)?;
        self.cg.callee(InstanceDef::Fn(callee), callee_args, self.env, &sig)
    }

    /// Makes `sig`, of the named parameters of `func`, the one `func` is defined with when it
    /// takes `...` and is compiled here: every call of it declares it the same.
    fn declare_variadic(&self, func: FunctionId, sig: &mut Signature) -> Result<()> {
        let signature = FunctionSignature::of(self.cg.db, func);
        if signature.is_varargs()
            && signature.has_body()
            && let Some(variadic) = Variadic::of(self.cg.target, sig)?
        {
            variadic.rest(sig);
        }
        Ok(())
    }

    /// The discriminant of the enum at `addr`, as a `to`.
    fn discriminant(&mut self, addr: Value, ty: Ty<'db>, to: AbiScalar) -> Result<Value> {
        let db = self.cg.db;
        let TyKind::Adt(def, _) = ty.kind() else {
            return Ok(self.iconst(to.ty, 0));
        };
        let AdtId::EnumId(id) = def.def_id() else {
            return Ok(self.iconst(to.ty, 0));
        };
        let layout = self.layout(ty)?;
        let enum_variants = id.enum_variants(db);
        let discr_of = |index: usize| {
            db.const_eval_discriminant(enum_variants.variants[index].0)
                .map_err(|e| CodegenError::Frontend(format!("discriminant: {e:?}")))
        };
        match &layout.variants {
            // There is no value to read it from, so this cannot be reached; rustc leaves the
            // result undefined.
            Variants::Empty => Ok(self.iconst(to.ty, 0)),
            Variants::Single { index } => {
                let discr = discr_of(index.0)?;
                Ok(self.iconst(to.ty, discr as u128))
            }
            Variants::Multiple { tag, tag_encoding, tag_field, variants } => {
                let tag = self.scalar(*tag)?;
                let offset = layout.fields.offset(tag_field.index()).bytes() as i32;
                let value = self.builder.ins().load(tag.ty, MEM, addr, offset);
                match tag_encoding {
                    TagEncoding::Direct => Ok(self.int_cast(value, tag, to)),
                    TagEncoding::Niche { untagged_variant, niche_variants, niche_start } => {
                        for (index, _) in variants.iter_enumerated() {
                            if discr_of(index.0)? != index.0 as i128 {
                                unsupported!("niche enum with explicit discriminants");
                            }
                        }
                        let first = niche_variants.start.0;
                        let count = niche_variants.last.0 - first;
                        let start = self.iconst(tag.ty, *niche_start);
                        let relative = self.builder.ins().isub(value, start);
                        let is_niche = self.builder.ins().icmp_imm(
                            IntCC::UnsignedLessThanOrEqual,
                            relative,
                            count as i64,
                        );
                        // The index is worked out in a register and widened last: an
                        // `i128` of it is two of every instruction.
                        let narrow = match to.ty {
                            types::I128 if tag.ty != types::I128 => {
                                AbiScalar { ty: types::I64, signed: false, ..to }
                            }
                            _ => to,
                        };
                        let relative =
                            self.int_cast(relative, AbiScalar { signed: false, ..tag }, narrow);
                        let niche = self.builder.ins().iadd_imm(relative, first as i64);
                        let untagged = self.iconst(narrow.ty, untagged_variant.0 as u128);
                        let index = self.builder.ins().select(is_niche, niche, untagged);
                        Ok(self.int_cast(index, narrow, to))
                    }
                }
            }
        }
    }

    /// Layout whose `fields` describe `variant`. Writes the tag for multi-variant enums.
    /// `None` for a variant the layout has no room for, which is one without values: code
    /// building it cannot be reached.
    fn variant_layout(
        &mut self,
        dst: Value,
        variant: VariantId,
        args: GenericArgs<'db>,
    ) -> Result<Option<Arc<Layout>>> {
        let db = self.cg.db;
        let adt: AdtId = variant.adt_id(db);
        let layout = self.layout(Ty::new_adt(self.interner, adt, args))?;
        let (tag, tag_encoding, tag_field) = match &layout.variants {
            Variants::Multiple { tag, tag_encoding, tag_field, .. } => {
                (tag, tag_encoding, tag_field)
            }
            Variants::Single { index } => {
                let inhabited = match variant {
                    VariantId::EnumVariantId(variant) => variant.index(db) == index.0,
                    _ => true,
                };
                return Ok(inhabited.then_some(layout));
            }
            Variants::Empty => return Ok(None),
        };
        let VariantId::EnumVariantId(variant) = variant else {
            unsupported!("multi-variant non-enum");
        };
        let index = variant.index(db);
        let tag_ty = self.scalar(*tag)?.ty;
        let offset = layout.fields.offset(tag_field.index()).bytes() as i32;
        let tag_value = match tag_encoding {
            TagEncoding::Direct => {
                let discr = db
                    .const_eval_discriminant(variant)
                    .map_err(|e| CodegenError::Frontend(format!("discriminant: {e:?}")))?;
                Some(discr as u128)
            }
            TagEncoding::Niche { untagged_variant, .. } if untagged_variant.0 == index => None,
            TagEncoding::Niche { niche_variants, niche_start, .. } => {
                Some(((index - niche_variants.start.0) as u128).wrapping_add(*niche_start))
            }
        };
        if let Some(tag_value) = tag_value {
            let value = self.iconst(tag_ty, tag_value);
            self.builder.ins().store(MEM, value, dst, offset);
        }
        Ok(Some(Arc::new(Layout::for_variant(&layout, RustcEnumVariantIdx(index)))))
    }

    fn write_fields(&mut self, dst: Value, layout: &Layout, operands: &[Operand]) -> Result<()> {
        if operands.len() > layout.fields.count() {
            unsupported!("constructor with more operands than the layout has fields");
        }
        for (i, operand) in operands.iter().enumerate() {
            let (src, ty) = self.operand(operand)?;
            let size = self.layout(ty)?.size;
            let field = self.builder.ins().iadd_imm(dst, layout.fields.offset(i).bytes() as i64);
            self.copy(field, src, size);
        }
        Ok(())
    }

    fn binary_op(&mut self, op: &BinOp, lhs: &Operand, rhs: &Operand) -> Result<Value> {
        Ok(self.overflowing_op(op, lhs, rhs)?.0)
    }

    /// `lhs op rhs`, which never panics: runtime MIR checks with an `Assert` before it. For
    /// `AddWithOverflow` and the like, also whether it overflowed.
    fn overflowing_op(
        &mut self,
        op: &BinOp,
        lhs: &Operand,
        rhs: &Operand,
    ) -> Result<(Value, Option<Value>)> {
        let (op, overflowing) = match op {
            BinOp::AddWithOverflow => (&BinOp::Add, true),
            BinOp::SubWithOverflow => (&BinOp::Sub, true),
            BinOp::MulWithOverflow => (&BinOp::Mul, true),
            op => (op, false),
        };
        let (mut l_addr, mut ty) = self.operand(lhs)?;
        let (mut r_addr, mut r_ty) = self.operand(rhs)?;
        // RA lowers comparisons of references without the deref.
        // Where the innermost reference is stored, for the length of a `str`.
        let (mut l_ref, mut r_ref) = (None, None);
        while let TyKind::Ref(_, inner, _) = ty.kind() {
            ty = inner;
            l_ref = Some(l_addr);
            l_addr = self.builder.ins().load(ptr(), MEM, l_addr, 0);
        }
        while let TyKind::Ref(_, inner, _) = r_ty.kind() {
            r_ty = inner;
            r_ref = Some(r_addr);
            r_addr = self.builder.ins().load(ptr(), MEM, r_addr, 0);
        }
        // A constant pattern of a type that has no `==` of its own: rustc matches such a
        // constant field by field, never through `PartialEq`.
        if let BinOp::Eq | BinOp::Ne = op
            && matches!(
                ty.kind(),
                TyKind::Adt(..)
                    | TyKind::Tuple(_)
                    | TyKind::Array(..)
                    | TyKind::Slice(_)
                    | TyKind::Str
            )
        {
            let metas = match (l_ref, r_ref) {
                (Some(l_ref), Some(r_ref)) if self.is_unsized(ty) => Some((
                    self.builder.ins().load(ptr(), MEM, l_ref, meta_at()),
                    self.builder.ins().load(ptr(), MEM, r_ref, meta_at()),
                )),
                _ => None,
            };
            let (differ, done) = (self.builder.create_block(), self.builder.create_block());
            self.builder.append_block_param(done, types::I8);
            self.branch_unless_equal(l_addr, r_addr, metas, ty, differ)?;
            let equal = self.builder.ins().iconst(types::I8, matches!(op, BinOp::Eq) as i64);
            self.builder.ins().jump(done, &[equal.into()]);
            self.builder.switch_to_block(differ);
            let unequal = self.builder.ins().iconst(types::I8, matches!(op, BinOp::Ne) as i64);
            self.builder.ins().jump(done, &[unequal.into()]);
            self.builder.switch_to_block(done);
            return Ok((self.builder.block_params(done)[0], None));
        }
        if matches!(ty.kind(), TyKind::RawPtr(..))
            && matches!(self.layout(ty)?.backend_repr, BackendRepr::ScalarPair { .. })
        {
            // Wide pointers compare by address, then by metadata.
            let cc = match op {
                BinOp::Eq => IntCC::Equal,
                BinOp::Ne => IntCC::NotEqual,
                BinOp::Lt => IntCC::UnsignedLessThan,
                BinOp::Le => IntCC::UnsignedLessThanOrEqual,
                BinOp::Gt => IntCC::UnsignedGreaterThan,
                BinOp::Ge => IntCC::UnsignedGreaterThanOrEqual,
                _ => unsupported!("{op:?} on wide pointers"),
            };
            let [l_ptr, r_ptr] =
                [l_addr, r_addr].map(|it| self.builder.ins().load(ptr(), MEM, it, 0));
            let [l_meta, r_meta] =
                [l_addr, r_addr].map(|it| self.builder.ins().load(ptr(), MEM, it, meta_at()));
            let same_ptr = self.builder.ins().icmp(IntCC::Equal, l_ptr, r_ptr);
            let by_meta = self.builder.ins().icmp(cc, l_meta, r_meta);
            let by_ptr = self.builder.ins().icmp(cc, l_ptr, r_ptr);
            let value = match op {
                BinOp::Eq => self.builder.ins().band(same_ptr, by_meta),
                BinOp::Ne => self.builder.ins().bor(by_ptr, by_meta),
                _ => self.builder.ins().select(same_ptr, by_meta, by_ptr),
            };
            return Ok((value, None));
        }
        let scalar = self.scalar_of(ty)?;
        let r_scalar = self.scalar_of(r_ty)?;
        // A literal is an immediate, and the constant it was given an address as is not read.
        let l = match self.literal_bits(lhs) {
            Some((bits, _)) => self.iconst(scalar.ty, bits),
            None => self.builder.ins().load(scalar.ty, MEM, l_addr, 0),
        };
        let r = match self.literal_bits(rhs) {
            Some((bits, _)) => self.iconst(r_scalar.ty, bits),
            None => self.builder.ins().load(r_scalar.ty, MEM, r_addr, 0),
        };
        let signed = scalar.signed;
        if scalar.ty.is_float() {
            return Ok((self.float_binary_op(op, l, r)?, None));
        }
        let cc = match (op, signed) {
            (BinOp::Eq, _) => Some(IntCC::Equal),
            (BinOp::Ne, _) => Some(IntCC::NotEqual),
            (BinOp::Lt, true) => Some(IntCC::SignedLessThan),
            (BinOp::Lt, false) => Some(IntCC::UnsignedLessThan),
            (BinOp::Le, true) => Some(IntCC::SignedLessThanOrEqual),
            (BinOp::Le, false) => Some(IntCC::UnsignedLessThanOrEqual),
            (BinOp::Gt, true) => Some(IntCC::SignedGreaterThan),
            (BinOp::Gt, false) => Some(IntCC::UnsignedGreaterThan),
            (BinOp::Ge, true) => Some(IntCC::SignedGreaterThanOrEqual),
            (BinOp::Ge, false) => Some(IntCC::UnsignedGreaterThanOrEqual),
            _ => None,
        };
        if let Some(cc) = cc {
            return Ok((self.builder.ins().icmp(cc, l, r), None));
        }
        if let BinOp::Mul = op
            && self.builder.func.dfg.value_type(l) == types::I128
        {
            let (value, overflow) = self.wide_mul_overflow(l, r, signed)?;
            return Ok((value, overflowing.then_some(overflow)));
        }
        let ins = self.builder.ins();
        let value = match op {
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                unreachable!("comparisons return above")
            }
            BinOp::BitAnd => ins.band(l, r),
            BinOp::BitOr => ins.bor(l, r),
            BinOp::BitXor => ins.bxor(l, r),
            BinOp::Add | BinOp::Sub | BinOp::Mul => {
                let (value, overflow) = match (op, signed) {
                    (BinOp::Add, true) => ins.sadd_overflow(l, r),
                    (BinOp::Add, false) => ins.uadd_overflow(l, r),
                    (BinOp::Sub, true) => ins.ssub_overflow(l, r),
                    (BinOp::Sub, false) => ins.usub_overflow(l, r),
                    (BinOp::Mul, true) => ins.smul_overflow(l, r),
                    (_, _) => ins.umul_overflow(l, r),
                };
                return Ok((value, overflowing.then_some(overflow)));
            }
            // An `Assert` before it has ruled out a zero divisor and `MIN / -1`.
            BinOp::Div | BinOp::Rem => self.divide(l, r, matches!(op, BinOp::Div), signed)?,
            // The amount is taken modulo the width, as rustc's `Shl` and `Shr` do.
            BinOp::Shl | BinOp::Shr => {
                let (r, _) = self.shift_amount_word(r);
                match (op, signed) {
                    (BinOp::Shl, _) => self.builder.ins().ishl(l, r),
                    (_, true) => self.builder.ins().sshr(l, r),
                    (_, false) => self.builder.ins().ushr(l, r),
                }
            }
            BinOp::Offset => unsupported!("pointer offset"),
            BinOp::AddWithOverflow | BinOp::SubWithOverflow | BinOp::MulWithOverflow => {
                unreachable!("taken apart above")
            }
        };
        Ok((value, None))
    }

    /// A shift amount as at most one machine word, and whether the part cut off was not zero.
    /// A shift takes its amount modulo the width of the value, which the low word decides.
    fn shift_amount_word(&mut self, amount: Value) -> (Value, Option<Value>) {
        if self.builder.func.dfg.value_type(amount) != types::I128 {
            return (amount, None);
        }
        let (low, high) = self.builder.ins().isplit(amount);
        let beyond = self.builder.ins().icmp_imm(IntCC::NotEqual, high, 0);
        (low, Some(beyond))
    }

    fn float_binary_op(&mut self, op: &BinOp, l: Value, r: Value) -> Result<Value> {
        let ins = self.builder.ins();
        Ok(match op {
            BinOp::Eq => ins.fcmp(FloatCC::Equal, l, r),
            BinOp::Ne => ins.fcmp(FloatCC::NotEqual, l, r),
            BinOp::Lt => ins.fcmp(FloatCC::LessThan, l, r),
            BinOp::Le => ins.fcmp(FloatCC::LessThanOrEqual, l, r),
            BinOp::Gt => ins.fcmp(FloatCC::GreaterThan, l, r),
            BinOp::Ge => ins.fcmp(FloatCC::GreaterThanOrEqual, l, r),
            BinOp::Add => ins.fadd(l, r),
            BinOp::Sub => ins.fsub(l, r),
            BinOp::Mul => ins.fmul(l, r),
            BinOp::Div => ins.fdiv(l, r),
            BinOp::Rem => {
                let ty = self.builder.func.dfg.value_type(l);
                let name = if ty == types::F32 { "fmodf" } else { "fmod" };
                self.float_libcall(name, ty, &[l, r])?
            }
            _ => unsupported!("float operator {op:?}"),
        })
    }

    /// Calls a libm function taking and returning `ty`.
    fn float_libcall(&mut self, name: &str, ty: Type, args: &[Value]) -> Result<Value> {
        let mut sig = self.cg.make_signature();
        sig.params.extend(args.iter().map(|_| AbiParam::new(ty)));
        sig.returns.push(AbiParam::new(ty));
        let callee = self.cg.import(name, &sig)?;
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        Ok(self.invoke(callee, args)[0])
    }

    /// `as` between a float and an integer or another float.
    fn float_cast(&mut self, value: Value, from: AbiScalar, to: AbiScalar) -> Result<Value> {
        if from.ty == types::I128 || to.ty == types::I128 {
            // compiler-builtins' conversions, which saturate and turn NaN into zero as `as` does.
            let name = match (from.ty, from.signed, to.ty, to.signed) {
                (types::F32, _, _, true) => "__fixsfti",
                (types::F32, _, _, false) => "__fixunssfti",
                (types::F64, _, _, true) => "__fixdfti",
                (types::F64, _, _, false) => "__fixunsdfti",
                (_, true, types::F32, _) => "__floattisf",
                (_, false, types::F32, _) => "__floatuntisf",
                (_, true, types::F64, _) => "__floattidf",
                (_, false, types::F64, _) => "__floatuntidf",
                _ => unsupported!("cast from {} to {}", from.ty, to.ty),
            };
            return self.builtin(name, &[from.ty], to.ty, &[value]);
        }
        let ins = self.builder.ins();
        Ok(match (from.ty.is_float(), to.ty.is_float()) {
            (true, true) if from.ty == to.ty => value,
            (true, true) if to.ty == types::F64 => ins.fpromote(types::F64, value),
            (true, true) => ins.fdemote(types::F32, value),
            (false, true) => {
                let wide = if from.ty.bits() < 32 {
                    if from.signed {
                        ins.sextend(types::I32, value)
                    } else {
                        ins.uextend(types::I32, value)
                    }
                } else {
                    value
                };
                if from.signed {
                    self.builder.ins().fcvt_from_sint(to.ty, wide)
                } else {
                    self.builder.ins().fcvt_from_uint(to.ty, wide)
                }
            }
            (true, false) => {
                // Saturating, with NaN becoming zero, as `as` requires.
                if to.ty.bits() >= 32 {
                    if to.signed {
                        ins.fcvt_to_sint_sat(to.ty, value)
                    } else {
                        ins.fcvt_to_uint_sat(to.ty, value)
                    }
                } else {
                    let wide = ins.fcvt_to_sint_sat(types::I32, value);
                    let bits = to.ty.bits();
                    let (min, max) = if to.signed {
                        (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1)
                    } else {
                        (0, (1i64 << bits) - 1)
                    };
                    let min = self.builder.ins().iconst(types::I32, min);
                    let max = self.builder.ins().iconst(types::I32, max);
                    let clamped = self.builder.ins().smax(wide, min);
                    let clamped = self.builder.ins().smin(clamped, max);
                    self.builder.ins().ireduce(to.ty, clamped)
                }
            }
            (false, false) => unreachable!("float cast between integers"),
        })
    }

    /// Whether arithmetic of the body being compiled panics when it overflows.
    fn checks_overflow(&self) -> Result<bool> {
        let Some(body) = self.body else { return Ok(false) };
        if !body_written_checked(self.cg.db, body) {
            return Ok(false);
        }
        Ok(self.cg.checked)
    }

    /// Branches to a call of rustc's `panic_const_*` when `cond` is set.
    fn panic_if(&mut self, cond: Value, op: OverflowOp) -> Result<()> {
        let name = match op {
            OverflowOp::Add => "add_overflow",
            OverflowOp::Sub => "sub_overflow",
            OverflowOp::Mul => "mul_overflow",
            OverflowOp::Div => "div_overflow",
            OverflowOp::Rem => "rem_overflow",
            OverflowOp::Neg => "neg_overflow",
            OverflowOp::Shl => "shl_overflow",
            OverflowOp::Shr => "shr_overflow",
            OverflowOp::DivByZero => "div_by_zero",
            OverflowOp::RemByZero => "rem_by_zero",
        };
        let path = format!("core::panicking::panic_const::panic_const_{name}");
        let panic_block = self.builder.create_block();
        let ok_block = self.builder.create_block();
        self.builder.set_cold_block(panic_block);
        self.builder.ins().brif(cond, panic_block, &[], ok_block, &[]);

        self.builder.switch_to_block(panic_block);
        let location = self.caller_location()?;
        let mut sig = self.cg.make_signature();
        sig.params.push(AbiParam::new(ptr()));
        let callee = self.cg.import_path(&path, &sig);
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        self.invoke(callee, &[location]);
        self.builder.ins().trap(TrapCode::user(1).expect("non-zero trap code"));

        self.builder.switch_to_block(ok_block);
        Ok(())
    }

    /// Whether `expr` is what `line!()`, `column!()` or `file!()` expanded to, and the
    /// expansion it is in.
    fn location_macro(
        &self,
        body: &MirBody<'db>,
        expr: ExprId,
    ) -> Option<(BuiltinFnLikeExpander, HirFileId)> {
        let InferBodyId::DefWithBodyId(owner) = body.owner else { return None };
        location_macros(self.cg.db, owner).get(&expr).copied()
    }

    /// Whether a promoted expression takes its value from where it is in the source.
    fn mentions_location(&self, body: &MirBody<'db>, expr: ExprId) -> bool {
        let InferBodyId::DefWithBodyId(owner) = body.owner else { return false };
        if self.location_macro(body, expr).is_some()
            || located_concats(self.cg.db, owner).contains_key(&expr)
        {
            return true;
        }
        let mut found = false;
        Body::of(self.cg.db, owner)
            .walk_child_exprs(expr, |child| found |= self.mentions_location(body, child));
        found
    }

    /// The `&str` of a literal that a `concat!` with `line!()`, `column!()` or `file!()`
    /// among its arguments expanded to: the expansion has their placeholders in it.
    fn located_concat(&mut self, expr: ExprId) -> Result<Option<Value>> {
        let db = self.cg.db;
        let Some(InferBodyId::DefWithBodyId(owner)) = self.body.map(|it| it.owner) else {
            return Ok(None);
        };
        let Some(call) = located_concats(db, owner)
            .get(&expr)
            .and_then(|file_id| file_id.macro_file())
            .map(|it| it.loc(db).to_node(db))
        else {
            return Ok(None);
        };
        let Some(parts) = concat_parts(&call.value) else {
            unsupported!("`concat!` of a place in the source and what is not a literal");
        };
        let mut text = Vec::new();
        for part in parts {
            text.push(match part {
                ConcatPart::Text(text) => TextPart::Text(text),
                ConcatPart::Location(expander, range) => {
                    let Some((file, line, col)) = location_of(db, call.file_id, range) else {
                        unsupported!("`concat!` of a place that is not in a file");
                    };
                    match expander {
                        BuiltinFnLikeExpander::Line => TextPart::Text(line.to_string()),
                        BuiltinFnLikeExpander::Column => TextPart::Text(col.to_string()),
                        _ => TextPart::FilePath(file),
                    }
                }
            });
        }
        let text = self.cg.text(text);
        let text = self.cg.declare_data_in_func(text, self.builder.func);
        Ok(Some(self.builder.ins().symbol_value(ptr(), text)))
    }

    /// The value of a literal that `line!()`, `column!()` or `file!()` expanded to.
    /// rust-analyzer expands them to placeholders, since a real value would make every body
    /// below an edit change.
    fn builtin_location(&mut self, expr: ExprId, ty: Ty<'db>) -> Result<Option<Value>> {
        let db = self.cg.db;
        if let Some(text) = self.located_concat(expr)? {
            return Ok(Some(text));
        }
        let Some((expander, file_id)) = self.body.and_then(|it| self.location_macro(it, expr))
        else {
            return Ok(None);
        };
        let Some(site) = written_call(db, file_id) else { return Ok(None) };
        let range = InFile::new(site.file_id, written_range(&site.value));
        let FileRange { file_id, range } = range.original_node_file_range_rooted(db);
        let Some((file, line, col)) = location_of(db, file_id.into(), range) else {
            return Ok(None);
        };
        let number = match expander {
            BuiltinFnLikeExpander::Line => line,
            BuiltinFnLikeExpander::Column => col,
            _ => {
                let path = self.cg.file_path(file);
                let path = self.cg.declare_data_in_func(path, self.builder.func);
                return Ok(Some(self.builder.ins().symbol_value(ptr(), path)));
            }
        };
        if !matches!(ty.kind(), TyKind::Uint(UintTy::U32)) {
            return Ok(None);
        }
        let layout = self.layout(ty)?;
        let addr = self.stack_slot(&layout);
        let value = self.builder.ins().iconst(types::I32, number as i64);
        self.builder.ins().store(MEM, value, addr, 0);
        Ok(Some(addr))
    }

    /// Notes the locals a debugger can be told of: the ones written with a name whose value
    /// is in a stack slot of its own, or in the future of an `async` body. What a closure
    /// captured is no local of its body, and is told of by the type of the closure alone.
    fn note_variables(&mut self) {
        let Some(body) = self.body.filter(|body| Self::names_are_written(body)) else { return };
        // The slot an `async` body has the address of its future in.
        let future = match (body.coroutine.is_some(), body.param_locals.first()) {
            (true, Some(&receiver)) => self.slot_of(receiver),
            _ => None,
        };
        for (binding, &local) in body.binding_locals.iter() {
            let param = body.param_locals.contains(&local);
            let ty = body.locals[local].ty.clone();
            let held = self.held_at.iter().find(|it| it.0 == local).map(|it| it.1);
            if let (Some(slot), Some(offset)) = (future, held) {
                self.cg.named.push(Named { binding, param, ty, slot, pointer: Some(offset) });
            } else if let Some(slot) = self.slot_of(local) {
                self.cg.named.push(Named { binding, param, ty, slot, pointer: None });
            }
        }
    }

    /// The stack slot a local is the whole of, when it is kept in one.
    fn slot_of(&self, local: LocalId) -> Option<ir::StackSlot> {
        let &LocalSlot { addr, borrowed: false } = self.locals.get(local)? else { return None };
        let dfg = &self.builder.func.dfg;
        let ir::ValueDef::Result(inst, 0) = dfg.value_def(addr) else { return None };
        match dfg.insts[inst] {
            ir::InstructionData::StackAddr { stack_slot, offset, .. } if i32::from(offset) == 0 => {
                Some(stack_slot)
            }
            _ => None,
        }
    }

    /// Whether the locals of a body are of names its source has.
    fn names_are_written(body: &MirBody) -> bool {
        matches!(body.owner, InferBodyId::DefWithBodyId(_))
    }

    /// Keeps where a parameter passed in memory is in a slot of the frame, when it is
    /// written with a name, for a debugger to find it by: the register it comes in is soon
    /// used for something else.
    fn note_passed_in_memory(&mut self, body: &MirBody, local: LocalId, addr: Value) {
        if !Self::names_are_written(body) {
            return;
        }
        let Some((binding, _)) = body.binding_locals.iter().find(|it| *it.1 == local) else {
            return;
        };
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            ptr().bytes(),
            3,
        ));
        let kept = self.builder.ins().stack_addr(ptr(), slot, 0);
        self.builder.ins().store(MEM, addr, kept, 0);
        let ty = body.locals[local].ty.clone();
        self.cg.named.push(Named { binding, param: true, ty, slot, pointer: Some(0) });
    }

    /// Has what is built from here on be of the statement at `span`.
    fn at(&mut self, span: MirSpan) {
        self.span = span;
        let loc = self.cg.source_loc(span);
        self.builder.set_srcloc(loc);
    }

    /// A `core::panic::Location` (`{ file: &str, line: u32, col: u32 }`) for `#[track_caller]`.
    fn caller_location(&mut self) -> Result<Value> {
        if let Some(location) = self.location {
            return Ok(location);
        }
        let at = match self.body.map(|body| body.owner) {
            Some(InferBodyId::DefWithBodyId(owner)) => Some((owner, self.span)),
            _ => None,
        };
        let location = self.cg.location(at);
        let location = self.cg.declare_data_in_func(location, self.builder.func);
        Ok(self.builder.ins().symbol_value(ptr(), location))
    }

    fn terminator(&mut self, terminator: &TerminatorKind) -> Result<()> {
        match terminator {
            TerminatorKind::Goto { target }
            | TerminatorKind::FalseEdge { real_target: target, .. }
            | TerminatorKind::FalseUnwind { real_target: target, .. } => {
                self.builder.ins().jump(self.blocks[*target], &[]);
            }
            TerminatorKind::SwitchInt { discr, targets } => {
                if let Some(bits) = self.const_bits(discr) {
                    let target = self.blocks[targets.target_for_value(bits)];
                    self.builder.ins().jump(target, &[]);
                    return Ok(());
                }
                let register = self.register_of(discr);
                let (addr, ty) = match register {
                    Some(_) => (self.iconst(ptr(), 0), self.operand_ty(discr)?),
                    None => self.operand(discr)?,
                };
                let scalar = self.scalar_of(ty)?;
                let otherwise = self.blocks[targets.otherwise()];
                let mut switch = Switch::new();
                for (case, target) in targets.iter() {
                    switch.set_entry(case, self.blocks[target]);
                }
                // A discriminant is an `i128` to MIR and a small number: with the high half
                // told to be zero first, the rest is a switch on a register.
                let small = targets.iter().all(|(case, _)| case <= i64::MAX as u128);
                let value = if let Some(register) = register {
                    let value = self.builder.use_var(register.var);
                    let widened = match self.builder.func.dfg.value_def(value) {
                        ir::ValueDef::Result(inst, 0) => match self.builder.func.dfg.insts[inst] {
                            ir::InstructionData::Unary { opcode: ir::Opcode::Uextend, arg } => {
                                Some(arg)
                            }
                            _ => None,
                        },
                        _ => None,
                    }
                    .filter(|&arg| self.builder.func.dfg.value_type(arg) == types::I64);
                    if let (Some(low), true) = (widened, scalar.ty == types::I128 && small) {
                        // The high half is zero when the value was a word a moment ago.
                        low
                    } else if scalar.ty == types::I128 && small {
                        let (low, high) = self.builder.ins().isplit(value);
                        let fits = self.builder.create_block();
                        self.builder.ins().brif(high, otherwise, &[], fits, &[]);
                        self.builder.switch_to_block(fits);
                        low
                    } else {
                        value
                    }
                } else if scalar.ty == types::I128
                    && small
                    && self.cg.target_config().pointer_bytes() == 8
                {
                    let high = self.builder.ins().load(types::I64, MEM, addr, 8);
                    let low = self.builder.create_block();
                    self.builder.ins().brif(high, otherwise, &[], low, &[]);
                    self.builder.switch_to_block(low);
                    self.builder.ins().load(types::I64, MEM, addr, 0)
                } else {
                    self.builder.ins().load(scalar.ty, MEM, addr, 0)
                };
                switch.emit(&mut self.builder, value, otherwise);
            }
            TerminatorKind::Return => {
                if let Some(state) = &self.suspended {
                    let (addr, ty) = (state.addr, state.ty);
                    self.store_state(addr, ty, RETURNED);
                }
                self.return_()?;
            }
            TerminatorKind::Yield { resume, .. } => {
                let Some(state) = &self.suspended else {
                    unsupported!("yield outside of an async body");
                };
                let Some(index) = state.held.yields.iter().position(|it| it.resume == *resume)
                else {
                    unsupported!("yield to a block that is not resumed in");
                };
                let (addr, ty) = (state.addr, state.ty);
                self.store_state(addr, ty, SUSPENDED + index as i64);
                self.return_()?;
            }
            TerminatorKind::Unreachable => {
                self.builder.ins().trap(TrapCode::user(2).expect("non-zero trap code"));
            }
            TerminatorKind::Drop { place, target, .. } => {
                let place = self.place_ref(place)?;
                self.drop_value(place.addr, place.meta, place.ty.ty)?;
                self.builder.ins().jump(self.blocks[*target], &[]);
            }
            TerminatorKind::UnwindResume => {
                // An `async` body that unwinds is poisoned, as rustc has it.
                if let Some(state) = &self.suspended {
                    let (addr, ty) = (state.addr, state.ty);
                    self.store_state(addr, ty, PANICKED);
                }
                let slot = self.exception_slot();
                let exception = self.builder.ins().stack_load(ptr(), ptr(), slot, 0);
                self.resume_unwinding(exception)?;
            }
            // A suspended future has been dropped: its state already says it returned.
            TerminatorKind::CoroutineDrop => self.return_()?,
            TerminatorKind::Assert { cond, expected, msg, target, .. } => {
                // rustc's codegen skips an overflow check where overflow is not checked.
                if !msg.is_optional_overflow_check() || self.checks_overflow()? {
                    let (holds, _) = self.load_scalar(cond)?;
                    let cc = if *expected { IntCC::Equal } else { IntCC::NotEqual };
                    let failed = self.builder.ins().icmp_imm(cc, holds, 0);
                    let op = match msg {
                        AssertKind::BoundsCheck { len, index } => {
                            let (len, _) = self.load_scalar(len)?;
                            let (index, _) = self.load_scalar(index)?;
                            self.bounds_panic_if(failed, index, len)?;
                            None
                        }
                        AssertKind::Overflow(BinOp::Add, ..) => Some(OverflowOp::Add),
                        AssertKind::Overflow(BinOp::Sub, ..) => Some(OverflowOp::Sub),
                        AssertKind::Overflow(BinOp::Mul, ..) => Some(OverflowOp::Mul),
                        AssertKind::Overflow(BinOp::Div, ..) => Some(OverflowOp::Div),
                        AssertKind::Overflow(BinOp::Rem, ..) => Some(OverflowOp::Rem),
                        AssertKind::Overflow(BinOp::Shl, ..) => Some(OverflowOp::Shl),
                        AssertKind::Overflow(BinOp::Shr, ..) => Some(OverflowOp::Shr),
                        AssertKind::Overflow(op, ..) => unsupported!("overflow check of {op:?}"),
                        AssertKind::OverflowNeg(_) => Some(OverflowOp::Neg),
                        AssertKind::DivisionByZero(_) => Some(OverflowOp::DivByZero),
                        AssertKind::RemainderByZero(_) => Some(OverflowOp::RemByZero),
                    };
                    if let Some(op) = op {
                        self.panic_if(failed, op)?;
                    }
                }
                self.builder.ins().jump(self.blocks[*target], &[]);
            }
            TerminatorKind::Call { func, args, destination, target, .. } => {
                self.call(func, args, destination, *target)?
            }
            _ => unsupported!("terminator {terminator:?}"),
        }
        Ok(())
    }

    fn call(
        &mut self,
        func: &Operand,
        args: &[Operand],
        destination: &StoredPlace,
        target: Option<BasicBlockId>,
    ) -> Result<()> {
        self.given_away = Some(GivenAway {
            result: destination.local,
            whole: FxHashSet::default(),
            read: Vec::new(),
        });
        let done = self.call_with(func, args, destination, target);
        self.given_away = None;
        done
    }

    fn call_with(
        &mut self,
        func: &Operand,
        args: &[Operand],
        destination: &StoredPlace,
        target: Option<BasicBlockId>,
    ) -> Result<()> {
        let (dst, dst_ty) = self.place(destination)?;
        let fn_ty = self.operand_ty(func)?;
        if let TyKind::FnPtr(..) = fn_ty.kind() {
            let (addr, _) = self.operand(func)?;
            let pointer = self.builder.ins().load(ptr(), MEM, addr, 0);
            let values = self.operands(args)?;
            let sig = fn_ty.fn_sig(self.interner);
            let named = sig.c_variadic().then(|| sig.inputs().skip_binder().len());
            self.emit_call(Callee::Pointer(pointer, sig.abi(), named), &values, dst, dst_ty.ty)?;
            return self.finish_call(destination, target);
        }
        let TyKind::FnDef(def, generic_args) = fn_ty.kind() else {
            unsupported!("call of {fn_ty:?}");
        };
        let func = match def.0 {
            CallableDefId::FunctionId(func) => func,
            CallableDefId::StructId(id) => {
                if let Some(layout) = self.variant_layout(dst, id.into(), generic_args)? {
                    self.write_fields(dst, &layout, args)?;
                }
                return self.finish_call(destination, target);
            }
            CallableDefId::EnumVariantId(id) => {
                if let Some(layout) = self.variant_layout(dst, id.into(), generic_args)? {
                    self.write_fields(dst, &layout, args)?;
                }
                return self.finish_call(destination, target);
            }
        };
        let db = self.cg.db;
        if fncache::is_code(db, func) {
            self.cache_code(generic_args, dst)?;
            return self.finish_call(destination, target);
        }
        let lang_items = self.interner.lang_items();
        if fn_trait_methods(lang_items).iter().any(|family| family.contains(&Some(func))) {
            self.call_fn_trait(func, generic_args, args, dst, dst_ty.ty)?;
            return self.finish_call(destination, target);
        }
        if [lang_items.DropInPlace, lang_items.DropGlue].contains(&Some(func)) {
            // The body is a placeholder for what the compiler generates.
            let [pointer] = args else {
                unsupported!("drop glue with {} arguments", args.len());
            };
            let ty = generic_args.type_at(0);
            let (src, _) = self.operand(pointer)?;
            let addr = self.builder.ins().load(ptr(), MEM, src, 0);
            let meta =
                self.is_unsized(ty).then(|| self.builder.ins().load(ptr(), MEM, src, meta_at()));
            self.drop_value(addr, meta, ty)?;
            return self.finish_call(destination, target);
        }
        if FunctionSignature::is_intrinsic(db, func)
            && FunctionSignature::of(db, func).name.as_str() == "const_eval_select"
        {
            // `const_eval_select((a, b), const_fn, runtime_fn)` is `runtime_fn(a, b)` at run time.
            let [tuple, _, runtime] = args else {
                unsupported!("const_eval_select with {} arguments", args.len());
            };
            let TyKind::FnDef(runtime, runtime_args) = self.operand_ty(runtime)?.kind() else {
                unsupported!("const_eval_select with a non-fn-item runtime function");
            };
            let CallableDefId::FunctionId(runtime) = runtime.0 else {
                unsupported!("const_eval_select with a constructor");
            };
            let (tuple_addr, tuple_ty) = self.operand(tuple)?;
            let TyKind::Tuple(fields) = tuple_ty.kind() else {
                unsupported!("const_eval_select with non-tuple arguments");
            };
            let layout = self.layout(tuple_ty)?;
            let mut values = Vec::new();
            for (i, ty) in fields.iter().enumerate() {
                let offset = layout.fields.offset(i).bytes() as i64;
                values.push((self.builder.ins().iadd_imm(tuple_addr, offset), ty));
            }
            self.call_direct(runtime, runtime_args, &values, dst, dst_ty.ty)?;
            return self.finish_call(destination, target);
        }
        if FunctionSignature::is_intrinsic(db, func) {
            match self.intrinsic(func, generic_args, args, dst, dst_ty.ty)? {
                Intrinsic::Inline => return self.finish_call(destination, target),
                Intrinsic::Diverges => return Ok(()),
                Intrinsic::Unknown => {
                    let overridden = AttrFlags::query(db, func.into())
                        .contains(AttrFlags::RUSTC_INTRINSIC_MUST_BE_OVERRIDDEN);
                    let signature = FunctionSignature::of(db, func);
                    // These have a body that only stands in for the const evaluator.
                    let compile_time = matches!(
                        signature.name.as_str(),
                        "const_allocate"
                            | "const_deallocate"
                            | "const_make_global"
                            | "size_of_type_id"
                            | "type_id_vtable"
                            | "type_of"
                    );
                    if overridden || compile_time || !signature.has_body() {
                        unsupported!("intrinsic `{}`", signature.name.as_str());
                    }
                }
            }
        }
        if let Some(name) = link_name(self.cg.db, func)
            && let Some(name) = name.strip_prefix("llvm.")
        {
            self.llvm_intrinsic(name, args, dst, dst_ty.ty)?;
            return self.finish_call(destination, target);
        }
        let values = self.operands(args)?;
        self.call_direct(func, generic_args, &values, dst, dst_ty.ty)?;
        self.finish_call(destination, target)
    }

    fn operands(&mut self, operands: &[Operand]) -> Result<Vec<(Value, Ty<'db>)>> {
        operands.iter().map(|it| self.operand(it)).collect()
    }

    /// `Fn::call`, `FnMut::call_mut` or `FnOnce::call_once`. For a call expression MIR passes
    /// the callee first and then the arguments one by one, not as a tuple.
    fn call_fn_trait(
        &mut self,
        method: FunctionId,
        method_args: GenericArgs<'db>,
        args: &[Operand],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<()> {
        let Some((callee, rest)) = args.split_first() else {
            unsupported!("Fn-trait call without a callee");
        };
        // A `dyn FnOnce` called by value, as `Box<dyn FnOnce>` does with what it holds: the
        // `call_once` slot takes a pointer to the callee and leaves it dropped.
        if let OperandKind::Copy(place) | OperandKind::Move(place) = &callee.kind {
            let place = self.place_ref(place)?;
            if let TyKind::Dynamic(..) = place.ty.ty.kind() {
                let dyn_ty = place.ty.ty;
                let Some(vtable) = place.meta else {
                    unsupported!("Fn-trait call on {dyn_ty:?} without a vtable");
                };
                // Marks the callee as moved out of.
                self.operand(callee)?;
                let Some(trait_ref) = self.principal(dyn_ty, dyn_ty) else {
                    unsupported!("vtable call on {dyn_ty:?}");
                };
                let Some(slot) = self.vtable_layout(trait_ref).method(method) else {
                    unsupported!("method missing from the vtable of {dyn_ty:?}");
                };
                let pointer = self.builder.ins().load(ptr(), MEM, vtable, slot as i32 * meta_at());
                let thin = self.scratch();
                self.builder.ins().store(MEM, place.addr, thin, 0);
                let thin_ty = Ty::new_imm_ptr(self.interner, Ty::new_unit(self.interner));
                let mut values = vec![(thin, thin_ty)];
                let tuple_ty = method_args.type_at(1);
                match (&self.operands(rest)?[..], tuple_ty.kind()) {
                    (&[(tuple, ty)], TyKind::Tuple(fields)) if ty == tuple_ty => {
                        let layout = self.layout(tuple_ty)?;
                        values.extend(fields.iter().enumerate().map(|(i, field)| {
                            let offset = layout.fields.offset(i).bytes() as i64;
                            (self.builder.ins().iadd_imm(tuple, offset), field)
                        }));
                    }
                    (rest, _) => values.extend_from_slice(rest),
                }
                let callee = Callee::Pointer(pointer, ExternAbi::Rust, None);
                return self.emit_call(callee, &values, dst, dst_ty);
            }
        }
        let (mut addr, mut ty) = self.operand(callee)?;
        let mut values = self.operands(rest)?;
        // A call written as `f.call_mut(args)` does pass the tuple, which the callee takes
        // apart all the same.
        let tuple_ty = method_args.type_at(1);
        if let (&[(tuple, ty)], TyKind::Tuple(fields)) = (&values[..], tuple_ty.kind())
            && ty == tuple_ty
        {
            let layout = self.layout(tuple_ty)?;
            values = fields
                .iter()
                .enumerate()
                .map(|(i, field)| {
                    let offset = layout.fields.offset(i).bytes() as i64;
                    (self.builder.ins().iadd_imm(tuple, offset), field)
                })
                .collect();
        }
        // The innermost reference to the callee and the address it is stored at.
        let mut by_ref = None;
        while let TyKind::Ref(_, inner, mutability) = ty.kind() {
            if self.implements_fn_itself(method, ty, tuple_ty) {
                break;
            }
            by_ref = Some((addr, ty, mutability));
            if let TyKind::Dynamic(..) = inner.kind() {
                let Some(method) = self.fn_method_of_object(inner) else {
                    unsupported!("Fn-trait call on {inner:?}");
                };
                return self.call_virtual(method, inner, addr, ty, &values, dst, dst_ty);
            }
            addr = self.builder.ins().load(ptr(), MEM, addr, 0);
            ty = inner;
        }
        match ty.kind() {
            TyKind::FnDef(def, generic_args) => {
                let func = match def.0 {
                    CallableDefId::FunctionId(func) => func,
                    CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) => {
                        let TyKind::Adt(_, adt_args) = dst_ty.kind() else {
                            unsupported!("constructor of {dst_ty:?}");
                        };
                        let layout = match def.0 {
                            CallableDefId::EnumVariantId(id) => {
                                self.variant_layout(dst, id.into(), adt_args)?
                            }
                            _ => Some(self.layout(dst_ty)?),
                        };
                        let Some(layout) = layout else {
                            return Ok(());
                        };
                        for (i, &(src, ty)) in values.iter().enumerate() {
                            let size = self.layout(ty)?.size;
                            let offset = layout.fields.offset(i).bytes() as i64;
                            let field = self.builder.ins().iadd_imm(dst, offset);
                            self.copy(field, src, size);
                        }
                        return Ok(());
                    }
                };
                if FunctionSignature::is_intrinsic(self.cg.db, func) {
                    unsupported!("intrinsic called through an Fn trait");
                }
                self.call_direct(func, generic_args, &values, dst, dst_ty)
            }
            TyKind::FnPtr(..) => {
                let pointer = self.builder.ins().load(ptr(), MEM, addr, 0);
                let abi = ty.fn_sig(self.interner).abi();
                self.emit_call(Callee::Pointer(pointer, abi, None), &values, dst, dst_ty)
            }
            TyKind::Closure(..) | TyKind::CoroutineClosure(..) => {
                let (id, parent_args) = match ty.kind() {
                    TyKind::Closure(id, generic_args) => {
                        (id.0, generic_args.as_closure().parent_args())
                    }
                    TyKind::CoroutineClosure(id, generic_args) => {
                        let closure = generic_args.as_coroutine_closure();
                        if let TyKind::Coroutine(future, future_args) = dst_ty.kind()
                            && is_by_move_coroutine(
                                self.cg.db,
                                future.0,
                                future_args.as_coroutine(),
                            )
                        {
                            let captured = closure.tupled_upvars_ty();
                            return self.future_by_move(dst, dst_ty, &values, addr, ty, captured);
                        }
                        (id.0.as_closure(self.cg.db), closure.parent_args())
                    }
                    _ => unreachable!(),
                };
                let parent_args = GenericArgs::new_from_slice(parent_args);
                let body = self.body_types(InstanceDef::Closure(id.as_id()), parent_args)?;
                if body.params.len() != values.len() + 1 {
                    unsupported!("closure called with a tuple of arguments");
                }
                // The body takes the closure by reference or by value depending on its kind,
                // whichever trait method the call went through.
                let self_ty = body.params[0].as_ref();
                let self_arg = if self_ty.as_reference().is_some() {
                    let slot = self.scratch();
                    self.builder.ins().store(MEM, addr, slot, 0);
                    (slot, self_ty)
                } else {
                    (addr, ty)
                };
                values.insert(0, self_arg);
                let def = InstanceDef::Closure(id.as_id());
                let call = |this: &mut Self| {
                    this.emit_call(Callee::Instance(def, parent_args), &values, dst, dst_ty)
                };
                // A closure that is passed by value to a body that only borrows it is the
                // caller's to drop, whether the body returns or unwinds.
                if by_ref.is_none() && self_ty.as_reference().is_some() {
                    self.guarded(call, |this| this.drop_value(addr, None, ty))?;
                    self.drop_value(addr, None, ty)
                } else {
                    call(self)
                }
            }
            // A type with its own `Fn` impls, which take the arguments as a tuple.
            TyKind::Adt(..) | TyKind::Ref(..) => {
                let layout = self.layout(tuple_ty)?;
                let tuple = self.stack_slot(&layout);
                for (i, &(src, ty)) in values.iter().enumerate() {
                    let size = self.layout(ty)?.size;
                    let offset = layout.fields.offset(i).bytes() as i64;
                    let field = self.builder.ins().iadd_imm(tuple, offset);
                    self.copy(field, src, size);
                }
                let [by_shared, by_mut, _] = fn_trait_methods(self.interner.lang_items())
                    .into_iter()
                    .find(|family| family.contains(&Some(method)))
                    .expect("an Fn-trait method");
                let (method, self_arg) = match by_ref {
                    None => (Some(method), (addr, ty)),
                    Some((slot, ref_ty, Mutability::Not)) => (by_shared, (slot, ref_ty)),
                    Some((slot, ref_ty, Mutability::Mut)) => (by_mut, (slot, ref_ty)),
                };
                let Some(method) = method else {
                    unsupported!("Fn-trait call on {ty:?}");
                };
                let args = GenericArgs::new_from_slice(&[ty.into(), tuple_ty.into()]);
                self.call_direct(method, args, &[self_arg, (tuple, tuple_ty)], dst, dst_ty)
            }
            _ => unsupported!("Fn-trait call on {ty:?}"),
        }
    }

    /// The future of an `async` closure that is called by value though it could be called
    /// again: it takes over the arguments and what the closure at `closure` captured.
    fn future_by_move(
        &mut self,
        dst: Value,
        future_ty: Ty<'db>,
        arguments: &[(Value, Ty<'db>)],
        closure: Value,
        closure_ty: Ty<'db>,
        captured: Ty<'db>,
    ) -> Result<()> {
        let layout = self.layout(future_ty)?;
        let closure_layout = self.layout(closure_ty)?;
        let mut fields = arguments.to_vec();
        for (i, ty) in captured.tuple_fields().iter().enumerate() {
            let offset = closure_layout.fields.offset(i).bytes() as i64;
            fields.push((self.builder.ins().iadd_imm(closure, offset), ty));
        }
        for (i, (src, ty)) in fields.into_iter().enumerate() {
            let size = self.layout(ty)?.size;
            let field = self.builder.ins().iadd_imm(dst, layout.fields.offset(i).bytes() as i64);
            self.copy(field, src, size);
        }
        self.start_coroutine(dst, &layout)
    }

    /// Marks the future at `dst` as not polled yet.
    fn start_coroutine(&mut self, dst: Value, layout: &Layout) -> Result<()> {
        // The last field says where the body left off.
        let Some(tag) = CoroutineFrame::state_of(layout) else {
            unsupported!("an async block without a state");
        };
        let tag = self.scalar(tag)?.ty;
        let state = layout.fields.offset(layout.fields.count() - 1).bytes();
        let state = self.builder.ins().iadd_imm(dst, state as i64);
        self.store_state(state, tag, UNRESUMED);
        Ok(())
    }

    /// Whether the reference type `ty` has an `Fn` impl written for it, as opposed to the ones
    /// every reference to a callable has, which call what it points to.
    fn implements_fn_itself(&self, method: FunctionId, ty: Ty<'db>, tuple_ty: Ty<'db>) -> bool {
        let db = self.cg.db;
        let args = GenericArgs::new_from_slice(&[ty.into(), tuple_ty.into()]);
        let (either::Either::Left(callee), _) = db.lookup_impl_method(self.env, method, args)
        else {
            return false;
        };
        let ItemContainerId::ImplId(impl_) = callee.lookup(db).container else {
            return false;
        };
        match db.impl_self_ty(impl_).skip_binder().kind() {
            TyKind::Ref(_, pointee, _) => !matches!(pointee.kind(), TyKind::Param(_)),
            _ => false,
        }
    }

    /// Calls trait method `func` on the trait object behind the fat pointer at `receiver`.
    #[allow(clippy::too_many_arguments)]
    fn call_virtual(
        &mut self,
        func: FunctionId,
        dyn_ty: Ty<'db>,
        receiver: Value,
        receiver_ty: Ty<'db>,
        rest: &[(Value, Ty<'db>)],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<()> {
        let receiver_layout = self.layout(receiver_ty)?;
        if !matches!(receiver_layout.backend_repr, BackendRepr::ScalarPair { .. })
            || receiver_layout.size.bytes_usize() != 2 * word()
        {
            unsupported!("vtable call with receiver {receiver_ty:?}");
        }
        let Some(trait_ref) = self.principal(dyn_ty, dyn_ty) else {
            unsupported!("vtable call on {dyn_ty:?}");
        };
        let Some(slot) = self.vtable_layout(trait_ref).method(func) else {
            unsupported!("method missing from the vtable of {dyn_ty:?}");
        };
        let data = self.builder.ins().load(ptr(), MEM, receiver, 0);
        let vtable = self.builder.ins().load(ptr(), MEM, receiver, meta_at());
        let pointer = self.builder.ins().load(ptr(), MEM, vtable, slot as i32 * meta_at());
        // The method takes the data pointer alone where the caller holds a fat pointer.
        let thin = self.scratch();
        self.builder.ins().store(MEM, data, thin, 0);
        let thin_ty = Ty::new_imm_ptr(self.interner, Ty::new_unit(self.interner));
        let mut values = vec![(thin, thin_ty)];
        values.extend_from_slice(rest);
        if track_caller(self.cg.db, func) {
            let location = self.caller_location()?;
            let slot = self.scratch();
            self.builder.ins().store(MEM, location, slot, 0);
            values.push((slot, thin_ty));
        }
        self.emit_call(Callee::Pointer(pointer, ExternAbi::Rust, None), &values, dst, dst_ty)
    }

    /// The method a call through a reference to `dyn_ty` goes to: `Fn::call` where the
    /// object's trait is `Fn` or has it above it, `FnMut::call_mut` likewise. Which one is in
    /// the vtable depends on the object's trait, not on the trait the call was written against.
    fn fn_method_of_object(&self, dyn_ty: Ty<'db>) -> Option<FunctionId> {
        let lang_items = self.interner.lang_items();
        let vtable = self.vtable_layout(self.principal(dyn_ty, dyn_ty)?);
        [lang_items.Fn_call, lang_items.FnMut_call_mut]
            .into_iter()
            .flatten()
            .find(|&method| vtable.method(method).is_some())
    }

    /// The principal trait of `dyn_ty`, as implemented by `self_ty`.
    fn principal(&self, dyn_ty: Ty<'db>, self_ty: Ty<'db>) -> Option<TraitRef<'db>> {
        let TyKind::Dynamic(bounds, _) = dyn_ty.kind() else {
            return None;
        };
        let principal = bounds.principal()?.with_self_ty(self.interner, self_ty);
        Some(self.plain_trait(self.interner.instantiate_bound_regions_with_erased(principal)))
    }

    /// `trait_ref` without its lifetimes and with its projections resolved: two ways of
    /// writing one trait are one trait in a vtable.
    fn plain_trait(&self, trait_ref: TraitRef<'db>) -> TraitRef<'db> {
        let erased = vtable::without_lifetimes(self.interner, trait_ref);
        crate::next_solver::obligation_ctxt::ObligationCtxt::new(&self.infcx)
            .deeply_normalize(
                &crate::next_solver::infer::traits::ObligationCause::dummy(),
                self.env.param_env,
                erased,
            )
            .unwrap_or(erased)
    }

    /// The vtable of `trait_ref`, with each supertrait in the one form `plain_trait` gives it.
    fn vtable_layout(&self, trait_ref: TraitRef<'db>) -> VtableLayout<'db> {
        VtableLayout::of(self.interner, trait_ref, &|it| self.plain_trait(it))
    }

    /// Writes the value of `from_ty` at `src` to `dst` as `to_ty`, which it is unsized to: a
    /// pointer gets its metadata, and a struct around one is unsized through its fields.
    fn unsize(&mut self, src: Value, from_ty: Ty<'db>, dst: Value, to_ty: Ty<'db>) -> Result<()> {
        let (from, to) = (self.layout(from_ty)?, self.layout(to_ty)?);
        if from_ty == to_ty {
            self.copy(dst, src, to.size);
            return Ok(());
        }
        match (from_ty.kind(), to_ty.kind()) {
            (
                TyKind::RawPtr(from_pointee, _) | TyKind::Ref(_, from_pointee, _),
                TyKind::RawPtr(to_pointee, _) | TyKind::Ref(_, to_pointee, _),
            ) => {
                let old = (from.size.bytes() == 2 * word() as u64)
                    .then(|| self.builder.ins().load(ptr(), MEM, src, meta_at()));
                let meta = self.unsize_metadata(from_pointee, to_pointee, old)?;
                let ptr = self.builder.ins().load(ptr(), MEM, src, 0);
                self.builder.ins().store(MEM, ptr, dst, 0);
                self.builder.ins().store(MEM, meta, dst, meta_at());
            }
            // `NonNull` holds its pointer as a pattern type.
            (TyKind::Pat(from_ty, _), TyKind::Pat(to_ty, _)) => {
                self.unsize(src, from_ty, dst, to_ty)?
            }
            (TyKind::Adt(def, args), TyKind::Adt(to_def, to_args))
                if def.def_id() == to_def.def_id() =>
            {
                let AdtId::StructId(id) = def.def_id() else {
                    unsupported!("unsizing {from_ty:?} to {to_ty:?}");
                };
                // `CoerceUnsized` changes one field, `PhantomData` aside, and where the others
                // are may change with it.
                let field_types = self.cg.db.field_types(id.into());
                for (i, (_, field)) in field_types.iter().enumerate() {
                    let from_field = field.ty().instantiate(self.interner, args).skip_norm_wip();
                    let to_field = field.ty().instantiate(self.interner, to_args).skip_norm_wip();
                    let (from_field, to_field) =
                        (self.normalize(from_field), self.normalize(to_field));
                    if self.layout(to_field)?.size.bytes() == 0 {
                        continue;
                    }
                    let src =
                        self.builder.ins().iadd_imm(src, from.fields.offset(i).bytes() as i64);
                    let dst = self.builder.ins().iadd_imm(dst, to.fields.offset(i).bytes() as i64);
                    self.unsize(src, from_field, dst, to_field)?;
                }
            }
            _ => unsupported!("unsizing {from_ty:?} to {to_ty:?}"),
        }
        Ok(())
    }

    /// The metadata of a pointer to `from` after unsizing its pointee to `to`. `old` is the
    /// metadata the pointer has now, if it is fat already.
    fn unsize_metadata(&mut self, from: Ty<'db>, to: Ty<'db>, old: Option<Value>) -> Result<Value> {
        match (from.kind(), to.kind()) {
            (TyKind::Array(_, len), TyKind::Slice(_)) => {
                let Some(len) = try_const_usize(self.cg.db, len) else {
                    unsupported!("array with unevaluated length");
                };
                Ok(self.builder.ins().iconst(ptr(), len as i64))
            }
            (TyKind::Dynamic(..), TyKind::Dynamic(..)) => {
                let Some(vtable) = old else {
                    unsupported!("upcast of a thin pointer");
                };
                let (Some(trait_ref), Some(target)) =
                    (self.principal(from, from), self.principal(to, from))
                else {
                    // Dropping the principal: every vtable has the header.
                    return Ok(vtable);
                };
                if trait_ref == target {
                    // Only auto traits or lifetimes change.
                    return Ok(vtable);
                }
                match self.vtable_layout(trait_ref).upcast(target) {
                    Some(Upcast::Same) => Ok(vtable),
                    Some(Upcast::Load(slot)) => {
                        Ok(self.builder.ins().load(ptr(), MEM, vtable, slot as i32 * meta_at()))
                    }
                    None => unsupported!("upcast of {from:?} to {to:?}"),
                }
            }
            (_, TyKind::Dynamic(..)) => {
                if self.is_unsized(from) {
                    unsupported!("unsizing {from:?} to {to:?}");
                }
                let trait_ref = self.principal(to, from);
                let vtable = self.vtable(from, trait_ref)?;
                let vtable = self.cg.declare_data_in_func(vtable, self.builder.func);
                Ok(self.builder.ins().symbol_value(ptr(), vtable))
            }
            (TyKind::Adt(def, args), TyKind::Adt(to_def, to_args))
                if def.def_id() == to_def.def_id() =>
            {
                // A struct is unsized through its last field.
                let AdtId::StructId(id) = def.def_id() else {
                    unsupported!("unsizing {from:?}");
                };
                let field_types = self.cg.db.field_types(id.into());
                let Some((_, last)) = field_types.iter().last() else {
                    unsupported!("unsizing a struct without fields");
                };
                // The field may be an associated type of a parameter: an array of one, a slice
                // of the other.
                let from = last.ty().instantiate(self.interner, args).skip_norm_wip();
                let to = last.ty().instantiate(self.interner, to_args).skip_norm_wip();
                let (from, to) = (self.normalize(from), self.normalize(to));
                self.unsize_metadata(from, to, old)
            }
            _ => unsupported!("unsizing {from:?} to {to:?}"),
        }
    }

    /// The vtable of `ty` for `trait_ref`, or with the header alone when the trait object has
    /// no principal trait.
    fn vtable(&mut self, ty: Ty<'db>, trait_ref: Option<TraitRef<'db>>) -> Result<DataId> {
        let key = (ty.store(), trait_ref.map(|it| (it.def_id.0, it.args.store())));
        if let Some(id) = self.cg.vtable(&key) {
            return Ok(id);
        }
        let layout = self.layout(ty)?;
        let slots = trait_ref.map_or_else(Vec::new, |it| self.vtable_layout(it).slots);
        let mut bytes = vec![0u8; (vtable::HEADER + slots.len()) * word()];
        bytes[word()..2 * word()].copy_from_slice(&layout.size.bytes().to_le_bytes()[..word()]);
        bytes[2 * word()..3 * word()]
            .copy_from_slice(&layout.align.abi.bytes().to_le_bytes()[..word()]);
        let mut blob =
            ConstBlob { bytes, relocs: Vec::new(), fn_relocs: Vec::new(), align: word() as u64 };
        if self.glue_of(ty) != DropGlue::None {
            let mut sig = self.cg.make_signature();
            sig.params.push(AbiParam::new(ptr()));
            let args = GenericArgs::new_from_slice(&[ty.into()]);
            let glue = self.cg.callee(InstanceDef::DropGlue, args, self.env, &sig)?;
            blob.fn_relocs.push((0, glue));
        }
        for (index, slot) in slots.into_iter().enumerate() {
            let offset = ((vtable::HEADER + index) * word()) as u32;
            match slot {
                // A slot nothing can call through is left empty, as rustc leaves it.
                Slot::Method(func, trait_ref) if self.method_bounds_hold(func, trait_ref) => {
                    let method = self.vtable_method(func, trait_ref)?;
                    blob.fn_relocs.push((offset, method));
                }
                Slot::Method(..) => {}
                Slot::Vptr(trait_ref) => {
                    let vptr = self.vtable(ty, Some(trait_ref))?;
                    blob.relocs.push((offset, vptr, 0));
                }
            }
        }
        Ok(self.cg.define_vtable(key, blob))
    }

    /// What trait method `func` is called with through the vtable of `trait_ref`: only
    /// lifetimes can be generic in a method that is in a vtable.
    fn vtable_method_args(
        &self,
        func: FunctionId,
        trait_ref: TraitRef<'db>,
    ) -> Option<GenericArgs<'db>> {
        let interner = self.interner;
        let mut generic = false;
        let args =
            GenericArgs::fill_rest(interner, func.into(), trait_ref.args.iter(), |_, id, _| {
                generic |= !matches!(id, hir_def::GenericParamId::LifetimeParamId(_));
                Region::new_erased(interner).into()
            });
        (!generic).then_some(args)
    }

    /// Whether the `where` clauses trait method `func` has of its own hold for `trait_ref`.
    /// A method they do not hold for can never be called.
    fn method_bounds_hold(&self, func: FunctionId, trait_ref: TraitRef<'db>) -> bool {
        let Some(args) = self.vtable_method_args(func, trait_ref) else { return true };
        let interner = self.interner;
        let cause = crate::next_solver::infer::traits::ObligationCause::dummy();
        let mut ocx = crate::next_solver::obligation_ctxt::ObligationCtxt::new(&self.infcx);
        let bounds = crate::GenericPredicates::query_own_explicit(self.cg.db, func.into());
        for bound in bounds.iter_instantiated(interner, args.as_slice()) {
            ocx.register_obligation(crate::next_solver::infer::traits::Obligation::new(
                interner,
                cause,
                self.env.param_env,
                bound.skip_norm_wip(),
            ));
        }
        ocx.try_evaluate_obligations().is_empty()
    }

    /// The function a vtable slot for trait method `func` points to.
    fn vtable_method(&mut self, func: FunctionId, trait_ref: TraitRef<'db>) -> Result<FuncId> {
        let db = self.cg.db;
        let interner = self.interner;
        let self_ty = trait_ref.args.type_at(0);
        let lang_items = interner.lang_items();
        let [fn_methods, _] = fn_trait_methods(lang_items);
        let once = Some(func) == lang_items.FnOnce_call_once;
        let written =
            !matches!(self_ty.kind(), TyKind::Closure(..) | TyKind::FnDef(..) | TyKind::FnPtr(..));
        if fn_methods.contains(&Some(func)) && !written {
            return self.vtable_fn_method(self_ty, once);
        }
        // A type with an impl of the trait, as `Box<dyn Fn()>` has: `call_once` takes it by
        // value, and the slot is called with a pointer to it.
        if once {
            let (callee, callee_args, sig, mode) = self.call_once_impl(trait_ref.args)?;
            if matches!(mode, PassMode::Indirect) {
                return self.cg.callee(InstanceDef::Fn(callee), callee_args, self.env, &sig);
            }
            let sig = Self::by_pointer(&sig, mode);
            return self.cg.callee(InstanceDef::CallOnceShim, trait_ref.args, self.env, &sig);
        }
        if Some(func) == lang_items.FuturePoll
            && let Some((def, parent_args)) = coroutine_instance(db, self_ty)
        {
            let sig = self.body_signature(self.body_types(def, parent_args)?)?;
            return self.cg.callee(def, parent_args, self.env, &sig);
        }
        let Some(args) = self.vtable_method_args(func, trait_ref) else {
            unsupported!("generic method in a vtable");
        };
        let (callee, callee_args) = db.lookup_impl_method(self.env, func, args);
        let either::Either::Left(callee) = callee else {
            unsupported!("builtin derive method in a vtable");
        };
        if !FunctionSignature::of(db, callee).has_body() {
            let method = FunctionSignature::of(db, callee).name.as_str();
            unsupported!("no impl of `{method}` found for the vtable of {self_ty:?}");
        }
        let body = self.body_types(InstanceDef::Fn(callee), callee_args)?;
        let Some(receiver) = body.params.first() else {
            unsupported!("vtable method without a receiver");
        };
        // A by-value `self` reaches the method through a pointer and needs a shim, unless the
        // method takes it through a pointer too.
        if receiver.as_ref() == self_ty {
            let layout = self.layout(self_ty)?;
            let mode = self.pass_mode(&layout, false)?;
            if !matches!(mode, PassMode::Indirect) {
                if track_caller(db, callee) {
                    unsupported!("vtable method taking `self` by value that tracks its caller");
                }
                let sig = Self::by_pointer(&self.body_signature(body)?, mode);
                return self.cg.callee(
                    InstanceDef::VtableShim(callee),
                    callee_args,
                    self.env,
                    &sig,
                );
            }
        }
        // A method declared `#[track_caller]` in the trait is given a location by the call. One
        // that is so only in the impl is called without, and the shim supplies it.
        if track_caller(db, callee) && !track_caller(db, func) {
            let def = CallableDefId::FunctionId(callee).into();
            return self.reified(Ty::new_fn_def(interner, def, callee_args), false);
        }
        let mut sig = self.body_signature(body)?;
        if track_caller(db, callee) {
            sig.params.push(AbiParam::new(ptr()));
        }
        self.cg.callee(InstanceDef::Fn(callee), callee_args, self.env, &sig)
    }

    /// `call`, `call_mut` or `call_once` in the vtable of a closure.
    fn vtable_fn_method(&mut self, ty: Ty<'db>, once: bool) -> Result<FuncId> {
        let TyKind::Closure(id, generic_args) = ty.kind() else {
            return self.reified(ty, true);
        };
        let parent_args = GenericArgs::new_from_slice(generic_args.as_closure().parent_args());
        let body = self.body_types(InstanceDef::Closure(id.0.as_id()), parent_args)?;
        let sig = self.body_signature(body)?;
        let Some(receiver) = body.params.first() else {
            unsupported!("closure without a receiver");
        };
        let receiver_ty = receiver.as_ref();
        let closure = InstanceDef::Closure(id.0.as_id());
        if receiver_ty.as_reference().is_none() {
            // The body takes the closure by value. Only when that means "through a pointer to
            // memory the body owns" is it what the `call_once` slot is called with.
            if !once {
                unsupported!("vtable shim for a closure taken by value");
            }
            let layout = self.layout(receiver_ty)?;
            let mode = self.pass_mode(&layout, false)?;
            if matches!(mode, PassMode::Indirect) {
                return self.cg.callee(closure, parent_args, self.env, &sig);
            }
            let sig = Self::by_pointer(&sig, mode);
            let args = GenericArgs::new_from_slice(&[ty.into()]);
            return self.cg.callee(InstanceDef::CallOnceShim, args, self.env, &sig);
        }
        if once {
            let args = GenericArgs::new_from_slice(&[ty.into()]);
            return self.cg.callee(InstanceDef::CallOnceShim, args, self.env, &sig);
        }
        self.cg.callee(closure, parent_args, self.env, &sig)
    }

    /// The `ReifyShim` of a fn item or fn pointer type.
    fn reified(&mut self, ty: Ty<'db>, receiver: bool) -> Result<FuncId> {
        let sig = self.reify_signature(ty, receiver)?.1;
        let args = GenericArgs::new_from_slice(&[ty.into()]);
        self.cg.callee(InstanceDef::ReifyShim { receiver }, args, self.env, &sig)
    }

    /// What a `ReifyShim` is called with and how.
    fn reify_signature(
        &mut self,
        ty: Ty<'db>,
        receiver: bool,
    ) -> Result<(rustc_type_ir::FnSig<DbInterner<'db>>, Signature, FnAbi)> {
        if !matches!(ty.kind(), TyKind::FnDef(..) | TyKind::FnPtr(..)) {
            unsupported!("Fn-trait vtable of {ty:?}");
        }
        let fn_sig = self.interner.instantiate_bound_regions_with_erased(ty.fn_sig(self.interner));
        if fn_sig.c_variadic() {
            unsupported!("shim for a variadic function");
        }
        // A vtable is called as a Rust function, whatever the function behind it is.
        let abi = if receiver { ExternAbi::Rust } else { fn_sig.abi() };
        let abi = self.fn_abi(abi, fn_sig.output(), fn_sig.inputs().iter().copied())?;
        let mut sig = self.signature(&abi.args, abi.ret);
        if receiver {
            let sret = matches!(abi.ret, PassMode::Indirect);
            sig.params.insert(sret as usize, AbiParam::new(ptr()));
        }
        Ok((fn_sig, sig, abi))
    }

    /// The body of a `ReifyShim`: calls what the type names with the arguments it is handed.
    fn reify_shim(&mut self, ty: Ty<'db>, receiver: bool) -> Result<()> {
        let (fn_sig, sig, FnAbi { ret, args: modes }) = self.reify_signature(ty, receiver)?;
        self.builder.func.signature = sig;
        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);
        let mut incoming = self.builder.block_params(entry).to_vec().into_iter();
        let dst_ty = fn_sig.output();
        let dst_layout = self.layout(dst_ty)?;
        let dst = match ret {
            PassMode::Indirect => incoming.next().expect("sret param"),
            _ => self.stack_slot(&dst_layout),
        };
        let receiver = if receiver { incoming.next() } else { None };
        let mut values = Vec::new();
        for (&ty, mode) in fn_sig.inputs().iter().zip(modes) {
            let layout = self.layout(ty)?;
            let addr = match mode {
                PassMode::OnStack { padded: true, .. } => {
                    incoming.nth(1).expect("a parameter in memory after its padding")
                }
                PassMode::Indirect | PassMode::OnStack { .. } => {
                    incoming.next().expect("indirect param")
                }
                _ => self.stack_slot(&layout),
            };
            self.receive(mode, &mut incoming, addr, &layout);
            values.push((addr, ty));
        }
        match ty.kind() {
            TyKind::FnDef(def, generic_args) => match def.0 {
                CallableDefId::FunctionId(func) => {
                    if FunctionSignature::is_intrinsic(self.cg.db, func) {
                        unsupported!("shim for an intrinsic");
                    }
                    let db = self.cg.db;
                    let (callee, _) = db.lookup_impl_method(self.env, func, generic_args);
                    if let either::Either::Left(callee) = callee
                        && track_caller(db, callee)
                    {
                        let location = self.cg.declaration(callee);
                        let location = self.cg.declare_data_in_func(location, self.builder.func);
                        self.location = Some(self.builder.ins().symbol_value(ptr(), location));
                    }
                    self.call_direct(func, generic_args, &values, dst, dst_ty)?;
                }
                CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) => {
                    let TyKind::Adt(_, adt_args) = dst_ty.kind() else {
                        unsupported!("constructor of {dst_ty:?}");
                    };
                    let layout = match def.0 {
                        CallableDefId::EnumVariantId(id) => {
                            self.variant_layout(dst, id.into(), adt_args)?
                        }
                        _ => Some(self.layout(dst_ty)?),
                    };
                    if let Some(layout) = layout {
                        for (i, &(src, ty)) in values.iter().enumerate() {
                            let size = self.layout(ty)?.size;
                            let offset = layout.fields.offset(i).bytes() as i64;
                            let field = self.builder.ins().iadd_imm(dst, offset);
                            self.copy(field, src, size);
                        }
                    }
                }
            },
            _ => {
                let receiver = receiver.expect("a fn pointer is only reified for a vtable");
                let pointer = self.builder.ins().load(ptr(), MEM, receiver, 0);
                self.emit_call(Callee::Pointer(pointer, fn_sig.abi(), None), &values, dst, dst_ty)?;
            }
        }
        let mut results = Vec::new();
        if !matches!(ret, PassMode::Indirect) {
            self.pass(ret, dst, &dst_layout, &mut results);
        }
        self.builder.ins().return_(&results);
        Ok(())
    }

    /// The body of the `main` the C runtime calls: `lang_start` with the program's `main`,
    /// as rustc writes it.
    fn start_shim(&mut self, main: FunctionId) -> Result<()> {
        let Some(start) = self.interner.lang_items().Start else {
            unsupported!("no `lang_start`");
        };
        let fn_ty = |func: FunctionId, args| {
            Ty::new_fn_def(self.interner, CallableDefId::FunctionId(func).into(), args)
        };
        let sig_of = |ty: Ty<'db>| {
            self.interner.instantiate_bound_regions_with_erased(ty.fn_sig(self.interner))
        };
        let main_ty = fn_ty(main, GenericArgs::new_from_slice(&[]));
        let returned = self.normalize(sig_of(main_ty).output());
        let start_args = GenericArgs::new_from_slice(&[returned.into()]);
        let start_sig = sig_of(fn_ty(start, start_args));
        let mut sig = Signature::new(self.cg.isa.default_call_conv());
        sig.params = vec![AbiParam::new(types::I32), AbiParam::new(ptr())];
        sig.returns = vec![AbiParam::new(types::I32)];
        self.builder.func.signature = sig;
        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);
        let &[argc, argv] = self.builder.block_params(entry) else {
            unsupported!("`main` is called with a count and the arguments");
        };
        let argc = self.builder.ins().sextend(types::I64, argc);
        let callee = self.fn_item(main_ty)?;
        let callee = self.cg.declare_func_address_in_func(callee, self.builder.func);
        let pointer = self.builder.ins().func_addr(ptr(), callee);
        // What `SIGPIPE` does is left as the runtime has it.
        let sigpipe = self.builder.ins().iconst(types::I8, 0);
        let &[main_param, argc_ty, argv_ty, sigpipe_ty] = start_sig.inputs().as_slice() else {
            unsupported!("a `lang_start` that takes other than four arguments");
        };
        let mut values = Vec::new();
        for (value, ty) in
            [(pointer, main_param), (argc, argc_ty), (argv, argv_ty), (sigpipe, sigpipe_ty)]
        {
            let ty = self.normalize(ty);
            let layout = self.layout(ty)?;
            let slot = self.stack_slot(&layout);
            self.builder.ins().store(MEM, value, slot, 0);
            values.push((slot, ty));
        }
        let code_ty = start_sig.output();
        let layout = self.layout(code_ty)?;
        let code = self.stack_slot(&layout);
        self.call_direct(start, start_args, &values, code, code_ty)?;
        let code = self.builder.ins().load(ptr(), MEM, code, 0);
        let code =
            if ptr() == types::I32 { code } else { self.builder.ins().ireduce(types::I32, code) };
        self.builder.ins().return_(&[code]);
        Ok(())
    }

    /// The function a closure that captures nothing is as a function pointer.
    fn closure_fn_pointer(&mut self, ty: Ty<'db>) -> Result<FuncId> {
        let TyKind::Closure(id, generic_args) = ty.kind() else {
            unsupported!("fn pointer to {ty:?}");
        };
        let parent_args = GenericArgs::new_from_slice(generic_args.as_closure().parent_args());
        let body = self.body_types(InstanceDef::Closure(id.0.as_id()), parent_args)?;
        let mut sig = self.body_signature(body)?;
        let Some(receiver) = body.params.first() else {
            unsupported!("closure without a receiver");
        };
        if receiver.as_ref().as_reference().is_none() {
            // Taken by value, a closure without captures is not passed at all.
            return self.cg.callee(InstanceDef::Closure(id.0.as_id()), parent_args, self.env, &sig);
        }
        let sret = sig.params.first().is_some_and(|it| it.purpose == ArgumentPurpose::StructReturn);
        sig.params.remove(sret as usize);
        let args = GenericArgs::new_from_slice(&[ty.into()]);
        self.cg.callee(InstanceDef::FnPointerShim, args, self.env, &sig)
    }

    /// The body of a `FnPointerShim`: the signature of the closure body less its receiver.
    fn fn_pointer_shim(&mut self, ty: Ty<'db>) -> Result<()> {
        let TyKind::Closure(id, generic_args) = ty.kind() else {
            unsupported!("fn pointer shim for {ty:?}");
        };
        let parent_args = GenericArgs::new_from_slice(generic_args.as_closure().parent_args());
        let body = self.body_types(InstanceDef::Closure(id.0.as_id()), parent_args)?;
        let sig = self.body_signature(body)?;
        let sret = sig.params.first().is_some_and(|it| it.purpose == ArgumentPurpose::StructReturn);
        let mut shim = sig.clone();
        shim.params.remove(sret as usize);
        self.builder.func.signature = shim;
        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);
        let mut params = self.builder.block_params(entry).to_vec();
        let receiver = self.builder.ins().iconst(ptr(), 1);
        params.insert(sret as usize, receiver);
        let callee =
            self.cg.callee(InstanceDef::Closure(id.0.as_id()), parent_args, self.env, &sig)?;
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        let results = self.invoke(callee, &params);
        self.builder.ins().return_(&results);
        Ok(())
    }

    /// The `FnOnce::call_once` written for a type, given the type and the tuple of what it
    /// is called with, and how the function takes the type.
    fn call_once_impl(
        &mut self,
        args: GenericArgs<'db>,
    ) -> Result<(FunctionId, GenericArgs<'db>, Signature, PassMode)> {
        let Some(func) = self.interner.lang_items().FnOnce_call_once else {
            unsupported!("no `FnOnce::call_once`");
        };
        let (callee, callee_args) = self.cg.db.lookup_impl_method(self.env, func, args);
        let either::Either::Left(callee) = callee else {
            unsupported!("builtin derive method in a vtable");
        };
        if !FunctionSignature::of(self.cg.db, callee).has_body() {
            unsupported!("no impl of `call_once` found for {:?}", args.type_at(0));
        }
        let body = self.body_types(InstanceDef::Fn(callee), callee_args)?;
        let sig = self.body_signature(body)?;
        let Some(receiver) = body.params.first() else {
            unsupported!("`call_once` without a receiver");
        };
        let layout = self.layout(receiver.as_ref())?;
        let mode = self.pass_mode(&layout, false)?;
        Ok((callee, callee_args, sig, mode))
    }

    /// The body of a `CallOnceShim` for a type with an impl of `FnOnce`: calls it with the
    /// value the shim is handed a pointer to.
    fn call_once_by_pointer(&mut self, args: GenericArgs<'db>) -> Result<()> {
        let (callee, callee_args, sig, mode) = self.call_once_impl(args)?;
        let layout = self.layout(args.type_at(0))?;
        self.call_by_pointer(callee, callee_args, &sig, mode, &layout)
    }

    /// The body of a `VtableShim`: calls the method with the value the shim is handed a
    /// pointer to.
    fn vtable_shim(&mut self, func: FunctionId, args: GenericArgs<'db>) -> Result<()> {
        let body = self.body_types(InstanceDef::Fn(func), args)?;
        let sig = self.body_signature(body)?;
        let Some(receiver) = body.params.first() else {
            unsupported!("vtable method without a receiver");
        };
        let layout = self.layout(receiver.as_ref())?;
        let mode = self.pass_mode(&layout, false)?;
        self.call_by_pointer(func, args, &sig, mode, &layout)
    }

    /// The body of a shim that is handed a pointer to the receiver of `callee`, which takes
    /// it by value as `mode` says.
    fn call_by_pointer(
        &mut self,
        callee: FunctionId,
        callee_args: GenericArgs<'db>,
        sig: &Signature,
        mode: PassMode,
        layout: &Layout,
    ) -> Result<()> {
        self.builder.func.signature = Self::by_pointer(sig, mode);
        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);
        let mut params = self.builder.block_params(entry).to_vec();
        let sret = sig.params.first().is_some_and(|it| it.purpose == ArgumentPurpose::StructReturn);
        let Some(&receiver) = params.get(sret as usize) else {
            unsupported!("shim for a receiver that is not passed");
        };
        let callee = self.cg.callee(InstanceDef::Fn(callee), callee_args, self.env, sig)?;
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        let mut values = Vec::new();
        self.pass(mode, receiver, layout, &mut values);
        params.splice(sret as usize..sret as usize + 1, values);
        let results = self.invoke(callee, &params);
        self.builder.ins().return_(&results);
        Ok(())
    }

    /// The body of a `CallOnceShim`: same signature as the closure body, which borrows the
    /// closure, but the shim owns it and so drops it after the call.
    fn call_once_shim(&mut self, ty: Ty<'db>) -> Result<()> {
        let TyKind::Closure(id, generic_args) = ty.kind() else {
            unsupported!("call_once shim for {ty:?}");
        };
        let parent_args = GenericArgs::new_from_slice(generic_args.as_closure().parent_args());
        let body = self.body_types(InstanceDef::Closure(id.0.as_id()), parent_args)?;
        let sig = self.body_signature(body)?;
        let Some(receiver) = body.params.first() else {
            unsupported!("closure without a receiver");
        };
        let receiver_ty = receiver.as_ref();
        let by_value = receiver_ty.as_reference().is_none();
        let layout = self.layout(receiver_ty)?;
        let mode = self.pass_mode(&layout, false)?;
        self.builder.func.signature =
            if by_value { Self::by_pointer(&sig, mode) } else { sig.clone() };
        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);
        let mut params = self.builder.block_params(entry).to_vec();
        let sret = sig.params.first().is_some_and(|it| it.purpose == ArgumentPurpose::StructReturn);
        let Some(&closure) = params.get(sret as usize) else {
            unsupported!("call_once shim for a closure that is not passed");
        };
        let callee =
            self.cg.callee(InstanceDef::Closure(id.0.as_id()), parent_args, self.env, &sig)?;
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        if by_value {
            // The body owns the closure and takes it in registers.
            let mut values = Vec::new();
            self.pass(mode, closure, &layout, &mut values);
            params.splice(sret as usize..sret as usize + 1, values);
            let results = self.invoke(callee, &params);
            self.builder.ins().return_(&results);
            return Ok(());
        }
        let results = self.invoke(callee, &params);
        self.drop_value(closure, None, ty)?;
        self.builder.ins().return_(&results);
        Ok(())
    }

    /// `sig` with the receiver, which is passed as `mode` says, behind a pointer instead.
    fn by_pointer(sig: &Signature, mode: PassMode) -> Signature {
        let sret = sig.params.first().is_some_and(|it| it.purpose == ArgumentPurpose::StructReturn);
        let count = match mode {
            PassMode::Ignore => 0,
            PassMode::Pair(..) => 2,
            PassMode::SpilledPair { spilled, .. } => {
                spilled.iter().map(|it| it.map_or(1, |padded| 1 + usize::from(padded))).sum()
            }
            _ => 1,
        };
        let mut sig = sig.clone();
        sig.params.splice(sret as usize..sret as usize + count, [AbiParam::new(ptr())]);
        sig
    }

    /// Calls `func` with arguments that live at the given addresses.
    fn call_direct(
        &mut self,
        func: FunctionId,
        generic_args: GenericArgs<'db>,
        args: &[(Value, Ty<'db>)],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<()> {
        let db = self.cg.db;
        if Some(func) == self.interner.lang_items().FuturePoll
            && let Some(callee) = self.coroutine_callee(self.normalize(generic_args.type_at(0)))
        {
            return self.emit_call(callee, args, dst, dst_ty);
        }
        if is_dyn_method(self.interner, self.env.param_env, func, generic_args).is_some() {
            let Some((&(receiver, receiver_ty), rest)) = args.split_first() else {
                unsupported!("vtable call without a receiver");
            };
            let dyn_ty = generic_args.type_at(0);
            return self.call_virtual(func, dyn_ty, receiver, receiver_ty, rest, dst, dst_ty);
        }
        let (callee, callee_args) = db.lookup_impl_method(self.env, func, generic_args);
        let either::Either::Left(callee) = callee else {
            unsupported!("call to builtin derive method");
        };
        if callee == func
            && let ItemContainerId::TraitId(trait_) = func.lookup(db).container
            && Some(trait_) == self.interner.lang_items().Clone
            && FunctionSignature::of(db, func).name.as_str() == "clone"
            && let &[(receiver, _)] = args
        {
            let src = self.builder.ins().load(ptr(), MEM, receiver, 0);
            return self.clone_builtin(func, src, dst, dst_ty);
        }
        // `FnPtr::addr` is the compiler's to supply as well: a function pointer is its address.
        if callee == func
            && let ItemContainerId::TraitId(trait_) = func.lookup(db).container
            && Some(trait_) == self.interner.lang_items().FnPtrTrait
            && let &[(receiver, _)] = args
        {
            let addr = self.builder.ins().load(ptr(), MEM, receiver, 0);
            self.builder.ins().store(MEM, addr, dst, 0);
            return Ok(());
        }
        self.emit_call(Callee::Instance(InstanceDef::Fn(callee), callee_args), args, dst, dst_ty)
    }

    /// `Clone::clone` of a type whose impl the compiler supplies: tuples and arrays clone their
    /// parts, and what is left is `Copy`.
    fn clone_builtin(
        &mut self,
        clone: FunctionId,
        src: Value,
        dst: Value,
        ty: Ty<'db>,
    ) -> Result<()> {
        let layout = self.layout(ty)?;
        let clone_part = |this: &mut Self, src: Value, dst: Value, part: Ty<'db>| {
            let reference = Ty::new_ref(
                this.interner,
                Region::new_erased(this.interner),
                part,
                Mutability::Not,
            );
            let slot = this.scratch();
            this.builder.ins().store(MEM, src, slot, 0);
            let args = GenericArgs::new_from_slice(&[part.into()]);
            this.call_direct(clone, args, &[(slot, reference)], dst, part)
        };
        // A closure clones as the tuple of what it captured.
        let fields = match ty.kind() {
            TyKind::Tuple(fields) => Some(fields),
            TyKind::Closure(..) | TyKind::CoroutineClosure(..) => {
                match ty.closure_upvars_ty().map(|it| it.kind()) {
                    Some(TyKind::Tuple(fields)) => Some(fields),
                    _ => unsupported!("builtin `Clone` of a closure without tupled upvars"),
                }
            }
            _ => None,
        };
        match ty.kind() {
            _ if let Some(fields) = fields => {
                for (i, field) in fields.iter().enumerate() {
                    let offset = layout.fields.offset(i).bytes() as i64;
                    let src = self.builder.ins().iadd_imm(src, offset);
                    let part = self.builder.ins().iadd_imm(dst, offset);
                    self.guarded(
                        |this| clone_part(this, src, part, field),
                        |this| {
                            for (done, field) in fields.iter().enumerate().take(i) {
                                let offset = layout.fields.offset(done).bytes() as i64;
                                let done = this.builder.ins().iadd_imm(dst, offset);
                                this.drop_value(done, None, field)?;
                            }
                            Ok(())
                        },
                    )?;
                }
            }
            TyKind::Array(element, _) => {
                let size = self.layout(element)?.size.bytes() as i64;
                let len = self.builder.ins().iconst(ptr(), layout.fields.count() as i64);
                let header = self.builder.create_block();
                let body = self.builder.create_block();
                let done = self.builder.create_block();
                self.builder.append_block_param(header, ptr());
                let zero = self.builder.ins().iconst(ptr(), 0);
                self.builder.ins().jump(header, &[zero.into()]);
                self.builder.switch_to_block(header);
                let index = self.builder.block_params(header)[0];
                let more = self.builder.ins().icmp(IntCC::UnsignedLessThan, index, len);
                self.builder.ins().brif(more, body, &[], done, &[]);
                self.builder.switch_to_block(body);
                let offset = self.builder.ins().imul_imm(index, size);
                let src = self.builder.ins().iadd(src, offset);
                let part = self.builder.ins().iadd(dst, offset);
                self.guarded(
                    |this| clone_part(this, src, part, element),
                    |this| this.drop_elements(dst, index, element),
                )?;
                let next = self.builder.ins().iadd_imm(index, 1);
                self.builder.ins().jump(header, &[next.into()]);
                self.builder.switch_to_block(done);
            }
            TyKind::FnDef(..)
            | TyKind::FnPtr(..)
            | TyKind::Never
            | TyKind::RawPtr(..)
            | TyKind::Ref(_, _, Mutability::Not) => self.copy(dst, src, layout.size),
            _ => unsupported!("builtin `Clone` of {ty:?}"),
        }
        Ok(())
    }

    /// The body of a callee that is small and of the standard library, built to be put where
    /// it is called instead of compiled as a function of its own. rustc gets such a function
    /// with its helpers already inlined into it, and so compiles far fewer of them.
    fn inlined_callee(
        &mut self,
        def: InstanceDef,
        args: GenericArgs<'db>,
        sig: &Signature,
    ) -> Option<ir::FuncRef> {
        /// `RA_JIT_INLINE=0` calls every function.
        static ENABLED: LazyLock<bool> =
            LazyLock::new(|| std::env::var_os("RA_JIT_INLINE").is_none_or(|it| it != "0"));
        let db = self.cg.db;
        if !*ENABLED || !self.cg.target.exception_in_register() || Target::current().is_wasm() {
            return None;
        }
        let erased = |args: GenericArgs<'db>| {
            let interner = DbInterner::new_with(db, self.env.krate);
            rustc_type_ir::fold_regions(interner, args, |region, _| match region.kind() {
                rustc_type_ir::RegionKind::ReBound(..) => region,
                _ => Region::new_erased(interner),
            })
        };
        // A closure is built in by a compile at a level above 1 alone.
        if let InstanceDef::Closure(_) = def {
            let instance = Instance { def, args: erased(args).store(), checked: self.cg.checked };
            let limit = tier::inline_limit(db, &instance)?;
            let body = tier::hot_body(db, &instance, &self.env.store(), limit, false)?;
            return (body.func.signature == *sig)
                .then(|| self.cg.declare_inlined(&body, self.builder.func));
        }
        let InstanceDef::Fn(func) = def else { return None };
        let signature = FunctionSignature::of(db, func);
        if signature.abi != ExternAbi::Rust
            || signature.is_varargs()
            || FunctionSignature::is_intrinsic(db, func)
        {
            return None;
        }
        let args = erased(args);
        let instance = Instance { def, args: args.store(), checked: self.cg.checked };
        // A compile at a level above 1 builds in what it calls that is hot or small, of any
        // crate, as rustc's optimized builds do.
        if let Some(limit) = tier::inline_limit(db, &instance)
            && let Some(body) =
                tier::hot_body(db, &instance, &self.env.store(), limit, track_caller(db, func))
            && body.func.signature == *sig
        {
            return Some(self.cg.declare_inlined(&body, self.builder.func));
        }
        if track_caller(db, func) {
            return None;
        }
        if !func.krate(db).data(db).origin.is_lang() {
            return None;
        }
        // A function that is neither generic nor `#[inline]` is in the library as rustc
        // compiled it, and calling that one is less work than building it again.
        let flags = AttrFlags::query(db, func.into());
        if !flags.contains(AttrFlags::IS_INLINE) && args.iter().all(|it| it.region().is_some()) {
            return None;
        }
        let body = inlinable(db, instance, self.env.store()).as_ref()?;
        if body.func.signature != *sig {
            return None;
        }
        Some(self.cg.declare_inlined(body, self.builder.func))
    }

    /// Shows LLVM the body of the callee `id` a compile at a level above 1 calls, where
    /// `tier::seen_body` has one.
    fn show_callee(
        &mut self,
        def: InstanceDef,
        args: GenericArgs<'db>,
        sig: &Signature,
        id: FuncId,
    ) {
        let db = self.cg.db;
        let tracked = match def {
            InstanceDef::Fn(func) => {
                let signature = FunctionSignature::of(db, func);
                if signature.abi != ExternAbi::Rust
                    || signature.is_varargs()
                    || FunctionSignature::is_intrinsic(db, func)
                {
                    return;
                }
                track_caller(db, func)
            }
            InstanceDef::Closure(_) | InstanceDef::DropGlue => false,
            _ => return,
        };
        let interner = DbInterner::new_with(db, self.env.krate);
        let args = rustc_type_ir::fold_regions(interner, args, |region, _| match region.kind() {
            rustc_type_ir::RegionKind::ReBound(..) => region,
            _ => Region::new_erased(interner),
        });
        let instance = Instance { def, args: args.store(), checked: self.cg.checked };
        if let Some((body, never)) = tier::seen_body(db, &instance, &self.env.store(), tracked) {
            if body.func.signature == *sig {
                self.cg.declare_seen(id, &body, never);
            } else if std::env::var_os("RA_JIT_DUMP_TIER").is_some() {
                eprintln!("tier: signature differs: {} vs {}", body.func.signature, sig);
            }
        }
    }

    /// Calls `callee` with `values`, which are what `sig` says, and stores the result in `dst`.
    fn call_as(
        &mut self,
        callee: Callee<'db>,
        sig: Signature,
        values: &[Value],
        ret_mode: PassMode,
        dst: Value,
        ret_layout: &Layout,
    ) -> Result<()> {
        let results = match callee {
            Callee::Instance(def, generic_args) => {
                let callee = match self.inlined_callee(def, generic_args, &sig) {
                    Some(callee) => callee,
                    None => {
                        let callee = self.cg.callee(def, generic_args, self.env, &sig)?;
                        self.show_callee(def, generic_args, &sig, callee);
                        self.cg.declare_func_in_func(callee, self.builder.func)
                    }
                };
                self.invoke(callee, values)
            }
            Callee::Pointer(pointer, ..) => {
                let sig = self.builder.import_signature(sig);
                self.invoke_indirect(sig, pointer, values)
            }
        };
        self.receive(ret_mode, &mut results.into_iter(), dst, ret_layout);
        Ok(())
    }

    /// Passes the arguments, which live at the given addresses, the way the Rust ABI wants them
    /// and stores the result in `dst`.
    fn emit_call(
        &mut self,
        callee: Callee<'db>,
        args: &[(Value, Ty<'db>)],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<()> {
        let ret_layout = self.layout(dst_ty)?;
        let abi = match callee {
            Callee::Instance(InstanceDef::Fn(func), _) => {
                FunctionSignature::of(self.cg.db, func).abi
            }
            Callee::Instance(..) => ExternAbi::Rust,
            Callee::Pointer(_, abi, _) => abi,
        };
        let FnAbi { ret: ret_mode, args: arg_modes } =
            self.fn_abi(abi, dst_ty, args.iter().map(|&(_, ty)| ty))?;
        let mut values = Vec::new();
        if let PassMode::Indirect = ret_mode {
            values.push(dst);
        }
        // The calls before this one have returned.
        self.free_argument_slots.append(&mut self.argument_slots);
        for (&(addr, ty), &mode) in args.iter().zip(&arg_modes) {
            let layout = self.layout(ty)?;
            self.pass(mode, addr, &layout, &mut values);
        }
        let mut sig = self.signature(&arg_modes, ret_mode);
        if let Callee::Instance(InstanceDef::Fn(func), _) = callee
            && track_caller(self.cg.db, func)
        {
            sig.params.push(AbiParam::new(ptr()));
            values.push(self.caller_location()?);
        }
        // How many of the arguments are for named parameters, when the rest are for `...`.
        let named = match callee {
            Callee::Instance(InstanceDef::Fn(func), _) => {
                let signature = FunctionSignature::of(self.cg.db, func);
                signature.is_varargs().then(|| signature.params.len())
            }
            Callee::Instance(..) => None,
            Callee::Pointer(_, _, named) => named,
        };
        let Some(named) = named else {
            return self.call_as(callee, sig, &values, ret_mode, dst, &ret_layout);
        };
        if Target::current().is_wasm() {
            // The arguments after the named ones are written to memory, each at its
            // own alignment, and the function is given where.
            if arg_modes.iter().any(|mode| !matches!(mode, PassMode::Direct(_))) {
                unsupported!("variadic call with an argument that is not one scalar");
            }
            let rest = values.split_off(named);
            sig.params.truncate(named);
            sig.params.push(AbiParam::new(ptr()));
            let mut offsets = Vec::with_capacity(rest.len());
            let mut size = 0u32;
            for &value in &rest {
                let bytes = self.builder.func.dfg.value_type(value).bytes();
                size = size.next_multiple_of(bytes);
                offsets.push(size);
                size += bytes;
            }
            let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                size.max(1),
                4,
            ));
            let buffer = self.builder.ins().stack_addr(ptr(), slot, 0);
            for (value, offset) in rest.into_iter().zip(offsets) {
                self.builder.ins().store(MEM, value, buffer, offset as i32);
            }
            values.push(buffer);
            return self.call_as(callee, sig, &values, ret_mode, dst, &ret_layout);
        }
        // Each call of a variadic function has a signature of its own, and the
        // function is declared with the one of its named parameters.
        let named = arg_modes[..named]
            .iter()
            .map(|&mode| {
                let mut params = Vec::new();
                self.push_abi_params(mode, &mut params);
                params.len()
            })
            .sum::<usize>()
            + usize::from(matches!(ret_mode, PassMode::Indirect));
        let mut pointer = match callee {
            Callee::Instance(def, generic_args) => {
                let mut declared = sig.clone();
                declared.params.truncate(named);
                if let InstanceDef::Fn(func) = def {
                    self.declare_variadic(func, &mut declared)?;
                }
                let callee = self.cg.callee(def, generic_args, self.env, &declared)?;
                let callee = self.cg.declare_func_address_in_func(callee, self.builder.func);
                self.builder.ins().func_addr(ptr(), callee)
            }
            Callee::Pointer(pointer, ..) => pointer,
        };
        let floats = sig.params.iter().any(|param| param.value_type.is_float());
        match (self.cg.target.call_conv, self.cg.target.arch) {
            (isa::CallConv::AppleAarch64, _) => self.apple_variadic(&mut sig, &mut values, named),
            (isa::CallConv::SystemV, Arch::X86_64) if floats => {
                pointer = self.counted_variadic(&mut sig, &mut values, pointer)?;
            }
            (isa::CallConv::WindowsFastcall, _) if floats => {
                // The callee reads its variable arguments from where it saved the
                // integer registers, so a float goes in one of those.
                for (param, value) in sig.params[named..].iter_mut().zip(&mut values[named..]) {
                    if param.value_type == types::F64 {
                        let flags = MemFlagsData::new();
                        *value = self.builder.ins().bitcast(types::I64, flags, *value);
                        *param = AbiParam::new(types::I64);
                    }
                }
            }
            // The variable arguments are passed as named ones are.
            (isa::CallConv::SystemV, Arch::Aarch64) => (),
            _ if floats => unsupported!("float passed to a variadic function"),
            _ => (),
        }
        let sig = self.builder.import_signature(sig);
        let results = self.invoke_indirect(sig, pointer, &values);
        self.receive(ret_mode, &mut results.into_iter(), dst, &ret_layout);
        Ok(())
    }

    /// Makes a call of the variadic function at `callee` say how many vector registers hold
    /// arguments, as the System V convention of x86-64 has it say in `al`. Cranelift has no
    /// way to, so the call goes through a function that says it, which takes the address of
    /// the callee as one more argument after the others. Returns its address.
    fn counted_variadic(
        &mut self,
        sig: &mut Signature,
        values: &mut Vec<Value>,
        callee: Value,
    ) -> Result<Value> {
        let (mut ints, mut floats, mut stack) = (0, 0, 0);
        for param in &sig.params {
            match param.purpose {
                ArgumentPurpose::Normal | ArgumentPurpose::StructReturn => (),
                _ => unsupported!("an argument in memory with a float to a variadic function"),
            }
            match param.value_type {
                ty if ty.is_float() && floats < 8 => floats += 1,
                ty if ty.is_float() => stack += 1,
                ty if ty.is_int() && ty.bytes() <= 8 && ints < 6 => ints += 1,
                ty if ty.is_int() && ty.bytes() <= 8 => stack += 1,
                _ => unsupported!("a float and a wide argument to a variadic function"),
            }
        }
        sig.params.push(AbiParam::new(ptr()));
        values.push(callee);
        let through = self.cg.assembly(asm::counted_variadic(ints, stack), sig);
        let through = self.cg.declare_func_address_in_func(through, self.builder.func);
        Ok(self.builder.ins().func_addr(ptr(), through))
    }

    /// Makes a call pass what follows its first `named` parameters the way Apple passes the
    /// variable arguments of a C function on AArch64: each in eight bytes of stack, whatever
    /// registers are free. Cranelift knows no such thing, so arguments that carry nothing
    /// take the registers that are left.
    fn apple_variadic(&mut self, sig: &mut Signature, values: &mut Vec<Value>, named: usize) {
        let (mut ints, mut floats) = (8usize, 8usize);
        for param in &sig.params[..named] {
            match param.purpose {
                ArgumentPurpose::StructReturn => (),
                _ if param.value_type.is_float() => floats = floats.saturating_sub(1),
                _ => ints = ints.saturating_sub(param.value_type.bytes().div_ceil(8) as usize),
            }
        }
        for (param, value) in sig.params[named..].iter_mut().zip(&mut values[named..]) {
            if param.value_type.is_int() && param.value_type.bits() < 64 {
                *value = match param.extension {
                    ir::ArgumentExtension::Sext => self.builder.ins().sextend(types::I64, *value),
                    _ => self.builder.ins().uextend(types::I64, *value),
                };
                *param = AbiParam::new(types::I64);
            }
        }
        let zero = self.builder.ins().iconst(types::I64, 0);
        let zero_float = self.builder.ins().f64const(0.0);
        sig.params.splice(
            named..named,
            std::iter::repeat_n(AbiParam::new(types::I64), ints)
                .chain(std::iter::repeat_n(AbiParam::new(types::F64), floats)),
        );
        values.splice(
            named..named,
            std::iter::repeat_n(zero, ints).chain(std::iter::repeat_n(zero_float, floats)),
        );
    }

    fn intrinsic(
        &mut self,
        func: FunctionId,
        generic_args: GenericArgs<'db>,
        args: &[Operand],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<Intrinsic> {
        let name = FunctionSignature::of(self.cg.db, func).name.as_str();
        match (name, args) {
            (
                "wrapping_add" | "unchecked_add" | "wrapping_sub" | "unchecked_sub"
                | "wrapping_mul" | "unchecked_mul" | "unchecked_div" | "exact_div"
                | "unchecked_rem" | "unchecked_shl" | "unchecked_shr" | "rotate_left"
                | "rotate_right" | "saturating_add" | "saturating_sub" | "three_way_compare",
                [lhs, rhs],
            ) => {
                let (l, scalar) = self.load_scalar(lhs)?;
                let (r, r_scalar) = self.load_scalar(rhs)?;
                let signed = scalar.signed;
                let r = match name {
                    "unchecked_shl" | "unchecked_shr" => self.shift_amount_word(r).0,
                    _ => r,
                };
                let value = match name {
                    "wrapping_add" | "unchecked_add" => self.builder.ins().iadd(l, r),
                    "wrapping_sub" | "unchecked_sub" => self.builder.ins().isub(l, r),
                    "wrapping_mul" | "unchecked_mul" => self.builder.ins().imul(l, r),
                    "unchecked_div" | "exact_div" => self.divide(l, r, true, signed)?,
                    "unchecked_rem" => self.divide(l, r, false, signed)?,
                    "unchecked_shl" => self.builder.ins().ishl(l, r),
                    "unchecked_shr" if signed => self.builder.ins().sshr(l, r),
                    "unchecked_shr" => self.builder.ins().ushr(l, r),
                    "rotate_left" => self.builder.ins().rotl(l, r),
                    "rotate_right" => self.builder.ins().rotr(l, r),
                    "three_way_compare" => {
                        let (gt, lt) = if signed {
                            (IntCC::SignedGreaterThan, IntCC::SignedLessThan)
                        } else {
                            (IntCC::UnsignedGreaterThan, IntCC::UnsignedLessThan)
                        };
                        let gt = self.builder.ins().icmp(gt, l, r);
                        let lt = self.builder.ins().icmp(lt, l, r);
                        self.builder.ins().isub(gt, lt)
                    }
                    _ => self.saturating(name == "saturating_add", l, r, scalar),
                };
                let _ = r_scalar;
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("add_with_overflow" | "sub_with_overflow" | "mul_with_overflow", [lhs, rhs]) => {
                let (l, scalar) = self.load_scalar(lhs)?;
                let (r, _) = self.load_scalar(rhs)?;
                let wide = name == "mul_with_overflow" && scalar.ty == types::I128;
                let ins = self.builder.ins();
                let (value, overflow) = match (name, scalar.signed) {
                    (_, signed) if wide => self.wide_mul_overflow(l, r, signed)?,
                    ("add_with_overflow", true) => ins.sadd_overflow(l, r),
                    ("add_with_overflow", false) => ins.uadd_overflow(l, r),
                    ("sub_with_overflow", true) => ins.ssub_overflow(l, r),
                    ("sub_with_overflow", false) => ins.usub_overflow(l, r),
                    (_, true) => ins.smul_overflow(l, r),
                    (_, false) => ins.umul_overflow(l, r),
                };
                let layout = self.layout(dst_ty)?;
                self.builder.ins().store(MEM, value, dst, layout.fields.offset(0).bytes() as i32);
                self.builder.ins().store(
                    MEM,
                    overflow,
                    dst,
                    layout.fields.offset(1).bytes() as i32,
                );
            }
            (
                "ctpop" | "ctlz" | "ctlz_nonzero" | "cttz" | "cttz_nonzero" | "bswap"
                | "bitreverse",
                [arg],
            ) => {
                let (value, from) = self.load_scalar(arg)?;
                let value = match name {
                    "ctpop" => self.builder.ins().popcnt(value),
                    "ctlz" | "ctlz_nonzero" => self.builder.ins().clz(value),
                    "cttz" | "cttz_nonzero" => self.builder.ins().ctz(value),
                    "bswap" if from.ty == types::I8 => value,
                    "bswap" => self.builder.ins().bswap(value),
                    _ => self.builder.ins().bitrev(value),
                };
                let to = self.scalar_of(dst_ty)?;
                let value = self.int_cast(value, AbiScalar { signed: false, ..from }, to);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("size_of" | "align_of" | "min_align_of" | "pref_align_of", []) => {
                let layout = self.layout(generic_args.type_at(0))?;
                let bytes =
                    if name == "size_of" { layout.size.bytes() } else { layout.align.abi.bytes() };
                let value = self.builder.ins().iconst(ptr(), bytes as i64);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("type_name", []) => {
                let ty = generic_args.type_at(0);
                let Some(name) = super::type_name::rustc_type_name(self.cg.db, ty) else {
                    unsupported!("`type_name` of `{ty:?}`");
                };
                let len = self.builder.ins().iconst(ptr(), name.len() as i64);
                let data = self.cg.define_const(ConstBlob {
                    bytes: name.into_bytes(),
                    relocs: Vec::new(),
                    fn_relocs: Vec::new(),
                    align: 1,
                })?;
                let data = self.cg.declare_data_in_func(data, self.builder.func);
                let ptr = self.builder.ins().symbol_value(ptr(), data);
                self.builder.ins().store(MEM, ptr, dst, 0);
                self.builder.ins().store(MEM, len, dst, meta_at());
            }
            ("needs_drop", []) => {
                let ty = generic_args.type_at(0);
                let needs = self.glue_of(ty) != DropGlue::None;
                let value = self.builder.ins().iconst(types::I8, i64::from(needs));
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("ub_checks" | "contract_checks" | "is_val_statically_known", _) => {
                let value = self.builder.ins().iconst(types::I8, 0);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("overflow_checks", []) => {
                let value = self.builder.ins().iconst(types::I8, i64::from(self.cg.checked));
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("va_arg", [list])
                if matches!(
                    self.cg.target.variable_arguments(),
                    Some(VariableArguments::Homed | VariableArguments::Stack)
                ) =>
            {
                let size = self.layout(dst_ty)?.size;
                let bytes = size.bytes() as i64;
                let homed = self.cg.target.variable_arguments() == Some(VariableArguments::Homed);
                // Windows has what does not fill a register exactly behind an address, which
                // is what the caller passed: a copy of its own.
                let by_reference = homed && !matches!(bytes, 1 | 2 | 4 | 8);
                // Apple has a scalar wider than eight bytes at a multiple of its size.
                let aligned = !homed && bytes > 8;
                if aligned {
                    self.scalar_of(dst_ty)?;
                }
                // The list is the address of the next argument, each of them in eight bytes,
                // or in as many as it is wide.
                let (list, _) = self.load_scalar(list)?;
                let mut slot = self.builder.ins().load(ptr(), MEM, list, 0);
                if aligned {
                    slot = self.builder.ins().iadd_imm(slot, bytes - 1);
                    slot = self.builder.ins().band_imm(slot, -bytes);
                }
                let step = if homed { 8 } else { (bytes + 7) & !7 };
                let next = self.builder.ins().iadd_imm(slot, step);
                self.builder.ins().store(MEM, next, list, 0);
                let from = match by_reference {
                    true => self.builder.ins().load(ptr(), MEM, slot, 0),
                    false => slot,
                };
                self.copy(dst, from, size);
            }
            ("va_arg", [list]) => {
                if self.cg.target.variable_arguments() != Some(VariableArguments::Registers) {
                    unsupported!("intrinsic `va_arg` for this target");
                }
                let arch = self.cg.target.arch;
                let scalar = self.scalar_of(dst_ty)?;
                // A 128-bit integer is in two registers that start at an even one, or in
                // sixteen bytes of the stack at a multiple of sixteen.
                let wide = scalar.ty == types::I128 && arch == Arch::Aarch64;
                if scalar.ty.bytes() > 8 && !wide {
                    unsupported!("`va_arg` of {dst_ty:?}");
                }
                let float = scalar.ty.is_float();
                let step = match (float, wide) {
                    (true, _) => VariadicRegisters::FLOAT_BYTES,
                    (false, true) => 16,
                    (false, false) => 8,
                };
                let (list, _) = self.load_scalar(list)?;
                let [saved, stack, done] = [(); 3].map(|()| self.builder.create_block());
                self.builder.append_block_param(done, ptr());
                // An argument is in the registers the function saved while its kind of
                // register lasts, and on the stack after that.
                let stack_at = if arch == Arch::Aarch64 {
                    // The list counts up to zero, from the end of what was saved.
                    let (at, end) = if float { (28, 16) } else { (24, 8) };
                    let offset = self.builder.ins().load(types::I32, MEM, list, at);
                    let some = self.builder.create_block();
                    let none =
                        self.builder.ins().icmp_imm(IntCC::SignedGreaterThanOrEqual, offset, 0);
                    self.builder.ins().brif(none, stack, &[], some, &[]);

                    // What does not fit in the registers left takes them out of use.
                    self.builder.switch_to_block(some);
                    let offset = match wide {
                        true => {
                            let up = self.builder.ins().iadd_imm(offset, 15);
                            self.builder.ins().band_imm(up, -16)
                        }
                        false => offset,
                    };
                    let next = self.builder.ins().iadd_imm(offset, i64::from(step));
                    self.builder.ins().store(MEM, next, list, at);
                    let fits = self.builder.ins().icmp_imm(IntCC::SignedLessThanOrEqual, next, 0);
                    self.builder.ins().brif(fits, saved, &[], stack, &[]);

                    self.builder.switch_to_block(saved);
                    let area = self.builder.ins().load(ptr(), MEM, list, end);
                    let far = self.builder.ins().sextend(ptr(), offset);
                    let from = self.builder.ins().iadd(area, far);
                    self.builder.ins().jump(done, &[from.into()]);
                    0
                } else {
                    let ints = VariadicRegisters::int_count(arch) * 8;
                    let (at, end) = match float {
                        false => (0, ints),
                        true => {
                            (4, ints + VariadicRegisters::FLOATS * VariadicRegisters::FLOAT_BYTES)
                        }
                    };
                    let offset = self.builder.ins().load(types::I32, MEM, list, at);
                    let fits = self.builder.ins().icmp_imm(
                        IntCC::UnsignedLessThanOrEqual,
                        offset,
                        i64::from(end - step),
                    );
                    self.builder.ins().brif(fits, saved, &[], stack, &[]);

                    self.builder.switch_to_block(saved);
                    let area = self.builder.ins().load(ptr(), MEM, list, 16);
                    let wide = self.builder.ins().uextend(ptr(), offset);
                    let from = self.builder.ins().iadd(area, wide);
                    let next = self.builder.ins().iadd_imm(offset, i64::from(step));
                    self.builder.ins().store(MEM, next, list, at);
                    self.builder.ins().jump(done, &[from.into()]);
                    8
                };

                self.builder.switch_to_block(stack);
                let from = self.builder.ins().load(ptr(), MEM, list, stack_at);
                let from = match wide {
                    true => {
                        let up = self.builder.ins().iadd_imm(from, 15);
                        self.builder.ins().band_imm(up, -16)
                    }
                    false => from,
                };
                let next = self.builder.ins().iadd_imm(from, if wide { 16 } else { 8 });
                self.builder.ins().store(MEM, next, list, stack_at);
                self.builder.ins().jump(done, &[from.into()]);

                self.builder.switch_to_block(done);
                let from = self.builder.block_params(done)[0];
                let value = self.builder.ins().load(scalar.ty, MEM, from, 0);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            // Out of range is undefined, so `as`'s saturating cast is one of its meanings.
            ("float_to_int_unchecked", [arg]) => {
                let (value, from) = self.load_scalar(arg)?;
                let to = self.scalar_of(dst_ty)?;
                let value = self.float_cast(value, from, to)?;
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("transmute" | "transmute_unchecked" | "black_box" | "likely" | "unlikely", [arg]) => {
                self.cg.opaque |= name == "black_box";
                let (src, ty) = self.operand(arg)?;
                let size = self.layout(ty)?.size;
                if size != self.layout(dst_ty)?.size {
                    unsupported!("transmute between differently sized types");
                }
                self.copy(dst, src, size);
            }
            // Cranelift's atomics are always sequentially consistent, the strongest ordering.
            ("atomic_load", [ptr]) => {
                let scalar = self.scalar_of(dst_ty)?;
                let (ptr, _) = self.load_scalar(ptr)?;
                let value = self.builder.ins().atomic_load(scalar.ty, MEM, ptr);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("atomic_store", [ptr, value]) => {
                let (ptr, _) = self.load_scalar(ptr)?;
                let (value, _) = self.load_scalar(value)?;
                self.builder.ins().atomic_store(MEM, value, ptr);
            }
            (
                "atomic_xchg" | "atomic_xadd" | "atomic_xsub" | "atomic_and" | "atomic_nand"
                | "atomic_or" | "atomic_xor" | "atomic_max" | "atomic_min" | "atomic_umax"
                | "atomic_umin",
                [ptr, value],
            ) => {
                let op = match name {
                    "atomic_xchg" => AtomicRmwOp::Xchg,
                    "atomic_xadd" => AtomicRmwOp::Add,
                    "atomic_xsub" => AtomicRmwOp::Sub,
                    "atomic_and" => AtomicRmwOp::And,
                    "atomic_nand" => AtomicRmwOp::Nand,
                    "atomic_or" => AtomicRmwOp::Or,
                    "atomic_xor" => AtomicRmwOp::Xor,
                    "atomic_max" => AtomicRmwOp::Smax,
                    "atomic_min" => AtomicRmwOp::Smin,
                    "atomic_umax" => AtomicRmwOp::Umax,
                    _ => AtomicRmwOp::Umin,
                };
                let scalar = self.scalar_of(dst_ty)?;
                let (ptr, _) = self.load_scalar(ptr)?;
                let (value, _) = self.load_scalar(value)?;
                let old = self.builder.ins().atomic_rmw(scalar.ty, MEM, op, ptr, value);
                self.builder.ins().store(MEM, old, dst, 0);
            }
            ("atomic_cxchg" | "atomic_cxchgweak", [ptr, expected, new]) => {
                let (ptr, _) = self.load_scalar(ptr)?;
                let (expected, _) = self.load_scalar(expected)?;
                let (new, _) = self.load_scalar(new)?;
                let old = self.builder.ins().atomic_cas(MEM, ptr, expected, new);
                let swapped = self.builder.ins().icmp(IntCC::Equal, old, expected);
                let layout = self.layout(dst_ty)?;
                self.builder.ins().store(MEM, old, dst, layout.fields.offset(0).bytes() as i32);
                self.builder.ins().store(MEM, swapped, dst, layout.fields.offset(1).bytes() as i32);
            }
            ("atomic_fence" | "atomic_singlethreadfence", []) => {
                self.builder.ins().fence();
            }
            ("read_via_copy" | "volatile_load" | "unaligned_volatile_load", [ptr]) => {
                let (ptr, _) = self.load_scalar(ptr)?;
                let size = self.layout(dst_ty)?.size;
                self.copy(dst, ptr, size);
            }
            ("write_via_move" | "volatile_store" | "unaligned_volatile_store", [ptr, value]) => {
                let (ptr, _) = self.load_scalar(ptr)?;
                let (src, ty) = self.operand(value)?;
                let size = self.layout(ty)?.size;
                self.copy(ptr, src, size);
            }
            ("write_box_via_move", [boxed, value]) => {
                let (boxed, boxed_ty) = self.operand(boxed)?;
                let (src, ty) = self.operand(value)?;
                let size = self.layout(ty)?.size;
                let ptr = self.builder.ins().load(ptr(), MEM, boxed, 0);
                self.copy(ptr, src, size);
                let size = self.layout(boxed_ty)?.size;
                self.copy(dst, boxed, size);
            }
            ("offset" | "arith_offset", [ptr, count]) => {
                let Some(pointee) = self.operand_ty(ptr)?.builtin_deref(true) else {
                    unsupported!("offset of a non-pointer");
                };
                let size = self.layout(pointee)?.size.bytes() as i64;
                let (ptr, _) = self.load_scalar(ptr)?;
                let (count, _) = self.load_scalar(count)?;
                let bytes = self.builder.ins().imul_imm(count, size);
                let value = self.builder.ins().iadd(ptr, bytes);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("ptr_mask", [ptr, mask]) => {
                let (ptr, _) = self.load_scalar(ptr)?;
                let (mask, _) = self.load_scalar(mask)?;
                let value = self.builder.ins().band(ptr, mask);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("ptr_offset_from" | "ptr_offset_from_unsigned", [a, b]) => {
                let size = self.layout(generic_args.type_at(0))?.size.bytes() as i64;
                let (a, _) = self.load_scalar(a)?;
                let (b, _) = self.load_scalar(b)?;
                let bytes = self.builder.ins().isub(a, b);
                let size = self.builder.ins().iconst(ptr(), size);
                let value = self.builder.ins().sdiv(bytes, size);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("copy" | "copy_nonoverlapping" | "write_bytes", [a, b, count]) => {
                let size = self.layout(generic_args.type_at(0))?.size.bytes() as i64;
                let (a, _) = self.load_scalar(a)?;
                let (b, _) = self.load_scalar(b)?;
                let (count, _) = self.load_scalar(count)?;
                let bytes = self.builder.ins().imul_imm(count, size);
                let config = self.cg.target_config();
                match name {
                    // `copy(src, dst, count)`
                    "copy" => self.builder.call_memmove(config, b, a, bytes),
                    "copy_nonoverlapping" => self.builder.call_memcpy(config, b, a, bytes),
                    // `write_bytes(dst, value, count)`
                    _ => self.builder.call_memset(config, a, b, bytes),
                }
            }
            (
                "assume"
                | "cold_path"
                | "forget"
                | "assert_inhabited"
                | "assert_zero_valid"
                | "assert_mem_uninitialized_valid",
                _,
            ) => (),
            ("unreachable" | "abort" | "breakpoint", _) => {
                self.builder.ins().trap(TrapCode::user(2).expect("non-zero trap code"));
                return Ok(Intrinsic::Diverges);
            }
            ("raw_eq" | "compare_bytes", [a, b, ..]) => {
                let (a, _) = self.load_scalar(a)?;
                let (b, _) = self.load_scalar(b)?;
                let len = match args {
                    [_, _, len] => self.load_scalar(len)?.0,
                    _ => {
                        let size = self.layout(generic_args.type_at(0))?.size.bytes();
                        self.builder.ins().iconst(ptr(), size as i64)
                    }
                };
                let mut value = self.memcmp(a, b, len)?;
                if name == "raw_eq" {
                    value = self.builder.ins().icmp_imm(IntCC::Equal, value, 0);
                }
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (
                "simd_and"
                | "simd_or"
                | "simd_xor"
                | "simd_add"
                | "simd_sub"
                | "simd_mul"
                | "simd_shl"
                | "simd_shr"
                | "simd_eq"
                | "simd_ne"
                | "simd_lt"
                | "simd_le"
                | "simd_gt"
                | "simd_ge"
                | "simd_div"
                | "simd_saturating_add"
                | "simd_saturating_sub",
                [lhs, rhs],
            ) => {
                let (l, ty) = self.operand(lhs)?;
                let (r, _) = self.operand(rhs)?;
                let (lane, count) = self.simd_lanes(ty)?;
                let (out, _) = self.simd_lanes(dst_ty)?;
                let signed = lane.signed;
                if lane.ty.is_float() {
                    self.simd_float_lanes(name, (l, r), dst, lane, out, count)?;
                    return Ok(Intrinsic::Inline);
                }
                if name == "simd_div" {
                    unsupported!("`simd_div` of integers");
                }
                for i in 0..count {
                    let offset = (i * lane.ty.bytes() as u64) as i32;
                    let a = self.builder.ins().load(lane.ty, MEM, l, offset);
                    let b = self.builder.ins().load(lane.ty, MEM, r, offset);
                    let cc = match (name, signed) {
                        ("simd_eq", _) => Some(IntCC::Equal),
                        ("simd_ne", _) => Some(IntCC::NotEqual),
                        ("simd_lt", true) => Some(IntCC::SignedLessThan),
                        ("simd_lt", false) => Some(IntCC::UnsignedLessThan),
                        ("simd_le", true) => Some(IntCC::SignedLessThanOrEqual),
                        ("simd_le", false) => Some(IntCC::UnsignedLessThanOrEqual),
                        ("simd_gt", true) => Some(IntCC::SignedGreaterThan),
                        ("simd_gt", false) => Some(IntCC::UnsignedGreaterThan),
                        ("simd_ge", true) => Some(IntCC::SignedGreaterThanOrEqual),
                        ("simd_ge", false) => Some(IntCC::UnsignedGreaterThanOrEqual),
                        _ => None,
                    };
                    let value = match (cc, name) {
                        // A true lane is all ones.
                        (Some(cc), _) => {
                            let flag = self.builder.ins().icmp(cc, a, b);
                            let flag = self.builder.ins().uextend(types::I64, flag);
                            let mask = self.builder.ins().ineg(flag);
                            if out.ty.bits() < 64 {
                                self.builder.ins().ireduce(out.ty, mask)
                            } else {
                                mask
                            }
                        }
                        (None, "simd_and") => self.builder.ins().band(a, b),
                        (None, "simd_or") => self.builder.ins().bor(a, b),
                        (None, "simd_xor") => self.builder.ins().bxor(a, b),
                        (None, "simd_add") => self.builder.ins().iadd(a, b),
                        (None, "simd_sub") => self.builder.ins().isub(a, b),
                        (None, "simd_shl") => self.builder.ins().ishl(a, b),
                        (None, "simd_shr") if signed => self.builder.ins().sshr(a, b),
                        (None, "simd_shr") => self.builder.ins().ushr(a, b),
                        (None, "simd_saturating_add") => self.saturating(true, a, b, lane),
                        (None, "simd_saturating_sub") => self.saturating(false, a, b, lane),
                        (None, _) => self.builder.ins().imul(a, b),
                    };
                    self.builder.ins().store(MEM, value, dst, (i * out.ty.bytes() as u64) as i32);
                }
            }
            ("simd_select", [mask, on_true, on_false]) => {
                let (mask, mask_ty) = self.operand(mask)?;
                let (on_true, _) = self.operand(on_true)?;
                let (on_false, _) = self.operand(on_false)?;
                let (mask_lane, count) = self.simd_lanes(mask_ty)?;
                let (lane, _) = self.simd_lanes(dst_ty)?;
                for i in 0..count {
                    let offset = (i * lane.ty.bytes() as u64) as i32;
                    let mask_offset = (i * mask_lane.ty.bytes() as u64) as i32;
                    // The top bit of a mask lane decides.
                    let chosen = self.builder.ins().load(mask_lane.ty, MEM, mask, mask_offset);
                    let chosen = self.builder.ins().icmp_imm(IntCC::SignedLessThan, chosen, 0);
                    let a = self.builder.ins().load(lane.ty, MEM, on_true, offset);
                    let b = self.builder.ins().load(lane.ty, MEM, on_false, offset);
                    let value = self.builder.ins().select(chosen, a, b);
                    self.builder.ins().store(MEM, value, dst, offset);
                }
            }
            ("simd_extract", [vector, index]) => {
                let (vector, ty) = self.operand(vector)?;
                let (index, _) = self.load_scalar(index)?;
                let (lane, _) = self.simd_lanes(ty)?;
                let index = self.builder.ins().uextend(ptr(), index);
                let offset = self.builder.ins().imul_imm(index, lane.ty.bytes() as i64);
                let addr = self.builder.ins().iadd(vector, offset);
                let value = self.builder.ins().load(lane.ty, MEM, addr, 0);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("simd_insert", [vector, index, value]) => {
                let (vector, ty) = self.operand(vector)?;
                let (index, _) = self.load_scalar(index)?;
                let (value, _) = self.load_scalar(value)?;
                let (lane, _) = self.simd_lanes(ty)?;
                let size = self.layout(ty)?.size;
                self.copy(dst, vector, size);
                let index = self.builder.ins().uextend(ptr(), index);
                let offset = self.builder.ins().imul_imm(index, lane.ty.bytes() as i64);
                let addr = self.builder.ins().iadd(dst, offset);
                self.builder.ins().store(MEM, value, addr, 0);
            }
            ("simd_shuffle", [first, second, indices]) => {
                let (first, ty) = self.operand(first)?;
                let (second, _) = self.operand(second)?;
                // `[u32; N]`, or a wrapper of one. Lanes of `second` come after `first`'s.
                let (indices, _) = self.operand(indices)?;
                let layout = self.layout(ty)?;
                let (lane, _) = self.simd_lanes(ty)?;
                let (_, count) = self.simd_lanes(dst_ty)?;
                let lane_size = lane.ty.bytes() as u64;
                let both = self.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    (layout.size.bytes() * 2) as u32,
                    layout.align.abi.bytes().min(16).trailing_zeros() as u8,
                ));
                let both = self.builder.ins().stack_addr(ptr(), both, 0);
                self.copy(both, first, layout.size);
                let upper = self.builder.ins().iadd_imm(both, layout.size.bytes() as i64);
                self.copy(upper, second, layout.size);
                let mut lanes = Vec::new();
                for i in 0..count {
                    let index = self.builder.ins().load(types::I32, MEM, indices, (i * 4) as i32);
                    let index = self.builder.ins().uextend(ptr(), index);
                    let offset = self.builder.ins().imul_imm(index, lane_size as i64);
                    let addr = self.builder.ins().iadd(both, offset);
                    lanes.push(self.builder.ins().load(lane.ty, MEM, addr, 0));
                }
                for (i, value) in lanes.into_iter().enumerate() {
                    self.builder.ins().store(MEM, value, dst, (i as u64 * lane_size) as i32);
                }
            }
            ("simd_splat", [arg]) => {
                let (src, _) = self.operand(arg)?;
                let (lane, count) = self.simd_lanes(dst_ty)?;
                let value = self.builder.ins().load(lane.ty, MEM, src, 0);
                for i in 0..count {
                    let offset = (i * lane.ty.bytes() as u64) as i32;
                    self.builder.ins().store(MEM, value, dst, offset);
                }
            }
            // `simd_cast` leaves a float out of the range of the integer undefined; saturating
            // as `simd_as` does is one of the answers it may give.
            ("simd_cast" | "simd_as", [arg]) => {
                let (src, ty) = self.operand(arg)?;
                let (from, count) = self.simd_lanes(ty)?;
                let (to, _) = self.simd_lanes(dst_ty)?;
                for i in 0..count {
                    let offset = (i * from.ty.bytes() as u64) as i32;
                    let value = self.builder.ins().load(from.ty, MEM, src, offset);
                    let value = if from.ty.is_float() || to.ty.is_float() {
                        self.float_cast(value, from, to)?
                    } else {
                        self.int_cast(value, from, to)
                    };
                    self.builder.ins().store(MEM, value, dst, (i * to.ty.bytes() as u64) as i32);
                }
            }
            ("simd_bitmask", [arg]) => {
                let (src, ty) = self.operand(arg)?;
                let (lane, count) = self.simd_lanes(ty)?;
                let BackendRepr::Scalar(_) = self.layout(dst_ty)?.backend_repr else {
                    unsupported!("simd_bitmask into {dst_ty:?}");
                };
                let to = self.scalar_of(dst_ty)?;
                if count > 64 || lane.ty.bits() > 64 {
                    unsupported!("simd_bitmask of {count} lanes");
                }
                // Bit `i` of the result is the top bit of lane `i`.
                let mut bits = self.builder.ins().iconst(types::I64, 0);
                for i in 0..count {
                    let offset = (i * lane.ty.bytes() as u64) as i32;
                    let value = self.builder.ins().load(lane.ty, MEM, src, offset);
                    let top = self.builder.ins().ushr_imm(value, lane.ty.bits() as i64 - 1);
                    let top = if lane.ty.bits() < 64 {
                        self.builder.ins().uextend(types::I64, top)
                    } else {
                        top
                    };
                    let top = self.builder.ins().ishl_imm(top, i as i64);
                    bits = self.builder.ins().bor(bits, top);
                }
                let from = AbiScalar { ty: types::I64, signed: false };
                let value = self.int_cast(bits, from, to);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("simd_reduce_and" | "simd_reduce_or" | "simd_reduce_xor", [arg]) => {
                let (src, ty) = self.operand(arg)?;
                let (lane, count) = self.simd_lanes(ty)?;
                if lane.ty.is_float() {
                    unsupported!("`{name}` of floats");
                }
                let mut acc = self.builder.ins().load(lane.ty, MEM, src, 0);
                for i in 1..count {
                    let offset = (i * lane.ty.bytes() as u64) as i32;
                    let value = self.builder.ins().load(lane.ty, MEM, src, offset);
                    acc = match name {
                        "simd_reduce_and" => self.builder.ins().band(acc, value),
                        "simd_reduce_or" => self.builder.ins().bor(acc, value),
                        _ => self.builder.ins().bxor(acc, value),
                    };
                }
                self.builder.ins().store(MEM, acc, dst, 0);
            }
            ("simd_reduce_all" | "simd_reduce_any", [arg]) => {
                let (src, ty) = self.operand(arg)?;
                let (lane, count) = self.simd_lanes(ty)?;
                let all = name == "simd_reduce_all";
                let mut acc = self.builder.ins().iconst(types::I8, all as i64);
                for i in 0..count {
                    let offset = (i * lane.ty.bytes() as u64) as i32;
                    let value = self.builder.ins().load(lane.ty, MEM, src, offset);
                    let set = self.builder.ins().icmp_imm(IntCC::NotEqual, value, 0);
                    acc = if all {
                        self.builder.ins().band(acc, set)
                    } else {
                        self.builder.ins().bor(acc, set)
                    };
                }
                self.builder.ins().store(MEM, acc, dst, 0);
            }
            (
                "fabs"
                | "fabsf32"
                | "fabsf64"
                | "sqrtf32"
                | "sqrtf64"
                | "floorf32"
                | "floorf64"
                | "ceilf32"
                | "ceilf64"
                | "truncf32"
                | "truncf64"
                | "round_ties_even_f32"
                | "round_ties_even_f64"
                | "roundf32"
                | "roundf64",
                [arg],
            ) => {
                let (value, scalar) = self.load_scalar(arg)?;
                if !scalar.ty.is_float() {
                    unsupported!("intrinsic `{name}` on {:?}", scalar.ty);
                }
                let op = name.trim_end_matches("f32").trim_end_matches("f64").trim_end_matches('_');
                let value = match op {
                    "fabs" => self.builder.ins().fabs(value),
                    "sqrt" => self.builder.ins().sqrt(value),
                    "floor" => self.builder.ins().floor(value),
                    "ceil" => self.builder.ins().ceil(value),
                    "trunc" => self.builder.ins().trunc(value),
                    "round_ties_even" => self.builder.ins().nearest(value),
                    _ => {
                        let libm = if scalar.ty == types::F32 { "roundf" } else { "round" };
                        self.float_libcall(libm, scalar.ty, &[value])?
                    }
                };
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("powif32" | "powif64", [base, power]) => {
                let (base, scalar) = self.load_scalar(base)?;
                let (power, _) = self.load_scalar(power)?;
                let builtin = if scalar.ty == types::F32 { "__powisf2" } else { "__powidf2" };
                let value =
                    self.builtin(builtin, &[scalar.ty, types::I32], scalar.ty, &[base, power])?;
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (
                "copysignf32" | "copysignf64" | "minnumf32" | "minnumf64" | "maxnumf32"
                | "maxnumf64" | "powf32" | "powf64" | "fadd_fast" | "fsub_fast" | "fmul_fast"
                | "fdiv_fast" | "fadd_algebraic" | "fsub_algebraic" | "fmul_algebraic"
                | "fdiv_algebraic",
                [a, b],
            ) => {
                let (l, scalar) = self.load_scalar(a)?;
                let (r, _) = self.load_scalar(b)?;
                if !scalar.ty.is_float() {
                    unsupported!("intrinsic `{name}` on {:?}", scalar.ty);
                }
                let single = scalar.ty == types::F32;
                let value = match name {
                    "copysignf32" | "copysignf64" => self.builder.ins().fcopysign(l, r),
                    "minnumf32" | "minnumf64" => self.float_libcall(
                        if single { "fminf" } else { "fmin" },
                        scalar.ty,
                        &[l, r],
                    )?,
                    "maxnumf32" | "maxnumf64" => self.float_libcall(
                        if single { "fmaxf" } else { "fmax" },
                        scalar.ty,
                        &[l, r],
                    )?,
                    "powf32" | "powf64" => {
                        self.float_libcall(if single { "powf" } else { "pow" }, scalar.ty, &[l, r])?
                    }
                    "fadd_fast" | "fadd_algebraic" => self.builder.ins().fadd(l, r),
                    "fsub_fast" | "fsub_algebraic" => self.builder.ins().fsub(l, r),
                    "fmul_fast" | "fmul_algebraic" => self.builder.ins().fmul(l, r),
                    _ => self.builder.ins().fdiv(l, r),
                };
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("powif32" | "powif64", [base, power]) => {
                let (base, float) = self.load_scalar(base)?;
                let (power, _) = self.load_scalar(power)?;
                // compiler-builtins multiplies as LLVM's `llvm.powi` is lowered to do.
                let name = if float.ty == types::F32 { "__powisf2" } else { "__powidf2" };
                let value =
                    self.builtin(name, &[float.ty, types::I32], float.ty, &[base, power])?;
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("fmaf32" | "fmaf64" | "fmuladdf32" | "fmuladdf64", [a, b, c]) => {
                let (a, scalar) = self.load_scalar(a)?;
                let (b, _) = self.load_scalar(b)?;
                let (c, _) = self.load_scalar(c)?;
                let value = if name.starts_with("fmuladd") {
                    let product = self.builder.ins().fmul(a, b);
                    self.builder.ins().fadd(product, c)
                } else {
                    let libm = if scalar.ty == types::F32 { "fmaf" } else { "fma" };
                    self.float_libcall(libm, scalar.ty, &[a, b, c])?
                };
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("discriminant_value", [arg]) => {
                // The reference may be a wide one, of a struct that ends in a slice.
                let (reference, _) = self.operand(arg)?;
                let ptr = self.builder.ins().load(ptr(), MEM, reference, 0);
                let to = self.scalar_of(dst_ty)?;
                let value = self.discriminant(ptr, generic_args.type_at(0), to)?;
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("ptr_metadata", [arg]) => {
                let (src, ty) = self.operand(arg)?;
                if let BackendRepr::ScalarPair { .. } = self.layout(ty)?.backend_repr {
                    let meta = self.builder.ins().load(ptr(), MEM, src, meta_at());
                    self.builder.ins().store(MEM, meta, dst, 0);
                }
            }
            ("aggregate_raw_ptr", [data, meta]) => {
                let (data, _) = self.operand(data)?;
                let data = self.builder.ins().load(ptr(), MEM, data, 0);
                self.builder.ins().store(MEM, data, dst, 0);
                if let BackendRepr::ScalarPair { .. } = self.layout(dst_ty)?.backend_repr {
                    let (meta, _) = self.load_scalar(meta)?;
                    self.builder.ins().store(MEM, meta, dst, meta_at());
                }
            }
            ("slice_get_unchecked", [slice, index]) => {
                let size = self.layout(generic_args.type_at(2))?.size.bytes() as i64;
                let (slice, _) = self.operand(slice)?;
                let ptr = self.builder.ins().load(ptr(), MEM, slice, 0);
                let (index, _) = self.load_scalar(index)?;
                let offset = self.builder.ins().imul_imm(index, size);
                let value = self.builder.ins().iadd(ptr, offset);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("vtable_size" | "vtable_align", [vtable]) => {
                let (vtable, _) = self.load_scalar(vtable)?;
                let offset = if name == "vtable_size" { meta_at() } else { 2 * meta_at() };
                let value = self.builder.ins().load(ptr(), MEM, vtable, offset);
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("size_of_val" | "align_of_val", [arg]) => {
                let (src, _) = self.operand(arg)?;
                let ty = generic_args.type_at(0);
                let meta = self
                    .is_unsized(ty)
                    .then(|| self.builder.ins().load(ptr(), MEM, src, meta_at()));
                let (size, align) = self.size_and_align_of_val(ty, meta)?;
                let value = if name == "size_of_val" { size } else { align };
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("catch_unwind", [try_fn, data, catch_fn]) => {
                let (try_fn, _) = self.load_scalar(try_fn)?;
                let (data, _) = self.load_scalar(data)?;
                let (catch_fn, _) = self.load_scalar(catch_fn)?;
                let mut sig = self.cg.make_signature();
                sig.params.push(AbiParam::new(ptr()));
                let try_sig = self.builder.import_signature(sig.clone());
                sig.params.push(AbiParam::new(ptr()));
                let catch_sig = self.builder.import_signature(sig);

                // Whether it unwound is an `i32` or a `bool`, by the toolchain.
                let outcome = self.layout(dst_ty)?.size.bytes();
                let outcome = Type::int_with_byte_size(outcome as u16).ok_or_else(|| {
                    CodegenError::Unsupported(format!("`catch_unwind` giving {outcome} bytes"))
                })?;
                let returned = self.builder.create_block();
                let caught = self.builder.create_block();
                let done = self.builder.create_block();
                self.builder.append_block_param(caught, ptr());
                self.builder.append_block_param(done, outcome);
                if self.cg.target.exception_in_register() {
                    let normal = self.builder.func.dfg.block_call(returned, &[]);
                    let unwound =
                        self.builder.func.dfg.block_call(caught, &[ir::BlockArg::TryCallExn(0)]);
                    let tag = ir::ExceptionTag::with_number(unwind::CATCH).expect("a small tag");
                    let table =
                        self.builder.func.dfg.exception_tables.push(ir::ExceptionTableData::new(
                            try_sig,
                            normal,
                            [ir::ExceptionTableItem::Tag(tag, unwound)],
                        ));
                    self.builder.ins().try_call_indirect(try_fn, &[data], table);
                } else {
                    self.guarded_call(caught, |builder| {
                        builder.ins().call_indirect(try_sig, try_fn, &[data])
                    });
                    self.builder.ins().jump(returned, &[]);
                }

                self.builder.switch_to_block(returned);
                let zero = self.builder.ins().iconst(outcome, 0);
                self.builder.ins().jump(done, &[zero.into()]);

                self.builder.switch_to_block(caught);
                self.builder.set_cold_block(caught);
                let exception = self.builder.block_params(caught)[0];
                if self.cg.target.exception_in_register() {
                    self.invoke_indirect(catch_sig, catch_fn, &[data, exception]);
                } else {
                    // The handler takes the thrown object, and what it was brought in is ours.
                    let object = self.builder.ins().iadd_imm(exception, unwind::seh::OBJECT);
                    self.invoke_indirect(catch_sig, catch_fn, &[data, object]);
                    let mut sig = self.cg.make_signature();
                    sig.params.push(AbiParam::new(ptr()));
                    let release = self.cg.import(unwind::seh::RELEASE, &sig)?;
                    let release = self.cg.declare_func_in_func(release, self.builder.func);
                    self.builder.ins().call(release, &[exception]);
                }
                let one = self.builder.ins().iconst(outcome, 1);
                self.builder.ins().jump(done, &[one.into()]);

                self.builder.switch_to_block(done);
                let result = self.builder.block_params(done)[0];
                self.builder.ins().store(MEM, result, dst, 0);
            }
            ("caller_location", []) => {
                let value = self.caller_location()?;
                self.builder.ins().store(MEM, value, dst, 0);
            }
            _ => return Ok(Intrinsic::Unknown),
        }
        Ok(Intrinsic::Inline)
    }

    /// The signed `wide` lanes of two vectors, each saturated to half its width, signed or
    /// not: every 16 bytes of the result are those of `a` and then those of `b`.
    fn pack_lanes(
        &mut self,
        a: &Operand,
        b: &Operand,
        dst: Value,
        wide: Type,
        signed: bool,
    ) -> Result<()> {
        let (a, ty) = self.operand(a)?;
        let (b, _) = self.operand(b)?;
        let narrow = wide.half_width().expect("a lane of 16 or 32 bits");
        let bits = narrow.bits();
        let (least, most) = if signed {
            (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1)
        } else {
            (0, (1i64 << bits) - 1)
        };
        let (wide_bytes, narrow_bytes) = (wide.bytes() as i32, narrow.bytes() as i32);
        let per_half = 8 / wide_bytes;
        for half in (0..self.layout(ty)?.size.bytes() as i32).step_by(16) {
            for (from, src) in [a, b].into_iter().enumerate() {
                for lane in 0..per_half {
                    let value = self.builder.ins().load(wide, MEM, src, half + lane * wide_bytes);
                    let most = self.builder.ins().iconst(wide, most);
                    let least = self.builder.ins().iconst(wide, least);
                    let value = self.builder.ins().smin(value, most);
                    let value = self.builder.ins().smax(value, least);
                    let value = self.builder.ins().ireduce(narrow, value);
                    let at = half + (from as i32 * per_half + lane) * narrow_bytes;
                    self.builder.ins().store(MEM, value, dst, at);
                }
            }
        }
        Ok(())
    }

    /// Calls a function of one instruction on two vectors, with where it reads a third
    /// operand and writes its result.
    fn vector_assembly(
        &mut self,
        text: String,
        a: &Operand,
        b: &Operand,
        dst: Value,
    ) -> Result<()> {
        let (a, _) = self.operand(a)?;
        let (b, _) = self.operand(b)?;
        let mut sig = self.cg.make_signature();
        sig.params.extend([AbiParam::new(ptr()); 3]);
        let callee = self.cg.assembly(text, &sig);
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        self.builder.ins().call(callee, &[a, b, dst]);
        Ok(())
    }

    /// A function `core::arch` declares as `llvm.{name}`: something LLVM implements, with no
    /// body anywhere.
    fn llvm_intrinsic(
        &mut self,
        name: &str,
        args: &[Operand],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<()> {
        match (name, args) {
            ("x86.sse3.ldu.dq" | "x86.avx.ldu.dq.256", [ptr]) => {
                let (ptr, _) = self.load_scalar(ptr)?;
                let size = self.layout(dst_ty)?.size;
                self.copy(dst, ptr, size);
            }
            ("x86.ssse3.pshuf.b.128" | "x86.avx2.pshuf.b", [table, indices]) => {
                let (table, ty) = self.operand(table)?;
                let (indices, _) = self.operand(indices)?;
                let size = self.layout(ty)?.size.bytes();
                // Each 16-byte half looks up bytes in itself.
                for i in 0..size {
                    let index = self.builder.ins().load(types::I8, MEM, indices, i as i32);
                    let slot = self.builder.ins().band_imm(index, 0x0f);
                    let slot = self.builder.ins().uextend(ptr(), slot);
                    let addr = self.builder.ins().iadd(table, slot);
                    let found = self.builder.ins().load(types::I8, MEM, addr, (i & !15) as i32);
                    let zero = self.builder.ins().iconst(types::I8, 0);
                    let cleared = self.builder.ins().icmp_imm(IntCC::SignedLessThan, index, 0);
                    let value = self.builder.ins().select(cleared, zero, found);
                    self.builder.ins().store(MEM, value, dst, i as i32);
                }
            }
            ("x86.avx2.permd" | "x86.avx2.permps", [table, indices]) => {
                let (table, _) = self.operand(table)?;
                let (indices, _) = self.operand(indices)?;
                // Each of the eight lanes is the lane its index names, by its low three bits.
                let lanes: Vec<Value> = (0..8)
                    .map(|lane| {
                        let index = self.builder.ins().load(types::I32, MEM, indices, lane * 4);
                        let slot = self.builder.ins().band_imm(index, 7);
                        let slot = self.builder.ins().uextend(ptr(), slot);
                        let offset = self.builder.ins().ishl_imm(slot, 2);
                        let addr = self.builder.ins().iadd(table, offset);
                        self.builder.ins().load(types::I32, MEM, addr, 0)
                    })
                    .collect();
                for (lane, value) in lanes.into_iter().enumerate() {
                    self.builder.ins().store(MEM, value, dst, lane as i32 * 4);
                }
            }
            ("x86.sse2.psad.bw" | "x86.avx2.psad.bw" | "x86.avx512.psad.bw.512", [a, b]) => {
                let (a, ty) = self.operand(a)?;
                let (b, _) = self.operand(b)?;
                // Each 64 bits are the sum of how far apart eight bytes of the two are.
                for lane in (0..self.layout(ty)?.size.bytes() as i32).step_by(8) {
                    let mut sum = self.builder.ins().iconst(types::I64, 0);
                    for byte in lane..lane + 8 {
                        let l = self.builder.ins().load(types::I8, MEM, a, byte);
                        let r = self.builder.ins().load(types::I8, MEM, b, byte);
                        let more = self.builder.ins().umax(l, r);
                        let less = self.builder.ins().umin(l, r);
                        let apart = self.builder.ins().isub(more, less);
                        let apart = self.builder.ins().uextend(types::I64, apart);
                        sum = self.builder.ins().iadd(sum, apart);
                    }
                    self.builder.ins().store(MEM, sum, dst, lane);
                }
            }
            ("x86.sse2.pmadd.wd" | "x86.avx2.pmadd.wd" | "x86.avx512.pmaddw.d.512", [a, b]) => {
                let (a, ty) = self.operand(a)?;
                let (b, _) = self.operand(b)?;
                // Each 32 bits are the sum of the products of two pairs of signed words.
                for lane in (0..self.layout(ty)?.size.bytes() as i32).step_by(4) {
                    let mut sum = self.builder.ins().iconst(types::I32, 0);
                    for word in [lane, lane + 2] {
                        let l = self.builder.ins().load(types::I16, MEM, a, word);
                        let r = self.builder.ins().load(types::I16, MEM, b, word);
                        let l = self.builder.ins().sextend(types::I32, l);
                        let r = self.builder.ins().sextend(types::I32, r);
                        let product = self.builder.ins().imul(l, r);
                        sum = self.builder.ins().iadd(sum, product);
                    }
                    self.builder.ins().store(MEM, sum, dst, lane);
                }
            }
            (
                "x86.ssse3.pmadd.ub.sw.128" | "x86.avx2.pmadd.ub.sw" | "x86.avx512.pmaddubs.w.512",
                [a, b],
            ) => {
                let (a, ty) = self.operand(a)?;
                let (b, _) = self.operand(b)?;
                // Each 16 bits are the saturated sum of two products: an unsigned byte of
                // `a` by a signed one of `b`.
                for lane in (0..self.layout(ty)?.size.bytes() as i32).step_by(2) {
                    let mut sum = self.builder.ins().iconst(types::I32, 0);
                    for byte in [lane, lane + 1] {
                        let l = self.builder.ins().load(types::I8, MEM, a, byte);
                        let r = self.builder.ins().load(types::I8, MEM, b, byte);
                        let l = self.builder.ins().uextend(types::I32, l);
                        let r = self.builder.ins().sextend(types::I32, r);
                        let product = self.builder.ins().imul(l, r);
                        sum = self.builder.ins().iadd(sum, product);
                    }
                    let most = self.builder.ins().iconst(types::I32, i64::from(i16::MAX));
                    let least = self.builder.ins().iconst(types::I32, i64::from(i16::MIN));
                    let sum = self.builder.ins().smin(sum, most);
                    let sum = self.builder.ins().smax(sum, least);
                    let sum = self.builder.ins().ireduce(types::I16, sum);
                    self.builder.ins().store(MEM, sum, dst, lane);
                }
            }
            ("x86.sse2.packsswb.128" | "x86.avx2.packsswb" | "x86.avx512.packsswb.512", [a, b]) => {
                self.pack_lanes(a, b, dst, types::I16, true)?
            }
            ("x86.sse2.packuswb.128" | "x86.avx2.packuswb" | "x86.avx512.packuswb.512", [a, b]) => {
                self.pack_lanes(a, b, dst, types::I16, false)?
            }
            ("x86.sse2.packssdw.128" | "x86.avx2.packssdw" | "x86.avx512.packssdw.512", [a, b]) => {
                self.pack_lanes(a, b, dst, types::I32, true)?
            }
            ("x86.sse41.packusdw" | "x86.avx2.packusdw" | "x86.avx512.packusdw.512", [a, b]) => {
                self.pack_lanes(a, b, dst, types::I32, false)?
            }
            (
                "x86.sha1msg1" | "x86.sha1msg2" | "x86.sha1nexte" | "x86.sha256msg1"
                | "x86.sha256msg2",
                [a, b],
            ) => {
                let instruction = format!("{} xmm0, xmm1", name.trim_start_matches("x86."));
                self.vector_assembly(asm::vector_instruction(&instruction, "xmm0"), a, b, dst)?;
            }
            ("x86.sha256rnds2", [a, b, k]) => {
                let (k, ty) = self.operand(k)?;
                let size = self.layout(ty)?.size;
                self.copy(dst, k, size);
                // The instruction takes its third vector in `xmm0`.
                let rounds = "movdqa xmm3, xmm0\nmovdqa xmm0, xmm2\nsha256rnds2 xmm3, xmm1";
                self.vector_assembly(asm::vector_instruction(rounds, "xmm3"), a, b, dst)?;
            }
            ("x86.sha1rnds4", [a, b, function]) => {
                let (function, _) = self.load_scalar(function)?;
                self.builder.ins().store(MEM, function, dst, 0);
                self.vector_assembly(asm::sha1rnds4(), a, b, dst)?;
            }
            ("x86.xgetbv", [register]) => {
                let (register, _) = self.load_scalar(register)?;
                let mut sig = self.cg.make_signature();
                sig.params.push(AbiParam::new(types::I32));
                sig.returns.push(AbiParam::new(types::I64));
                let callee = self.cg.assembly(asm::xgetbv(), &sig);
                let callee = self.cg.declare_func_in_func(callee, self.builder.func);
                let call = self.builder.ins().call(callee, &[register]);
                let value = self.builder.inst_results(call)[0];
                self.builder.ins().store(MEM, value, dst, 0);
            }
            ("x86.sse2.pause", []) | ("aarch64.isb", [_]) => {
                let text = if name == "aarch64.isb" { asm::isb() } else { asm::pause() };
                let sig = self.cg.make_signature();
                let callee = self.cg.assembly(text, &sig);
                let callee = self.cg.declare_func_in_func(callee, self.builder.func);
                self.builder.ins().call(callee, &[]);
            }
            ("x86.vcvtps2ph.128" | "x86.vcvtps2ph.256", [singles, rounding]) => {
                let (singles, _) = self.operand(singles)?;
                let (rounding, _) = self.load_scalar(rounding)?;
                let mut sig = self.cg.make_signature();
                sig.params.extend([AbiParam::new(ptr()), AbiParam::new(ptr())]);
                sig.params.push(AbiParam::new(types::I32));
                let register = if name.ends_with("256") { "ymm0" } else { "xmm0" };
                let callee = self.cg.assembly(asm::vcvtps2ph(register), &sig);
                let callee = self.cg.declare_func_in_func(callee, self.builder.func);
                self.builder.ins().call(callee, &[singles, dst, rounding]);
            }
            ("x86.pclmulqdq" | "x86.pclmulqdq.256" | "x86.pclmulqdq.512", [a, b, halves]) => {
                let (a, ty) = self.operand(a)?;
                let (b, _) = self.operand(b)?;
                let (halves, _) = self.load_scalar(halves)?;
                // Bit 0 says which half of each 128 bits of `a` is multiplied, bit 4 of `b`.
                let half = |fx: &mut Self, bit: i64| {
                    let chosen = fx.builder.ins().ushr_imm(halves, bit);
                    let chosen = fx.builder.ins().band_imm(chosen, 1);
                    let chosen = fx.builder.ins().uextend(ptr(), chosen);
                    fx.builder.ins().ishl_imm(chosen, 3)
                };
                let (of_a, of_b) = (half(self, 0), half(self, 4));
                for lane in (0..self.layout(ty)?.size.bytes() as i32).step_by(16) {
                    let l = self.builder.ins().iadd(a, of_a);
                    let l = self.builder.ins().load(types::I64, MEM, l, lane);
                    let r = self.builder.ins().iadd(b, of_b);
                    let r = self.builder.ins().load(types::I64, MEM, r, lane);
                    // The product without carries: `l` shifted by every bit set in `r`, xored.
                    let mut low = self.builder.ins().iconst(types::I64, 0);
                    let mut high = low;
                    for bit in 0..64 {
                        let set = self.builder.ins().ushr_imm(r, bit);
                        let set = self.builder.ins().band_imm(set, 1);
                        let mask = self.builder.ins().ineg(set);
                        let part = self.builder.ins().ishl_imm(l, bit);
                        let part = self.builder.ins().band(part, mask);
                        low = self.builder.ins().bxor(low, part);
                        if bit != 0 {
                            let part = self.builder.ins().ushr_imm(l, 64 - bit);
                            let part = self.builder.ins().band(part, mask);
                            high = self.builder.ins().bxor(high, part);
                        }
                    }
                    self.builder.ins().store(MEM, low, dst, lane);
                    self.builder.ins().store(MEM, high, dst, lane + 8);
                }
            }
            // A hint to the cache, which the program behaves the same without.
            ("prefetch.p0", [_address, _write, _locality, _cache]) => {}
            _ => unsupported!("LLVM intrinsic `{name}`"),
        }
        Ok(())
    }

    /// The lane type and lane count of a vector type. Lanes sit in memory in order.
    fn simd_lanes(&self, ty: Ty<'db>) -> Result<(AbiScalar, u64)> {
        match self.layout(ty)?.backend_repr {
            BackendRepr::SimdVector { element, count } => {
                Ok((self.scalar(element)?, count.as_u64()))
            }
            _ => unsupported!("vector operation on {ty:?}"),
        }
    }

    /// Calls a function of compiler-builtins, which are C's by convention.
    fn builtin(&mut self, name: &str, params: &[Type], ret: Type, args: &[Value]) -> Result<Value> {
        // 64-bit Windows takes an integer of 128 bits by reference and returns it as a
        // vector.
        let windows = self.cg.target.call_conv == isa::CallConv::WindowsFastcall;
        let wide = |ty: Type| windows && ty == types::I128;
        let mut sig = self.cg.make_signature();
        sig.params
            .extend(params.iter().map(|&ty| AbiParam::new(if wide(ty) { ptr() } else { ty })));
        sig.returns.push(AbiParam::new(if wide(ret) { types::I64X2 } else { ret }));
        let callee = self.cg.import(name, &sig)?;
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        let mut passed = Vec::with_capacity(args.len());
        for (&value, &ty) in args.iter().zip(params) {
            passed.push(if wide(ty) {
                let slot = self.scratch_wide();
                self.builder.ins().store(MEM, value, slot, 0);
                slot
            } else {
                value
            });
        }
        let value = self.invoke(callee, &passed)[0];
        if !wide(ret) {
            return Ok(value);
        }
        let slot = self.scratch_wide();
        self.builder.ins().store(MEM, value, slot, 0);
        Ok(self.builder.ins().load(types::I128, MEM, slot, 0))
    }

    /// The quotient of two integers, or the remainder. Cranelift has neither for 128 bits, so
    /// those call compiler-builtins.
    fn divide(&mut self, l: Value, r: Value, div: bool, signed: bool) -> Result<Value> {
        if self.builder.func.dfg.value_type(l) == types::I128 {
            let mut sig = self.cg.make_signature();
            sig.params.extend([AbiParam::new(types::I128), AbiParam::new(types::I128)]);
            sig.returns.push(AbiParam::new(types::I128));
            let name = match (div, signed) {
                (true, true) => "__divti3",
                (true, false) => "__udivti3",
                (false, true) => "__modti3",
                (false, false) => "__umodti3",
            };
            return self.builtin(name, &[types::I128; 2], types::I128, &[l, r]);
        }
        let ins = self.builder.ins();
        Ok(match (div, signed) {
            (true, true) => ins.sdiv(l, r),
            (true, false) => ins.udiv(l, r),
            (false, true) => ins.srem(l, r),
            (false, false) => ins.urem(l, r),
        })
    }

    /// Cranelift has no 128-bit multiply that reports overflow, so this calls the one in
    /// `compiler_builtins`.
    fn wide_mul_overflow(&mut self, l: Value, r: Value, signed: bool) -> Result<(Value, Value)> {
        let name = if signed { "__rust_i128_mulo" } else { "__rust_u128_mulo" };
        let overflow = self.scratch();
        let params = [types::I128, types::I128, ptr()];
        let value = self.builtin(name, &params, types::I128, &[l, r, overflow])?;
        let overflow = self.builder.ins().load(types::I32, MEM, overflow, 0);
        let overflow = self.builder.ins().icmp_imm(IntCC::NotEqual, overflow, 0);
        Ok((value, overflow))
    }

    fn memcmp(&mut self, a: Value, b: Value, len: Value) -> Result<Value> {
        let mut sig = self.cg.make_signature();
        sig.params.extend([AbiParam::new(ptr()); 3]);
        sig.returns.push(AbiParam::new(types::I32));
        let memcmp = self.cg.import("memcmp", &sig)?;
        let memcmp = self.cg.declare_func_in_func(memcmp, self.builder.func);
        let call = self.builder.ins().call(memcmp, &[a, b, len]);
        Ok(self.builder.inst_results(call)[0])
    }

    /// A vector operation of two operands on lanes that are floats, one lane at a time. A
    /// comparison is all ones in the lanes where it holds, in integers as wide as the floats.
    fn simd_float_lanes(
        &mut self,
        name: &str,
        (l, r): (Value, Value),
        dst: Value,
        lane: AbiScalar,
        out: AbiScalar,
        count: u64,
    ) -> Result<()> {
        let cc = match name {
            "simd_eq" => Some(FloatCC::Equal),
            "simd_ne" => Some(FloatCC::NotEqual),
            "simd_lt" => Some(FloatCC::LessThan),
            "simd_le" => Some(FloatCC::LessThanOrEqual),
            "simd_gt" => Some(FloatCC::GreaterThan),
            "simd_ge" => Some(FloatCC::GreaterThanOrEqual),
            _ => None,
        };
        for i in 0..count {
            let offset = (i * lane.ty.bytes() as u64) as i32;
            let a = self.builder.ins().load(lane.ty, MEM, l, offset);
            let b = self.builder.ins().load(lane.ty, MEM, r, offset);
            let value = match (cc, name) {
                (Some(cc), _) => {
                    let flag = self.builder.ins().fcmp(cc, a, b);
                    let flag = self.builder.ins().uextend(types::I64, flag);
                    let mask = self.builder.ins().ineg(flag);
                    if out.ty.bits() < 64 { self.builder.ins().ireduce(out.ty, mask) } else { mask }
                }
                (None, "simd_add") => self.builder.ins().fadd(a, b),
                (None, "simd_sub") => self.builder.ins().fsub(a, b),
                (None, "simd_mul") => self.builder.ins().fmul(a, b),
                (None, "simd_div") => self.builder.ins().fdiv(a, b),
                (None, _) => unsupported!("`{name}` of floats"),
            };
            self.builder.ins().store(MEM, value, dst, (i * out.ty.bytes() as u64) as i32);
        }
        Ok(())
    }

    fn saturating(&mut self, add: bool, l: Value, r: Value, scalar: AbiScalar) -> Value {
        let bits = scalar.ty.bits();
        let ins = self.builder.ins();
        let (value, overflow) = match (add, scalar.signed) {
            (true, true) => ins.sadd_overflow(l, r),
            (true, false) => ins.uadd_overflow(l, r),
            (false, true) => ins.ssub_overflow(l, r),
            (false, false) => ins.usub_overflow(l, r),
        };
        let limit = if scalar.signed {
            // Overflow went past MAX when the left operand is non-negative, past MIN otherwise.
            let min = self.iconst(scalar.ty, 1u128 << (bits - 1));
            let max = self.iconst(scalar.ty, (1u128 << (bits - 1)) - 1);
            let negative = self.builder.ins().icmp_imm(IntCC::SignedLessThan, l, 0);
            self.builder.ins().select(negative, min, max)
        } else if add {
            self.iconst(scalar.ty, u128::MAX >> (128 - bits))
        } else {
            self.iconst(scalar.ty, 0)
        };
        self.builder.ins().select(overflow, limit, value)
    }

    fn int_cast(&mut self, value: Value, from: AbiScalar, to: AbiScalar) -> Value {
        if to.ty.bits() < from.ty.bits() {
            self.builder.ins().ireduce(to.ty, value)
        } else if to.ty.bits() == from.ty.bits() {
            value
        } else if from.signed {
            self.builder.ins().sextend(to.ty, value)
        } else {
            self.builder.ins().uextend(to.ty, value)
        }
    }

    /// Calls a function that may unwind. When this function has values to drop, the call gets a
    /// landing pad that drops them and resumes unwinding.
    fn invoke(&mut self, callee: ir::FuncRef, args: &[Value]) -> Vec<Value> {
        let Some(pad) = self.landing_pad_for_call() else {
            let call = self.builder.ins().call(callee, args);
            return self.builder.inst_results(call).to_vec();
        };
        if !self.cg.target.exception_in_register() {
            return self.guarded_call(pad, |builder| builder.ins().call(callee, args));
        }
        let sig = self.builder.func.dfg.ext_funcs[callee].signature;
        let (table, after) = self.exception_table(sig, pad);
        self.builder.ins().try_call(callee, args, table);
        self.builder.switch_to_block(after);
        self.builder.block_params(after).to_vec()
    }

    fn invoke_indirect(&mut self, sig: ir::SigRef, pointer: Value, args: &[Value]) -> Vec<Value> {
        let Some(pad) = self.landing_pad_for_call() else {
            let call = self.builder.ins().call_indirect(sig, pointer, args);
            return self.builder.inst_results(call).to_vec();
        };
        if !self.cg.target.exception_in_register() {
            return self
                .guarded_call(pad, |builder| builder.ins().call_indirect(sig, pointer, args));
        }
        let (table, after) = self.exception_table(sig, pad);
        self.builder.ins().try_call_indirect(pointer, args, table);
        self.builder.switch_to_block(after);
        self.builder.block_params(after).to_vec()
    }

    /// The block that cleans up after a call that unwinds, if there is anything to clean up.
    fn landing_pad_for_call(&mut self) -> Option<ir::Block> {
        if self.in_cleanup {
            return None;
        }
        if self.inner_pad.is_some() {
            return self.inner_pad;
        }
        let target = self.unwind_to?;
        if let Some(&(_, pad)) = self.pads.iter().find(|(it, _)| *it == target) {
            return Some(pad);
        }
        let pad = self.builder.create_block();
        self.builder.append_block_param(pad, ptr());
        self.builder.set_cold_block(pad);
        self.pads.push((target, pad));
        Some(pad)
    }

    fn exception_table(
        &mut self,
        sig: ir::SigRef,
        cleanup: ir::Block,
    ) -> (ir::ExceptionTable, ir::Block) {
        let after = self.builder.create_block();
        let returns: Vec<_> =
            self.builder.func.dfg.signatures[sig].returns.iter().map(|it| it.value_type).collect();
        let mut results = Vec::new();
        for (i, &ty) in returns.iter().enumerate() {
            self.builder.append_block_param(after, ty);
            results.push(ir::BlockArg::TryCallRet(i as u32));
        }
        let normal = self.builder.func.dfg.block_call(after, &results);
        let unwound = self.builder.func.dfg.block_call(cleanup, &[ir::BlockArg::TryCallExn(0)]);
        let tag = ir::ExceptionTag::with_number(unwind::CLEANUP).expect("a small tag");
        let table = self.builder.func.dfg.exception_tables.push(ir::ExceptionTableData::new(
            sig,
            normal,
            [ir::ExceptionTableItem::Tag(tag, unwound)],
        ));
        (table, after)
    }

    /// Makes a call that goes to `pad` when it unwinds, where a call cannot name its pad:
    /// the unwinder has the call return with the exception in a slot of the frame that is
    /// empty otherwise, and the call is followed by a look at it.
    fn guarded_call(
        &mut self,
        pad: ir::Block,
        call: impl FnOnce(&mut FunctionBuilder<'_>) -> ir::Inst,
    ) -> Vec<Value> {
        self.guarded = true;
        let loc = self.cg.source_loc(self.span);
        self.builder.set_srcloc(unwind::seh::guarded(loc));
        let call = call(&mut self.builder);
        self.at(self.span);
        let results = self.builder.inst_results(call).to_vec();
        let frame = self.builder.ins().get_frame_pointer(ptr());
        let exception = self.builder.ins().load(ptr(), MEM, frame, unwind::seh::ARRIVAL);
        let (unwound, returned) = (self.builder.create_block(), self.builder.create_block());
        self.builder.set_cold_block(unwound);
        self.builder.ins().brif(exception, unwound, &[], returned, &[]);
        self.builder.switch_to_block(unwound);
        let none = self.builder.ins().iconst(ptr(), 0);
        self.builder.ins().store(MEM, none, frame, unwind::seh::ARRIVAL);
        self.builder.ins().jump(pad, &[exception.into()]);
        self.builder.switch_to_block(returned);
        results
    }

    /// Fills in the landing pads of a body in `MirPhase::Runtime`: each keeps the exception
    /// and goes on to its cleanup block, which ends in `UnwindResume`.
    fn emit_pads(&mut self) {
        for (target, pad) in std::mem::take(&mut self.pads) {
            self.builder.switch_to_block(pad);
            let exception = self.builder.block_params(pad)[0];
            let slot = self.exception_slot();
            self.builder.ins().stack_store(ptr(), exception, slot, 0);
            self.builder.ins().jump(self.blocks[target], &[]);
        }
    }

    fn exception_slot(&mut self) -> ir::StackSlot {
        *self.exception.get_or_insert_with(|| {
            self.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                8,
                3,
            ))
        })
    }

    fn store_state(&mut self, state: Value, ty: Type, value: i64) {
        let value = self.builder.ins().iconst(ty, value);
        self.builder.ins().store(MEM, value, state, 0);
    }

    /// Returns what is in the local that holds the function's result.
    fn return_(&mut self) -> Result<()> {
        let body = self.body.expect("MIR body");
        let local = returned_local(body);
        let mut values = Vec::new();
        if !matches!(self.ret_mode, PassMode::Indirect) {
            let layout = self.layout(body.locals[local].ty.as_ref())?;
            let addr = self.slot_addr(self.locals[local].addr);
            self.pass(self.ret_mode, addr, &layout, &mut values);
        }
        self.builder.ins().return_(&values);
        Ok(())
    }

    /// The way into an `async` body. `poll` carries on from where the body last returned
    /// `Poll::Pending`. A null context instead asks for the future to be dropped: what it
    /// captured if it never ran, and what is in scope where it yielded if it is suspended,
    /// which the yield's drop path in the MIR does.
    fn enter_coroutine(
        &mut self,
        body: &MirBody<'db>,
        state: &SuspendedState,
        addr: Value,
        ty: Ty<'db>,
        layout: &Layout,
    ) -> Result<()> {
        let Some(crate::mir::CoroutineLocals { context, .. }) = body.coroutine else {
            unsupported!("async body without a context");
        };
        let context = self.builder.ins().load(ptr(), MEM, self.locals[context].addr, 0);
        let at = self.builder.ins().load(state.ty, MEM, state.addr, 0);
        let at = match state.ty {
            types::I64 => at,
            _ => self.builder.ins().uextend(types::I64, at),
        };
        let (poll, drop) = (self.builder.create_block(), self.builder.create_block());
        self.builder.ins().brif(context, poll, &[], drop, &[]);

        let finished = self.builder.create_block();
        self.builder.switch_to_block(poll);
        let mut switch = Switch::new();
        switch.set_entry(UNRESUMED as u128, self.blocks[body.start_block]);
        for (index, point) in state.held.yields.iter().enumerate() {
            switch.set_entry(SUSPENDED as u128 + index as u128, self.blocks[point.resume]);
        }
        switch.emit(&mut self.builder, at, finished);
        // FIXME: rustc panics with "`async fn` resumed after completion".
        self.builder.switch_to_block(finished);
        self.builder.ins().trap(TrapCode::user(3).expect("non-zero trap code"));

        let (unresumed, done) = (self.builder.create_block(), self.builder.create_block());
        // Dropping is laid out behind the body, so that the code a function begins with is
        // that of its first statement and not of the drops of a future given up half-way.
        for block in [drop, unresumed] {
            self.builder.set_cold_block(block);
        }
        self.builder.switch_to_block(drop);
        self.store_state(state.addr, state.ty, RETURNED);
        let mut dropping = Switch::new();
        dropping.set_entry(UNRESUMED as u128, unresumed);
        for (index, point) in state.held.yields.iter().enumerate() {
            let Some(dropped) = point.drop else {
                unsupported!("a yield without a drop path");
            };
            dropping.set_entry(SUSPENDED as u128 + index as u128, self.blocks[dropped]);
        }
        dropping.emit(&mut self.builder, at, done);
        self.in_cleanup = true;
        self.builder.switch_to_block(unresumed);
        self.at(Self::returns_at(body));
        let TyKind::Coroutine(_, args) = ty.kind() else {
            unsupported!("async body of {ty:?}");
        };
        for (i, field) in args.as_coroutine().tupled_upvars_ty().tuple_fields().iter().enumerate() {
            let field_addr =
                self.builder.ins().iadd_imm(addr, layout.fields.offset(i).bytes() as i64);
            self.drop_value(field_addr, None, field)?;
        }
        self.builder.ins().jump(done, &[]);
        self.in_cleanup = false;
        self.builder.switch_to_block(done);
        self.return_()
    }

    /// Where the body is written to end, which is where rustc has an `async` block dropped.
    fn returns_at(body: &MirBody<'db>) -> MirSpan {
        let returns = body.basic_blocks.iter().find_map(|(_, block)| match &block.terminator {
            Some(crate::mir::Terminator { kind: TerminatorKind::Return, span }) => Some(*span),
            _ => None,
        });
        returns.unwrap_or(MirSpan::Unknown)
    }

    /// Calls the `poll` of the `async` block of type `ty`, which is what `Future::poll` is for
    /// it. `args` are the pinned reference and the context.
    fn coroutine_callee(&mut self, ty: Ty<'db>) -> Option<Callee<'db>> {
        let (def, parent_args) = coroutine_instance(self.cg.db, ty)?;
        Some(Callee::Instance(def, parent_args))
    }

    /// Drops the `async` block at `addr`, by polling it with no context.
    fn drop_coroutine(&mut self, addr: Value, ty: Ty<'db>) -> Result<()> {
        let Some((def, parent_args)) = coroutine_instance(self.cg.db, ty) else {
            unsupported!("drop glue for {ty:?}");
        };
        let body = self.body_types(def, parent_args)?;
        let Some(poll_ty) = &body.poll else {
            unsupported!("async block whose body is not one");
        };
        let mut values = Vec::new();
        for (param, value) in body.params.iter().zip([Some(addr), None]) {
            let slot = self.scratch();
            let value = value.unwrap_or_else(|| self.builder.ins().iconst(ptr(), 0));
            self.builder.ins().store(MEM, value, slot, 0);
            values.push((slot, param.as_ref()));
        }
        let poll_ty = poll_ty.as_ref();
        let layout = self.layout(poll_ty)?;
        let ignored = self.stack_slot(&layout);
        self.emit_call(Callee::Instance(def, parent_args), &values, ignored, poll_ty)
    }

    fn resume_unwinding(&mut self, exception: Value) -> Result<()> {
        let mut sig = self.cg.make_signature();
        sig.params.push(AbiParam::new(ptr()));
        let resume = if self.cg.target.exception_in_register() {
            "_Unwind_Resume"
        } else {
            unwind::seh::RESUME
        };
        let resume = self.cg.import(resume, &sig)?;
        let resume = self.cg.declare_func_in_func(resume, self.builder.func);
        self.builder.ins().call(resume, &[exception]);
        self.builder.ins().trap(ir::TrapCode::user(1).expect("a nonzero trap code"));
        Ok(())
    }

    /// Runs a type's `Drop` impl. If that unwinds the fields are dropped all the same, on the
    /// way out.
    fn call_drop_impl_guarded(
        &mut self,
        addr: Value,
        meta: Option<Value>,
        ty: Ty<'db>,
    ) -> Result<()> {
        self.guarded(
            |this| this.call_drop_impl(addr, meta, ty),
            |this| this.drop_adt_fields(addr, meta, ty),
        )
    }

    /// Runs `body`, and `cleanup` on the way out if a call that `body` makes unwinds.
    fn guarded(
        &mut self,
        body: impl FnOnce(&mut Self) -> Result<()>,
        cleanup: impl FnOnce(&mut Self) -> Result<()>,
    ) -> Result<()> {
        if self.in_cleanup {
            return body(self);
        }
        let outer = self.landing_pad_for_call();
        let pad = self.builder.create_block();
        self.builder.append_block_param(pad, ptr());
        self.builder.set_cold_block(pad);
        let enclosing = self.inner_pad.replace(pad);
        let done = body(self);
        self.inner_pad = enclosing;
        done?;
        let Some(after) = self.builder.current_block() else {
            unsupported!("guarded call without a block to return to");
        };
        self.builder.switch_to_block(pad);
        let exception = self.builder.block_params(pad)[0];
        self.in_cleanup = true;
        let cleaned = cleanup(self);
        self.in_cleanup = false;
        cleaned?;
        match outer {
            Some(outer) => {
                self.builder.ins().jump(outer, &[exception.into()]);
            }
            None => self.resume_unwinding(exception)?,
        }
        self.builder.switch_to_block(after);
        Ok(())
    }

    fn drop_value(&mut self, addr: Value, meta: Option<Value>, ty: Ty<'db>) -> Result<()> {
        let ty = self.normalize(ty);
        if self.glue_of(ty) == DropGlue::None {
            return Ok(());
        }
        let mut sig = self.cg.make_signature();
        sig.params.push(AbiParam::new(ptr()));
        let mut values = vec![addr];
        if self.is_unsized(ty) {
            let Some(meta) = meta else {
                unsupported!("drop of unsized {ty:?} without metadata");
            };
            sig.params.push(AbiParam::new(ptr()));
            values.push(meta);
        }
        let args = GenericArgs::new_from_slice(&[ty.into()]);
        let callee = self.cg.callee(InstanceDef::DropGlue, args, self.env, &sig)?;
        self.show_callee(InstanceDef::DropGlue, args, &sig, callee);
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        self.invoke(callee, &values);
        Ok(())
    }

    /// The body of `drop_in_place::<ty>`.
    fn drop_glue(&mut self, ty: Ty<'db>) -> Result<()> {
        let mut sig = self.cg.make_signature();
        sig.params.push(AbiParam::new(ptr()));
        if self.is_unsized(ty) {
            sig.params.push(AbiParam::new(ptr()));
        }
        self.builder.func.signature = sig;
        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.builder.switch_to_block(entry);
        let params = self.builder.block_params(entry).to_vec();
        self.drop_contents(params[0], params.get(1).copied(), ty)?;
        self.builder.ins().return_(&[]);
        Ok(())
    }

    /// Drops the fields of a struct or of the variant an enum is in.
    /// Jumps to `differ` unless the values at `l` and `r` are the same value of `ty`, compared
    /// the way a constant pattern is: field by field and through references. `metas` are the
    /// lengths of the two when `ty` is unsized.
    fn branch_unless_equal(
        &mut self,
        l: Value,
        r: Value,
        metas: Option<(Value, Value)>,
        ty: Ty<'db>,
        differ: ir::Block,
    ) -> Result<()> {
        let db = self.cg.db;
        let ty = self.normalize(ty);
        match ty.kind() {
            TyKind::Ref(_, pointee, _) => {
                let metas = self.is_unsized(pointee).then(|| {
                    (
                        self.builder.ins().load(ptr(), MEM, l, meta_at()),
                        self.builder.ins().load(ptr(), MEM, r, meta_at()),
                    )
                });
                let l = self.builder.ins().load(ptr(), MEM, l, 0);
                let r = self.builder.ins().load(ptr(), MEM, r, 0);
                self.branch_unless_equal(l, r, metas, pointee, differ)
            }
            TyKind::Str | TyKind::Slice(_) | TyKind::Array(..) => {
                let element = match ty.kind() {
                    TyKind::Slice(element) | TyKind::Array(element, _) => element,
                    _ => Ty::new_uint(self.interner, rustc_type_ir::UintTy::U8),
                };
                let len = match (ty.kind(), metas) {
                    (TyKind::Array(_, len), _) => {
                        let Some(len) = try_const_usize(db, len) else {
                            unsupported!("array with unevaluated length");
                        };
                        self.builder.ins().iconst(ptr(), len as i64)
                    }
                    (_, Some((l_len, r_len))) => {
                        let same = self.builder.ins().icmp(IntCC::Equal, l_len, r_len);
                        self.branch_unless(same, differ);
                        l_len
                    }
                    (_, None) => unsupported!("comparison of a slice without a length"),
                };
                let size = self.layout(element)?.size.bytes() as i64;
                // Elements without padding or references are equal exactly when their bytes
                // are.
                if matches!(
                    element.kind(),
                    TyKind::Int(_) | TyKind::Uint(_) | TyKind::Bool | TyKind::Char
                ) {
                    let bytes = self.builder.ins().imul_imm(len, size);
                    let order = self.memcmp(l, r, bytes)?;
                    let same = self.builder.ins().icmp_imm(IntCC::Equal, order, 0);
                    self.branch_unless(same, differ);
                    return Ok(());
                }
                let header = self.builder.create_block();
                let body = self.builder.create_block();
                let done = self.builder.create_block();
                self.builder.append_block_param(header, ptr());
                let zero = self.builder.ins().iconst(ptr(), 0);
                self.builder.ins().jump(header, &[zero.into()]);
                self.builder.switch_to_block(header);
                let index = self.builder.block_params(header)[0];
                let more = self.builder.ins().icmp(IntCC::UnsignedLessThan, index, len);
                self.builder.ins().brif(more, body, &[], done, &[]);
                self.builder.switch_to_block(body);
                let offset = self.builder.ins().imul_imm(index, size);
                let l = self.builder.ins().iadd(l, offset);
                let r = self.builder.ins().iadd(r, offset);
                self.branch_unless_equal(l, r, None, element, differ)?;
                let next = self.builder.ins().iadd_imm(index, 1);
                self.builder.ins().jump(header, &[next.into()]);
                self.builder.switch_to_block(done);
                Ok(())
            }
            TyKind::Tuple(tys) => {
                let layout = self.layout(ty)?;
                for (i, field) in tys.iter().enumerate() {
                    let offset = layout.fields.offset(i).bytes() as i64;
                    let l = self.builder.ins().iadd_imm(l, offset);
                    let r = self.builder.ins().iadd_imm(r, offset);
                    self.branch_unless_equal(l, r, None, field, differ)?;
                }
                Ok(())
            }
            TyKind::Adt(adt_def, subst) => {
                let layout = self.layout(ty)?;
                match adt_def.def_id() {
                    AdtId::UnionId(_) => unsupported!("comparison of a union"),
                    AdtId::StructId(id) => {
                        self.branch_unless_fields_equal(l, r, &layout, id.into(), subst, differ)
                    }
                    AdtId::EnumId(id) => {
                        let wide = match &layout.variants {
                            Variants::Multiple { tag, .. } => DiscriminantWidth::of(tag),
                            Variants::Empty => return Ok(()),
                            Variants::Single { index } => {
                                let variant = id
                                    .enum_variants(db)
                                    .variants
                                    .values()
                                    .map(|&(variant, _)| variant)
                                    .find(|variant| variant.index(db) == index.0);
                                let Some(variant) = variant else { return Ok(()) };
                                return self.branch_unless_fields_equal(
                                    l,
                                    r,
                                    &layout,
                                    variant.into(),
                                    subst,
                                    differ,
                                );
                            }
                        };
                        let Variants::Multiple { .. } = &layout.variants else {
                            return Ok(());
                        };
                        let l_discr = self.discriminant(l, ty, wide.scalar())?;
                        let r_discr = self.discriminant(r, ty, wide.scalar())?;
                        let same = self.builder.ins().icmp(IntCC::Equal, l_discr, r_discr);
                        self.branch_unless(same, differ);
                        let done = self.builder.create_block();
                        let mut switch = Switch::new();
                        let mut arms = Vec::new();
                        for &(variant, _) in id.enum_variants(db).variants.values() {
                            if db.field_types(variant.into()).iter().next().is_none() {
                                continue;
                            }
                            let value = db.const_eval_discriminant(variant).map_err(|e| {
                                CodegenError::Frontend(format!("discriminant: {e:?}"))
                            })?;
                            let block = self.builder.create_block();
                            switch.set_entry(wide.entry(value), block);
                            arms.push((block, variant));
                        }
                        switch.emit(&mut self.builder, l_discr, done);
                        for (block, variant) in arms {
                            self.builder.switch_to_block(block);
                            // A constant of a pattern is never in such a variant, and here
                            // the two are in the same one.
                            if !self.fields_compare_in_parts(variant.into(), subst, &mut vec![ty]) {
                                self.builder.ins().jump(differ, &[]);
                                continue;
                            }
                            let index = RustcEnumVariantIdx(variant.index(db));
                            let layout = &Layout::for_variant(&layout, index);
                            self.branch_unless_fields_equal(
                                l,
                                r,
                                layout,
                                variant.into(),
                                subst,
                                differ,
                            )?;
                            self.builder.ins().jump(done, &[]);
                        }
                        self.builder.switch_to_block(done);
                        Ok(())
                    }
                }
            }
            // `NonNull` holds its pointer as a pattern type.
            TyKind::Pat(base, _) => self.branch_unless_equal(l, r, metas, base, differ),
            // A pointer to something unsized is equal in its address and its metadata.
            TyKind::RawPtr(pointee, _) if self.is_unsized(pointee) => {
                for offset in [0, meta_at()] {
                    let l = self.builder.ins().load(ptr(), MEM, l, offset);
                    let r = self.builder.ins().load(ptr(), MEM, r, offset);
                    let same = self.builder.ins().icmp(IntCC::Equal, l, r);
                    self.branch_unless(same, differ);
                }
                Ok(())
            }
            _ => {
                let scalar = self.scalar_of(ty)?;
                let l = self.builder.ins().load(scalar.ty, MEM, l, 0);
                let r = self.builder.ins().load(scalar.ty, MEM, r, 0);
                let same = if scalar.ty.is_float() {
                    self.builder.ins().fcmp(FloatCC::Equal, l, r)
                } else {
                    self.builder.ins().icmp(IntCC::Equal, l, r)
                };
                self.branch_unless(same, differ);
                Ok(())
            }
        }
    }

    /// Whether [`Self::branch_unless_equal`] can compare two values of `ty`. `seen` are the
    /// types that hold this one.
    fn compares_in_parts(&self, ty: Ty<'db>, seen: &mut Vec<Ty<'db>>) -> bool {
        let ty = self.normalize(ty);
        if seen.contains(&ty) {
            return true;
        }
        match ty.kind() {
            TyKind::Str => true,
            TyKind::Ref(_, inner, _) | TyKind::Slice(inner) | TyKind::Array(inner, _) => {
                self.compares_in_parts(inner, seen)
            }
            TyKind::Tuple(tys) => tys.iter().all(|ty| self.compares_in_parts(ty, seen)),
            TyKind::Adt(adt_def, subst) => match adt_def.def_id() {
                AdtId::UnionId(_) => false,
                // Each variant is looked at once the two are known to be in it.
                AdtId::EnumId(_) => true,
                AdtId::StructId(id) => {
                    seen.push(ty);
                    let compares = self.fields_compare_in_parts(id.into(), subst, seen);
                    seen.pop();
                    compares
                }
            },
            _ => self.scalar_of(ty).is_ok(),
        }
    }

    fn fields_compare_in_parts(
        &self,
        variant: VariantId,
        subst: GenericArgs<'db>,
        seen: &mut Vec<Ty<'db>>,
    ) -> bool {
        self.cg.db.field_types(variant).iter().all(|(_, ty)| {
            let ty = ty.ty().instantiate(self.interner, subst).skip_norm_wip();
            self.compares_in_parts(ty, seen)
        })
    }

    fn branch_unless_fields_equal(
        &mut self,
        l: Value,
        r: Value,
        layout: &Layout,
        variant: VariantId,
        subst: GenericArgs<'db>,
        differ: ir::Block,
    ) -> Result<()> {
        let field_types = self.cg.db.field_types(variant);
        for (field, ty) in field_types.iter() {
            let ty = ty.ty().instantiate(self.interner, subst).skip_norm_wip();
            let offset = layout.fields.offset(u32::from(field.into_raw()) as usize).bytes() as i64;
            let l = self.builder.ins().iadd_imm(l, offset);
            let r = self.builder.ins().iadd_imm(r, offset);
            self.branch_unless_equal(l, r, None, ty, differ)?;
        }
        Ok(())
    }

    /// Jumps to `otherwise` when `condition` is false, and carries on in a new block.
    fn branch_unless(&mut self, condition: Value, otherwise: ir::Block) {
        let next = self.builder.create_block();
        self.builder.ins().brif(condition, next, &[], otherwise, &[]);
        self.builder.switch_to_block(next);
    }

    fn drop_adt_fields(&mut self, addr: Value, meta: Option<Value>, ty: Ty<'db>) -> Result<()> {
        let db = self.cg.db;
        let TyKind::Adt(adt_def, subst) = ty.kind() else { return Ok(()) };
        {
            {
                let layout = self.layout(ty)?;
                match adt_def.def_id() {
                    AdtId::UnionId(_) => (),
                    AdtId::StructId(id) => {
                        let manually_drop = StructSignature::of(db, id)
                            .flags
                            .contains(StructFlags::IS_MANUALLY_DROP);
                        if !manually_drop {
                            self.drop_fields(addr, &layout, id.into(), subst, ty, meta)?;
                        }
                    }
                    AdtId::EnumId(id) => match &layout.variants {
                        Variants::Empty => (),
                        Variants::Single { index } => {
                            let variant = id
                                .enum_variants(db)
                                .variants
                                .values()
                                .map(|&(variant, _)| variant)
                                .find(|variant| variant.index(db) == index.0);
                            if let Some(variant) = variant {
                                self.drop_fields(addr, &layout, variant.into(), subst, ty, meta)?;
                            }
                        }
                        Variants::Multiple { tag, .. } => {
                            let wide = DiscriminantWidth::of(tag);
                            let discr = self.discriminant(addr, ty, wide.scalar())?;
                            let done = self.builder.create_block();
                            let mut switch = Switch::new();
                            let mut arms = Vec::new();
                            for &(variant, _) in id.enum_variants(db).variants.values() {
                                let value = db.const_eval_discriminant(variant).map_err(|e| {
                                    CodegenError::Frontend(format!("discriminant: {e:?}"))
                                })?;
                                let block = self.builder.create_block();
                                switch.set_entry(wide.entry(value), block);
                                arms.push((block, variant));
                            }
                            switch.emit(&mut self.builder, discr, done);
                            for (block, variant) in arms {
                                self.builder.switch_to_block(block);
                                let index = RustcEnumVariantIdx(variant.index(db));
                                let layout = &Layout::for_variant(&layout, index);
                                self.drop_fields(addr, layout, variant.into(), subst, ty, meta)?;
                                self.builder.ins().jump(done, &[]);
                            }
                            self.builder.switch_to_block(done);
                        }
                    },
                }
            }
        }
        Ok(())
    }

    fn drop_contents(&mut self, addr: Value, meta: Option<Value>, ty: Ty<'db>) -> Result<()> {
        let db = self.cg.db;
        // The future an `async fn` returns is still `impl Future` in its signature.
        let ty = self.normalize(ty);
        match ty.kind() {
            TyKind::Adt(adt_def, subst) => {
                if adt_def.is_box() {
                    // The compiler drops the pointee; `Box`'s `Drop` impl only frees the memory.
                    let pointee = subst.type_at(0);
                    let pointer = self.builder.ins().load(ptr(), MEM, addr, 0);
                    let pointee_meta = self
                        .is_unsized(pointee)
                        .then(|| self.builder.ins().load(ptr(), MEM, addr, meta_at()));
                    // The memory is freed even if dropping what it holds unwinds.
                    self.guarded(
                        |this| this.drop_value(pointer, pointee_meta, pointee),
                        |this| {
                            this.call_drop_impl(addr, meta, ty)?;
                            this.drop_adt_fields(addr, meta, ty)
                        },
                    )?;
                }
                if adt_def.destructor(self.interner).is_some() {
                    self.call_drop_impl_guarded(addr, meta, ty)?;
                }
                self.drop_adt_fields(addr, meta, ty)?;
            }
            TyKind::Tuple(tys) => self.drop_tuple_fields(addr, ty, tys.iter().collect())?,
            TyKind::Closure(..) | TyKind::CoroutineClosure(..) => {
                let Some(TyKind::Tuple(tys)) = ty.closure_upvars_ty().map(|it| it.kind()) else {
                    unsupported!("drop glue for a closure without tupled upvars");
                };
                self.drop_tuple_fields(addr, ty, tys.iter().collect())?
            }
            TyKind::Coroutine(..) => self.drop_coroutine(addr, ty)?,
            TyKind::Array(element, len) => {
                let Some(len) = try_const_usize(db, len) else {
                    unsupported!("array with unevaluated length");
                };
                let len = self.builder.ins().iconst(ptr(), len as i64);
                self.drop_elements(addr, len, element)?;
            }
            TyKind::Slice(element) => {
                let Some(len) = meta else {
                    unsupported!("drop of a slice without a length");
                };
                self.drop_elements(addr, len, element)?;
            }
            TyKind::Dynamic(..) => {
                let Some(vtable) = meta else {
                    unsupported!("drop of `dyn` without a vtable");
                };
                // The first vtable entry is `drop_in_place`, null when there is nothing to drop.
                let glue = self.builder.ins().load(ptr(), MEM, vtable, 0);
                let call = self.builder.create_block();
                let done = self.builder.create_block();
                self.builder.ins().brif(glue, call, &[], done, &[]);
                self.builder.switch_to_block(call);
                let mut sig = self.cg.make_signature();
                sig.params.push(AbiParam::new(ptr()));
                let sig = self.builder.import_signature(sig);
                self.invoke_indirect(sig, glue, &[addr]);
                self.builder.ins().jump(done, &[]);
                self.builder.switch_to_block(done);
            }
            _ => unsupported!("drop glue for {ty:?}"),
        }
        Ok(())
    }

    fn drop_fields(
        &mut self,
        addr: Value,
        layout: &Layout,
        variant: VariantId,
        subst: GenericArgs<'db>,
        container: Ty<'db>,
        meta: Option<Value>,
    ) -> Result<()> {
        let field_types = self.cg.db.field_types(variant);
        let fields: Vec<_> = field_types
            .iter()
            .map(|(field, ty)| {
                let ty = ty.ty().instantiate(self.interner, subst).skip_norm_wip();
                (layout.fields.offset(u32::from(field.into_raw()) as usize), ty)
            })
            .filter(|&(_, ty)| self.needs_drop(ty))
            .collect();
        self.drop_ladder(fields.len(), |this, at| {
            let (offset, ty) = fields[at];
            if this.is_unsized(this.normalize(ty)) {
                let field_addr = this.unsized_field_addr(addr, meta, container, offset, ty)?;
                this.drop_value(field_addr, meta, ty)
            } else {
                let field_addr = this.builder.ins().iadd_imm(addr, offset.bytes() as i64);
                let field_addr = this.aligned_for_drop(field_addr, ty, container)?;
                this.drop_value(field_addr, None, ty)
            }
        })
    }

    fn drop_tuple_fields(&mut self, addr: Value, ty: Ty<'db>, fields: Vec<Ty<'db>>) -> Result<()> {
        let layout = self.layout(ty)?;
        let fields: Vec<_> =
            fields.into_iter().enumerate().filter(|&(_, field)| self.needs_drop(field)).collect();
        self.drop_ladder(fields.len(), |this, at| {
            let (index, field) = fields[at];
            let offset = layout.fields.offset(index).bytes() as i64;
            let field_addr = this.builder.ins().iadd_imm(addr, offset);
            this.drop_value(field_addr, None, field)
        })
    }

    fn needs_drop(&self, ty: Ty<'db>) -> bool {
        let ty = self.normalize(ty);
        self.glue_of(ty) != DropGlue::None
    }

    /// Runs `drop` for each of `0..count` in turn. If one unwinds the ones after it still run,
    /// on the way out.
    fn drop_ladder(
        &mut self,
        count: usize,
        mut drop: impl FnMut(&mut Self, usize) -> Result<()>,
    ) -> Result<()> {
        if self.in_cleanup || count < 2 {
            return (0..count).try_for_each(|at| drop(self, at));
        }
        let outer = self.landing_pad_for_call();
        let pads: Vec<ir::Block> = (1..count)
            .map(|_| {
                let pad = self.builder.create_block();
                self.builder.append_block_param(pad, ptr());
                self.builder.set_cold_block(pad);
                pad
            })
            .collect();
        let enclosing = self.inner_pad;
        let mut dropped = Ok(());
        for at in 0..count {
            self.inner_pad = pads.get(at).copied().or(enclosing);
            dropped = drop(self, at);
            if dropped.is_err() {
                break;
            }
        }
        self.inner_pad = enclosing;
        dropped?;
        let Some(after) = self.builder.current_block() else {
            unsupported!("drop without a block to return to");
        };
        self.in_cleanup = true;
        let mut cleaned = Ok(());
        for at in 1..count {
            self.builder.switch_to_block(pads[at - 1]);
            let exception = self.builder.block_params(pads[at - 1])[0];
            cleaned = drop(self, at);
            if cleaned.is_err() {
                break;
            }
            match (pads.get(at), outer) {
                (Some(&next), _) | (None, Some(next)) => {
                    self.builder.ins().jump(next, &[exception.into()]);
                }
                (None, None) => cleaned = self.resume_unwinding(exception),
            }
        }
        self.in_cleanup = false;
        cleaned?;
        self.builder.switch_to_block(after);
        Ok(())
    }

    fn drop_elements(&mut self, addr: Value, len: Value, element: Ty<'db>) -> Result<()> {
        let size = self.layout(element)?.size.bytes() as i64;
        let done = self.builder.create_block();
        let zero = self.builder.ins().iconst(ptr(), 0);
        if self.in_cleanup {
            self.drop_elements_from(addr, len, element, size, zero, None, done)?;
        } else {
            // If an element's drop unwinds, the ones after it are still dropped on the way out.
            let outer = self.landing_pad_for_call();
            let pad = self.builder.create_block();
            self.builder.append_block_param(pad, ptr());
            self.builder.set_cold_block(pad);
            let index = self.drop_elements_from(addr, len, element, size, zero, Some(pad), done)?;
            self.builder.switch_to_block(pad);
            let exception = self.builder.block_params(pad)[0];
            let rest = self.builder.ins().iadd_imm(index, 1);
            let resume = self.builder.create_block();
            self.builder.set_cold_block(resume);
            self.in_cleanup = true;
            let cleaned = self.drop_elements_from(addr, len, element, size, rest, None, resume);
            self.in_cleanup = false;
            cleaned?;
            self.builder.switch_to_block(resume);
            match outer {
                Some(outer) => {
                    self.builder.ins().jump(outer, &[exception.into()]);
                }
                None => self.resume_unwinding(exception)?,
            }
        }
        self.builder.switch_to_block(done);
        Ok(())
    }

    /// Drops the elements from `start` on and jumps to `done`, unwinding to `pad` if there is
    /// one. Returns the index of the element being dropped.
    #[allow(clippy::too_many_arguments)]
    fn drop_elements_from(
        &mut self,
        addr: Value,
        len: Value,
        element: Ty<'db>,
        size: i64,
        start: Value,
        pad: Option<ir::Block>,
        done: ir::Block,
    ) -> Result<Value> {
        let header = self.builder.create_block();
        let body = self.builder.create_block();
        self.builder.append_block_param(header, ptr());
        self.builder.ins().jump(header, &[start.into()]);
        self.builder.switch_to_block(header);
        let index = self.builder.block_params(header)[0];
        let more = self.builder.ins().icmp(IntCC::UnsignedLessThan, index, len);
        self.builder.ins().brif(more, body, &[], done, &[]);
        self.builder.switch_to_block(body);
        let offset = self.builder.ins().imul_imm(index, size);
        let element_addr = self.builder.ins().iadd(addr, offset);
        let enclosing = self.inner_pad;
        if pad.is_some() {
            self.inner_pad = pad;
        }
        let dropped = self.drop_value(element_addr, None, element);
        self.inner_pad = enclosing;
        dropped?;
        let next = self.builder.ins().iadd_imm(index, 1);
        self.builder.ins().jump(header, &[next.into()]);
        Ok(index)
    }

    /// Calls `<ty as Drop>::drop` on the value at `addr`, whose metadata is `meta` when the
    /// type is unsized.
    fn call_drop_impl(&mut self, addr: Value, meta: Option<Value>, ty: Ty<'db>) -> Result<()> {
        let Some(drop_fn) = self.interner.lang_items().Drop_drop else {
            unsupported!("no `Drop::drop` lang item");
        };
        let reference =
            Ty::new_ref(self.interner, Region::new_erased(self.interner), ty, Mutability::Mut);
        let slot = self.scratch_pair();
        self.builder.ins().store(MEM, addr, slot, 0);
        if let Some(meta) = meta {
            self.builder.ins().store(MEM, meta, slot, meta_at());
        }
        let args = GenericArgs::new_from_slice(&[ty.into()]);
        let unit = Ty::new_unit(self.interner);
        self.call_direct(drop_fn, args, &[(slot, reference)], slot, unit)
    }

    fn finish_call(
        &mut self,
        destination: &StoredPlace,
        target: Option<BasicBlockId>,
    ) -> Result<()> {
        let _ = destination;
        match target {
            Some(target) => self.builder.ins().jump(self.blocks[target], &[]),
            None => self.builder.ins().trap(TrapCode::user(2).expect("non-zero trap code")),
        };
        Ok(())
    }
}

/// Prints a body one statement a line, for debugging with `RA_JIT_DUMP_MIR`.
fn dump_mir(body: &MirBody<'_>) {
    let compact = |text: String| {
        let mut text = text
            .replace("Idx::<Local>(", "_(")
            .replace("Idx::<BasicBlock>(", "bb(")
            .replace(", projection: [] }", " }")
            .replace("StoredPlace { local: ", "{ ")
            .replace(", span: None }", " }")
            .replace("Operand { kind: ", "{ ");
        while let Some(start) = text.find(", span: Some(") {
            let end = text[start..].find("))").map_or(text.len(), |end| start + end + 2);
            text.replace_range(start..end, "");
        }
        text
    };
    for (local, data) in body.locals.iter() {
        eprintln!("  let {}: {:?}", compact(format!("{local:?}")), data.ty);
    }
    for (id, block) in body.basic_blocks.iter() {
        eprintln!("{}:", compact(format!("{id:?}")));
        for statement in &block.statements {
            eprintln!("    {}", compact(format!("{:?}", statement.kind)));
        }
        if let Some(terminator) = &block.terminator {
            eprintln!("    -> {}", compact(format!("{:?}", terminator.kind)));
        }
    }
}

/// The locals that can be kept in a register as far as the body goes: temporaries written
/// whole by assignments only, that nothing takes the address of.
fn register_locals(body: &MirBody<'_>, forwarded: &FxHashMap<LocalId, LocalId>) -> Vec<LocalId> {
    let whole = |place: &StoredPlace| place.projection.as_slice().is_empty().then_some(place.local);
    // Every mention of the whole local, and the ones that are a value read or an assignment.
    let mut mentions: FxHashMap<LocalId, (usize, usize)> = FxHashMap::default();
    body.clone().walk_places(|place| {
        if let Some(local) = whole(place) {
            mentions.entry(local).or_default().0 += 1;
        }
    });
    let mut plain = |place: &StoredPlace| {
        if let Some(local) = whole(place) {
            mentions.entry(local).or_default().1 += 1;
        }
    };
    let read = |operand: &Operand, plain: &mut dyn FnMut(&StoredPlace)| {
        if let OperandKind::Copy(place) | OperandKind::Move(place) = &operand.kind {
            plain(place);
        }
    };
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(to, rvalue) = &statement.kind {
                plain(to);
                for operand in operands_of(rvalue) {
                    read(operand, &mut plain);
                }
            }
        }
        match block.terminator.as_ref().map(|it| &it.kind) {
            Some(TerminatorKind::SwitchInt { discr, .. }) => read(discr, &mut plain),
            Some(TerminatorKind::Call { func, args, .. }) => {
                read(func, &mut plain);
                for arg in args.iter() {
                    read(arg, &mut plain);
                }
            }
            _ => (),
        }
    }
    let bound: rustc_hash::FxHashSet<LocalId> = body
        .binding_locals
        .values()
        .chain(&body.param_locals)
        .copied()
        .chain(body.promoted_locals.iter().map(|it| it.0))
        .chain([returned_local(body)])
        .chain(forwarded.iter().flat_map(|(&temp, &to)| [temp, to]))
        .collect();
    body.locals
        .iter()
        .map(|(local, _)| local)
        .filter(|local| {
            !bound.contains(local) && mentions.get(local).is_none_or(|(all, plain)| all == plain)
        })
        .collect()
}

/// The temporaries a `let` is initialized through, each with the binding it is copied to right
/// after it is written: `let buffer = [0u8; N]` builds the array in a temporary and copies it.
/// The two can be one slot, the binding holding nothing yet when the temporary is written, and
/// a frame is not twice the size of what it declares.
fn forwarded_temps(body: &MirBody<'_>) -> FxHashMap<LocalId, LocalId> {
    let whole = |place: &StoredPlace| place.projection.as_slice().is_empty().then_some(place.local);
    let mut forwarded = FxHashMap::default();
    if body.coroutine.is_some() {
        return forwarded;
    }
    for (_, block) in body.basic_blocks.iter() {
        // The temporary written last, by a statement of this block or by the call that led
        // here, and whether the binding's storage has started since.
        let mut written = None;
        for statement in &block.statements {
            match &statement.kind {
                StatementKind::StorageLive(local) => {
                    if let Some((_, live)) = &mut written {
                        *live = Some(*local);
                    }
                }
                StatementKind::FakeRead(_) | StatementKind::Nop => (),
                StatementKind::Assign(
                    to,
                    Rvalue::Use(Operand {
                        kind: OperandKind::Copy(from) | OperandKind::Move(from),
                        ..
                    }),
                ) if let Some((temp, live)) = written
                    && whole(from) == Some(temp)
                    && whole(to).is_some()
                    && whole(to) == live =>
                {
                    forwarded.insert(temp, to.local);
                    written = None;
                }
                // A constant written to another local in between, as a drop flag is.
                StatementKind::Assign(
                    to,
                    Rvalue::Use(Operand {
                        kind: OperandKind::Constant { .. } | OperandKind::Allocation { .. },
                        ..
                    }),
                ) if let Some((temp, live)) = written
                    && let Some(to) = whole(to)
                    && to != temp
                    && Some(to) != live => {}
                StatementKind::Assign(to, _) => written = whole(to).map(|it| (it, None)),
                _ => written = None,
            }
        }
        // FIXME: a temporary a call returns into is copied in the block the call leads to.
    }
    if forwarded.is_empty() {
        return forwarded;
    }
    // Written once and read once, and by nothing else: the binding is not what it is made of.
    let mut mentions: FxHashMap<LocalId, usize> = FxHashMap::default();
    body.clone().walk_places(|place| *mentions.entry(place.local).or_default() += 1);
    let fake_reads = |local: LocalId| {
        body.basic_blocks
            .iter()
            .flat_map(|(_, block)| &block.statements)
            .filter(|it| matches!(&it.kind, StatementKind::FakeRead(place) if place.local == local))
            .count()
    };
    // A drop of the temporary is not a use: it holds a value only between the two statements,
    // which unwind to nowhere, and a drop of it then finds the slot holding its value.
    let drops = |local: LocalId| {
        body.basic_blocks
            .iter()
            .filter_map(|(_, block)| block.terminator.as_ref())
            .filter(
                |it| matches!(&it.kind, TerminatorKind::Drop { place, .. } if place.local == local),
            )
            .count()
    };
    let bound: rustc_hash::FxHashSet<LocalId> = body
        .binding_locals
        .values()
        .chain(&body.param_locals)
        .copied()
        .chain(body.promoted_locals.iter().map(|it| it.0))
        .chain([returned_local(body)])
        .collect();
    forwarded.retain(|&temp, to| {
        !bound.contains(&temp)
            && mentions[&temp] - fake_reads(temp) - drops(temp) == 2
            && body.locals[temp].ty == body.locals[*to].ty
            && !body.promoted_locals.iter().any(|it| it.0 == *to)
            && *to != returned_local(body)
    });
    forwarded
}

/// The operands an rvalue reads.
fn operands_of(rvalue: &Rvalue) -> Vec<&Operand> {
    match rvalue {
        Rvalue::UnaryOp(_, it)
        | Rvalue::Cast(_, it, _)
        | Rvalue::Repeat(it, _)
        | Rvalue::Use(it) => {
            vec![it]
        }
        Rvalue::CheckedBinaryOp(_, left, right) => vec![left, right],
        Rvalue::Aggregate(_, operands) => operands.iter().collect(),
        _ => Vec::new(),
    }
}
