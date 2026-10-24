//! The borrow checker: rustc's `rustc_borrowck` over the bodies MIR lowering builds.
//!
//! Each body is checked on its own, closures and coroutines with it, as rustc checks a body
//! and the closures in it together, and the result is a query of the body's owner, so an edit checks again
//! only the bodies it changes. What it reports carries rustc's error codes and the span rustc
//! points at; `docs/borrowck.md` in the repository describes what is checked and how.

extern crate ra_ap_rustc_ast_ir as rustc_ast_ir;
extern crate ra_ap_rustc_type_ir as rustc_type_ir;

mod bitset;
mod cfg;
mod check;
mod liveness;
mod loans;
mod moves;
mod names;
mod polonius;
mod regions;

use std::sync::LazyLock;

use base_db::Crate;
use hir_def::{
    DefWithBodyId, HasModule,
    expr_store::ExpressionStore,
    hir::{BindingAnnotation, BindingId, ExprId},
};
use rustc_hash::FxHashMap;
use salsa::SalsaValue;

use hir_ty::{
    InferBodyId, InferenceResult,
    closure_analysis::CapturedPlace,
    db::{HirDatabase, InternedCoroutineId},
    next_solver::{
        DbInterner, ParamEnv, Ty, TypingMode,
        infer::{DbInternerInferExt, InferCtxt},
    },
};

use hir_ty::mir::{
    LocalId, MirBody, MirSpan, Operand, OperandKind, PlaceElem, PlaceTy, StoredPlace,
};

/// Which of rustc's borrow checkers to match: the one stable Rust has, or the location
/// sensitive one nightly Rust has by default (`-Zpolonius=next`), which accepts more.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Nll,
    Polonius,
    Off,
}

/// `RA_JIT_BORROWCK`: `polonius` (the default), `nll` or `off`.
pub fn mode() -> Mode {
    static MODE: LazyLock<Mode> =
        LazyLock::new(|| match std::env::var("RA_JIT_BORROWCK").as_deref() {
            Ok("nll") => Mode::Nll,
            Ok("off" | "0") => Mode::Off,
            _ => Mode::Polonius,
        });
    *MODE
}

/// `RA_JIT_BORROWCK_DEBUG=1` prints the regions, constraints and loans of every body checked,
/// `RA_JIT_BORROWCK_DEBUG=name` those of the functions called `name` and their closures.
pub(crate) fn debugging(db: &dyn HirDatabase, owner: InferBodyId<'_>) -> bool {
    let Ok(wanted) = std::env::var("RA_JIT_BORROWCK_DEBUG") else { return false };
    match owner {
        InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(it)) if wanted != "1" => {
            hir_def::signatures::FunctionSignature::of(db, it).name.as_str() == wanted
        }
        _ => wanted == "1",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SalsaValue)]
pub struct BorrowckDiagnostic {
    /// rustc's error code, as `E0382`, or `unused_mut` for the lint the borrow checker gives.
    pub code: &'static str,
    pub span: MirSpan,
    pub message: String,
    /// Reported at the function's signature rather than at `span`, as rustc reports an
    /// `impl Trait`'s hidden type.
    pub at_signature: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, SalsaValue)]
pub struct BorrowckResult {
    pub diagnostics: Vec<BorrowckDiagnostic>,
    /// Why the body, or a closure in it, was not checked: MIR lowering does not support
    /// something in it.
    pub skipped: Option<String>,
}

/// The errors rustc's borrow checker reports in `def` and the closures in it.
#[salsa::tracked(returns(ref))]
pub fn borrowck<'db>(db: &'db dyn HirDatabase, def: DefWithBodyId) -> BorrowckResult {
    let mode = mode();
    let mut result = BorrowckResult::default();
    if mode == Mode::Off {
        return result;
    }
    let owner = InferBodyId::DefWithBodyId(def);
    let infer = InferenceResult::of(db, owner);
    if infer.has_type_mismatches() || infer.is_erroneous() || !infer.diagnostics().is_empty() {
        return result;
    }
    // rustc borrow checks no body its lowering or type check found an error in, which an
    // error of either taints: a cast that is not one, `..base` in a destructuring assignment.
    let (_, source_map) = hir_def::expr_store::Body::with_source_map(db, def);
    if source_map.diagnostics().iter().any(|it| {
        !matches!(it, hir_def::expr_store::ExpressionStoreDiagnostics::InactiveCode { .. })
    }) {
        return result;
    }
    // A `static` of a type that is not `Sized` is rustc's E0277, which keeps it from being
    // borrow checked; a `const` is checked all the same.
    if let DefWithBodyId::StaticId(it) = def
        && let Some(ty) = db.value_ty(it.into())
        && {
            use hir_ty::next_solver::TyKind;
            use rustc_type_ir::inherent::IntoKind;
            matches!(
                ty.instantiate_identity().skip_norm_wip().kind(),
                TyKind::Str | TyKind::Slice(_) | TyKind::Dynamic(..)
            )
        }
    {
        return result;
    }
    let body = match db.mir_body(owner) {
        Ok(body) => body,
        Err(error) => {
            result.skipped = Some(format!("{error:?}"));
            return result;
        }
    };
    let store = ExpressionStore::of(db, owner.expression_store_owner(db));
    let mut bodies = vec![(body, None, None)];
    let mut index = 0;
    while let Some(&(body, _, _)) = bodies.get(index) {
        index += 1;
        for &closure in &body.closures {
            match db.mir_body_for_closure(closure) {
                Ok(it) => bodies.push((it, Some(closure.loc(db).expr), None)),
                Err(error) => {
                    result.skipped.get_or_insert_with(|| format!("closure: {error:?}"));
                }
            }
        }
        // The body of an `async` block or `async fn`, checked as rustc checks a coroutine: as
        // a closure of the body that makes it.
        for coroutine in coroutines(body) {
            match db.mir_body_for_coroutine(coroutine) {
                Ok(it) => bodies.push((it, Some(coroutine.loc(db).expr), None)),
                Err(error) => {
                    result.skipped.get_or_insert_with(|| format!("coroutine: {error:?}"));
                }
            }
        }
        // A `const { .. }` block, which MIR lowering makes a constant of its own, checked with
        // the body as rustc checks an inline const.
        for konst in inline_consts(db, body) {
            match db.mir_body(InferBodyId::AnonConstId(konst)) {
                Ok(it) => bodies.push((it, None, Some(konst.loc(db).expr))),
                Err(error) => {
                    result.skipped.get_or_insert_with(|| format!("inline const: {error:?}"));
                }
            }
        }
    }
    // A closure after the closures in it and before the body that makes it, which proves what
    // the closure needs of it.
    let mut requirements = FxHashMap::default();
    let mut mut_upvars = FxHashMap::default();
    let mut diagnostics = Vec::new();
    for (body, closure, inline_const) in bodies.into_iter().rev() {
        let mut ctx = Ctx::new(db, owner, store, infer, body, closure, mode);
        ctx.inline_const = inline_const.is_some();
        if closure.is_none() && inline_const.is_none() && drops_in_const(&ctx, def, body) {
            return result;
        }
        let mut found = Vec::new();
        let (count, needs, used_mut_upvars) =
            check::check(&ctx, body, &requirements, &mut_upvars, &mut found);
        if ctx.tainted.get() {
            return result;
        }
        diagnostics.push(found);
        if let Some(nested) = closure.or(inline_const) {
            requirements.insert(nested, (count, needs));
            mut_upvars.insert(nested, used_mut_upvars);
        }
    }
    result.diagnostics.extend(diagnostics.into_iter().rev().flatten());
    if debugging(db, owner) {
        for it in &result.diagnostics {
            eprintln!("diagnostic {it:?}");
        }
    }
    result
}

/// rustc's E0493, from checking a const context before borrow checking it: a `const` or
/// `static` item or a `const fn` drops a value of a type with drop glue, and rustc does not
/// borrow check it. What is moved out of, in part too, is taken not to be dropped.
fn drops_in_const(ctx: &Ctx<'_, '_>, def: DefWithBodyId, body: &MirBody<'_>) -> bool {
    use hir_ty::mir::{OperandKind, TerminatorKind};
    let const_context = match def {
        DefWithBodyId::ConstId(_) | DefWithBodyId::StaticId(_) => true,
        DefWithBodyId::FunctionId(it) => {
            hir_def::signatures::FunctionSignature::of(ctx.db, it).is_const()
        }
        _ => false,
    };
    if !const_context {
        return false;
    }
    let mut moved = rustc_hash::FxHashSet::default();
    let mut visit = |operand: &hir_ty::mir::Operand| {
        if let OperandKind::Move(place) = &operand.kind {
            moved.insert(place.local);
        }
    };
    for (_, block) in body.basic_blocks.iter() {
        for it in &block.statements {
            it.kind.for_each_operand(&mut visit);
        }
        if let Some(terminator) = &block.terminator {
            terminator.kind.for_each_operand(&mut visit);
        }
    }
    body.basic_blocks.iter().any(|(_, block)| {
        let Some(terminator) = &block.terminator else { return false };
        let TerminatorKind::Drop { place, .. } = &terminator.kind else { return false };
        place.projection.as_slice().is_empty()
            && place.local != return_slot()
            && !moved.contains(&place.local)
            && hir_ty::drop::has_drop_glue(
                &ctx.infcx,
                body.locals[place.local].ty.as_ref(),
                ctx.env,
            ) == hir_ty::drop::DropGlue::HasDropGlue
    })
}

/// The coroutines `body` makes, in the order it makes them.
fn coroutines<'db>(body: &MirBody<'db>) -> Vec<InternedCoroutineId<'db>> {
    use hir_ty::mir::{AggregateKind, Rvalue, StatementKind};
    use hir_ty::next_solver::TyKind;
    use rustc_type_ir::inherent::IntoKind;
    let mut out = Vec::new();
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(_, Rvalue::Aggregate(AggregateKind::Closure(ty), _)) =
                &statement.kind
                && let TyKind::Coroutine(id, _) = ty.as_ref().kind()
                && !out.contains(&id.0)
            {
                out.push(id.0);
            }
        }
    }
    out
}

/// The `const { .. }` blocks `body` evaluates, in the order it does.
fn inline_consts<'db>(
    db: &'db dyn HirDatabase,
    body: &MirBody<'db>,
) -> Vec<hir_ty::db::AnonConstId<'db>> {
    use hir_ty::db::GeneralConstId;
    use hir_ty::mir::{OperandKind, Rvalue, StatementKind};
    use hir_ty::next_solver::ConstKind;
    use rustc_type_ir::inherent::IntoKind;
    let mut out = Vec::new();
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(_, Rvalue::Use(operand)) = &statement.kind
                && let OperandKind::Constant { konst, .. } = &operand.kind
                && let ConstKind::Unevaluated(unevaluated) = konst.as_ref().kind()
                && let GeneralConstId::AnonConstId(id) = unevaluated.def.0
                && id.is_inline_const(db)
                && !out.contains(&id)
            {
                out.push(id);
            }
        }
    }
    out
}

/// What the checks ask of the types and the source of a body.
pub(crate) struct Ctx<'a, 'db> {
    pub(crate) db: &'db dyn HirDatabase,
    pub(crate) owner: InferBodyId<'db>,
    pub(crate) infcx: InferCtxt<'db>,
    pub(crate) env: ParamEnv<'db>,
    pub(crate) krate: Crate,
    pub(crate) store: &'a ExpressionStore,
    #[allow(dead_code)]
    pub(crate) infer: &'a InferenceResult<'db>,
    #[allow(dead_code)]
    pub(crate) mode: Mode,
    /// The user variable each local is, if it is one.
    pub(crate) bindings: FxHashMap<LocalId, BindingId>,
    /// In the body of a closure, its captures, by field of the closure.
    pub(crate) upvars: Vec<Upvar>,
    /// In the body of a closure, how it is called.
    pub(crate) closure_kind: Option<rustc_type_ir::ClosureKind>,
    pub(crate) closure_local: Option<LocalId>,
    /// The closure or `async` block this is the body of.
    pub(crate) closure_expr: Option<ExprId>,
    /// This is the body of a `const { .. }` block, whose type's lifetimes are its function's.
    pub(crate) inline_const: bool,
    /// For each expression in a `match` arm's guard, the variables the arm's pattern binds:
    /// rustc binds them by reference in the guard, so it cannot move out of them.
    pub(crate) guard_bindings: FxHashMap<ExprId, Vec<BindingId>>,
    /// A where clause of a call does not hold, rustc's E0277, which rust-analyzer does not
    /// report: rustc does not borrow check a body whose type check fails.
    pub(crate) tainted: std::cell::Cell<bool>,
}

fn guard_bindings(store: &ExpressionStore) -> FxHashMap<ExprId, Vec<BindingId>> {
    let mut out: FxHashMap<ExprId, Vec<BindingId>> = FxHashMap::default();
    for (_, expr) in store.exprs() {
        let hir_def::hir::Expr::Match { arms, .. } = expr else { continue };
        for arm in arms.iter() {
            let Some(guard) = arm.guard else { continue };
            let mut bindings = Vec::new();
            store.walk_bindings_in_pat(arm.pat, |it| bindings.push(it));
            if bindings.is_empty() {
                continue;
            }
            let mut stack = vec![guard];
            while let Some(expr) = stack.pop() {
                out.entry(expr).or_default().extend(bindings.iter().copied());
                store.walk_child_exprs(expr, |it| stack.push(it));
            }
        }
    }
    out
}

/// A capture of a closure, in its body.
pub(crate) struct Upvar {
    pub(crate) binding: BindingId,
    /// The local lowering copies the capture into.
    pub(crate) local: LocalId,
    pub(crate) by_ref: bool,
    pub(crate) mutable: bool,
    /// The captured place is behind a reference.
    pub(crate) behind_deref: bool,
    pub(crate) place: CapturedPlace,
}

impl<'a, 'db> Ctx<'a, 'db> {
    fn new(
        db: &'db dyn HirDatabase,
        owner: InferBodyId<'db>,
        store: &'a ExpressionStore,
        infer: &'a InferenceResult<'db>,
        body: &MirBody<'db>,
        closure: Option<ExprId>,
        mode: Mode,
    ) -> Self {
        let krate = owner.krate(db);
        let interner = DbInterner::new_with(db, krate).with_regions();
        let env = normalized_env(interner, db.trait_environment(owner.generic_def(db)));
        let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
        let bindings =
            body.binding_locals.iter().map(|(binding, &local)| (local, binding)).collect();
        let mut upvars = Vec::new();
        let mut closure_kind = None;
        let mut closure_local = None;
        if let Some(expr) = closure {
            closure_kind = closure_kind_of(infer, expr);
            closure_local = body.param_locals.first().copied();
            if let Some(data) = infer.closures_data.get(&expr) {
                let mut locals: Vec<LocalId> =
                    body.upvar_locals.values().flatten().map(|(local, _)| *local).collect();
                locals.sort_by_key(|it| it.into_raw().into_u32());
                for (capture, local) in data.min_captures.values().flatten().zip(locals) {
                    upvars.push(Upvar {
                        binding: capture.captured_local(),
                        local,
                        by_ref: capture.is_by_ref(),
                        mutable: capture.mutability == rustc_ast_ir::Mutability::Mut,
                        behind_deref: capture.place.deref_tys().any(|ty| {
                            !matches!(
                                rustc_type_ir::inherent::IntoKind::kind(ty),
                                hir_ty::next_solver::TyKind::Adt(def, _) if def.is_box()
                            )
                        }),
                        place: capture.clone(),
                    });
                }
            }
        }
        Ctx {
            db,
            owner,
            infcx,
            env,
            krate,
            store,
            infer,
            mode,
            bindings,
            upvars,
            closure_kind,
            closure_local,
            closure_expr: closure,
            inline_const: false,
            guard_bindings: guard_bindings(store),
            tainted: std::cell::Cell::new(false),
        }
    }

    /// Whether reading the discriminant of `place` at `span` is a cast of an enum, which moves
    /// it, as rustc casts the enum value itself (`*e as usize` is a move out of `*e`).
    pub(crate) fn cast_moves(
        &self,
        body: &MirBody<'db>,
        place: &StoredPlace,
        span: MirSpan,
    ) -> bool {
        matches!(span, MirSpan::ExprId(expr) if matches!(self.store[expr], hir_def::hir::Expr::Cast { .. }))
            && !self.is_copy(self.place_ty(body, place.local, place.projection.as_slice()).ty)
    }

    /// Whether moving `local` with `projection` at `span` moves out of a variable its `match`
    /// arm's guard is being evaluated for (rustc's "cannot move out of `x` in pattern guard").
    pub(crate) fn moves_out_of_guard(
        &self,
        local: LocalId,
        projection: &[PlaceElem],
        span: MirSpan,
    ) -> bool {
        let MirSpan::ExprId(expr) = span else { return false };
        let Some(binding) = self.bindings.get(&local) else { return false };
        !projection.iter().any(|it| matches!(it, hir_ty::mir::ProjectionElem::Deref))
            && self.guard_bindings.get(&expr).is_some_and(|it| it.contains(binding))
    }

    pub(crate) fn place_ty(
        &self,
        body: &MirBody<'db>,
        local: LocalId,
        projection: &[PlaceElem],
    ) -> PlaceTy<'db> {
        PlaceTy::from_ty(body.locals[local].ty.as_ref()).multi_projection_ty(
            &self.infcx,
            self.env,
            projection,
        )
    }

    pub(crate) fn project(&self, ty: PlaceTy<'db>, elem: &PlaceElem) -> PlaceTy<'db> {
        ty.projection_ty(&self.infcx, elem, self.env)
    }

    /// Whether a value of `ty` neither needs dropping nor has interior mutability, which a
    /// constant must to be promoted.
    pub(crate) fn is_plain(&self, ty: Ty<'db>) -> bool {
        hir_ty::drop::has_drop_glue(&self.infcx, ty, self.env) == hir_ty::drop::DropGlue::None
            && hir_ty::drop::is_freeze(self.db, self.infcx.interner, ty)
    }

    pub(crate) fn has_destructor(&self, adt: hir_def::AdtId) -> bool {
        hir_ty::drop::destructor(self.db, adt).is_some()
    }

    pub(crate) fn is_copy(&self, ty: Ty<'db>) -> bool {
        self.infcx.type_is_copy_modulo_regions(self.env, ty)
    }

    /// The place `operand` moves out of. Lowering says a value a closure captures by value is
    /// moved even when its type is `Copy`.
    pub(crate) fn moved_place<'o>(
        &self,
        body: &MirBody<'db>,
        operand: &'o Operand,
    ) -> Option<&'o StoredPlace> {
        let OperandKind::Move(place) = &operand.kind else { return None };
        let ty = self.place_ty(body, place.local, place.projection.as_slice()).ty;
        (!self.is_copy(ty)).then_some(place)
    }

    /// Whether `local` is a variable declared without `mut`, which can only be assigned once.
    pub(crate) fn is_immutable(&self, local: LocalId) -> bool {
        if self.upvars.iter().any(|it| it.local == local) {
            return false;
        }
        match self.bindings.get(&local) {
            Some(&binding) => self.store[binding].mode != BindingAnnotation::Mutable,
            None => false,
        }
    }

    pub(crate) fn upvar_of_local(&self, local: LocalId) -> Option<usize> {
        self.upvars.iter().position(|it| it.local == local)
    }
}

/// `env` with the projections in its where clauses that name no parameter normalized, as
/// rustc normalizes a param env before it uses it. Unnormalized, a where clause
/// `<Option<&'static C> as Q>::ReadOnly: Q` is a candidate for every `X: Q`, and normalizing it
/// asks those where clauses again: the solver gives up and leaves `<X as Q>::Item` rigid.
fn normalized_env<'db>(interner: DbInterner<'db>, env: ParamEnv<'db>) -> ParamEnv<'db> {
    use hir_ty::next_solver::{
        Clauses, infer::traits::ObligationCause, obligation_ctxt::ObligationCtxt,
    };
    use rustc_type_ir::TypeVisitableExt as _;
    let global = |clause: &hir_ty::next_solver::Clause<'db>| {
        clause.has_aliases() && !clause.has_param() && !clause.has_infer()
    };
    if !env.clauses().iter().any(|it| global(&it)) {
        return env;
    }
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let clauses = env.clauses().iter().map(|clause| {
        if !global(&clause) {
            return clause;
        }
        let ocx = ObligationCtxt::new(&infcx);
        match ocx.deeply_normalize(&ObligationCause::dummy(), ParamEnv::empty(), clause) {
            Ok(normalized) if !normalized.has_infer() => normalized,
            _ => clause,
        }
    });
    ParamEnv::new(Clauses::new_from_iter(clauses.collect::<Vec<_>>()))
}

fn closure_kind_of(
    infer: &InferenceResult<'_>,
    expr: ExprId,
) -> Option<rustc_type_ir::ClosureKind> {
    use rustc_type_ir::inherent::{GenericArgs as _, IntoKind};
    match infer.expr_ty(expr).kind() {
        hir_ty::next_solver::TyKind::Closure(_, args) => Some(args.as_closure().kind()),
        hir_ty::next_solver::TyKind::CoroutineClosure(_, args) => {
            Some(args.as_coroutine_closure().kind())
        }
        _ => None,
    }
}

/// The local a body returns in, as `hir_ty::mir`'s own private `return_slot`.
pub(crate) fn return_slot() -> LocalId {
    LocalId::from_raw(la_arena::RawIdx::from(0))
}

/// Whether `rvalue` compares, which takes its operands by reference whatever their type:
/// rust-analyzer lowers a constant pattern of a type that is not `Copy` to `==` on the value.
pub(crate) fn compares(rvalue: &hir_ty::mir::Rvalue) -> bool {
    use hir_ty::mir::{BinOp, Rvalue};
    match rvalue {
        Rvalue::BinaryOp(op, ..) | Rvalue::CheckedBinaryOp(op, ..) => {
            matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge)
        }
        _ => false,
    }
}
