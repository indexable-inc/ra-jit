//! Regions, as rustc's `renumber.rs`, `universal_regions.rs`, `type_check` and
//! `region_infer` in `rustc_borrowck`.
//!
//! Every region in the types of a body becomes a variable. The regions of the signature are
//! the universal ones: they hold on all of the body and past its end. Relating the types each
//! statement puts together says which regions outlive which; liveness says at which points a
//! region must hold; propagating both gives each region the points it holds at, which is
//! where a loan of that region is in scope. A universal region made to outlive another the
//! signature does not say it outlives is rustc's "lifetime may not live long enough".

use hir_def::{
    AdtId, CallableDefId, DefWithBodyId, GenericDefId, ItemContainerId, Lookup as _,
    hir::{Pat, Statement},
    resolver::HasResolver as _,
    signatures::FunctionSignature,
};
use rustc_hash::{FxHashMap, FxHashSet};
use rustc_type_ir::{
    ClauseKind, OutlivesPredicate, RegionKind, Variance,
    inherent::{
        GenericArgs as _, IntoKind, ParamEnv as _, Region as _, SliceLike as _, Term as _, Ty as _,
    },
};

use hir_ty::{
    GenericPredicates, InferBodyId,
    db::GeneralConstId,
    mir::{
        AggregateKind, BinOp, BorrowKind, CastKind, LocalId, MirBody, MirSpan, Operand,
        OperandKind, PlaceElem, PlaceTy, ProjectionElem, Rvalue, StatementKind, TerminatorKind,
    },
    next_solver::{Clause, ConstKind, GenericArgKind, GenericArgs, Region, Ty, TyKind},
};

use super::{
    Ctx,
    bitset::BitSet,
    cfg::{Cfg, Location},
};

/// A region variable.
pub(super) type Rv = usize;

/// Inference variables for lowering a written type in a body, `_` and lifetimes left out.
struct InferVars<'a, 'db>(&'a hir_ty::next_solver::infer::InferCtxt<'db>);
impl<'db> hir_ty::TyLoweringInferVarsCtx<'db> for InferVars<'_, 'db> {
    fn next_ty_var(&mut self, span: hir_ty::Span) -> Ty<'db> {
        self.0.next_ty_var(span)
    }
    fn next_const_var(&mut self, span: hir_ty::Span) -> hir_ty::next_solver::Const<'db> {
        self.0.next_const_var(span)
    }
    fn next_region_var(&mut self, span: hir_ty::Span) -> Region<'db> {
        self.0.next_region_var(span)
    }
}

/// A `let` with a type written out, in the block it is a statement of.
#[derive(Clone, Copy)]
struct Annotation {
    pat: hir_def::hir::PatId,
    type_ref: hir_def::type_ref::TypeRefId,
    initializer: Option<hir_def::hir::ExprId>,
    block: hir_def::hir::ExprId,
}

/// Why one region outlives another, as rustc's `ConstraintCategory`; the most interesting
/// first, which is the order errors pick the constraint to point at by.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) enum Category {
    Return,
    TypeAnnotation,
    Cast,
    CallArgument,
    Assignment,
    Usage,
    Predicate,
    Boring,
    Internal,
    /// What normalizing the signature needs, which rustc points at the signature.
    Signature,
    /// A region of a returned `impl Trait`'s hidden type outliving the lifetime chosen for
    /// it, rustc's `OpaqueType`.
    Opaque,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Outlives {
    pub(super) sup: Rv,
    pub(super) sub: Rv,
    pub(super) location: Option<Location>,
    pub(super) category: Category,
    pub(super) span: MirSpan,
    /// A closure's requirement, proven where the closure is made.
    pub(super) from_closure: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum UniversalKind {
    Static,
    /// The body itself: every universal region outlives it.
    FnBody,
    /// A lifetime parameter.
    Named,
    /// An elided lifetime of a parameter.
    AnonParam,
    /// Any other region of the signature, as an elided lifetime of the return type that
    /// elision gives no lifetime to.
    Anon,
    /// In a closure, a region of the closure's type: one of the body that creates it, which
    /// rustc's `UniversalRegions` calls external. The closure's own, the late-bound lifetimes
    /// of its signature and the borrow of its environment, are `AnonParam`.
    Creator,
    /// A lifetime of a `for<'a>` the trait solver proves something for, as rustc's
    /// placeholders: any lifetime at all, so it outlives only itself and is outlived by
    /// `'static`.
    Placeholder,
}

/// The position among a closure's creator's regions of the anonymous lifetime 0 of the impl
/// its enclosing method is in (`impl Foo<'_>`), past those of the closure's type.
pub(super) const IMPL_ANON: usize = 1 << 20;

/// The position among a closure's creator's regions of the enclosing item's late-bound
/// lifetime 0.
pub(super) const LATE_PARENT: usize = 1 << 24;

/// In a `const { .. }` block, the position of its function's lifetime parameter at `index`:
/// [`EARLY_PARENT`] + `index`.
pub(super) const EARLY_PARENT: usize = 1 << 28;

/// What a closure needs of its creator's regions that it cannot prove itself, as rustc's
/// `ClosureOutlivesRequirement`: that the region at position `longer` among the regions of
/// the closure's type outlives the one at `shorter`, or `'static` for `None`. With a `subject`,
/// that type parameter outlives it instead, a type test the creator proves.
#[derive(Clone, Copy, Debug)]
pub(super) struct Requirement<'db> {
    pub(super) subject: Option<Ty<'db>>,
    pub(super) longer: usize,
    pub(super) shorter: Option<usize>,
    pub(super) category: Category,
    pub(super) span: MirSpan,
}

/// That the type parameter `ty` must outlive `lower`, as rustc's `TypeTest`.
pub(super) struct TypeTest<'db> {
    pub(super) ty: Ty<'db>,
    pub(super) lower: Rv,
    pub(super) span: MirSpan,
    /// From a `for<'a> T: 'a` bound, which rustc reports without a code.
    pub(super) forall: bool,
}

pub(super) struct Regions<'db> {
    pub(super) universals: Vec<(Rv, UniversalKind)>,
    universal_index: FxHashMap<Rv, usize>,
    pub(super) static_rv: Rv,
    /// The universal region of each early-bound lifetime parameter, by its index.
    pub(super) early: FxHashMap<u32, Rv>,
    /// Per universal region, the universal regions it is known to outlive.
    known: Vec<BitSet>,
    /// The type parameters known to outlive a region.
    pub(super) type_bounds: Vec<(Ty<'db>, Rv)>,
    /// The type of each local, its regions variables.
    pub(super) local_tys: Vec<Ty<'db>>,
    /// The region of the loan each borrow makes.
    pub(super) loan_regions: FxHashMap<Location, Rv>,
    pub(super) outlives: Vec<Outlives>,
    pub(super) type_tests: Vec<TypeTest<'db>>,
    /// Whether universal regions are checked against each other: not in a closure whose
    /// signature could not be split into its own regions and its creator's.
    pub(super) check_universals: bool,
    /// The body is a method of a trait impl, whose signature the trait fixes: rustc's
    /// `is_bound_region_in_impl_item`, which keeps E0621 from suggesting a change to it.
    pub(super) is_impl_item: bool,
    /// The universal regions of the return type (an `async fn`'s declared one) and of `self`:
    /// rustc's `is_return_type_anon` and `is_self_anon`, for which E0621 is not given either.
    pub(super) unsuggested: FxHashSet<usize>,
    /// In a closure called by reference, the universal region of that reference.
    pub(super) closure_env: Option<usize>,
    /// In a closure, the position among the regions of its type of each of its creator's
    /// universal regions, by universal region.
    pub(super) creator_positions: FxHashMap<usize, usize>,
    /// The regions of a returned `impl Trait`'s hidden type, each of which must be one of
    /// `choices`, `'static` and the lifetimes the `impl Trait` captures: rustc's member
    /// constraints. Empty when what it captures is not known exactly.
    pub(super) members: Vec<Rv>,
    pub(super) choices: Vec<Rv>,
    /// The body returns an `impl Trait` that captures no lifetime, of a hidden type that names
    /// no other alias: what it returns must be `'static`, rustc's `OpaqueType` constraint.
    pub(super) returns_static_opaque: bool,
    /// A variable a function item or constructor is called through, read at the call: rust-
    /// analyzer lowers `f(x)` with `let f = Foo::<&'a u32>` as a call of the item itself.
    pub(super) callee_reads: Vec<(LocalId, Location)>,
}

impl<'db> Regions<'db> {
    pub(super) fn universal(&self, rv: Rv) -> Option<usize> {
        self.universal_index.get(&rv).copied()
    }

    pub(super) fn knows(&self, longer: usize, shorter: usize) -> bool {
        self.known[longer].contains(shorter)
    }

    /// The regions the where clauses say the type parameter `ty` outlives.
    pub(super) fn type_bounds_of(&self, ty: Ty<'db>) -> Vec<Rv> {
        self.type_bounds
            .iter()
            .filter(|&&(bounded, _)| bounded == ty)
            .map(|&(_, region)| region)
            .collect()
    }

    /// Whether the type parameter `ty` is known to outlive the universal region `universal`.
    pub(super) fn type_outlives_known(&self, ty: Ty<'_>, universal: usize) -> bool {
        self.type_bounds.iter().any(|&(bounded, region)| {
            bounded == ty && self.universal(region).is_some_and(|it| self.knows(it, universal))
        })
    }
}

pub(super) fn rv_of(region: Region<'_>) -> Option<Rv> {
    match region.kind() {
        RegionKind::ReVar(vid) => Some(vid.as_usize()),
        _ => None,
    }
}

/// Each region in `ty`, in order, those under a binder aside.
/// The associated types `ty` names, with their arguments.
fn projections<'db>(ty: Ty<'db>) -> Vec<(hir_ty::next_solver::SolverDefId<'db>, GenericArgs<'db>)> {
    struct Find<'db>(Vec<(hir_ty::next_solver::SolverDefId<'db>, GenericArgs<'db>)>);
    impl<'db> rustc_type_ir::TypeVisitor<hir_ty::next_solver::DbInterner<'db>> for Find<'db> {
        type Result = ();
        fn visit_ty(&mut self, ty: Ty<'db>) {
            if let TyKind::Alias(alias) = ty.kind()
                && let rustc_type_ir::AliasTyKind::Projection { .. } = alias.kind
            {
                self.0.push((alias.kind.def_id(), alias.args));
            }
            rustc_type_ir::TypeSuperVisitable::super_visit_with(&ty, self)
        }
    }
    let mut find = Find(Vec::new());
    rustc_type_ir::TypeVisitable::visit_with(&ty, &mut find);
    find.0
}

/// Which arguments of the `impl Trait` `def`, of `len` of them, its hidden type may name: the
/// others are bivariant in rustc's `variances_of` the opaque type, and its liveness skips
/// them. Every one, unless `use<..>` lists them; before edition 2024, outside a trait and its
/// impls and outside an `async fn`, the type parameters and the lifetimes its bounds name.
/// `None` where this does not work it out, which counts every argument captured.
pub(super) fn opaque_captures(
    db: &dyn hir_ty::db::HirDatabase,
    def: hir_ty::next_solver::SolverDefId<'_>,
    len: usize,
) -> Option<Vec<bool>> {
    use hir_def::{
        GenericParamId,
        hir::{
            generics::GenericParamDataRef,
            type_ref::{LifetimeRef, TypeBound, TypeRef, UseArgRef},
        },
    };
    use rustc_type_ir::Interner as _;
    let hir_ty::next_solver::SolverDefId::InternedOpaqueTyId(opaque) = def else { return None };
    let hir_ty::ImplTraitId::ReturnTypeImplTrait(function, index) = opaque.loc(db) else {
        return None;
    };
    let params: Vec<_> = hir_ty::generics::generics(db, function.into()).iter(true).collect();
    // After the function's, a lifetime for each its parameters leave out.
    let elided = hir_ty::elided_lifetimes(db, function).len() as usize;
    if params.len() + elided != len {
        return None;
    }
    let is_lifetime = |id: &GenericParamId| matches!(id, GenericParamId::LifetimeParamId(_));
    let signature = FunctionSignature::of(db, function);
    let written = signature.ret_type.and_then(|ret| match &signature.store[ret] {
        TypeRef::ImplTrait(bounds) if index.into_raw().into_u32() == 0 => Some(bounds),
        _ => None,
    });
    let listed = written.and_then(|bounds| {
        bounds.iter().find_map(|bound| match bound {
            TypeBound::Use(args) => Some(args),
            _ => None,
        })
    });
    let mut named = FxHashSet::default();
    let mut unknown = false;
    let interner = hir_ty::next_solver::DbInterner::new_no_crate(db);
    // A nested `impl Trait`, `impl Future<Output = impl Sized>`, names only what it captures.
    let mut without_self = hir_ty::next_solver::util::BottomUpFolder {
        interner,
        ty_op: |ty: Ty<'_>| match ty.kind() {
            TyKind::Alias(alias) if alias.kind.def_id() == def => Ty::new_unit(interner),
            TyKind::Alias(alias)
                if let rustc_type_ir::AliasTyKind::Opaque { .. } = alias.kind
                    && let Some(captured) =
                        opaque_captures(db, alias.kind.def_id(), alias.args.len()) =>
            {
                let named: Vec<Ty<'_>> = alias
                    .args
                    .iter()
                    .zip(captured)
                    .filter(|(_, captured)| *captured)
                    .filter_map(|(arg, _)| match arg.kind() {
                        GenericArgKind::Type(ty) => Some(ty),
                        GenericArgKind::Lifetime(region) => Some(Ty::new_ref(
                            interner,
                            region,
                            Ty::new_unit(interner),
                            rustc_ast_ir::Mutability::Not,
                        )),
                        _ => None,
                    })
                    .collect();
                Ty::new_tup(interner, &named)
            }
            _ => ty,
        },
        lt_op: |region| region,
        ct_op: |ct| ct,
    };
    for clause in interner.item_bounds(def).skip_binder() {
        let clause = rustc_type_ir::TypeFoldable::fold_with(clause, &mut without_self);
        rustc_type_ir::fold_regions(interner, clause, |region, _| {
            match region.kind() {
                RegionKind::ReEarlyParam(it) => {
                    named.insert(it.index as usize);
                }
                // `+ '_` where the lifetime elision finds no lifetime of the parameters.
                RegionKind::ReStatic | RegionKind::ReBound(..) | RegionKind::ReError(_) => (),
                _ => unknown = true,
            }
            region
        });
    }
    if let Some(listed) = listed {
        // `use<'_>` lists the lifetime the lifetime elision gives a return type.
        let output = listed
            .iter()
            .any(|arg| {
                matches!(arg, UseArgRef::Lifetime(it)
                    if matches!(signature.store[*it], LifetimeRef::Placeholder))
            })
            .then(|| hir_ty::elided_lifetimes(db, function).output)
            .flatten();
        let names =
            |lifetime: &LifetimeRef, param, data: &hir_def::hir::generics::LifetimeParamData| {
                match lifetime {
                    LifetimeRef::Named(name) => *name == data.name,
                    LifetimeRef::Param(it) => *it == param,
                    _ => false,
                }
            };
        let captured = params
            .iter()
            .map(|(id, data)| {
                let (
                    &GenericParamId::LifetimeParamId(param),
                    GenericParamDataRef::LifetimeParamData(data),
                ) = (id, data)
                else {
                    return true;
                };
                let written = match output {
                    Some(hir_ty::ElidedOutput::Written(it)) => names(
                        &signature.store[hir_def::type_ref::LifetimeRefId::from_raw(
                            la_arena::RawIdx::from_u32(it),
                        )],
                        param,
                        data,
                    ),
                    _ => false,
                };
                written
                    || listed.iter().any(|arg| match arg {
                        UseArgRef::Lifetime(it) => names(&signature.store[*it], param, data),
                        UseArgRef::Name(_) => false,
                    })
            })
            .chain(
                (0..elided as u32).map(|index| output == Some(hir_ty::ElidedOutput::Elided(index))),
            )
            .collect::<Vec<_>>();
        // A lifetime the bounds name and `use<..>` leaves out is an error before borrow
        // checking.
        let bounds_listed =
            named.iter().all(|&index| captured.get(index).copied().unwrap_or(false));
        return (!unknown && bounds_listed).then_some(captured);
    }
    if unknown {
        return None;
    }
    let in_trait = match function.lookup(db).container {
        ItemContainerId::TraitId(_) => true,
        ItemContainerId::ImplId(it) => db.impl_trait(it).is_some(),
        _ => false,
    };
    let edition = hir_def::HasModule::krate(&function, db).data(db).edition;
    // An `async fn`'s future captures everything; an `impl Trait` it declares it returns, the
    // future's output, is as any other.
    if edition.at_least_2024() || in_trait || signature.is_async() && written.is_some() {
        return None;
    }
    Some(
        params
            .iter()
            .map(|(id, _)| !is_lifetime(id))
            .chain(std::iter::repeat_n(false, elided))
            .enumerate()
            .map(|(index, not_lifetime)| not_lifetime || named.contains(&index))
            .collect(),
    )
}

/// Whether `ty` has an `impl Trait` in it.
fn has_opaque(ty: Ty<'_>) -> bool {
    struct Find;
    impl<'db> rustc_type_ir::TypeVisitor<hir_ty::next_solver::DbInterner<'db>> for Find {
        type Result = std::ops::ControlFlow<()>;
        fn visit_ty(&mut self, ty: Ty<'db>) -> Self::Result {
            if let TyKind::Alias(alias) = ty.kind()
                && let rustc_type_ir::AliasTyKind::Opaque { .. } = alias.kind
            {
                return std::ops::ControlFlow::Break(());
            }
            rustc_type_ir::TypeSuperVisitable::super_visit_with(&ty, self)
        }
    }
    rustc_type_ir::TypeVisitable::visit_with(&ty, &mut Find).is_break()
}

/// Whether `ty` has a closure or a coroutine in it, whose signature RA does not keep
/// higher-ranked: `|line: &String|` is `fn(&'1 String)` for one `'1`, not `for<'a>`.
fn names_closure(ty: Ty<'_>) -> bool {
    ty.walk().filter_map(|it| rustc_type_ir::inherent::GenericArg::as_type(&it)).any(|it| {
        matches!(
            it.kind(),
            TyKind::Closure(..) | TyKind::CoroutineClosure(..) | TyKind::Coroutine(..)
        )
    })
}

/// Whether `ty` names a lifetime other than in the arguments an `impl Trait` in it does not
/// capture: `impl Sized` in edition 2021 names none of its function's.
fn names_captured_lifetime<'db>(db: &dyn hir_ty::db::HirDatabase, ty: Ty<'db>) -> bool {
    struct Find<'d> {
        db: &'d dyn hir_ty::db::HirDatabase,
        found: bool,
    }
    impl<'db> rustc_type_ir::TypeVisitor<hir_ty::next_solver::DbInterner<'db>> for Find<'_> {
        type Result = ();
        fn visit_region(&mut self, region: Region<'db>) {
            self.found |= !matches!(region.kind(), RegionKind::ReBound(..));
        }
        fn visit_ty(&mut self, ty: Ty<'db>) {
            match ty.kind() {
                TyKind::Alias(alias)
                    if let rustc_type_ir::AliasTyKind::Opaque { .. } = alias.kind
                        && let Some(captured) =
                            opaque_captures(self.db, alias.kind.def_id(), alias.args.len()) =>
                {
                    for (arg, captured) in alias.args.iter().zip(captured) {
                        if captured {
                            rustc_type_ir::TypeVisitable::visit_with(&arg, self);
                        }
                    }
                }
                _ => rustc_type_ir::TypeSuperVisitable::super_visit_with(&ty, self),
            }
        }
    }
    let mut find = Find { db, found: false };
    rustc_type_ir::TypeVisitable::visit_with(&ty, &mut find);
    find.found
}

/// Whether `ty` is an `impl Trait` whose bounds say nothing of its hidden type's lifetimes, as
/// `impl Cap<'a> + Cap<'b>`: no `+ 'a`, and no associated type that names one. Nested in a
/// returned one's `Item = ..`, it adds nothing the returned one's member constraints need.
fn unbounded_opaque<'db>(
    db: &dyn hir_ty::db::HirDatabase,
    interner: hir_ty::next_solver::DbInterner<'db>,
    ty: Ty<'db>,
) -> bool {
    let TyKind::Alias(alias) = ty.kind() else { return false };
    if !matches!(alias.kind, rustc_type_ir::AliasTyKind::Opaque { .. }) {
        return false;
    }
    rustc_type_ir::Interner::item_bounds(interner, alias.kind.def_id())
        .iter_instantiated(interner, alias.args.as_slice())
        .all(|clause| match clause.kind().skip_binder() {
            ClauseKind::TypeOutlives(_) | ClauseKind::RegionOutlives(_) => false,
            ClauseKind::Projection(projection) => match projection.term.as_type() {
                Some(ty) => !names_captured_lifetime(db, ty) || unbounded_opaque(db, interner, ty),
                None => !rustc_type_ir::TypeVisitableExt::has_free_regions(&projection.term),
            },
            _ => true,
        })
}

/// The regions of a hidden type that rustc's `CollectMemberConstraintsVisitor` gives member
/// constraints: not those of a closure's parent arguments, nor an `impl Trait`'s arguments
/// it does not capture.
fn member_regions<'db>(
    db: &dyn hir_ty::db::HirDatabase,
    ty: Ty<'db>,
    aliases: bool,
    out: &mut Vec<Rv>,
) {
    struct Collect<'o, 'd> {
        db: &'d dyn hir_ty::db::HirDatabase,
        aliases: bool,
        out: &'o mut Vec<Rv>,
    }
    impl Collect<'_, '_> {
        fn args<'db>(&mut self, args: GenericArgs<'db>, skip: usize) {
            for arg in args.iter().skip(skip) {
                rustc_type_ir::TypeVisitable::visit_with(&arg, self);
            }
        }
    }
    impl<'db> rustc_type_ir::TypeVisitor<hir_ty::next_solver::DbInterner<'db>> for Collect<'_, '_> {
        type Result = ();
        fn visit_region(&mut self, region: Region<'db>) {
            if let Some(rv) = rv_of(region) {
                self.out.push(rv);
            }
        }
        fn visit_ty(&mut self, ty: Ty<'db>) {
            match ty.kind() {
                TyKind::Closure(_, args) => self.args(args, args.as_closure().parent_args().len()),
                TyKind::Coroutine(_, args) => {
                    self.args(args, args.as_coroutine().parent_args().len())
                }
                TyKind::CoroutineClosure(_, args) => {
                    self.args(args, args.as_coroutine_closure().parent_args().len())
                }
                TyKind::Alias(_) if !self.aliases => (),
                TyKind::Alias(alias)
                    if let rustc_type_ir::AliasTyKind::Opaque { .. } = alias.kind
                        && let Some(captured) =
                            opaque_captures(self.db, alias.kind.def_id(), alias.args.len()) =>
                {
                    for (arg, captured) in alias.args.iter().zip(captured) {
                        if captured {
                            rustc_type_ir::TypeVisitable::visit_with(&arg, self);
                        }
                    }
                }
                _ => rustc_type_ir::TypeSuperVisitable::super_visit_with(&ty, self),
            }
        }
    }
    rustc_type_ir::TypeVisitable::visit_with(&ty, &mut Collect { db, aliases, out });
}

pub(super) fn regions_of(ty: Ty<'_>, out: &mut Vec<Rv>) {
    struct Collect<'o>(&'o mut Vec<Rv>);
    impl<'db> rustc_type_ir::TypeVisitor<hir_ty::next_solver::DbInterner<'db>> for Collect<'_> {
        type Result = ();
        fn visit_region(&mut self, region: Region<'db>) {
            if let Some(rv) = rv_of(region) {
                self.0.push(rv);
            }
        }
    }
    rustc_type_ir::TypeVisitable::visit_with(&ty, &mut Collect(out));
}

pub(super) fn build<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    cfg: &Cfg,
    requirements: &FxHashMap<hir_def::hir::ExprId, (usize, Vec<Requirement<'db>>)>,
) -> Regions<'db> {
    let mut builder = Builder {
        ctx,
        body,
        regions: Regions {
            universals: Vec::new(),
            universal_index: FxHashMap::default(),
            static_rv: 0,
            known: Vec::new(),
            type_bounds: Vec::new(),
            early: FxHashMap::default(),
            local_tys: Vec::new(),
            loan_regions: FxHashMap::default(),
            outlives: Vec::new(),
            type_tests: Vec::new(),
            check_universals: ctx.closure_local.is_none(),
            is_impl_item: false,
            unsuggested: FxHashSet::default(),
            closure_env: None,
            creator_positions: FxHashMap::default(),
            members: Vec::new(),
            returns_static_opaque: false,
            callee_reads: Vec::new(),
            choices: Vec::new(),
        },
        requirements,
        early: FxHashMap::default(),
        late: FxHashMap::default(),
        parent_late: FxHashMap::default(),
        impl_self: None,
        returns_opaque: false,
        opaque_bounds: Vec::new(),
        elided_output: None,
        inferred: Default::default(),
        location: None,
        env_aliases: Vec::new(),
        annotated: FxHashMap::default(),
        const_positions: 0,
        aggregate_temps: aggregate_temps(body),
        synthesized: synthesized(ctx),
        placeholders: FxHashMap::default(),
        forall_universe: None,
    };
    builder.universal_regions();
    builder.env_aliases();
    builder.location = None;
    builder.signature_projections();
    builder.annotations();
    for &block in &cfg.order {
        let data = &body.basic_blocks[block];
        for (statement, it) in data.statements.iter().enumerate() {
            builder.location = Some(Location { block, statement });
            builder.statement(&it.kind, it.span);
        }
        if let Some(terminator) = &data.terminator {
            builder.location = Some(Location { block, statement: data.statements.len() });
            builder.terminator(&terminator.kind, terminator.span);
        }
    }
    // Last, as annotations and turbofishes can name lifetimes the signature does not.
    builder.known_relations();
    builder.regions
}

struct Builder<'c, 'a, 'db> {
    ctx: &'c Ctx<'a, 'db>,
    /// The associated types, regions erased, that a where clause or a parameter's type says
    /// outlive some region: rustc's declared bounds from the environment, known before
    /// `known_relations` says which regions.
    env_aliases: Vec<Ty<'db>>,
    /// The variables a `let` writes the type of, with that type where it is theirs.
    annotated: FxHashMap<LocalId, Option<Ty<'db>>>,
    /// In a `const { .. }` block, how many regions of its type have a position.
    const_positions: usize,
    body: &'c MirBody<'db>,
    regions: Regions<'db>,
    /// What each closure checked so far needs of the body that creates it, with how many
    /// regions the closure's type has.
    requirements: &'c FxHashMap<hir_def::hir::ExprId, (usize, Vec<Requirement<'db>>)>,
    /// The universal region of each lifetime parameter, by its index.
    early: FxHashMap<u32, Region<'db>>,
    /// The universal region of each of the signature's late-bound lifetimes.
    late: FxHashMap<u32, Region<'db>>,
    /// In a closure, the universal region of each late-bound lifetime of the enclosing item
    /// it names, one of its creator's.
    parent_late: FxHashMap<u32, Region<'db>>,
    /// The self type of the impl the body is in, as rust-analyzer gives it with `'_` an error,
    /// and with universal regions there: rustc's anonymous lifetime parameters of the impl.
    impl_self: Option<(Ty<'db>, Ty<'db>)>,
    /// The return type is an `impl Trait`, so the return slot has the type it hides, which
    /// implies nothing to the body.
    returns_opaque: bool,
    /// The trait and projection bounds of the returned `impl Trait`, of the type it hides,
    /// proven where the body first assigns the return slot.
    opaque_bounds: Vec<Clause<'db>>,
    /// The region lifetime elision gives the elided lifetimes of the return type.
    elided_output: Option<Region<'db>>,
    /// rustc's `inferred_outlives_of` the ADTs the body uses.
    inferred: hir_ty_wfcheck::Inferred<'db>,
    location: Option<Location>,
    /// rustc's `AggregateTemp`s: the temporaries a struct or variant expression, or a call of
    /// a constructor, builds its fields in.
    aggregate_temps: FxHashSet<LocalId>,
    /// The expressions lowering makes that the source does not write, as the `let x = x;` of
    /// each parameter of an `async fn`: rustc's desugarings, `Boring` to blame.
    synthesized: FxHashSet<hir_def::hir::ExprId>,
    /// The universal region of each placeholder the trait solver's answers name.
    placeholders: FxHashMap<Region<'db>, Region<'db>>,
    /// The universe of the placeholders a `for<'a>` predicate being proven is instantiated
    /// with, which a type outlives only by outliving `'static`.
    forall_universe: Option<rustc_type_ir::UniverseIndex>,
}

fn synthesized(ctx: &Ctx<'_, '_>) -> FxHashSet<hir_def::hir::ExprId> {
    let InferBodyId::DefWithBodyId(def) = ctx.owner else { return FxHashSet::default() };
    let (_, source_map) = hir_def::expr_store::Body::with_source_map(ctx.db, def);
    ctx.store
        .exprs()
        .map(|(expr, _)| expr)
        .filter(|&expr| source_map.expr_syntax(expr).is_err())
        .collect()
}

fn aggregate_temps(body: &MirBody<'_>) -> FxHashSet<LocalId> {
    let mut out = FxHashSet::default();
    let mut add = |operand: &Operand| {
        if let OperandKind::Move(place) = &operand.kind
            && place.projection.as_slice().is_empty()
        {
            out.insert(place.local);
        }
    };
    for (_, data) in body.basic_blocks.iter() {
        for it in &data.statements {
            if let StatementKind::Assign(
                _,
                Rvalue::Aggregate(AggregateKind::Adt(..) | AggregateKind::Union(..), operands),
            ) = &it.kind
            {
                operands.iter().for_each(&mut add);
            }
        }
        if let Some(terminator) = &data.terminator
            && let TerminatorKind::Call { func, args, .. } = &terminator.kind
            && let OperandKind::Constant { ty, .. } = &func.kind
            && let TyKind::FnDef(def, _) = ty.as_ref().kind()
            && !matches!(def.0, CallableDefId::FunctionId(_))
        {
            args.iter().for_each(&mut add);
        }
    }
    out
}

/// Where a constraint comes from.
#[derive(Clone, Copy)]
struct At {
    category: Category,
    span: MirSpan,
}

impl<'db> Builder<'_, '_, 'db> {
    fn interner(&self) -> hir_ty::next_solver::DbInterner<'db> {
        self.ctx.infcx.interner
    }

    fn fresh(&self) -> Region<'db> {
        self.ctx.infcx.next_region_var(hir_ty::Span::Dummy)
    }

    fn renumber<T: rustc_type_ir::TypeFoldable<hir_ty::next_solver::DbInterner<'db>>>(
        &self,
        value: T,
    ) -> T {
        let infcx = &self.ctx.infcx;
        rustc_type_ir::fold_regions(self.interner(), value, |region, _| match region.kind() {
            RegionKind::ReBound(..) => region,
            _ => infcx.next_region_var(hir_ty::Span::Dummy),
        })
    }

    /// Whether this is the body of a closure or a `const { .. }` block, whose type's regions
    /// and enclosing item's lifetimes are its creator's.
    fn nested(&self) -> bool {
        self.ctx.closure_local.is_some() || self.ctx.inline_const
    }

    fn new_universal(&mut self, kind: UniversalKind) -> Region<'db> {
        let region = self.fresh();
        let rv = rv_of(region).expect("a fresh region is a variable");
        self.regions.universal_index.insert(rv, self.regions.universals.len());
        self.regions.universals.push((rv, kind));
        region
    }

    fn placeholder(&mut self, placeholder: Region<'db>) -> Region<'db> {
        if let Some(&it) = self.placeholders.get(&placeholder) {
            return it;
        }
        let region = self.new_universal(UniversalKind::Placeholder);
        self.placeholders.insert(placeholder, region);
        region
    }

    fn static_region(&self) -> Region<'db> {
        Region::new_var(
            self.interner(),
            rustc_type_ir::RegionVid::from_usize(self.regions.static_rv),
        )
    }

    fn early_region(&mut self, index: u32) -> Region<'db> {
        if let Some(&it) = self.early.get(&index) {
            return it;
        }
        // A closure's or `const { .. }` block's enclosing item's lifetime parameters are its
        // creator's.
        let kind = match self.nested() {
            true => UniversalKind::Creator,
            false => UniversalKind::Named,
        };
        let region = self.new_universal(kind);
        if self.ctx.inline_const {
            self.regions
                .creator_positions
                .insert(self.regions.universals.len() - 1, EARLY_PARENT + index as usize);
        }
        self.early.insert(index, region);
        self.regions.early.insert(index, rv_of(region).unwrap());
        region
    }

    /// The universal region of the enclosing item's late-bound lifetime `var`: the signature's
    /// in the item, a creator's region of a closure's, at the position [`LATE_PARENT`] + `var`,
    /// as rustc's `for_each_late_bound_region_in_recursive_scope` makes them external.
    fn parent_late_region(&mut self, var: u32) -> Region<'db> {
        if !self.nested() {
            if let Some(&it) = self.late.get(&var) {
                return it;
            }
            let region = self.new_universal(UniversalKind::Named);
            self.late.insert(var, region);
            return region;
        }
        if let Some(&it) = self.parent_late.get(&var) {
            return it;
        }
        let region = self.new_universal(UniversalKind::Creator);
        self.regions
            .creator_positions
            .insert(self.regions.universals.len() - 1, LATE_PARENT + var as usize);
        self.parent_late.insert(var, region);
        region
    }

    /// `region` of the signature as a universal region; `None` for an elided one. `depth` is
    /// how many binders in the region is, those of the signature's own late-bound lifetimes
    /// being outside it.
    fn signature_region(
        &mut self,
        region: Region<'db>,
        depth: rustc_type_ir::DebruijnIndex,
    ) -> Option<Region<'db>> {
        match region.kind() {
            RegionKind::ReBound(rustc_type_ir::BoundVarIndexKind::Bound(debruijn), bound)
                if debruijn == depth =>
            {
                let var = bound.var.as_u32();
                if let Some(&it) = self.late.get(&var) {
                    return Some(it);
                }
                let kind = match bound.kind {
                    rustc_type_ir::BoundRegionKind::Named(_) => UniversalKind::Named,
                    _ => UniversalKind::AnonParam,
                };
                let region = self.new_universal(kind);
                self.late.insert(var, region);
                Some(region)
            }
            RegionKind::ReBound(..) => Some(region),
            // In a closure, a late-bound lifetime of the enclosing item is its creator's.
            RegionKind::ReLateParam(late) if self.nested() => {
                Some(self.parent_late_region(late.bound_region.var.as_u32()))
            }
            // A late-bound lifetime the body names, the same as in the signature.
            RegionKind::ReLateParam(late) => {
                let var = late.bound_region.var.as_u32();
                if let Some(&it) = self.late.get(&var) {
                    return Some(it);
                }
                let region = self.new_universal(UniversalKind::Named);
                self.late.insert(var, region);
                Some(region)
            }
            RegionKind::ReStatic => Some(self.static_region()),
            RegionKind::ReEarlyParam(early) => Some(self.early_region(early.index)),
            RegionKind::ReVar(_) => Some(region),
            _ => None,
        }
    }

    fn universal_regions(&mut self) {
        let static_region = self.new_universal(UniversalKind::Static);
        self.regions.static_rv = rv_of(static_region).unwrap();
        self.new_universal(UniversalKind::FnBody);
        let body = self.body;
        let mut local_tys: Vec<Option<Ty<'db>>> = vec![None; body.locals.len()];
        let return_local = super::return_slot();
        let owner = body.owner;
        if self.ctx.closure_local.is_some() {
            self.regions.check_universals = self.closure_signature(&mut local_tys);
        }
        if self.ctx.closure_local.is_some() && !self.regions.check_universals {
            for &local in body.param_locals.iter().chain([&return_local]) {
                let ty = body.locals[local].ty.as_ref();
                let ty = rustc_type_ir::fold_regions(self.interner(), ty, |region, _| match region
                    .kind()
                {
                    RegionKind::ReBound(..) => region,
                    _ => self.new_universal(UniversalKind::AnonParam),
                });
                local_tys[local.into_raw().into_u32() as usize] = Some(ty);
            }
        } else if self.ctx.closure_local.is_none() {
            // The declared signature, as the parameters' local types have their lifetimes
            // erased.
            let signature = match owner {
                InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)) => {
                    let sig = self
                        .ctx
                        .db
                        .callable_item_signature(CallableDefId::FunctionId(function))
                        .instantiate_identity()
                        .skip_norm_wip()
                        .skip_binder();
                    let mut inputs: Vec<Ty<'db>> = sig.inputs().iter().copied().collect();
                    let mut output = sig.output();
                    let written = hir_def::signatures::FunctionSignature::of(self.ctx.db, function);
                    // A C variadic's `ap: ...` is a `VaList` the signature does not list.
                    if written.is_varargs()
                        && inputs.len() + 1 == body.param_locals.len()
                        && let Some(&last) = body.param_locals.last()
                    {
                        inputs.push(body.locals[last].ty.as_ref());
                    }
                    if written.params.len() == inputs.len()
                        && let Some(impl_self) = self.impl_self_of(function)
                    {
                        // Only where `Self` is written: an elided `&str` in `impl Trait for &str`
                        // is the same type to rust-analyzer, but a lifetime of its own.
                        let store = &written.store;
                        let mut folder = Replace::new(self.interner(), impl_self);
                        for (input, &param) in inputs.iter_mut().zip(&written.params) {
                            if writes_self(store, param) {
                                *input =
                                    rustc_type_ir::TypeFoldable::fold_with(*input, &mut folder);
                            }
                        }
                        if written.ret_type.is_some_and(|it| writes_self(store, it)) {
                            output = rustc_type_ir::TypeFoldable::fold_with(output, &mut folder);
                        }
                    }
                    // Each elided input lifetime is its own region before `&dyn Trait` copies
                    // it into the object's default, so that the two are the same one. A
                    // parameter that elides one lifetime has it once however often its type
                    // does (`S2` of `type S2<'a> = S<'a, 'a>`).
                    let resolver = {
                        use hir_def::resolver::HasResolver;
                        function.resolver(self.ctx.db)
                    };
                    let aligned = written.params.len() == inputs.len();
                    let mut errors = vec![Vec::new(); inputs.len()];
                    for (index, input) in inputs.iter_mut().enumerate() {
                        let single = aligned
                            && written.params.get(index).is_some_and(|&param| {
                                elided_lifetimes(self.ctx.db, &resolver, &written.store, param)
                                    == Some(1)
                            });
                        let mut only = None;
                        let errors = &mut errors[index];
                        *input =
                            rustc_type_ir::fold_regions(self.interner(), *input, |region, _| {
                                let region = match region.kind() {
                                    RegionKind::ReError(_) if single => {
                                        *only.get_or_insert_with(|| {
                                            self.new_universal(UniversalKind::AnonParam)
                                        })
                                    }
                                    RegionKind::ReError(_) => {
                                        self.new_universal(UniversalKind::AnonParam)
                                    }
                                    _ => return region,
                                };
                                errors.push(region);
                                region
                            });
                    }
                    // What an `impl Trait` it returns has for the lifetimes its parameters
                    // leave out.
                    if aligned {
                        let regions =
                            hir_ty::elided_lifetimes(self.ctx.db, function).regions(&errors);
                        output =
                            hir_ty::with_elided_regions(self.ctx.db, output, function, &regions);
                    }
                    let mut defaults = ObjectDefaults {
                        db: self.ctx.db,
                        interner: self.interner(),
                        keep_static: writes_static_object(&written.store),
                        erase: false,
                    };
                    for input in &mut inputs {
                        *input = rustc_type_ir::TypeFoldable::fold_with(*input, &mut defaults);
                    }
                    output = rustc_type_ir::TypeFoldable::fold_with(output, &mut defaults);
                    (inputs.len() == body.param_locals.len()).then_some((inputs, output))
                }
                _ => None,
            };
            let mut inputs = Vec::new();
            for (index, &local) in body.param_locals.iter().enumerate() {
                let ty = match &signature {
                    Some((tys, _)) => tys[index],
                    None => body.locals[local].ty.as_ref(),
                };
                let ty = rustc_type_ir::fold_regions(self.interner(), ty, |region, depth| {
                    // Inference variables are ours only in a signature, from its impl's self type.
                    let region = match region.kind() {
                        RegionKind::ReVar(_) if signature.is_none() => None,
                        _ => self.signature_region(region, depth),
                    }
                    .unwrap_or_else(|| self.new_universal(UniversalKind::AnonParam));
                    if !matches!(region.kind(), RegionKind::ReBound(..)) {
                        inputs.push(region);
                    }
                    region
                });
                local_tys[local.into_raw().into_u32() as usize] = Some(ty);
            }
            // The lifetime elision gives the elided lifetimes of the return type.
            let self_region = match owner {
                InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function))
                    if FunctionSignature::of(self.ctx.db, function).has_self_param() =>
                {
                    body.param_locals.first().and_then(|&it| {
                        let ty = local_tys[it.into_raw().into_u32() as usize]?;
                        self.self_reference(function, ty).or(match ty.kind() {
                            TyKind::Ref(region, ..) => Some(region),
                            _ => None,
                        })
                    })
                }
                _ => None,
            };
            inputs.dedup();
            let elided = match (inputs.as_slice(), self_region) {
                (_, Some(region)) => Some(region),
                ([only], None) => Some(*only),
                _ => None,
            };
            self.elided_output = elided;
            // rustc's E0106, which rust-analyzer does not report: a return type that elides a
            // lifetime where no parameter has one, `fn f<R>(r: R) -> Box<dyn Foo + '_>`.
            if inputs.is_empty()
                && self_region.is_none()
                && self.ctx.closure_local.is_none()
                && let InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)) = owner
                && let written = FunctionSignature::of(self.ctx.db, function)
                && written.ret_type.is_some_and(|it| writes_elided(&written.store, it))
            {
                self.ctx.tainted.set(true);
            }
            let is_fn = matches!(owner, InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(_)));
            // A returned `impl Trait` is checked as the type it hides; its lifetimes are
            // `'static` when nothing it could capture is, and left free otherwise.
            let opaque = signature.as_ref().is_some_and(|(_, output)| has_opaque(*output));
            self.returns_opaque = opaque;
            let ty = match &signature {
                Some((_, output)) if !opaque => *output,
                // The type an `impl Trait` hides is inferred, the lifetimes of its trait objects
                // too; a trait object the signature writes next to it keeps its default.
                Some((_, output))
                    if let TyKind::Alias(alias) = output.kind()
                        && let rustc_type_ir::AliasTyKind::Opaque { .. } = alias.kind =>
                {
                    rustc_type_ir::TypeFoldable::fold_with(
                        body.locals[return_local].ty.as_ref(),
                        &mut ObjectDefaults {
                            db: self.ctx.db,
                            interner: self.interner(),
                            keep_static: writes_static_object(self.ctx.store),
                            erase: true,
                        },
                    )
                }
                _ => body.locals[return_local].ty.as_ref(),
            };
            let mut names_parameter = false;
            rustc_type_ir::fold_regions(self.interner(), ty, |region, _| {
                names_parameter |= matches!(
                    region.kind(),
                    RegionKind::ReEarlyParam(_) | RegionKind::ReLateParam(_)
                );
                region
            });
            // A lifetime parameter it captures is not a universal region until something names
            // it.
            let captures_parameter = signature.as_ref().is_some_and(|(_, output)| {
                let TyKind::Alias(alias) = output.kind() else { return false };
                let Some(captured) =
                    opaque_captures(self.ctx.db, alias.kind.def_id(), alias.args.len())
                else {
                    return false;
                };
                alias.args.iter().zip(captured).any(|(arg, captured)| {
                    captured
                        && matches!(arg.kind(), GenericArgKind::Lifetime(region)
                            if !matches!(region.kind(), RegionKind::ReStatic))
                })
            });
            let hidden_static = opaque
                && self.regions.universals.len() == 2
                && !names_parameter
                && !captures_parameter;
            // Of each `impl Trait` in it, `Box<impl Future>` too.
            let captures_lifetime = signature.as_ref().is_some_and(|(_, output)| {
                output
                    .walk()
                    .filter_map(|it| rustc_type_ir::inherent::GenericArg::as_type(&it))
                    .filter_map(|ty| match ty.kind() {
                        TyKind::Alias(alias)
                            if let rustc_type_ir::AliasTyKind::Opaque { .. } = alias.kind =>
                        {
                            Some(alias)
                        }
                        _ => None,
                    })
                    .any(|alias| {
                        let Some(captured) =
                            opaque_captures(self.ctx.db, alias.kind.def_id(), alias.args.len())
                        else {
                            return true;
                        };
                        alias.args.iter().zip(captured).any(|(arg, captured)| {
                            captured && matches!(arg.kind(), GenericArgKind::Lifetime(_))
                        })
                    })
            });
            self.regions.returns_static_opaque = hidden_static
                && !captures_lifetime
                && !rustc_type_ir::TypeVisitableExt::has_aliases(&ty);
            let ty = rustc_type_ir::fold_regions(self.interner(), ty, |region, depth| {
                if (opaque || signature.is_none()) && matches!(region.kind(), RegionKind::ReVar(_))
                {
                    return self.fresh();
                }
                self.signature_region(region, depth).unwrap_or_else(|| match (elided, is_fn) {
                    // A `const { .. }` block's type is its creator's, by position.
                    _ if self.ctx.inline_const => {
                        let region = self.new_universal(UniversalKind::Creator);
                        self.regions
                            .creator_positions
                            .insert(self.regions.universals.len() - 1, self.const_positions);
                        self.const_positions += 1;
                        region
                    }
                    _ if hidden_static => self.static_region(),
                    _ if opaque => self.fresh(),
                    (Some(it), _) => it,
                    (None, true) => self.new_universal(UniversalKind::Anon),
                    // The elided lifetimes of the type of a constant or a static are `'static`.
                    (None, false) => self.static_region(),
                })
            });
            local_tys[return_local.into_raw().into_u32() as usize] = Some(ty);
            // rustc proves an `impl Trait`'s bounds of the type it hides where the body
            // defines it: `impl Iterator<Item = Box<dyn Foo>>` hiding `Iter<'a, T>` needs the
            // where clauses of `Iter<'a, T>`'s `Iterator` impl and its `Item` the bound's. An
            // `async fn`'s output is related as it declares it, below.
            let is_async = match owner {
                InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)) => {
                    FunctionSignature::of(self.ctx.db, function).is_async()
                }
                _ => false,
            };
            if opaque
                && !is_async
                && let Some((_, output)) = &signature
                && let TyKind::Alias(alias) = output.kind()
                && let rustc_type_ir::AliasTyKind::Opaque { .. } = alias.kind
            {
                let interner = self.interner();
                let bounds = rustc_type_ir::Interner::item_bounds(interner, alias.kind.def_id());
                let mut hidden = Replace::new(interner, (*output, ty));
                for clause in bounds.iter_instantiated(interner, alias.args.as_slice()) {
                    let clause = clause.skip_norm_wip();
                    if !matches!(
                        clause.kind().skip_binder(),
                        ClauseKind::Trait(_) | ClauseKind::Projection(_)
                    ) {
                        continue;
                    }
                    let clause = rustc_type_ir::TypeFoldable::fold_with(clause, &mut hidden);
                    let clause =
                        rustc_type_ir::fold_regions(interner, clause, |region, depth| match region
                            .kind()
                        {
                            RegionKind::ReVar(_) => region,
                            _ => {
                                self.signature_region(region, depth).unwrap_or_else(|| self.fresh())
                            }
                        });
                    self.opaque_bounds.push(clause);
                }
            }
            let mut unsuggested = Vec::new();
            regions_of(ty, &mut unsuggested);
            if let InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)) = owner
                && FunctionSignature::of(self.ctx.db, function).has_self_param()
                && let Some(&local) = body.param_locals.first()
                && let Some(ty) = local_tys[local.into_raw().into_u32() as usize]
            {
                regions_of(ty, &mut unsuggested);
            }
            for rv in unsuggested {
                self.regions.unsuggested.extend(self.regions.universal(rv));
            }
            // Nor for an anonymous lifetime no parameter's written type has, the impl's in
            // `impl MyStruct<'_>` or a C variadic's `ap: ...`: rustc's `find_param_with_region`
            // finds no parameter.
            let variadic = match owner {
                InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)) => {
                    FunctionSignature::of(self.ctx.db, function).is_varargs()
                }
                _ => false,
            };
            let written_params = body.param_locals.len().saturating_sub(usize::from(variadic));
            let mut in_params = Vec::new();
            for &local in body.param_locals.iter().take(written_params) {
                if let Some(ty) = local_tys[local.into_raw().into_u32() as usize] {
                    regions_of(ty, &mut in_params);
                }
            }
            let in_params: FxHashSet<usize> =
                in_params.into_iter().filter_map(|rv| self.regions.universal(rv)).collect();
            for index in 0..self.regions.universals.len() {
                if self.regions.universals[index].1 == UniversalKind::AnonParam
                    && !in_params.contains(&index)
                {
                    self.regions.unsuggested.insert(index);
                }
            }
            // A C variadic's `VaList` lifetime is none of the function's generics, which are
            // all an `impl Trait` it returns could capture; a lifetime parameter no parameter
            // names is no choice of a region of the body.
            if opaque
                && !hidden_static
                && let Some((_, output)) = &signature
                && let Some((alias, hidden)) = returned_opaque(*output, ty)
                && let Some(captured) =
                    opaque_captures(self.ctx.db, alias.kind.def_id(), alias.args.len())
                        .or_else(|| variadic.then(|| vec![true; alias.args.len()]))
            {
                let mut choices = vec![self.regions.static_rv];
                let mut known = true;
                for (arg, captured) in alias.args.iter().zip(captured) {
                    if let GenericArgKind::Lifetime(region) = arg.kind()
                        && captured
                    {
                        match self
                            .signature_region(region, rustc_type_ir::INNERMOST)
                            .and_then(rv_of)
                        {
                            Some(rv) => choices.push(rv),
                            None if variadic => (),
                            None => known = false,
                        }
                    }
                }
                // What the bounds say the `impl Trait` outlives its hidden type must: rustc
                // proves them of it. A projection bound that names a lifetime constrains it in
                // ways not followed here, so its member constraints are left out.
                let mut outlived = Vec::new();
                let bounds =
                    rustc_type_ir::Interner::item_bounds(self.interner(), alias.kind.def_id());
                for clause in bounds.iter_instantiated(self.interner(), alias.args.as_slice()) {
                    match clause.kind().skip_binder() {
                        ClauseKind::TypeOutlives(OutlivesPredicate(bounded, region))
                            if let TyKind::Alias(bounded) = bounded.kind()
                                && bounded.kind.def_id() == alias.kind.def_id() =>
                        {
                            // Under the clause's own binder, the signature's is one further out.
                            let binder = rustc_type_ir::INNERMOST.shifted_in(1);
                            // A lifetime the bound elides, `+ '_`, rustc captures too.
                            let elided = matches!(region.kind(), RegionKind::ReError(_));
                            let rv = match elided {
                                true => self.elided_output.and_then(rv_of),
                                false => self.signature_region(region, binder).and_then(rv_of),
                            };
                            match rv {
                                Some(rv) => {
                                    outlived.push(rv);
                                    if elided && !choices.contains(&rv) {
                                        choices.push(rv);
                                    }
                                }
                                None => known = false,
                            }
                        }
                        ClauseKind::Projection(projection)
                            if match projection.term.as_type() {
                                Some(ty) => {
                                    names_captured_lifetime(self.ctx.db, ty)
                                        && !unbounded_opaque(self.ctx.db, self.interner(), ty)
                                }
                                None => rustc_type_ir::TypeVisitableExt::has_free_regions(
                                    &projection.term,
                                ),
                            } =>
                        {
                            known = false;
                        }
                        _ => (),
                    }
                }
                let mut members = Vec::new();
                member_regions(self.ctx.db, hidden, true, &mut members);
                members.retain(|&it| it != self.regions.static_rv);
                members.sort_unstable();
                members.dedup();
                // An alias's own bounds may already say it outlives them, so the regions within
                // one are left to the member constraints.
                let mut bounded = Vec::new();
                member_regions(self.ctx.db, hidden, false, &mut bounded);
                bounded.retain(|&it| it != self.regions.static_rv);
                bounded.sort_unstable();
                bounded.dedup();
                for &member in &bounded {
                    for &sub in &outlived {
                        self.regions.outlives.push(Outlives {
                            sup: member,
                            sub,
                            location: None,
                            category: Category::Boring,
                            span: MirSpan::Unknown,
                            from_closure: false,
                        });
                    }
                }
                if known {
                    self.regions.members = members;
                    self.regions.choices = choices;
                }
            }
            if let InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)) = owner {
                self.regions.is_impl_item = matches!(
                    function.lookup(self.ctx.db).container,
                    ItemContainerId::ImplId(it) if self.ctx.db.impl_trait(it).is_some()
                );
            }
        }
        // A variable with a type written out is related to it, lifetimes and all, as rustc
        // relates it to the user type annotation.
        let annotated = self.annotated_locals();
        for (local, data) in body.locals.iter() {
            let index = local.into_raw().into_u32() as usize;
            if local_tys[index].is_some() {
                continue;
            }
            let ty = data.ty.as_ref();
            local_tys[index] = Some(match annotated.get(&local) {
                Some(&annotation) => {
                    let written = self.annotation_ty(annotation).filter(|it| {
                        let interner = self.interner();
                        let erase = |ty: Ty<'db>| {
                            rustc_type_ir::fold_regions(interner, ty, |_, _| {
                                Region::new_static(interner)
                            })
                        };
                        erase(*it) == erase(ty)
                    });
                    self.annotated.insert(local, written);
                    match written {
                        Some(_) => self.renumber(ty),
                        None => self.written(ty),
                    }
                }
                None => self.renumber(ty),
            });
        }
        self.regions.local_tys = local_tys.into_iter().map(|it| it.unwrap()).collect();
    }

    /// The arguments of a closure's or coroutine's type with each region a universal region of
    /// its creator's, by position. They start with the `parents` generic arguments of the
    /// enclosing item, which rustc makes the item's own parameters where the closure is made.
    fn creator_args(&mut self, args: GenericArgs<'db>, parents: usize) -> GenericArgs<'db> {
        let args =
            rustc_type_ir::fold_regions(self.interner(), args, |region, _| match region.kind() {
                RegionKind::ReBound(..) => region,
                _ => {
                    let region = self.new_universal(UniversalKind::Creator);
                    let position = self.regions.creator_positions.len();
                    self.regions
                        .creator_positions
                        .insert(self.regions.universals.len() - 1, position);
                    region
                }
            });
        for (index, arg) in args.iter().take(parents).enumerate() {
            if let GenericArgKind::Lifetime(region) = arg.kind()
                && let Some(rv) = rv_of(region)
            {
                self.early.insert(index as u32, region);
                self.regions.early.insert(index as u32, rv);
            }
        }
        args
    }

    /// A closure's parameters, return slot and environment typed as rustc's `UniversalRegions`
    /// types them: each region of the closure's type a universal region of its creator's, and
    /// the late-bound lifetimes of its signature and the borrow of its environment its own.
    /// `false`, with nothing set, when the closure's type does not have the shape for it.
    fn closure_signature(&mut self, local_tys: &mut [Option<Ty<'db>>]) -> bool {
        let body = self.body;
        let Some((&env_local, params)) = body.param_locals.split_first() else { return false };
        let env = body.locals[env_local].ty.as_ref();
        let (closure, by_ref) = match env.kind() {
            TyKind::Ref(_, inner, mutability) => (inner, Some(mutability)),
            _ => (env, None),
        };
        let interner = self.interner();
        if let TyKind::Coroutine(def, args) = closure.kind() {
            // An `async` body is polled through `&mut` its future, with a `&mut Context` of the
            // caller's; it returns what its type says the future's output is.
            let args = self.creator_args(args, args.as_coroutine().parent_args().len());
            let mut coroutine = Ty::new_coroutine(interner, def, args);
            if let Some(mutability) = by_ref {
                let region = self.new_universal(UniversalKind::AnonParam);
                self.regions.closure_env = Some(self.regions.universals.len() - 1);
                coroutine = Ty::new_ref(interner, region, coroutine, mutability);
            }
            local_tys[env_local.into_raw().into_u32() as usize] = Some(coroutine);
            for &local in params {
                let ty = rustc_type_ir::fold_regions(
                    interner,
                    body.locals[local].ty.as_ref(),
                    |region, _| match region.kind() {
                        RegionKind::ReBound(..) => region,
                        _ => self.new_universal(UniversalKind::AnonParam),
                    },
                );
                local_tys[local.into_raw().into_u32() as usize] = Some(ty);
            }
            local_tys[super::return_slot().into_raw().into_u32() as usize] =
                Some(args.as_coroutine().return_ty());
            return true;
        }
        if let TyKind::CoroutineClosure(def, args) = closure.kind() {
            // An `async` closure called by reference returns a future whose captures are
            // borrowed for as long as that reference: rustc's `AsyncFn` lending.
            let TyKind::Coroutine(coroutine, _) =
                body.locals[super::return_slot()].ty.as_ref().kind()
            else {
                return false;
            };
            let args = self.creator_args(args, args.as_coroutine_closure().parent_args().len());
            let mut env_ty = Ty::new_coroutine_closure(interner, def, args);
            let (kind, env_region) = match by_ref {
                Some(mutability) => {
                    let region = self.new_universal(UniversalKind::AnonParam);
                    self.regions.closure_env = Some(self.regions.universals.len() - 1);
                    env_ty = Ty::new_ref(interner, region, env_ty, mutability);
                    let kind = if mutability.is_mut() {
                        rustc_type_ir::ClosureKind::FnMut
                    } else {
                        rustc_type_ir::ClosureKind::Fn
                    };
                    (kind, region)
                }
                None => (rustc_type_ir::ClosureKind::FnOnce, Region::new_static(interner)),
            };
            let parts = args.as_coroutine_closure();
            let mut late = FxHashMap::default();
            let sig = rustc_type_ir::fold_regions(
                interner,
                parts.coroutine_closure_sig().skip_binder(),
                |region, depth| match region.kind() {
                    RegionKind::ReBound(
                        rustc_type_ir::BoundVarIndexKind::Bound(debruijn),
                        bound,
                    ) if debruijn == depth => *late
                        .entry(bound.var.as_u32())
                        .or_insert_with(|| self.new_universal(UniversalKind::AnonParam)),
                    _ => region,
                },
            );
            let TyKind::Tuple(inputs) = sig.tupled_inputs_ty.kind() else { return false };
            if inputs.len() != params.len() {
                return false;
            }
            local_tys[env_local.into_raw().into_u32() as usize] = Some(env_ty);
            for (&local, ty) in params.iter().zip(inputs.iter()) {
                local_tys[local.into_raw().into_u32() as usize] = Some(ty);
            }
            local_tys[super::return_slot().into_raw().into_u32() as usize] =
                Some(sig.to_coroutine_given_kind_and_upvars(
                    interner,
                    parts.parent_args(),
                    coroutine,
                    kind,
                    env_region,
                    parts.tupled_upvars_ty(),
                    parts.coroutine_captures_by_ref_ty(),
                ));
            return true;
        }
        let TyKind::Closure(def, args) = closure.kind() else { return false };
        let sig = args.as_closure().sig();
        let &[tupled] = sig.skip_binder().inputs().as_slice() else { return false };
        let TyKind::Tuple(inputs) = tupled.kind() else { return false };
        if inputs.len() != params.len() {
            return false;
        }
        let args = self.creator_args(args, args.as_closure().parent_args().len());
        let mut closure = Ty::new_closure(interner, def, args);
        if let Some(mutability) = by_ref {
            let region = self.new_universal(UniversalKind::AnonParam);
            self.regions.closure_env = Some(self.regions.universals.len() - 1);
            closure = Ty::new_ref(interner, region, closure, mutability);
        }
        let mut late = FxHashMap::default();
        let sig = rustc_type_ir::fold_regions(
            interner,
            args.as_closure().sig().skip_binder(),
            |region, depth| match region.kind() {
                RegionKind::ReBound(rustc_type_ir::BoundVarIndexKind::Bound(debruijn), bound)
                    if debruijn == depth =>
                {
                    *late
                        .entry(bound.var.as_u32())
                        .or_insert_with(|| self.new_universal(UniversalKind::AnonParam))
                }
                _ => region,
            },
        );
        let &[tupled] = sig.inputs().as_slice() else { return false };
        let TyKind::Tuple(inputs) = tupled.kind() else { return false };
        local_tys[env_local.into_raw().into_u32() as usize] = Some(closure);
        for (index, (&local, ty)) in params.iter().zip(inputs.iter()).enumerate() {
            let ty = self.elided_own(index, ty).unwrap_or(ty);
            let ty = self.written_object_default(index, ty).unwrap_or(ty);
            local_tys[local.into_raw().into_u32() as usize] = Some(ty);
        }
        local_tys[super::return_slot().into_raw().into_u32() as usize] = Some(sig.output());
        true
    }

    /// The closure's parameter `index` written `&dyn Trait`, as `&'r (dyn Trait + 'r)` with
    /// its reference's lifetime, as rustc's object lifetime default, where inference gave the
    /// object a region of its own.
    fn written_object_default(&mut self, index: usize, ty: Ty<'db>) -> Option<Ty<'db>> {
        let closure = self.ctx.closure_expr?;
        let hir_def::hir::Expr::Closure { arg_types, .. } = &self.ctx.store[closure] else {
            return None;
        };
        let written = self.lower_written((*arg_types.get(index)?)?, MirSpan::ExprId(closure));
        let TyKind::Ref(_, written, _) = written.kind() else { return None };
        let TyKind::Dynamic(_, object) = written.kind() else { return None };
        if !matches!(object.kind(), RegionKind::ReStatic) || writes_static_object(self.ctx.store) {
            return None;
        }
        let TyKind::Ref(region, inner, mutability) = ty.kind() else { return None };
        let TyKind::Dynamic(predicates, _) = inner.kind() else { return None };
        let interner = self.interner();
        Some(Ty::new_ref(
            interner,
            region,
            Ty::new_dynamic(interner, predicates, region),
            mutability,
        ))
    }

    /// The type of the closure's parameter `index`, where the closure is typed without an
    /// expected signature, with each lifetime elided in its written type a late-bound one of
    /// the closure's own, as rustc's closure signature binds them: `|x: &i32| -> &i32 { x }`
    /// returns what lives as long as the call, not as long as the closure's type says.
    fn elided_own(&mut self, index: usize, ty: Ty<'db>) -> Option<Ty<'db>> {
        let closure = self.ctx.closure_expr?;
        if !self.ctx.infer.closures_without_expectation.contains(&closure) {
            return None;
        }
        let hir_def::hir::Expr::Closure { arg_types, .. } = &self.ctx.store[closure] else {
            return None;
        };
        let written = self.lower_written((*arg_types.get(index)?)?, MirSpan::ExprId(closure));
        let regions = |ty: Ty<'db>| {
            let mut found = Vec::new();
            rustc_type_ir::fold_regions(self.interner(), ty, |region, _| {
                found.push(region);
                region
            });
            found
        };
        let elided: Vec<bool> = regions(written)
            .into_iter()
            .map(|region| {
                !matches!(
                    region.kind(),
                    RegionKind::ReEarlyParam(_)
                        | RegionKind::ReLateParam(_)
                        | RegionKind::ReStatic
                        | RegionKind::ReBound(..)
                )
            })
            .collect();
        if elided.len() != regions(ty).len() || !elided.contains(&true) {
            return None;
        }
        let mut position = 0;
        Some(rustc_type_ir::fold_regions(self.interner(), ty, |region, _| {
            position += 1;
            match elided[position - 1] {
                true => self.new_universal(UniversalKind::AnonParam),
                false => region,
            }
        }))
    }

    /// What a `let` with a type written out requires, as rustc's user type annotations: that
    /// the type is well-formed, and for a `_` pattern that the value has the type.
    fn annotations(&mut self) {
        let lets = self.lets();
        self.pattern_paths();
        self.path_arguments();
        self.closure_annotations();
        for annotation in lets {
            let (pat, initializer) = (annotation.pat, annotation.initializer);
            // A variable with a type written out has it, lifetimes and all.
            let local = match self.ctx.store[pat] {
                Pat::Bind { id, subpat: None } => self.body.binding_locals.get(id).copied(),
                _ => None,
            };
            let at = At { category: Category::TypeAnnotation, span: MirSpan::PatId(pat) };
            let ty = match local.and_then(|it| Some((it, *self.annotated.get(&it)?))) {
                Some((local, written)) => {
                    let local_ty = self.regions.local_tys[local.into_raw().into_u32() as usize];
                    if let Some(written) = written {
                        self.relate(local_ty, written, Variance::Invariant, at);
                    }
                    Some(local_ty)
                }
                None => self.annotation_ty(annotation),
            };
            let Some(ty) = ty else { continue };
            self.well_formed(ty, at, 0);
            if !matches!(self.ctx.store[pat], Pat::Bind { subpat: None, .. }) {
                self.annotated_bindings(pat, ty, at, true);
            }
            if !matches!(self.ctx.store[pat], Pat::Bind { .. })
                && let Some(initializer) = initializer
            {
                let value = match self.path_local(initializer) {
                    Some(local) => {
                        Some(self.regions.local_tys[local.into_raw().into_u32() as usize])
                    }
                    None => self
                        .ascribed_place(pat)
                        .map(|(local, projection)| self.place_ty(local, &projection).ty),
                };
                if let Some(value) = value {
                    self.relate(value, ty, Variance::Covariant, at);
                }
            }
        }
    }

    /// A closure's parameters and return slot have the types written for them, as rustc's
    /// `check_signature_annotation`: in `|x: &'a i32| -> &'static i32` that `x` is a
    /// `&'a i32` and the closure returns a `&'static i32`.
    fn closure_annotations(&mut self) {
        let Some(closure) = self.ctx.closure_expr else { return };
        let hir_def::hir::Expr::Closure {
            arg_types,
            ret_type,
            closure_kind: hir_def::hir::ClosureKind::Closure,
            ..
        } = &self.ctx.store[closure]
        else {
            return;
        };
        let params = self.body.param_locals.get(1..).unwrap_or_default();
        if !self.regions.check_universals || params.len() != arg_types.len() {
            return;
        }
        let span = MirSpan::ExprId(closure);
        let at = At { category: Category::Boring, span };
        let written: Vec<_> = params
            .iter()
            .copied()
            .zip(arg_types.iter().copied())
            .chain([(super::return_slot(), *ret_type)])
            .collect();
        for (local, type_ref) in written {
            let Some(type_ref) = type_ref else { continue };
            let ty = self.written(self.lower_written(type_ref, span));
            let local_ty = self.regions.local_tys[local.into_raw().into_u32() as usize];
            self.relate(local_ty, ty, Variance::Invariant, at);
        }
    }

    /// The variables a pattern `pat` with the type `ty` written binds have the parts of `ty`
    /// they bind, as rustc's user type projections: in `let (y, _): (&'static u32, u32)`,
    /// `y` is a `&'static u32`. Not through a binding by reference or a pattern that derefs
    /// what it matches by default binding modes.
    fn annotated_bindings(&mut self, pat: hir_def::hir::PatId, ty: Ty<'db>, at: At, top: bool) {
        if self.ctx.infer.pat_adjustments.contains_key(&pat) {
            return;
        }
        let interner = self.interner();
        match &self.ctx.store[pat] {
            Pat::Bind { id, subpat } => {
                if !top
                    && let Some(hir_ty::BindingMode(by_ref, _)) = self.ctx.infer.binding_mode(pat)
                    && let Some(&local) = self.body.binding_locals.get(*id)
                {
                    let local_ty = self.regions.local_tys[local.into_raw().into_u32() as usize];
                    // `ref y` borrows the part, which has the type.
                    match (by_ref, local_ty.kind()) {
                        (hir_ty::ByRef::No, _) => {
                            self.relate(local_ty, ty, Variance::Invariant, at)
                        }
                        (hir_ty::ByRef::Yes(_), TyKind::Ref(_, inner, _)) => {
                            self.relate(inner, ty, Variance::Invariant, at)
                        }
                        _ => (),
                    }
                }
                if let Some(subpat) = *subpat {
                    self.annotated_bindings(subpat, ty, at, false);
                }
            }
            Pat::Tuple { args, ellipsis: None } => {
                if let TyKind::Tuple(tys) = ty.kind()
                    && tys.len() == args.len()
                {
                    let args = args.clone();
                    for (pat, ty) in args.iter().zip(tys.iter()) {
                        self.annotated_bindings(*pat, ty, at, false);
                    }
                }
            }
            Pat::Ref { pat, .. } => {
                if let TyKind::Ref(_, inner, _) = ty.kind() {
                    self.annotated_bindings(*pat, inner, at, false);
                }
            }
            Pat::Record { args, ellipsis: _, .. } => {
                let TyKind::Adt(_, adt_args) = ty.kind() else { return };
                let Some(variant) = self.ctx.infer.variant_resolution_for_pat(pat) else { return };
                let fields = hir_def::signatures::VariantFields::of(self.ctx.db, variant);
                let types = self.ctx.db.field_types(variant);
                let parts: Vec<_> = args
                    .iter()
                    .filter_map(|it| {
                        let field = fields.field(&it.name)?;
                        Some((it.pat, types[field].ty().instantiate(interner, adt_args)))
                    })
                    .collect();
                for (pat, field) in parts {
                    let mut field = field.skip_norm_wip();
                    if rustc_type_ir::TypeVisitableExt::has_aliases(&field) {
                        field = self.normalize(field).unwrap_or(field);
                    }
                    self.annotated_bindings(pat, field, at, false);
                }
            }
            Pat::TupleStruct { args, ellipsis: None, .. } => {
                let TyKind::Adt(_, adt_args) = ty.kind() else { return };
                let Some(variant) = self.ctx.infer.variant_resolution_for_pat(pat) else { return };
                let types = self.ctx.db.field_types(variant);
                let parts: Vec<_> = args
                    .iter()
                    .zip(types.iter())
                    .map(|(pat, (_, field))| (*pat, field.ty().instantiate(interner, adt_args)))
                    .collect();
                for (pat, field) in parts {
                    let mut field = field.skip_norm_wip();
                    if rustc_type_ir::TypeVisitableExt::has_aliases(&field) {
                        field = self.normalize(field).unwrap_or(field);
                    }
                    self.annotated_bindings(pat, field, at, false);
                }
            }
            _ => (),
        }
    }

    /// The generic arguments a path in the body spells out are well-formed, reached or not,
    /// as rustc's user type annotations: `None::<<&'a () as Trait>::Ty>` that `&'a ()`
    /// implements `Trait`.
    fn path_arguments(&mut self) {
        let store = self.ctx.store;
        let mut parent = FxHashMap::default();
        for (expr, _) in store.exprs() {
            store.walk_child_exprs(expr, |child| {
                parent.insert(child, expr);
            });
        }
        // A path in a closure in this body is the closure's, which proves it.
        let owner = |mut expr| loop {
            match parent.get(&expr) {
                Some(&up) if matches!(store[up], hir_def::hir::Expr::Closure { .. }) => {
                    return Some(up);
                }
                Some(&up) => expr = up,
                None => return None,
            }
        };
        let mut found = Vec::new();
        let mut items = Vec::new();
        for (expr, it) in store.exprs() {
            let hir_def::hir::Expr::Path(path) = it else { continue };
            if owner(expr) != self.ctx.closure_expr {
                continue;
            }
            if spells_generics(path)
                && let Some(ty) = self.ctx.infer.type_of_expr.get(expr)
                && let TyKind::FnDef(def, args) = ty.as_ref().kind()
            {
                items.push((expr, def.0, args));
            }
            for args in path.generic_args().into_iter().flatten().flatten() {
                for arg in args.args.iter() {
                    if let hir_def::expr_store::path::GenericArg::Type(type_ref) = arg {
                        found.push((expr, *type_ref));
                    }
                }
            }
        }
        for (expr, type_ref) in found {
            let span = MirSpan::ExprId(expr);
            let ty = self.lower_written(type_ref, span);
            let ty = self.written(ty);
            self.well_formed(ty, At { category: Category::TypeAnnotation, span }, 0);
        }
        // A closure proves its calls' where clauses at them, which it can propagate.
        if self.ctx.closure_local.is_some() {
            return;
        }
        for (expr, def, args) in items {
            let args = self.written(args);
            self.callee_predicates(def, args, MirSpan::ExprId(expr));
        }
    }

    /// What a struct, variant or constant pattern whose path spells out generic arguments
    /// requires of the value it matches, `let Foo::<'static> { .. } = foo;` that `foo` is a
    /// `Foo<'static>`, as rustc's user type annotation on the pattern. Only for the whole
    /// pattern of a `let` or a `match` arm, matching a variable or what the `let` reads.
    fn pattern_paths(&mut self) {
        let spelled = |pat: &Pat| match pat {
            Pat::Record { path, .. } | Pat::TupleStruct { path, .. } | Pat::Path(path) => {
                spells_generics(path)
            }
            _ => false,
        };
        let mut found = Vec::new();
        for (_, expr) in self.ctx.store.exprs() {
            match expr {
                hir_def::hir::Expr::Block { statements, .. } => {
                    for statement in statements.iter() {
                        if let Statement::Let { pat, type_ref: None, initializer, .. } = statement
                            && spelled(&self.ctx.store[*pat])
                        {
                            found.push((*pat, *initializer));
                        }
                    }
                }
                hir_def::hir::Expr::Match { expr, arms } => {
                    for arm in arms.iter() {
                        if spelled(&self.ctx.store[arm.pat]) {
                            found.push((arm.pat, Some(*expr)));
                        }
                    }
                }
                _ => (),
            }
        }
        for (pat, value) in found {
            let Some(ty) = self.ctx.infer.type_of_pat.get(pat).map(|it| it.as_ref()) else {
                continue;
            };
            let value = match value.and_then(|it| self.path_local(it)) {
                Some(local) => Some(self.regions.local_tys[local.into_raw().into_u32() as usize]),
                None => self
                    .ascribed_place(pat)
                    .map(|(local, projection)| self.place_ty(local, &projection).ty),
            };
            let Some(value) = value else { continue };
            let ty = self.written(ty);
            let at = At { category: Category::TypeAnnotation, span: MirSpan::PatId(pat) };
            if let Pat::Path(_) = self.ctx.store[pat]
                && let Some(constant) = self.pattern_constant(pat)
            {
                // rustc ascribes a constant's type to what it matches contravariantly:
                // `x: &'x str` matches a `&'static str`, a `&'a str` only matches a
                // `&'static str` if `'a: 'static`.
                let constant = self.regions.local_tys[constant.into_raw().into_u32() as usize];
                self.relate(constant, value, Variance::Covariant, at);
            } else if let Pat::Path(_) = self.ctx.store[pat] {
                let common = self.renumber(value);
                self.relate(value, common, Variance::Covariant, at);
                self.relate(ty, common, Variance::Covariant, at);
            } else {
                self.relate(value, ty, Variance::Covariant, at);
            }
        }
    }

    /// The local a constant pattern's value is put in to be compared.
    fn pattern_constant(&self, pat: hir_def::hir::PatId) -> Option<LocalId> {
        self.body.basic_blocks.iter().find_map(|(_, block)| {
            block.statements.iter().find_map(|statement| match &statement.kind {
                StatementKind::Assign(place, Rvalue::Use(operand))
                    if statement.span == MirSpan::PatId(pat)
                        && matches!(operand.kind, OperandKind::Constant { .. })
                        && place.as_ref().projection.as_slice().is_empty() =>
                {
                    Some(place.as_ref().local)
                }
                _ => None,
            })
        })
    }

    /// The place the `let` of the pattern `pat` reads its value from: the fake read lowered at
    /// the pattern.
    fn ascribed_place(&self, pat: hir_def::hir::PatId) -> Option<(LocalId, Vec<PlaceElem>)> {
        self.body.basic_blocks.iter().find_map(|(_, block)| {
            block.statements.iter().find_map(|statement| match &statement.kind {
                StatementKind::FakeRead(place) if statement.span == MirSpan::PatId(pat) => {
                    let place = place.as_ref();
                    Some((place.local, place.projection.as_slice().to_vec()))
                }
                _ => None,
            })
        })
    }

    /// The local of the variable `expr` names, if it is a path to one of this body's.
    fn path_local(&self, expr: hir_def::hir::ExprId) -> Option<LocalId> {
        self.body.binding_locals.get(self.path_binding(expr)?).copied()
    }

    /// The variable a call's callee names, a capture of this closure's too.
    fn callee_local(&self, expr: hir_def::hir::ExprId) -> Option<LocalId> {
        let binding = self.path_binding(expr)?;
        self.body
            .binding_locals
            .get(binding)
            .copied()
            .or_else(|| self.ctx.upvars.iter().find(|it| it.binding == binding).map(|it| it.local))
    }

    fn path_binding(&self, expr: hir_def::hir::ExprId) -> Option<hir_def::hir::BindingId> {
        use hir_def::resolver::{HasResolver, ValueNs};
        let hir_def::hir::Expr::Path(path) = &self.ctx.store[expr] else { return None };
        let db = self.ctx.db;
        let owner = self.body.owner;
        let mut resolver = owner.resolver(db);
        let _guard = resolver.update_to_inner_scope(db, owner.expression_store_owner(db), expr);
        let hygiene = self.ctx.store.expr_path_hygiene(expr);
        match resolver.resolve_path_in_value_ns_fully(db, path, hygiene)? {
            ValueNs::LocalBinding(binding) => Some(binding),
            _ => None,
        }
    }

    /// The constraints the type `ty` holds being well-formed: in `&'a T` that `T: 'a`, and
    /// the where clauses of the structs and enums it names.
    fn well_formed(&mut self, ty: Ty<'db>, at: At, depth: usize) {
        if depth > 8 {
            return;
        }
        match ty.kind() {
            TyKind::Ref(region, inner, _) => {
                self.type_outlives(inner, region, at);
                self.well_formed(inner, at, depth + 1);
            }
            TyKind::Adt(def, args) => {
                self.predicates(GenericDefId::AdtId(def.def_id()), args, at, true);
                for arg in args.iter() {
                    if let GenericArgKind::Type(it) = arg.kind() {
                        self.well_formed(it, at, depth + 1);
                    }
                }
            }
            TyKind::Tuple(tys) => {
                for it in tys.iter() {
                    self.well_formed(it, at, depth + 1);
                }
            }
            TyKind::Array(inner, _) | TyKind::Slice(inner) | TyKind::RawPtr(inner, _) => {
                self.well_formed(inner, at, depth + 1)
            }
            // An associated type is well-formed where its trait is implemented, `<&'a () as
            // Trait>::Ty` with only `impl Trait for &'static ()` that `'a: 'static`.
            TyKind::Alias(alias) => {
                for it in alias.args.types() {
                    self.well_formed(it, at, depth + 1);
                }
                if let rustc_type_ir::AliasTyKind::Projection { .. } = alias.kind {
                    self.prove(alias.trait_ref(self.interner()), at);
                    // And what it normalizes to, as rustc checks the normalized signature of a
                    // call: `FnOnce(&'s str) -> [&'static &'s (); 0]` that `'s: 'static`. Not
                    // one with a trait object, whose lifetime rust-analyzer can make `'static`.
                    if rustc_type_ir::TypeVisitableExt::has_free_regions(&ty)
                        && let Some(normalized) = self.normalize(ty)
                        && !matches!(normalized.kind(), TyKind::Alias(..) | TyKind::Infer(_))
                        && !normalized.walk().any(|it| {
                            rustc_type_ir::inherent::GenericArg::as_type(&it).is_some_and(|it| {
                                matches!(it.kind(), TyKind::Dynamic(..) | TyKind::Infer(_))
                            })
                        })
                    {
                        self.well_formed(normalized, at, depth + 1);
                    }
                }
            }
            // As rustc, a function item is well-formed with its where clauses and its return
            // type, but what names its late-bound lifetimes is not checked.
            TyKind::FnDef(def, args) => {
                let generic_def: GenericDefId = match def.0 {
                    CallableDefId::FunctionId(it) => it.into(),
                    CallableDefId::StructId(it) => it.into(),
                    CallableDefId::EnumVariantId(it) => it.lookup(self.ctx.db).parent.into(),
                };
                self.predicates(generic_def, args, at, true);
                let output = self
                    .ctx
                    .db
                    .callable_item_signature(def.0)
                    .instantiate(self.interner(), args)
                    .skip_norm_wip()
                    .output()
                    .skip_binder();
                self.well_formed_unbound(output, at, depth + 1);
            }
            _ => (),
        }
    }

    /// What of `ty`'s well-formedness names no region bound outside it, as rustc's `wf`
    /// leaves out a requirement with an escaping bound region: of `(&'c (), &'b &'a ())` with
    /// `'c` late-bound, that `'a: 'b`.
    fn well_formed_unbound(&mut self, ty: Ty<'db>, at: At, depth: usize) {
        use rustc_type_ir::inherent::GenericArgs as _;
        if !rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&ty) {
            return self.well_formed(ty, at, depth);
        }
        if depth > 16 {
            return;
        }
        match ty.kind() {
            TyKind::Ref(_, inner, _)
            | TyKind::RawPtr(inner, _)
            | TyKind::Slice(inner)
            | TyKind::Array(inner, _) => self.well_formed_unbound(inner, at, depth + 1),
            TyKind::Tuple(tys) => {
                for ty in tys {
                    self.well_formed_unbound(ty, at, depth + 1);
                }
            }
            TyKind::Adt(_, args) => {
                for ty in args.types() {
                    self.well_formed_unbound(ty, at, depth + 1);
                }
            }
            _ => (),
        }
    }

    /// What a path naming the associated item of `container` through a type needs, as
    /// rustc's user type annotation on it: its generic arguments well-formed, and the impl's
    /// self type and trait arguments, `<Evil>::f` with `struct Evil<'a, 'b: 'a>` that `'b: 'a`.
    fn path_well_formed(
        &mut self,
        expr: hir_def::hir::ExprId,
        container: ItemContainerId,
        args: GenericArgs<'db>,
        span: MirSpan,
    ) {
        let hir_def::hir::Expr::Path(path) = &self.ctx.store[expr] else { return };
        if !names_through_type(self.ctx.db, path, container) {
            return;
        }
        let at = At { category: Category::TypeAnnotation, span };
        let mut tys: Vec<Ty<'db>> = args.types().collect();
        if let ItemContainerId::ImplId(impl_) = container {
            let db = self.ctx.db;
            tys.push(db.impl_self_ty(impl_).instantiate(self.interner(), args).skip_norm_wip());
            if let Some(trait_ref) = db.impl_trait(impl_) {
                let trait_ref = trait_ref.instantiate(self.interner(), args).skip_norm_wip();
                tys.extend(trait_ref.args.types().skip(1));
            }
        }
        for ty in tys {
            self.well_formed(ty, at, 0);
        }
    }

    fn annotated_locals(&self) -> FxHashMap<LocalId, Annotation> {
        let mut found = FxHashMap::default();
        for annotation in self.lets() {
            if let Pat::Bind { id, subpat: None } = self.ctx.store[annotation.pat]
                && let Some(&local) = self.body.binding_locals.get(id)
            {
                found.insert(local, annotation);
            }
        }
        found
    }

    /// The `let`s with a type written out.
    fn lets(&self) -> Vec<Annotation> {
        let mut found = Vec::new();
        for (block, expr) in self.ctx.store.exprs() {
            let hir_def::hir::Expr::Block { statements, .. } = expr else { continue };
            for statement in statements.iter() {
                if let Statement::Let { pat, type_ref: Some(type_ref), initializer, .. } = statement
                {
                    found.push(Annotation {
                        pat: *pat,
                        type_ref: *type_ref,
                        initializer: *initializer,
                        block,
                    });
                }
            }
        }
        found
    }

    fn env_aliases(&mut self) {
        fn behind_refs<'db>(ty: Ty<'db>, out: &mut Vec<Ty<'db>>, depth: usize) {
            if depth > 4 {
                return;
            }
            match ty.kind() {
                TyKind::Ref(_, inner, _) => {
                    alias_components(inner, out);
                    behind_refs(inner, out, depth + 1);
                }
                TyKind::Adt(_, args) => {
                    for it in args.types() {
                        behind_refs(it, out, depth + 1);
                    }
                }
                TyKind::Tuple(tys) => {
                    for it in tys.iter() {
                        behind_refs(it, out, depth + 1);
                    }
                }
                TyKind::Array(inner, _) | TyKind::Slice(inner) | TyKind::RawPtr(inner, _) => {
                    behind_refs(inner, out, depth + 1)
                }
                _ => (),
            }
        }
        let interner = self.interner();
        let mut aliases = Vec::new();
        for clause in self.ctx.env.caller_bounds().iter() {
            if let ClauseKind::TypeOutlives(OutlivesPredicate(ty, _)) = clause.kind().skip_binder()
                && let TyKind::Alias(_) = ty.kind()
            {
                aliases.push(ty);
            }
        }
        if self.ctx.closure_local.is_none() {
            let signature: Vec<Ty<'db>> = self
                .body
                .param_locals
                .iter()
                .chain((!self.returns_opaque).then_some(&super::return_slot()))
                .map(|&it| self.regions.local_tys[it.into_raw().into_u32() as usize])
                .collect();
            let mut found = Vec::new();
            for ty in signature {
                behind_refs(ty, &mut found, 0);
            }
            for ty in found {
                aliases.push(ty);
                if let Some(normalized) = self.normalize(ty) {
                    alias_components(normalized, &mut aliases);
                }
            }
        }
        self.env_aliases = aliases
            .into_iter()
            .map(|ty| {
                rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_static(interner))
            })
            .collect();
    }

    /// What normalizing the signature needs, as rustc normalizes the type of each parameter
    /// and the return type: the where clauses of the impl that a projection there is of,
    /// `<&'a () as Visitor<'d>>::Value` with only `impl<'a, 'd: 'a> Visitor<'d> for &'a ()`
    /// that `'d: 'a`.
    fn signature_projections(&mut self) {
        if self.ctx.closure_local.is_some() || self.ctx.inline_const {
            return;
        }
        let interner = self.interner();
        let mut trait_refs = Vec::new();
        for &local in self
            .body
            .param_locals
            .iter()
            .chain((!self.returns_opaque).then_some(&super::return_slot()))
        {
            let ty = self.regions.local_tys[local.into_raw().into_u32() as usize];
            for arg in ty.walk() {
                if let Some(it) = rustc_type_ir::inherent::GenericArg::as_type(&arg)
                    && let TyKind::Alias(alias) = it.kind()
                    && let rustc_type_ir::AliasTyKind::Projection { .. } = alias.kind
                {
                    let trait_ref = alias.trait_ref(interner);
                    if !trait_refs.contains(&trait_ref) {
                        trait_refs.push(trait_ref);
                    }
                }
            }
        }
        for trait_ref in trait_refs {
            self.prove(trait_ref, At { category: Category::Signature, span: MirSpan::Unknown });
        }
    }

    /// What the signature says of which universal regions outlive which: its where clauses
    /// and what its parameters' types imply, as `&'a T` that `T: 'a`.
    fn known_relations(&mut self) {
        let generic_def: Option<GenericDefId> = match self.body.owner {
            InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(it)) => Some(it.into()),
            InferBodyId::DefWithBodyId(DefWithBodyId::ConstId(it)) => Some(it.into()),
            InferBodyId::DefWithBodyId(DefWithBodyId::StaticId(it)) => Some(it.into()),
            _ => None,
        };
        let mut pairs = Vec::new();
        if let Some(def) = generic_def
            && self.ctx.closure_local.is_none()
        {
            for clause in GenericPredicates::query_all(self.ctx.db, def).skip_binder() {
                match clause.kind().skip_binder() {
                    ClauseKind::RegionOutlives(OutlivesPredicate(longer, shorter)) => {
                        if let (Some(longer), Some(shorter)) = (
                            self.signature_region(longer, rustc_type_ir::INNERMOST),
                            self.signature_region(shorter, rustc_type_ir::INNERMOST),
                        ) && let (Some(longer), Some(shorter)) = (rv_of(longer), rv_of(shorter))
                        {
                            pairs.push((longer, shorter));
                        }
                    }
                    ClauseKind::TypeOutlives(OutlivesPredicate(ty, region)) => {
                        if let Some(region) =
                            self.signature_region(region, rustc_type_ir::INNERMOST).and_then(rv_of)
                        {
                            let ty = self.signature_ty(ty);
                            self.regions.type_bounds.push((ty, region));
                        }
                    }
                    _ => (),
                }
            }
        }
        // The bounds the where clauses imply too, as of supertraits: `T: Trait` where
        // `trait Trait: 'static`.
        if self.ctx.closure_local.is_none() {
            let clauses: Vec<_> = self.ctx.env.caller_bounds().iter().collect();
            for clause in clauses {
                match clause.kind().skip_binder() {
                    ClauseKind::TypeOutlives(OutlivesPredicate(ty, region)) => {
                        if let Some(region) =
                            self.signature_region(region, rustc_type_ir::INNERMOST).and_then(rv_of)
                        {
                            let ty = self.signature_ty(ty);
                            self.regions.type_bounds.push((ty, region));
                        }
                    }
                    ClauseKind::RegionOutlives(OutlivesPredicate(longer, shorter)) => {
                        if let (Some(longer), Some(shorter)) = (
                            self.signature_region(longer, rustc_type_ir::INNERMOST),
                            self.signature_region(shorter, rustc_type_ir::INNERMOST),
                        ) && let (Some(longer), Some(shorter)) = (rv_of(longer), rv_of(shorter))
                        {
                            pairs.push((longer, shorter));
                        }
                    }
                    _ => (),
                }
            }
        }
        let signature: Vec<Ty<'db>> = self
            .body
            .param_locals
            .iter()
            .chain((!self.returns_opaque).then_some(&super::return_slot()))
            .map(|&it| self.regions.local_tys[it.into_raw().into_u32() as usize])
            .collect();
        let mut signature = signature;
        signature.extend(self.impl_header_types());
        let mut seen = FxHashSet::default();
        for ty in signature {
            self.implied_bounds(ty, &mut pairs, &mut seen, 0, false);
        }
        let count = self.regions.universals.len();
        let mut known = vec![BitSet::new(count); count];
        let static_index = self.regions.universal(self.regions.static_rv).unwrap();
        for (index, row) in known.iter_mut().enumerate() {
            row.insert(index);
            // Every universal region outlives the body.
            row.insert(1);
        }
        for shorter in 0..count {
            known[static_index].insert(shorter);
        }
        for (longer, shorter) in pairs {
            if let (Some(longer), Some(shorter)) =
                (self.regions.universal(longer), self.regions.universal(shorter))
            {
                known[longer].insert(shorter);
            }
        }
        // What is known is closed under transitivity.
        for middle in 0..count {
            for longer in 0..count {
                if known[longer].contains(middle) {
                    let row = known[middle].clone();
                    known[longer].union(&row);
                }
            }
        }
        self.regions.known = known;
    }

    /// The self type of the impl `function` is in with its anonymous lifetimes universal
    /// regions, when it has any.
    /// The region of the one `&Self` in the type of a method's `self` (`self: Box<&Self>`,
    /// `Pin<&mut Self>`), which rustc's lifetime elision gives the elided lifetimes of the
    /// return type.
    fn self_reference(&self, function: hir_def::FunctionId, ty: Ty<'db>) -> Option<Region<'db>> {
        let ItemContainerId::ImplId(impl_) = function.lookup(self.ctx.db).container else {
            return None;
        };
        let interner = self.interner();
        let erase = |ty: Ty<'db>| {
            let ty = rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_static(interner));
            super::liveness::normalize_erased(self.ctx, ty).unwrap_or(ty)
        };
        let self_ty = erase(self.ctx.db.impl_self_ty(impl_).instantiate_identity().skip_norm_wip());
        let mut found = Vec::new();
        let mut stack = vec![ty];
        while let Some(ty) = stack.pop() {
            match ty.kind() {
                TyKind::Ref(region, inner, _) if erase(inner) == self_ty => found.push(region),
                TyKind::Ref(_, inner, _) => stack.push(inner),
                TyKind::Adt(_, args) => {
                    stack.extend(args.iter().filter_map(|it| match it.kind() {
                        GenericArgKind::Type(ty) => Some(ty),
                        _ => None,
                    }))
                }
                _ => (),
            }
        }
        found.dedup();
        match found.as_slice() {
            [only] => Some(*only),
            _ => None,
        }
    }

    fn impl_self_of(&mut self, function: hir_def::FunctionId) -> Option<(Ty<'db>, Ty<'db>)> {
        let ItemContainerId::ImplId(impl_) = function.lookup(self.ctx.db).container else {
            return None;
        };
        let raw = self.ctx.db.impl_self_ty(impl_).instantiate_identity().skip_norm_wip();
        let mut anonymous = 0;
        let named =
            rustc_type_ir::fold_regions(self.interner(), raw, |region, _| match region.kind() {
                // In a closure, the impl's anonymous lifetimes are its creator's.
                RegionKind::ReError(_) if self.nested() => {
                    let region = self.new_universal(UniversalKind::Creator);
                    self.regions
                        .creator_positions
                        .insert(self.regions.universals.len() - 1, IMPL_ANON + anonymous);
                    anonymous += 1;
                    region
                }
                RegionKind::ReError(_) => {
                    anonymous += 1;
                    self.new_universal(UniversalKind::AnonParam)
                }
                _ => region,
            });
        if anonymous == 0 {
            return None;
        }
        self.impl_self = Some((raw, named));
        self.impl_self
    }

    /// The universal region of the anonymous lifetime `index` of the impl the body's method
    /// is in, `impl Foo<'_>`.
    fn impl_anon_region(&mut self, index: usize) -> Option<Region<'db>> {
        if self.impl_self.is_none()
            && let GenericDefId::FunctionId(function) = self.ctx.owner.generic_def(self.ctx.db)
        {
            self.impl_self_of(function);
        }
        let (raw, named) = self.impl_self?;
        let mut anonymous = Vec::new();
        let mut errors = Vec::new();
        rustc_type_ir::fold_regions(self.interner(), raw, |region, _| {
            errors.push(matches!(region.kind(), RegionKind::ReError(_)));
            region
        });
        let mut position = 0;
        rustc_type_ir::fold_regions(self.interner(), named, |region, _| {
            if errors.get(position).copied().unwrap_or(false) {
                anonymous.push(region);
            }
            position += 1;
            region
        });
        anonymous.get(index).copied()
    }

    /// The self type and trait arguments of the impl a method is in, whose well-formedness the
    /// method assumes as it does its parameters'.
    fn impl_header_types(&mut self) -> Vec<Ty<'db>> {
        if self.ctx.closure_local.is_some() {
            return Vec::new();
        }
        let container = match self.body.owner {
            InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(it)) => {
                it.lookup(self.ctx.db).container
            }
            InferBodyId::DefWithBodyId(DefWithBodyId::ConstId(it)) => {
                it.lookup(self.ctx.db).container
            }
            _ => return Vec::new(),
        };
        let ItemContainerId::ImplId(impl_) = container else {
            return Vec::new();
        };
        let db = self.ctx.db;
        let raw = db.impl_self_ty(impl_).instantiate_identity().skip_norm_wip();
        let mut tys = vec![match self.impl_self {
            Some((from, to)) if from == raw => to,
            _ => raw,
        }];
        if let Some(trait_ref) = db.impl_trait(impl_) {
            let trait_ref = trait_ref.instantiate_identity().skip_norm_wip();
            tys.extend(trait_ref.args.iter().skip(1).filter_map(|arg| match arg.kind() {
                GenericArgKind::Type(it) => Some(it),
                _ => None,
            }));
        }
        tys.into_iter()
            .map(|ty| {
                rustc_type_ir::fold_regions(self.interner(), ty, |region, depth| {
                    self.signature_region(region, depth).unwrap_or_else(|| self.fresh())
                })
            })
            .collect()
    }

    fn implied_bounds(
        &mut self,
        ty: Ty<'db>,
        pairs: &mut Vec<(Rv, Rv)>,
        seen: &mut FxHashSet<(Ty<'db>, bool)>,
        depth: usize,
        in_field: bool,
    ) {
        if depth > 4 || !seen.insert((ty, in_field)) {
            return;
        }
        match ty.kind() {
            TyKind::Ref(region, inner, _) => {
                if let Some(shorter) = self.region_rv(region) {
                    let referent = match inner.kind() {
                        TyKind::Alias(_) => self.normalize(inner).unwrap_or(inner),
                        _ => inner,
                    };
                    let mut components = Vec::new();
                    let mut params = Vec::new();
                    outlives_components(referent, &mut components, &mut params);
                    alias_components(referent, &mut params);
                    pairs.extend(components.into_iter().map(|longer| (longer, shorter)));
                    self.regions.type_bounds.extend(params.into_iter().map(|it| (it, shorter)));
                }
                self.implied_bounds(inner, pairs, seen, depth + 1, in_field);
            }
            TyKind::Adt(def, args) => {
                for arg in args.iter() {
                    if let GenericArgKind::Type(arg) = arg.kind() {
                        self.implied_bounds(arg, pairs, seen, depth + 1, in_field);
                    }
                }
                // What the fields imply is what rustc infers of the type's parameters.
                let adt = def.def_id();
                for variant in adt_variants(self.ctx.db, adt) {
                    for (_, field) in self.ctx.db.field_types(variant).iter() {
                        let field = field.ty().instantiate(self.interner(), args).skip_norm_wip();
                        self.implied_bounds(field, pairs, seen, depth + 1, true);
                    }
                }
                // And the bounds the type declares, which its well-formedness needs.
                let clauses: Vec<_> = GenericPredicates::query_all(self.ctx.db, adt.into())
                    .iter_instantiated(self.interner(), args.as_slice())
                    .map(|it| it.skip_norm_wip())
                    .collect();
                self.implied_by_clauses(clauses, pairs);
            }
            TyKind::Tuple(tys) => {
                for it in tys.iter() {
                    self.implied_bounds(it, pairs, seen, depth + 1, in_field);
                }
            }
            TyKind::Array(inner, _) | TyKind::Slice(inner) | TyKind::RawPtr(inner, _) => {
                self.implied_bounds(inner, pairs, seen, depth + 1, in_field)
            }
            // `dyn Any + 'r` is well-formed only where `'r` outlives what the trait requires of
            // `Self`: `trait Any: 'static`. Not in a field, of which rustc's inferred outlives
            // leave out what mentions `Self`.
            TyKind::Dynamic(predicates, region) if !in_field => {
                let Some(shorter) = rv_of(region) else { return };
                for predicate in predicates.iter() {
                    let trait_ = match predicate.skip_binder() {
                        rustc_type_ir::ExistentialPredicate::Trait(it) => it.def_id.0,
                        rustc_type_ir::ExistentialPredicate::AutoTrait(it) => it.0,
                        rustc_type_ir::ExistentialPredicate::Projection(_) => continue,
                    };
                    let requires_static = GenericPredicates::query_all(self.ctx.db, trait_.into())
                        .iter_identity()
                        .any(|clause| {
                            matches!(
                                clause.skip_norm_wip().kind().skip_binder(),
                                ClauseKind::TypeOutlives(OutlivesPredicate(ty, region))
                                    if matches!(ty.kind(), TyKind::Param(param) if param.index == 0)
                                        && matches!(region.kind(), RegionKind::ReStatic)
                            )
                        });
                    if requires_static {
                        pairs.push((shorter, self.regions.static_rv));
                    }
                }
            }
            // A projection implies what its arguments' well-formedness does as well as what its
            // normalized type does: `<&'a &'b () as Trait>::Type` that `'b: 'a`.
            // And the bounds of its trait, which its trait reference's well-formedness needs:
            // `<T as Trait<A>>::Assoc` that `A: 'static` where `trait Trait<A: 'static>`.
            TyKind::Alias(alias) => {
                if let rustc_type_ir::AliasTyKind::Projection { def_id } = alias.kind {
                    for it in alias.args.types() {
                        self.implied_bounds(it, pairs, seen, depth + 1, in_field);
                    }
                    let clauses: Vec<_> =
                        GenericPredicates::query_all(self.ctx.db, def_id.0.into())
                            .iter_instantiated(self.interner(), alias.args.as_slice())
                            .map(|it| it.skip_norm_wip())
                            .filter(|it| {
                                !rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(it)
                            })
                            .collect();
                    self.implied_by_clauses(clauses, pairs);
                }
                if let Some(normalized) = self.normalize(ty)
                    && normalized != ty
                {
                    self.implied_bounds(normalized, pairs, seen, depth + 1, in_field);
                }
            }
            _ => (),
        }
    }

    fn normalize(&mut self, ty: Ty<'db>) -> Option<Ty<'db>> {
        normalize(self.ctx, ty)
    }

    /// `ty` normalized, with what the solver needs of the regions already there for it as
    /// constraints at `at`: `<T as Fun>::F<'x>` with `T: Fun<F<'a> = [u8]>` is `[u8]` only
    /// where `'x` is `'a`.
    fn normalize_at(&mut self, ty: Ty<'db>, at: At) -> Option<Ty<'db>> {
        use hir_ty::next_solver::infer::region_constraints::Constraint;
        let infcx = &self.ctx.infcx;
        let before = infcx
            .inner
            .borrow_mut()
            .unwrap_region_constraints()
            .region_constraint_data()
            .constraints
            .len();
        let first_new = infcx.num_region_vars();
        let normalized = normalize(self.ctx, ty)?;
        let constraints: Vec<_> = infcx
            .inner
            .borrow_mut()
            .unwrap_region_constraints()
            .region_constraint_data()
            .constraints[before..]
            .to_vec();
        let rv = |this: &mut Self, region: Region<'db>| match region.kind() {
            RegionKind::ReVar(vid) if vid.index() < first_new => Some(vid.as_usize()),
            RegionKind::ReStatic => Some(this.regions.static_rv),
            RegionKind::ReEarlyParam(_) | RegionKind::ReLateParam(_) => {
                this.signature_region(region, rustc_type_ir::INNERMOST).and_then(rv_of)
            }
            _ => None,
        };
        let interner = self.interner();
        // Only what a where clause or `'static` gives: the solver relates the variables it was
        // given among themselves more tightly than rustc's relating of the normalized types.
        for constraint in constraints {
            // `a <= b`: `b` outlives `a`.
            let (shorter, longer) = match constraint {
                Constraint::VarSubVar(..) => continue,
                Constraint::RegSubVar(a, b) => (a, Region::new_var(interner, b)),
                Constraint::VarSubReg(a, b) => (Region::new_var(interner, a), b),
                Constraint::RegSubReg(a, b) => (a, b),
            };
            if let (Some(shorter), Some(longer)) = (rv(self, shorter), rv(self, longer)) {
                self.outlives(longer, shorter, at);
            }
        }
        Some(normalized)
    }

    /// What outlives clauses a well-formed type needs imply.
    fn implied_by_clauses(&mut self, clauses: Vec<Clause<'db>>, pairs: &mut Vec<(Rv, Rv)>) {
        for clause in clauses {
            match clause.kind().skip_binder() {
                ClauseKind::RegionOutlives(OutlivesPredicate(longer, shorter)) => {
                    if let (Some(longer), Some(shorter)) =
                        (self.region_rv(longer), self.region_rv(shorter))
                    {
                        pairs.push((longer, shorter));
                    }
                }
                ClauseKind::TypeOutlives(OutlivesPredicate(ty, region)) => {
                    if let Some(shorter) = self.region_rv(region) {
                        let mut components = Vec::new();
                        let mut params = Vec::new();
                        outlives_components(ty, &mut components, &mut params);
                        pairs.extend(components.into_iter().map(|it| (it, shorter)));
                        self.regions
                            .type_bounds
                            .extend(params.into_iter().chain([ty]).map(|it| (it, shorter)));
                    }
                }
                _ => (),
            }
        }
    }

    fn outlives(&mut self, sup: Rv, sub: Rv, at: At) {
        if sup != sub {
            let location = self.location;
            let category = match at.span {
                MirSpan::ExprId(expr)
                    if at.category != Category::Return && self.synthesized.contains(&expr) =>
                {
                    Category::Boring
                }
                _ => at.category,
            };
            self.regions.outlives.push(Outlives {
                sup,
                sub,
                location,
                category,
                span: at.span,
                from_closure: false,
            });
        }
    }

    /// Where an `async fn` makes the future of its body (`coroutine`), that the future's
    /// output is the declared return type: rustc proves the `Future<Output = ..>` bound of the
    /// `impl Future` the function returns of it. `typed` is the output as type inference
    /// gives it, with an elided lifetime an error, `output` the same renumbered.
    fn async_fn_output(
        &mut self,
        coroutine: hir_def::hir::ExprId,
        typed: Ty<'db>,
        output: Ty<'db>,
        span: MirSpan,
    ) {
        use hir_def::hir::{ClosureKind, CoroutineSource, Expr};
        if self.ctx.closure_local.is_some()
            || !matches!(
                self.ctx.store[coroutine],
                Expr::Closure {
                    closure_kind: ClosureKind::Coroutine { source: CoroutineSource::Fn, .. },
                    ..
                }
            )
        {
            return;
        }
        let InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)) = self.body.owner
        else {
            return;
        };
        let written = FunctionSignature::of(self.ctx.db, function);
        let mut defaults = ObjectDefaults {
            db: self.ctx.db,
            interner: self.interner(),
            keep_static: writes_static_object(&written.store),
            erase: false,
        };
        let typed = rustc_type_ir::TypeFoldable::fold_with(typed, &mut defaults);
        let typed = match self.async_declared_output(function) {
            Some(declared) => self.unnamed_hidden(typed, declared),
            None => typed,
        };
        let elided = self.elided_output;
        let declared = rustc_type_ir::fold_regions(self.interner(), typed, |region, depth| {
            match region.kind() {
                RegionKind::ReError(_) => elided.unwrap_or_else(|| self.fresh()),
                RegionKind::ReVar(_) => self.fresh(),
                _ => self.signature_region(region, depth).unwrap_or_else(|| self.fresh()),
            }
        });
        let mut unsuggested = Vec::new();
        regions_of(declared, &mut unsuggested);
        for rv in unsuggested {
            self.regions.unsuggested.extend(self.regions.universal(rv));
        }
        let at = At { category: Category::Boring, span };
        self.relate(output, declared, Variance::Invariant, at);
        self.async_output_members(function, output, span);
    }

    /// The `Output` of the `impl Future` an `async fn` returns, as its signature declares it.
    fn async_declared_output(&self, function: hir_def::FunctionId) -> Option<Ty<'db>> {
        use rustc_type_ir::Interner as _;
        let interner = self.interner();
        let sig = self
            .ctx
            .db
            .callable_item_signature(CallableDefId::FunctionId(function))
            .instantiate_identity()
            .skip_norm_wip()
            .skip_binder();
        let TyKind::Alias(future) = sig.output().kind() else { return None };
        interner
            .item_bounds(future.kind.def_id())
            .iter_instantiated(interner, future.args.as_slice())
            .find_map(|clause| match clause.kind().skip_binder() {
                ClauseKind::Projection(projection) => projection.term.as_type(),
                _ => None,
            })
    }

    /// `typed` with fresh regions where `declared` has an `impl Trait`: the lifetimes of a
    /// hidden type are rustc's to choose by member constraints, not the ones type inference
    /// unified, so `I { index: self, req }` of `-> Result<impl Iterator + 'b>` with `'a: 'b`
    /// can be `I<'b>`.
    fn unnamed_hidden(&self, typed: Ty<'db>, declared: Ty<'db>) -> Ty<'db> {
        let interner = self.interner();
        let zip = |typed: GenericArgs<'db>, declared: GenericArgs<'db>| {
            GenericArgs::new_from_iter(typed.iter().zip(declared.iter()).map(|(it, other)| match (
                it.ty(),
                other.ty(),
            ) {
                (Some(it), Some(other)) => self.unnamed_hidden(it, other).into(),
                _ => it,
            }))
        };
        match (typed.kind(), declared.kind()) {
            (kind, TyKind::Alias(alias))
                if matches!(alias.kind, rustc_type_ir::AliasTyKind::Opaque { .. })
                    && !matches!(kind, TyKind::Alias(_)) =>
            {
                rustc_type_ir::fold_regions(interner, typed, |_, _| self.fresh())
            }
            (TyKind::Adt(def, args), TyKind::Adt(other, declared))
                if def == other && args.len() == declared.len() =>
            {
                Ty::new(interner, TyKind::Adt(def, zip(args, declared)))
            }
            (TyKind::Tuple(tys), TyKind::Tuple(declared)) if tys.len() == declared.len() => {
                let tys: Vec<Ty<'db>> = tys
                    .iter()
                    .zip(declared.iter())
                    .map(|(it, other)| self.unnamed_hidden(it, other))
                    .collect();
                Ty::new_tup(interner, &tys)
            }
            (TyKind::Ref(region, ty, mutability), TyKind::Ref(_, other, _)) => {
                Ty::new_ref(interner, region, self.unnamed_hidden(ty, other), mutability)
            }
            _ => typed,
        }
    }

    /// rustc's member constraints for an `impl Trait` an `async fn` declares it returns: the
    /// regions of the future's `output` are of its hidden type.
    fn async_output_members(
        &mut self,
        function: hir_def::FunctionId,
        output: Ty<'db>,
        span: MirSpan,
    ) {
        use rustc_type_ir::Interner as _;
        let interner = self.interner();
        let Some(inner) = self.async_declared_output(function) else { return };
        let TyKind::Alias(alias) = inner.kind() else { return };
        let rustc_type_ir::AliasTyKind::Opaque { .. } = alias.kind else { return };
        let Some(captured) = opaque_captures(self.ctx.db, alias.kind.def_id(), alias.args.len())
        else {
            return;
        };
        // What its bounds say it outlives, its hidden type must, as for the `impl Trait` a
        // function returns.
        let binder = rustc_type_ir::INNERMOST.shifted_in(1);
        let mut outlived = Vec::new();
        for clause in interner
            .item_bounds(alias.kind.def_id())
            .iter_instantiated(interner, alias.args.as_slice())
        {
            match clause.kind().skip_binder() {
                ClauseKind::TypeOutlives(OutlivesPredicate(bounded, region))
                    if let TyKind::Alias(bounded) = bounded.kind()
                        && bounded.kind.def_id() == alias.kind.def_id() =>
                {
                    // Under its clause's binder too.
                    match self.signature_region(region, binder.shifted_in(1)).and_then(rv_of) {
                        Some(rv) => outlived.push(rv),
                        None => return,
                    }
                }
                ClauseKind::Projection(projection)
                    if match projection.term.as_type() {
                        Some(ty) => names_captured_lifetime(self.ctx.db, ty),
                        None => rustc_type_ir::TypeVisitableExt::has_free_regions(&projection.term),
                    } =>
                {
                    return;
                }
                _ => (),
            }
        }
        let mut choices = vec![self.regions.static_rv];
        for (arg, captured) in alias.args.iter().zip(captured) {
            if let GenericArgKind::Lifetime(region) = arg.kind()
                && captured
            {
                let Some(rv) = self.signature_region(region, binder).and_then(rv_of) else {
                    return;
                };
                choices.push(rv);
            }
        }
        let mut bounded = Vec::new();
        member_regions(self.ctx.db, output, false, &mut bounded);
        bounded.retain(|&it| it != self.regions.static_rv);
        bounded.sort_unstable();
        bounded.dedup();
        for &member in &bounded {
            for &sub in &outlived {
                self.regions.outlives.push(Outlives {
                    sup: member,
                    sub,
                    location: None,
                    category: Category::Return,
                    span,
                    from_closure: false,
                });
            }
        }
        let mut members = Vec::new();
        member_regions(self.ctx.db, output, true, &mut members);
        members.retain(|&it| it != self.regions.static_rv);
        members.sort_unstable();
        members.dedup();
        self.regions.members = members;
        self.regions.choices = choices;
    }

    /// rustc's `prove_closure_bounds` where the closure `ty` is made: its enclosing item's
    /// arguments are this body's parameters, and what the closure needs of this body's
    /// regions holds, blamed on what in the closure needs it.
    fn closure_requirements(
        &mut self,
        closure: hir_def::hir::ExprId,
        ty: Ty<'db>,
        parents: Vec<(u32, Region<'db>)>,
        span: MirSpan,
    ) {
        let at = At { category: Category::Boring, span };
        for (index, region) in parents {
            let own = self.early_region(index);
            self.relate_regions(region, own, Variance::Invariant, at);
        }
        let mut regions = Vec::new();
        regions_of(ty, &mut regions);
        self.apply_requirements(closure, regions);
    }

    /// What the closure or `const { .. }` block `nested` needs of this body's regions, the
    /// regions of its type at their positions being `regions`.
    fn apply_requirements(&mut self, nested: hir_def::hir::ExprId, regions: Vec<Rv>) {
        let Some((count, requirements)) = self.requirements.get(&nested) else { return };
        if regions.len() != *count {
            return;
        }
        let at_position = |this: &mut Self, position: usize| match position {
            _ if position >= EARLY_PARENT => {
                rv_of(this.early_region((position - EARLY_PARENT) as u32))
            }
            _ if position >= LATE_PARENT => {
                rv_of(this.parent_late_region((position - LATE_PARENT) as u32))
            }
            _ if position >= IMPL_ANON => {
                this.impl_anon_region(position - IMPL_ANON).and_then(rv_of)
            }
            _ => regions.get(position).copied(),
        };
        for requirement in requirements {
            let sub = match requirement.shorter {
                Some(it) => match at_position(self, it) {
                    Some(it) => it,
                    None => continue,
                },
                None => self.regions.static_rv,
            };
            if let Some(ty) = requirement.subject {
                self.regions.type_tests.push(TypeTest {
                    ty,
                    lower: sub,
                    span: requirement.span,
                    forall: false,
                });
                continue;
            }
            let Some(sup) = at_position(self, requirement.longer) else { continue };
            if sup != sub {
                self.regions.outlives.push(Outlives {
                    sup,
                    sub,
                    location: self.location,
                    category: requirement.category,
                    span: requirement.span,
                    from_closure: true,
                });
            }
        }
    }

    fn relate_regions(&mut self, a: Region<'db>, b: Region<'db>, variance: Variance, at: At) {
        let static_rv = self.regions.static_rv;
        let rv = |region: Region<'db>| match region.kind() {
            RegionKind::ReStatic => Some(static_rv),
            _ => rv_of(region),
        };
        let (Some(a), Some(b)) = (rv(a), rv(b)) else { return };
        match variance {
            Variance::Covariant => self.outlives(a, b, at),
            Variance::Contravariant => self.outlives(b, a, at),
            Variance::Invariant => {
                self.outlives(a, b, at);
                self.outlives(b, a, at);
            }
            Variance::Bivariant => (),
        }
    }

    /// That `a` is a subtype of `b` under `variance`.
    fn relate(&mut self, a: Ty<'db>, b: Ty<'db>, variance: Variance, at: At) {
        if a == b || variance == Variance::Bivariant {
            return;
        }
        match (a.kind(), b.kind()) {
            (TyKind::Ref(ra, ta, ma), TyKind::Ref(rb, tb, _)) => {
                self.relate_regions(ra, rb, variance, at);
                let inner = match ma {
                    rustc_ast_ir::Mutability::Not => variance,
                    rustc_ast_ir::Mutability::Mut => variance.xform(Variance::Invariant),
                };
                self.relate(ta, tb, inner, at);
            }
            // rust-analyzer lowers coercing `&T` to `*const T` as a borrow, where rustc's is
            // `&raw const *x` of the raw pointer's type.
            (TyKind::Ref(_, ta, _), TyKind::RawPtr(tb, ma))
            | (TyKind::RawPtr(ta, ma), TyKind::RawPtr(tb, _)) => {
                let inner = match ma {
                    rustc_ast_ir::Mutability::Not => variance,
                    rustc_ast_ir::Mutability::Mut => variance.xform(Variance::Invariant),
                };
                self.relate(ta, tb, inner, at);
            }
            (TyKind::Adt(da, aa), TyKind::Adt(db, ab)) if da == db => {
                let variances = self.ctx.db.variances_of(GenericDefId::AdtId(da.def_id()));
                self.relate_args(aa, ab, Some(variances.as_slice()), variance, at);
            }
            (TyKind::Tuple(xs), TyKind::Tuple(ys)) => {
                for (x, y) in xs.iter().zip(ys.iter()) {
                    self.relate(x, y, variance, at);
                }
            }
            (TyKind::Array(x, _), TyKind::Array(y, _)) | (TyKind::Slice(x), TyKind::Slice(y)) => {
                self.relate(x, y, variance, at)
            }
            (TyKind::FnPtr(sa, _), TyKind::FnPtr(sb, _)) => {
                let (xs, ys) =
                    (sa.skip_binder().inputs_and_output, sb.skip_binder().inputs_and_output);
                let count = xs.len().min(ys.len());
                for (index, (x, y)) in xs.iter().zip(ys.iter()).enumerate() {
                    let inner = if index + 1 == count {
                        variance
                    } else {
                        variance.xform(Variance::Contravariant)
                    };
                    self.relate(x, y, inner, at);
                }
            }
            (TyKind::Dynamic(pa, ra), TyKind::Dynamic(pb, rb)) => {
                self.relate_regions(ra, rb, variance, at);
                let unless_auto = |it: &rustc_type_ir::Binder<_, _>| {
                    !matches!(it.skip_binder(), rustc_type_ir::ExistentialPredicate::AutoTrait(_))
                };
                for (bound, y) in pa.iter().filter(unless_auto).zip(pb.iter().filter(unless_auto)) {
                    let (x, y) = (bound.skip_binder(), y.skip_binder());
                    match (x, y) {
                        (
                            rustc_type_ir::ExistentialPredicate::Trait(x),
                            rustc_type_ir::ExistentialPredicate::Trait(y),
                        ) if x.def_id == y.def_id => {
                            self.relate_args(x.args, y.args, None, Variance::Invariant, at)
                        }
                        // An upcast, `dyn Foo<'static>` to `dyn Bar<'static, 'a>` with
                        // `trait Foo<'a>: Bar<'a, 'a>`: the supertrait as the source's
                        // arguments instantiate it is the target's. A supertrait whose
                        // arguments the source's own `for<'a>` binds is left out: it
                        // constrains no region of the body.
                        (
                            rustc_type_ir::ExistentialPredicate::Trait(x),
                            rustc_type_ir::ExistentialPredicate::Trait(y),
                        ) => {
                            let interner = self.interner();
                            let source =
                                bound.rebind(x.with_self_ty(interner, Ty::new_unit(interner)));
                            let found = rustc_type_ir::elaborate::supertraits(interner, source)
                                .find(|it| it.def_id() == y.def_id)
                                .and_then(|it| it.no_bound_vars());
                            if let Some(found) = found {
                                let args = GenericArgs::new_from_iter(
                                    found.args.iter().skip(1).collect::<Vec<_>>(),
                                );
                                self.relate_args(args, y.args, None, Variance::Invariant, at);
                            }
                        }
                        // `dyn Foo<Bar = &'a u32>`: the binding is invariant.
                        (
                            rustc_type_ir::ExistentialPredicate::Projection(x),
                            rustc_type_ir::ExistentialPredicate::Projection(y),
                        ) if x.def_id == y.def_id => {
                            self.relate_args(x.args, y.args, None, Variance::Invariant, at);
                            if let (Some(x), Some(y)) = (
                                rustc_type_ir::inherent::Term::as_type(&x.term),
                                rustc_type_ir::inherent::Term::as_type(&y.term),
                            ) {
                                self.relate(x, y, Variance::Invariant, at);
                            }
                        }
                        _ => (),
                    }
                }
            }
            (TyKind::Alias(x), TyKind::Alias(y)) if x.kind == y.kind => {
                // Two projections are what they normalize to, as rustc relates them
                // normalized: `<S as Q>::Item<'w, 's>` and `<S as Q>::Item<'w, 'static>` are
                // both `&'w S` when the impl's `Item<'w, 's>` is.
                if matches!(x.kind, rustc_type_ir::AliasTyKind::Projection { .. }) {
                    let normalized_a = self.normalize_at(a, at).filter(|&it| it != a);
                    let normalized_b = self.normalize_at(b, at).filter(|&it| it != b);
                    if normalized_a.is_some() || normalized_b.is_some() {
                        let (a, b) = (normalized_a.unwrap_or(a), normalized_b.unwrap_or(b));
                        if !matches!(a.kind(), TyKind::Infer(_))
                            && !matches!(b.kind(), TyKind::Infer(_))
                        {
                            self.relate(a, b, variance, at);
                        }
                        return;
                    }
                }
                let variances = match x.kind {
                    rustc_type_ir::AliasTyKind::Opaque { .. } => {
                        opaque_captures(self.ctx.db, x.kind.def_id(), x.args.len()).map(|it| {
                            it.into_iter()
                                .map(|captured| match captured {
                                    true => Variance::Invariant,
                                    false => Variance::Bivariant,
                                })
                                .collect::<Vec<_>>()
                        })
                    }
                    _ => None,
                };
                self.relate_args(x.args, y.args, variances.as_deref(), Variance::Invariant, at)
            }
            // A signature's associated type against what it is,
            // `<&'a mut Vec<T> as IntoIterator>::IntoIter` and `IterMut<'_, T>`.
            (TyKind::Alias(x), _) | (_, TyKind::Alias(x))
                if matches!(x.kind, rustc_type_ir::AliasTyKind::Projection { .. }) =>
            {
                let normalize = |this: &mut Self, ty: Ty<'db>| match ty.kind() {
                    TyKind::Alias(_) => this.normalize_at(ty, at).filter(|&it| it != ty),
                    _ => Some(ty),
                };
                if let Some(a) = normalize(self, a)
                    && let Some(b) = normalize(self, b)
                    && !matches!((a.kind(), b.kind()), (TyKind::Alias(_), TyKind::Alias(_)))
                {
                    self.relate(a, b, variance, at);
                }
            }
            (TyKind::Closure(_, aa), TyKind::Closure(_, ab))
            | (TyKind::Coroutine(_, aa), TyKind::Coroutine(_, ab))
            | (TyKind::CoroutineClosure(_, aa), TyKind::CoroutineClosure(_, ab))
            | (TyKind::FnDef(_, aa), TyKind::FnDef(_, ab)) => {
                self.relate_args(aa, ab, None, Variance::Invariant, at)
            }
            _ => (),
        }
    }

    fn relate_args(
        &mut self,
        a: GenericArgs<'db>,
        b: GenericArgs<'db>,
        variances: Option<&[Variance]>,
        variance: Variance,
        at: At,
    ) {
        for (index, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            let param =
                variances.and_then(|it| it.get(index).copied()).unwrap_or(Variance::Invariant);
            let inner = variance.xform(param);
            match (x.kind(), y.kind()) {
                (GenericArgKind::Lifetime(x), GenericArgKind::Lifetime(y)) => {
                    self.relate_regions(x, y, inner, at)
                }
                (GenericArgKind::Type(x), GenericArgKind::Type(y)) => self.relate(x, y, inner, at),
                _ => (),
            }
        }
    }

    /// That `ty` outlives `region`: each region in it does, and each type parameter in it
    /// must, which is a type test.
    fn type_outlives(&mut self, ty: Ty<'db>, region: Region<'db>, at: At) {
        self.type_outlives_at(ty, region, at, 0);
    }

    fn type_outlives_at(&mut self, ty: Ty<'db>, region: Region<'db>, at: At, depth: usize) {
        let Some(rv) = self.region_rv(region) else { return };
        let mut components = Vec::new();
        let mut params = Vec::new();
        outlives_components(ty, &mut components, &mut params);
        for longer in components {
            self.outlives(longer, rv, at);
        }
        for param in params {
            self.regions.type_tests.push(TypeTest {
                ty: param,
                lower: rv,
                span: at.span,
                forall: false,
            });
        }
        if depth > 4 {
            return;
        }
        let mut aliases = Vec::new();
        alias_components(ty, &mut aliases);
        for alias in aliases {
            self.alias_outlives(alias, region, at, depth);
        }
    }

    /// `<T as Trait>::Item: 'r` as rustc's `alias_ty_must_outlive`: what it normalizes to
    /// outlives `'r`; else with no bound from the where clauses or the trait, and lifetimes in
    /// its arguments, each argument does; else the trait's one bound does, or a type test
    /// that some bound or every argument does.
    fn alias_outlives(&mut self, ty: Ty<'db>, region: Region<'db>, at: At, depth: usize) {
        let TyKind::Alias(alias) = ty.kind() else { return };
        if rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&ty)
            || ty.references_non_lt_error()
        {
            return;
        }
        let opaque = matches!(alias.kind, rustc_type_ir::AliasTyKind::Opaque { .. });
        if !opaque
            && let Some(normalized) = self.normalize(ty)
            && !matches!(normalized.kind(), TyKind::Alias(_) | TyKind::Infer(_))
        {
            self.type_outlives_at(normalized, region, at, depth + 1);
            return;
        }
        let Some(rv) = self.region_rv(region) else { return };
        let interner = self.interner();
        let erase = |ty: Ty<'db>| {
            rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_static(interner))
        };
        let erased = erase(ty);
        let in_env = self.env_aliases.contains(&erased);
        let from_trait: Vec<Region<'db>> =
            rustc_type_ir::Interner::item_bounds(interner, alias.kind.def_id())
                .iter_instantiated(interner, alias.args)
                .filter_map(|it| match it.skip_norm_wip().kind().skip_binder() {
                    ClauseKind::TypeOutlives(OutlivesPredicate(bounded, bound))
                        if erase(bounded) == erased =>
                    {
                        Some(bound)
                    }
                    _ => None,
                })
                .collect();
        let mut has_regions = false;
        rustc_type_ir::fold_regions(interner, alias.args, |region, _| {
            has_regions |= !matches!(region.kind(), RegionKind::ReBound(..));
            region
        });
        if !in_env && from_trait.is_empty() && (has_regions || opaque) {
            let captured = opaque
                .then(|| opaque_captures(self.ctx.db, alias.kind.def_id(), alias.args.len()))
                .flatten();
            for (index, arg) in alias.args.iter().enumerate() {
                if captured.as_ref().is_some_and(|it| !it[index]) {
                    continue;
                }
                match arg.kind() {
                    GenericArgKind::Lifetime(it) => {
                        if let Some(longer) = self.region_rv(it) {
                            self.outlives(longer, rv, at);
                        }
                    }
                    GenericArgKind::Type(it) => self.type_outlives_at(it, region, at, depth + 1),
                    GenericArgKind::Const(_) => (),
                }
            }
            return;
        }
        if !in_env
            && let Some(&bound) = from_trait.first()
            && from_trait.iter().all(|&it| it == bound)
        {
            if let Some(bound) = self
                .signature_region(bound, rustc_type_ir::INNERMOST)
                .and_then(rv_of)
                .or_else(|| self.region_rv(bound))
            {
                self.outlives(bound, rv, at);
            }
            return;
        }
        self.regions.type_tests.push(TypeTest { ty, lower: rv, span: at.span, forall: false });
    }

    fn place_ty(&self, local: LocalId, projection: &[PlaceElem]) -> PlaceTy<'db> {
        PlaceTy::from_ty(self.regions.local_tys[local.into_raw().into_u32() as usize])
            .multi_projection_ty(&self.ctx.infcx, self.ctx.env, projection)
    }

    fn operand_ty(&self, operand: &Operand) -> Option<Ty<'db>> {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => {
                Some(self.place_ty(place.local, place.projection.as_slice()).ty)
            }
            OperandKind::Constant { ty, .. } => Some(self.renumber(ty.as_ref())),
            OperandKind::Allocation { allocation } => Some(self.renumber(allocation.as_ref().ty)),
            // A reference to the static, every lifetime of its type `'static`, elided ones
            // too; mutable for a `static mut`, rustc's `*mut T`, so that what is written
            // through it is `'static`.
            OperandKind::Static(id) => {
                let db = self.ctx.db;
                let ty = db.value_ty((*id).into())?.instantiate_identity().skip_norm_wip();
                let ty = rustc_type_ir::fold_regions(self.interner(), ty, |region, _| match region
                    .kind()
                {
                    RegionKind::ReBound(..) => region,
                    _ => self.static_region(),
                });
                let mutable = hir_def::signatures::StaticSignature::of(db, *id)
                    .flags
                    .contains(hir_def::signatures::StaticFlags::MUTABLE);
                let mutability = match mutable {
                    true => rustc_ast_ir::Mutability::Mut,
                    false => rustc_ast_ir::Mutability::Not,
                };
                Some(Ty::new_ref(self.interner(), self.static_region(), ty, mutability))
            }
        }
    }

    /// `value` with the lifetimes a user wrote, the signature's, kept and the others fresh.
    fn written<T: rustc_type_ir::TypeFoldable<hir_ty::next_solver::DbInterner<'db>>>(
        &mut self,
        value: T,
    ) -> T {
        let keep_static = writes_static_object(self.ctx.store);
        let value = value.fold_with(&mut ObjectDefaults {
            db: self.ctx.db,
            interner: self.interner(),
            keep_static,
            erase: true,
        });
        let mut vars = FxHashMap::default();
        rustc_type_ir::fold_regions(self.interner(), value, |region, depth| match region.kind() {
            RegionKind::ReEarlyParam(_) | RegionKind::ReLateParam(_) | RegionKind::ReStatic => {
                self.signature_region(region, depth).unwrap_or_else(|| self.fresh())
            }
            RegionKind::ReBound(..) => region,
            RegionKind::ReVar(_) => *vars.entry(region).or_insert_with(|| self.fresh()),
            _ => self.fresh(),
        })
    }

    /// The expression a call's callee is: its path, or the method call.
    fn callee_expr(&self, func: &Operand, call: MirSpan) -> Option<hir_def::hir::ExprId> {
        match (func.span(), call) {
            (Some(MirSpan::ExprId(expr)), _) => Some(expr),
            (_, MirSpan::ExprId(call)) => match self.ctx.store[call] {
                hir_def::hir::Expr::Call { callee, .. } => Some(callee),
                _ => Some(call),
            },
            _ => None,
        }
    }

    /// The type of a callee whose path spells out generic arguments, `f::<&'a T>`, with the
    /// lifetimes written there: rustc's user type annotation on the path.
    fn written_callee(&mut self, func: &Operand, call: MirSpan) -> Option<Ty<'db>> {
        let ty = match &func.kind {
            OperandKind::Constant { ty, .. } => ty.as_ref(),
            OperandKind::Allocation { allocation } => allocation.as_ref().ty,
            _ => return None,
        };
        let expr = self.callee_expr(func, call)?;
        match &self.ctx.store[expr] {
            hir_def::hir::Expr::Path(path) if spells_generics(path) => {
                let ty = self.written(ty);
                if let TyKind::FnDef(def, args) = ty.kind()
                    && let CallableDefId::FunctionId(function) = def.0
                {
                    let container = function.lookup(self.ctx.db).container;
                    let parent =
                        hir_ty::generics::generics(self.ctx.db, function.into()).len_parent();
                    let args =
                        GenericArgs::new_from_iter(args.iter().take(parent).collect::<Vec<_>>());
                    self.written_self(path, container, args, MirSpan::ExprId(expr));
                }
                Some(ty)
            }
            // `x.f::<&'a T>()`: the method's own arguments as written, the impl's or trait's
            // inferred.
            hir_def::hir::Expr::MethodCall { generic_args: Some(_), .. } => {
                let TyKind::FnDef(def, args) = ty.kind() else { return None };
                let CallableDefId::FunctionId(function) = def.0 else { return None };
                let parent = hir_ty::generics::generics(self.ctx.db, function.into()).len_parent();
                let args = args.iter().enumerate().map(|(index, arg)| match index < parent {
                    true => self.renumber(arg),
                    false => self.written(arg),
                });
                let args = GenericArgs::new_from_iter(args.collect::<Vec<_>>());
                Some(Ty::new_fn_def(self.interner(), def, args))
            }
            _ => None,
        }
    }

    fn category_of_assignment(&self, local: LocalId, projection: &[PlaceElem]) -> Category {
        if !projection.is_empty() {
            return Category::Assignment;
        }
        if local == super::return_slot() {
            Category::Return
        } else if self.ctx.bindings.contains_key(&local) {
            Category::Assignment
        } else {
            Category::Boring
        }
    }

    /// rustc's `CopyBound`: a copied value's type is `Copy`, which `impl Copy for Foo<'static>`
    /// proves only for `'static`. References, pointers and scalars are `Copy` for any lifetime.
    fn copy_bound(&mut self, operand: &Operand, span: MirSpan) {
        let OperandKind::Copy(place) = &operand.kind else { return };
        let ty = self.place_ty(place.local, place.projection.as_slice()).ty;
        if !matches!(ty.kind(), TyKind::Adt(..) | TyKind::Tuple(..) | TyKind::Array(..)) {
            return;
        }
        let Some(copy) = self.interner().lang_items().Copy else { return };
        let trait_ref = rustc_type_ir::TraitRef::new(self.interner(), copy.into(), [ty]);
        let span = operand.span().unwrap_or(span);
        self.prove(trait_ref, At { category: Category::Predicate, span });
    }

    /// rustc's `check_local`: a variable's or temporary's type is `Sized`, which a where clause
    /// on an associated type, `<&'static T as A>::X: Sized`, proves for those lifetimes only.
    fn sized_local(&mut self, local: LocalId, ty: Ty<'db>, span: MirSpan) {
        if local == super::return_slot()
            || self.body.param_locals.contains(&local)
            || !matches!(ty.kind(), TyKind::Alias(alias) if matches!(alias.kind, rustc_type_ir::AliasTyKind::Projection { .. }))
        {
            return;
        }
        let Some(sized) = self.interner().lang_items().Sized else { return };
        let trait_ref = rustc_type_ir::TraitRef::new(self.interner(), sized.into(), [ty]);
        self.prove(trait_ref, At { category: Category::Predicate, span });
    }

    fn statement(&mut self, kind: &StatementKind, span: MirSpan) {
        kind.for_each_operand(&mut |operand| self.copy_bound(operand, span));
        let StatementKind::Assign(place, rvalue) = kind else { return };
        let projection = place.projection.as_slice();
        let dest = self.place_ty(place.local, projection).ty;
        let mut at = At { category: self.category_of_assignment(place.local, projection), span };
        // A field that is a place is the operand itself in rustc, with no temporary.
        if at.category == Category::Boring
            && projection.is_empty()
            && self.aggregate_temps.contains(&place.local)
            && !matches!(
                rvalue,
                Rvalue::Use(Operand { kind: OperandKind::Copy(_) | OperandKind::Move(_), .. })
            )
        {
            at.category = Category::Usage;
        }
        if projection.is_empty() {
            self.sized_local(place.local, dest, span);
            if place.local == super::return_slot() && !self.opaque_bounds.is_empty() {
                let bounds = std::mem::take(&mut self.opaque_bounds);
                self.clauses(bounds, At { category: Category::Predicate, span }, true);
            }
        }
        let first = self.regions.outlives.len();
        self.assign(place, rvalue, dest, at, span);
        // rustc relates a variable with a type written out to its annotation, so what flows
        // into a lifetime the annotation names, `let p: &'static T = ..`, is blamed on that.
        if projection.is_empty() && self.annotated.contains_key(&place.local) {
            for index in first..self.regions.outlives.len() {
                let constraint = &self.regions.outlives[index];
                if constraint.category == Category::Assignment
                    && self.regions.universal(constraint.sub).is_some()
                {
                    self.regions.outlives[index].category = Category::TypeAnnotation;
                }
            }
        }
    }

    fn assign(
        &mut self,
        place: &hir_ty::mir::StoredPlace,
        rvalue: &Rvalue,
        dest: Ty<'db>,
        at: At,
        span: MirSpan,
    ) {
        let projection = place.projection.as_slice();
        if let MirSpan::ExprId(expr) = span
            && let hir_def::hir::Expr::Cast { .. } = self.ctx.store[expr]
            && matches!(rvalue, Rvalue::Use(_) | Rvalue::Cast(..))
            && let Some(written) = self.ctx.infer.type_of_expr.get(expr)
        {
            let written = self.written(written.as_ref());
            let at = At { category: Category::TypeAnnotation, span };
            self.relate(dest, written, Variance::Invariant, at);
        }
        // A range pattern's bounds, `A::<'a>::X..=Y`, are named constants compared.
        if let Rvalue::BinaryOp(_, left, right) | Rvalue::CheckedBinaryOp(_, left, right) = rvalue {
            self.constant_ty(left, span);
            self.constant_ty(right, span);
        }
        match rvalue {
            // Raw and function pointers compare when they have a common supertype.
            Rvalue::BinaryOp(op, left, right) | Rvalue::CheckedBinaryOp(op, left, right)
                if matches!(
                    op,
                    BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
                ) =>
            {
                if let (Some(left), Some(right)) = (self.operand_ty(left), self.operand_ty(right))
                    && matches!(left.kind(), TyKind::RawPtr(..) | TyKind::FnPtr(..))
                {
                    let common = self.renumber(left);
                    let at = At { category: Category::CallArgument, span };
                    self.relate(left, common, Variance::Covariant, at);
                    self.relate(right, common, Variance::Covariant, at);
                }
            }
            Rvalue::Repeat(operand, len) => {
                if let (Some(ty), TyKind::Array(element, _)) =
                    (self.operand_ty(operand), dest.kind())
                {
                    self.relate(ty, element, Variance::Covariant, at);
                    // More than one copy of what is not a constant needs it `Copy`, which
                    // `impl Copy for Foo<'static>` proves only for `'static`.
                    let copies = hir_ty::consteval::try_const_usize(self.ctx.db, len.as_ref());
                    if !matches!(
                        operand.kind,
                        OperandKind::Constant { .. } | OperandKind::Allocation { .. }
                    ) && copies.is_none_or(|it| it > 1)
                        && let Some(copy) = self.interner().lang_items().Copy
                    {
                        let trait_ref =
                            rustc_type_ir::TraitRef::new(self.interner(), copy.into(), [ty]);
                        self.prove(trait_ref, At { category: Category::Predicate, span });
                    }
                }
            }
            Rvalue::Aggregate(AggregateKind::Array(_), operands) => {
                if let TyKind::Array(element, _) = dest.kind() {
                    for operand in operands.iter() {
                        if let Some(ty) = self.operand_ty(operand) {
                            let at = At { span: operand.span().unwrap_or(span), ..at };
                            self.relate(ty, element, Variance::Covariant, at);
                        }
                    }
                }
            }
            Rvalue::Aggregate(AggregateKind::Union(_, field), operands) => {
                if let (TyKind::Adt(_, args), Some(operand)) = (dest.kind(), operands.first())
                    && let Some(ty) = self.operand_ty(operand)
                {
                    let field_ty = self.ctx.db.field_types(field.parent)[field.local_id]
                        .ty()
                        .instantiate(self.interner(), args)
                        .skip_norm_wip();
                    self.relate(ty, field_ty, Variance::Covariant, at);
                }
            }
            _ => {
                let constant = match rvalue {
                    Rvalue::Use(operand) => {
                        self.constant_ty(operand, span).or_else(|| self.fn_item(operand, span))
                    }
                    _ => None,
                };
                if let Some(ty) = constant.or_else(|| self.rvalue_ty(rvalue, span)) {
                    self.relate(ty, dest, Variance::Covariant, at);
                }
            }
        }
    }

    /// The type of a named constant as rustc's type check gives it: its generic arguments
    /// renumbered, or kept as written where its path spells them out (rustc's user type
    /// annotation), with its where clauses proven; for a constant of an inherent impl named
    /// through a written self type, `<Foo<'a>>::C`, that type equal to the impl's.
    fn constant_ty(&mut self, operand: &Operand, span: MirSpan) -> Option<Ty<'db>> {
        let OperandKind::Constant { konst, .. } = &operand.kind else { return None };
        let ConstKind::Unevaluated(unevaluated) = konst.as_ref().kind() else { return None };
        let id = match unevaluated.def.0 {
            GeneralConstId::ConstId(id) => id,
            GeneralConstId::AnonConstId(konst) => return self.inline_const_ty(konst, operand),
            _ => return None,
        };
        let span = operand.span().unwrap_or(span);
        let path = match span {
            MirSpan::ExprId(expr) => match &self.ctx.store[expr] {
                hir_def::hir::Expr::Path(path) => Some(path),
                _ => None,
            },
            MirSpan::PatId(pat) => match &self.ctx.store[pat] {
                Pat::Path(path) => Some(path),
                _ => None,
            },
            _ => None,
        };
        let args = match path {
            Some(path) if spells_generics(path) => self.written(unevaluated.args),
            _ => self.renumber(unevaluated.args),
        };
        let db = self.ctx.db;
        let ty = self
            .declared(db.value_ty(id.into())?.instantiate(self.interner(), args).skip_norm_wip());
        self.predicates(id.into(), args, At { category: Category::Predicate, span }, true);
        if let MirSpan::ExprId(expr) = span {
            self.path_well_formed(expr, id.lookup(db).container, args, span);
        }
        if let Some(path) = path {
            self.written_self(path, id.lookup(db).container, args, span);
        }
        Some(ty)
    }

    /// The type of a `const { .. }` block where it is evaluated, its function's lifetimes this
    /// body's, with what the block needs of the regions of its type, as rustc relates an
    /// inline const's arguments.
    fn inline_const_ty(
        &mut self,
        konst: hir_ty::db::AnonConstId<'db>,
        operand: &Operand,
    ) -> Option<Ty<'db>> {
        let db = self.ctx.db;
        if !konst.is_inline_const(db) {
            return None;
        }
        let OperandKind::Constant { ty, .. } = &operand.kind else { return None };
        let mut positions = Vec::new();
        let ty =
            rustc_type_ir::fold_regions(self.interner(), ty.as_ref(), |region, depth| match region
                .kind()
            {
                RegionKind::ReBound(..) => region,
                RegionKind::ReEarlyParam(_) | RegionKind::ReLateParam(_) | RegionKind::ReStatic => {
                    self.signature_region(region, depth).unwrap_or_else(|| self.fresh())
                }
                _ => {
                    let region = self.fresh();
                    positions.extend(rv_of(region));
                    region
                }
            });
        self.apply_requirements(konst.loc(db).expr, positions);
        Some(ty)
    }

    /// For an associated item named through a written self type, `<Foo<'a>>::C` or
    /// `Foo::<'a>::new`, that type equal to the impl's or the trait's self type.
    fn written_self(
        &mut self,
        path: &hir_def::expr_store::path::Path,
        container: ItemContainerId,
        args: GenericArgs<'db>,
        span: MirSpan,
    ) {
        let written = match path.type_anchor() {
            Some(anchor) => Some(self.lower_written(anchor, span)),
            None => self.lower_written_qualifier(path, span),
        };
        let self_ty = match container {
            ItemContainerId::ImplId(impl_) => Some(self.declared(
                self.ctx.db.impl_self_ty(impl_).instantiate(self.interner(), args).skip_norm_wip(),
            )),
            ItemContainerId::TraitId(_) => args.types().next(),
            _ => None,
        };
        if let Some(written) = written
            && let Some(self_ty) = self_ty
        {
            let written = self.written(written);
            let at = At { category: Category::TypeAnnotation, span };
            self.relate(self_ty, written, Variance::Invariant, at);
        }
    }

    /// A function item used as a value, with its where clauses proven, as rustc's type check
    /// proves those of every function item it names (`let f: fn(&'a str) -> _ = extend;` or
    /// passing `extend` to a call); where its path spells out generic arguments,
    /// `let f = <&'static u32 as Trait>::method;`, with the lifetimes written there, as
    /// rustc's user type annotation on the path.
    fn fn_item(&mut self, operand: &Operand, span: MirSpan) -> Option<Ty<'db>> {
        let ty = match self.written_callee(operand, span) {
            Some(ty) => ty,
            None => match &operand.kind {
                OperandKind::Constant { .. } | OperandKind::Allocation { .. } => {
                    self.operand_ty(operand).filter(|it| matches!(it.kind(), TyKind::FnDef(..)))?
                }
                _ => return None,
            },
        };
        if let TyKind::FnDef(def, args) = ty.kind() {
            let span = operand.span().unwrap_or(span);
            self.callee_predicates(def.0, args, span);
            for ty in args.types() {
                self.well_formed(ty, At { category: Category::Boring, span }, 0);
            }
        }
        Some(ty)
    }

    /// A declared type instantiated with region variables: its `'static` the static region,
    /// any other region it names a fresh variable.
    fn declared(&self, ty: Ty<'db>) -> Ty<'db> {
        rustc_type_ir::fold_regions(self.interner(), ty, |region, _| match region.kind() {
            RegionKind::ReVar(_) | RegionKind::ReBound(..) => region,
            RegionKind::ReStatic => self.static_region(),
            _ => self.fresh(),
        })
    }

    /// The arguments of the self type of the impl this body is in, with the signature's
    /// lifetimes, where it is `adt`.
    fn impl_self_args(&mut self, adt: AdtId) -> Option<GenericArgs<'db>> {
        let db = self.ctx.db;
        let GenericDefId::FunctionId(function) = self.ctx.owner.generic_def(db) else {
            return None;
        };
        let ItemContainerId::ImplId(impl_) = function.lookup(db).container else { return None };
        let ty = db.impl_self_ty(impl_).instantiate_identity().skip_norm_wip();
        let TyKind::Adt(def, args) = ty.kind() else { return None };
        if def.def_id() != adt {
            return None;
        }
        // The impl's anonymous lifetimes (`impl MyStruct<'_>`) are universal regions.
        if self.nested() && self.impl_self.is_none() {
            self.impl_self_of(function);
        }
        if let Some((raw, named)) = self.impl_self
            && raw == ty
            && let TyKind::Adt(_, named) = named.kind()
        {
            return Some(rustc_type_ir::fold_regions(self.interner(), named, |region, depth| {
                match region.kind() {
                    RegionKind::ReVar(_) | RegionKind::ReBound(..) => region,
                    _ => self.signature_region(region, depth).unwrap_or_else(|| self.fresh()),
                }
            }));
        }
        Some(self.written(args))
    }

    /// A type written in the body, lowered as inference lowers it, its lifetimes the
    /// signature's where it names them.
    fn lower_written(&self, type_ref: hir_def::type_ref::TypeRefId, span: MirSpan) -> Ty<'db> {
        self.lowering(span, |ctx| ctx.lower_ty(type_ref))
    }

    /// A `let`'s written type with a region variable for each lifetime it leaves out, so an
    /// alias that uses its lifetime twice (`type P<'a> = (&'a u32, &'a u32)`) shares it, as
    /// rustc's user type annotation does. Its `_`s are type variables.
    fn lower_annotation(&self, type_ref: hir_def::type_ref::TypeRefId, span: MirSpan) -> Ty<'db> {
        self.lowering_with(span, true, |ctx| ctx.lower_ty(type_ref))
    }

    /// The type of a `let` with a type written out, with the lifetimes it names: its written
    /// type where that is the type inferred for it, else the inferred type.
    fn annotation_ty(&mut self, annotation: Annotation) -> Option<Ty<'db>> {
        let inferred = self.ctx.infer.type_of_pat.get(annotation.pat)?.as_ref();
        let lowered = self.lower_annotation(annotation.type_ref, MirSpan::ExprId(annotation.block));
        let ty = self.zip_annotation(lowered, inferred, &mut FxHashMap::default(), 0);
        Some(self.written(ty.unwrap_or(inferred)))
    }

    /// The written type `lowered` with each `_` in it the type inferred there, the same
    /// regions wherever the same `_` stands; `None` where the two differ otherwise.
    fn zip_annotation(
        &self,
        lowered: Ty<'db>,
        inferred: Ty<'db>,
        holes: &mut FxHashMap<Ty<'db>, Ty<'db>>,
        depth: usize,
    ) -> Option<Ty<'db>> {
        let interner = self.interner();
        if depth > 16 {
            return None;
        }
        let mut zip = |a: Ty<'db>, b: Ty<'db>| self.zip_annotation(a, b, holes, depth + 1);
        Some(match (lowered.kind(), inferred.kind()) {
            (TyKind::Infer(rustc_type_ir::InferTy::TyVar(_)), _) => {
                *holes.entry(lowered).or_insert_with(|| self.renumber(inferred))
            }
            (TyKind::Ref(region, a, m), TyKind::Ref(_, b, n)) if m == n => {
                Ty::new_ref(interner, region, zip(a, b)?, m)
            }
            (TyKind::RawPtr(a, m), TyKind::RawPtr(b, n)) if m == n => {
                Ty::new(interner, TyKind::RawPtr(zip(a, b)?, m))
            }
            (TyKind::Slice(a), TyKind::Slice(b)) => Ty::new(interner, TyKind::Slice(zip(a, b)?)),
            (TyKind::Array(a, len), TyKind::Array(b, inferred_len)) => {
                let len =
                    if matches!(len.kind(), ConstKind::Infer(_)) { inferred_len } else { len };
                Ty::new(interner, TyKind::Array(zip(a, b)?, len))
            }
            (TyKind::Tuple(a), TyKind::Tuple(b)) if a.len() == b.len() => {
                let tys: Option<Vec<_>> = a.iter().zip(b.iter()).map(|(a, b)| zip(a, b)).collect();
                Ty::new_tup(interner, &tys?)
            }
            (TyKind::Adt(def, a), TyKind::Adt(inferred_def, b))
                if def == inferred_def && a.len() == b.len() =>
            {
                let mut args = Vec::with_capacity(a.len());
                for (a, b) in a.iter().zip(b.iter()) {
                    args.push(match (a.kind(), b.kind()) {
                        (GenericArgKind::Type(a), GenericArgKind::Type(b)) => zip(a, b)?.into(),
                        (GenericArgKind::Const(a), GenericArgKind::Const(_))
                            if matches!(a.kind(), ConstKind::Infer(_)) =>
                        {
                            b
                        }
                        _ => a,
                    });
                }
                Ty::new(interner, TyKind::Adt(def, GenericArgs::new_from_iter(args)))
            }
            _ => {
                let erase = |ty: Ty<'db>| {
                    rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_static(interner))
                };
                if erase(lowered) != erase(inferred) {
                    return None;
                }
                lowered
            }
        })
    }

    /// The type a value path's qualifier spells out with generic arguments, `A<'a>` of
    /// `A::<'a>::C`, lowered as `lower_written` lowers.
    fn lower_written_qualifier(
        &self,
        path: &hir_def::expr_store::path::Path,
        span: MirSpan,
    ) -> Option<Ty<'db>> {
        let segments = path.segments();
        let qualifier = segments.get(segments.len().checked_sub(2)?)?;
        qualifier.args_and_bindings?;
        self.lowering(span, |ctx| ctx.lower_value_path_qualifier(path))
    }

    fn lowering<T>(
        &self,
        span: MirSpan,
        f: impl FnOnce(&mut hir_ty::TyLoweringContext<'db, '_>) -> T,
    ) -> T {
        self.lowering_with(span, false, f)
    }

    fn lowering_with<T>(
        &self,
        span: MirSpan,
        vars: bool,
        f: impl FnOnce(&mut hir_ty::TyLoweringContext<'db, '_>) -> T,
    ) -> T {
        let db = self.ctx.db;
        let store_owner = self.ctx.owner.expression_store_owner(db);
        let mut resolver = store_owner.resolver(db);
        let _guard = match span {
            MirSpan::ExprId(expr) => Some(resolver.update_to_inner_scope(db, store_owner, expr)),
            _ => None,
        };
        let generics = std::cell::OnceCell::new();
        let mut infer_vars = InferVars(&self.ctx.infcx);
        let mut ctx = hir_ty::TyLoweringContext::new(
            db,
            &resolver,
            hir_def::expr_store::ExpressionStore::of(db, store_owner),
            store_owner,
            self.ctx.owner.generic_def(db),
            &generics,
            hir_ty::LifetimeElisionKind::Infer,
            hir_ty::LifetimeLoweringMode::LateParam,
        )
        .with_infer_vars_behavior(vars.then_some(&mut infer_vars as _));
        if vars {
            ctx = ctx.with_elided_region_vars();
        }
        f(&mut ctx)
    }

    fn rvalue_ty(&mut self, rvalue: &Rvalue, span: MirSpan) -> Option<Ty<'db>> {
        let boring = At { category: Category::Boring, span };
        match rvalue {
            Rvalue::Use(operand) => self.operand_ty(operand),
            Rvalue::CopyForDeref(place) => {
                Some(self.place_ty(place.local, place.projection.as_slice()).ty)
            }
            Rvalue::Ref(kind, place) => {
                let region = self.fresh();
                let rv = rv_of(region).unwrap();
                if let Some(location) = self.location {
                    self.regions.loan_regions.insert(location, rv);
                }
                let projection = place.projection.as_slice();
                self.reborrow_constraints(place.local, projection, rv, boring);
                let ty = self.place_ty(place.local, projection).ty;
                let mutability = match kind {
                    BorrowKind::Mut { .. } => rustc_ast_ir::Mutability::Mut,
                    _ => rustc_ast_ir::Mutability::Not,
                };
                Some(Ty::new_ref(self.interner(), region, ty, mutability))
            }
            Rvalue::Cast(kind, operand, target) => {
                let target = self.renumber(target.as_ref());
                let source = match kind {
                    CastKind::PointerCoercion(hir_ty::PointerCast::ReifyFnPointer) => {
                        self.fn_item(operand, span).or_else(|| self.operand_ty(operand))?
                    }
                    _ => self.operand_ty(operand)?,
                };
                let at = At { category: Category::Cast, span };
                match kind {
                    // rustc's casts between function types: what is cast is a subtype of the
                    // function pointer it becomes, by signature.
                    CastKind::PointerCoercion(hir_ty::PointerCast::UnsafeFnPointer) => {
                        self.relate(source, target, Variance::Covariant, at)
                    }
                    CastKind::PointerCoercion(hir_ty::PointerCast::ReifyFnPointer) => {
                        if let TyKind::FnDef(def, args) = source.kind() {
                            let sig = self
                                .ctx
                                .db
                                .callable_item_signature(def.0)
                                .instantiate(self.interner(), args)
                                .skip_norm_wip();
                            // rustc instantiates the item's late-bound lifetimes and proves
                            // its signature well-formed with them: `&'a &'b ()` that `'b: 'a`.
                            let sig = self.ctx.infcx.instantiate_binder_with_fresh_vars(
                                hir_ty::Span::Dummy,
                                hir_ty::next_solver::infer::BoundRegionConversionTime::FnCall,
                                sig,
                            );
                            let sig = rustc_type_ir::fold_regions(self.interner(), sig, |r, _| {
                                match r.kind() {
                                    RegionKind::ReError(_) => self.fresh(),
                                    _ => r,
                                }
                            });
                            let wf = At { category: Category::Boring, span };
                            for ty in sig.inputs_and_output.iter() {
                                self.well_formed(ty, wf, 0);
                            }
                            let source =
                                Ty::new_fn_ptr(self.interner(), rustc_type_ir::Binder::dummy(sig));
                            self.relate(source, target, Variance::Covariant, at);
                        }
                    }
                    CastKind::PointerCoercion(hir_ty::PointerCast::ClosureFnPointer(_)) => {
                        if let TyKind::Closure(_, args) = source.kind()
                            && let TyKind::FnPtr(to, _) = target.kind()
                            && let sig = args.as_closure().sig().skip_binder()
                            && let &[tupled] = sig.inputs().as_slice()
                            && let TyKind::Tuple(inputs) = tupled.kind()
                            && let Some((&output, to_inputs)) =
                                to.skip_binder().inputs_and_output.as_slice().split_last()
                        {
                            for (input, &to) in inputs.iter().zip(to_inputs) {
                                self.relate(input, to, Variance::Contravariant, at);
                            }
                            self.relate(sig.output(), output, Variance::Covariant, at);
                        }
                    }
                    CastKind::PointerCoercion(hir_ty::PointerCast::Unsize) => {
                        self.unsize(source, target, at)
                    }
                    // rust-analyzer's kind for any reference cast to a raw pointer: rustc's
                    // `&raw const *x` of the pointer's type, then for an array (`&[T; N] as
                    // *const T`) its element a subtype of what the pointer points to.
                    CastKind::PointerCoercion(hir_ty::PointerCast::ArrayToPointer) => {
                        if let TyKind::Ref(_, from, _) | TyKind::RawPtr(from, _) = source.kind()
                            && let TyKind::RawPtr(to, mutability) = target.kind()
                        {
                            let interner = self.interner();
                            let erase = |ty| {
                                rustc_type_ir::fold_regions(interner, ty, |_, _| {
                                    Region::new_erased(interner)
                                })
                            };
                            match from.kind() {
                                TyKind::Array(element, _) if erase(element) == erase(to) => {
                                    self.relate(element, to, Variance::Covariant, at)
                                }
                                _ => {
                                    let variance = match mutability {
                                        rustc_ast_ir::Mutability::Not => Variance::Covariant,
                                        rustc_ast_ir::Mutability::Mut => Variance::Invariant,
                                    };
                                    self.relate(from, to, variance, at)
                                }
                            }
                        }
                    }
                    // `*const [T; N]` to `*const T`, rustc's `ArrayToPointer`; and between
                    // pointers to trait objects, the object's lifetime may only shorten and the
                    // principal trait's arguments stay, else a vtable method could be called
                    // with what it cannot take (rust-lang/rust#136702).
                    CastKind::PtrToPtr => {
                        if let (TyKind::RawPtr(from, _), TyKind::RawPtr(to, _)) =
                            (source.kind(), target.kind())
                        {
                            let (from_tail, to_tail) = (self.tail(from), self.tail(to));
                            let interner = self.interner();
                            let erase = |ty| {
                                rustc_type_ir::fold_regions(interner, ty, |_, _| {
                                    Region::new_erased(interner)
                                })
                            };
                            if let TyKind::Array(element, _) = from.kind()
                                && erase(element) == erase(to)
                            {
                                self.relate(element, to, Variance::Covariant, at)
                            } else if let (
                                TyKind::Dynamic(from_predicates, from_region),
                                TyKind::Dynamic(to_predicates, to_region),
                            ) = (from_tail.kind(), to_tail.kind())
                            {
                                use rustc_type_ir::inherent::BoundExistentialPredicates as _;
                                match (from_predicates.principal(), to_predicates.principal()) {
                                    (Some(_), Some(_)) => {
                                        self.relate(from_tail, to_tail, Variance::Covariant, at)
                                    }
                                    (None, None) => self.relate_regions(
                                        from_region,
                                        to_region,
                                        Variance::Covariant,
                                        at,
                                    ),
                                    _ => (),
                                }
                            }
                        }
                    }
                    CastKind::PointerCoercion(hir_ty::PointerCast::MutToConstPointer) => {
                        if let (TyKind::RawPtr(x, _), TyKind::RawPtr(y, _)) =
                            (source.kind(), target.kind())
                        {
                            self.relate(x, y, Variance::Covariant, at);
                        }
                    }
                    _ => (),
                }
                Some(target)
            }
            Rvalue::Aggregate(AggregateKind::Adt(variant, args), operands) => {
                let store = self.ctx.store;
                let spelled = |expr| match &store[expr] {
                    hir_def::hir::Expr::RecordLit { path, .. } | hir_def::hir::Expr::Path(path) => {
                        spells_generics(path)
                    }
                    _ => false,
                };
                // A constructor called through a variable, `let f = Foo::<&'a u32>; f(x)`, has
                // the variable's arguments.
                let through_local = match span {
                    MirSpan::ExprId(expr) => match store[expr] {
                        hir_def::hir::Expr::Call { callee, .. } => {
                            self.callee_local(callee).and_then(|local| {
                                let ty =
                                    self.regions.local_tys[local.into_raw().into_u32() as usize];
                                let ty = match ty.kind() {
                                    TyKind::Ref(_, inner, _) => inner,
                                    _ => ty,
                                };
                                match ty.kind() {
                                    TyKind::FnDef(def, args)
                                        if !matches!(def.0, CallableDefId::FunctionId(_)) =>
                                    {
                                        if let Some(location) = self.location {
                                            self.regions.callee_reads.push((local, location));
                                        }
                                        Some(args)
                                    }
                                    _ => None,
                                }
                            })
                        }
                        _ => None,
                    },
                    _ => None,
                };
                let db = self.ctx.db;
                let adt_of = |variant: hir_def::VariantId| match variant {
                    hir_def::VariantId::EnumVariantId(it) => AdtId::EnumId(it.lookup(db).parent),
                    hir_def::VariantId::StructId(it) => AdtId::StructId(it),
                    hir_def::VariantId::UnionId(it) => AdtId::UnionId(it),
                };
                let names_self = |expr| match &store[expr] {
                    hir_def::hir::Expr::RecordLit { path, .. } | hir_def::hir::Expr::Path(path) => {
                        path.is_self_type()
                    }
                    _ => false,
                };
                // `Self { .. }` and `Self(..)` in an impl are the impl's self type, with its
                // lifetimes: in `impl MyStruct<'_>`, not a `MyStruct<'a>`.
                let self_args = match span {
                    MirSpan::ExprId(expr)
                        if names_self(expr)
                            || matches!(store[expr],
                                hir_def::hir::Expr::Call { callee, .. } if names_self(callee)) =>
                    {
                        self.impl_self_args(adt_of(*variant))
                    }
                    _ => None,
                };
                let mut annotated = false;
                let args = match span {
                    _ if let Some(args) = self_args => args,
                    _ if let Some(args) = through_local => args,
                    MirSpan::ExprId(expr)
                        if spelled(expr)
                            || matches!(store[expr],
                                hir_def::hir::Expr::Call { callee, .. } if spelled(callee)) =>
                    {
                        annotated = true;
                        self.written(args.as_ref())
                    }
                    _ => self.renumber(args.as_ref()),
                };
                // The arguments a path spells out are well-formed, as rustc's user type
                // annotation: `Outer::<T, Inner<T>> { .. }` where `struct Inner<T: 'static>`.
                if annotated {
                    for ty in args.types() {
                        self.well_formed(ty, boring, 0);
                    }
                }
                let fields = self.ctx.db.field_types(*variant);
                for ((_, field), operand) in fields.iter().zip(operands.iter()) {
                    let field = field.ty().instantiate(self.interner(), args).skip_norm_wip();
                    if let Some(ty) = self.operand_ty(operand) {
                        let at = At { span: operand.span().unwrap_or(span), ..boring };
                        self.relate(ty, field, Variance::Covariant, at);
                    }
                }
                let adt = adt_of(*variant);
                self.predicates(adt.into(), args, boring, true);
                Some(Ty::new_adt(self.interner(), adt, args))
            }
            Rvalue::Aggregate(AggregateKind::Tuple(ty), operands) => {
                let ty = self.renumber(ty.as_ref());
                if let TyKind::Tuple(fields) = ty.kind() {
                    for (field, operand) in fields.iter().zip(operands.iter()) {
                        if let Some(it) = self.operand_ty(operand) {
                            self.relate(it, field, Variance::Covariant, boring);
                        }
                    }
                }
                Some(ty)
            }
            Rvalue::Aggregate(AggregateKind::Closure(ty), operands) => {
                let written = ty.as_ref();
                let ty = self.renumber(written);
                let lifetimes = |parents: &[hir_ty::next_solver::GenericArg<'db>]| {
                    parents
                        .iter()
                        .enumerate()
                        .filter_map(|(index, arg)| match arg.kind() {
                            GenericArgKind::Lifetime(region) => Some((index as u32, region)),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                };
                let made = match ty.kind() {
                    TyKind::Closure(def, args) => Some((
                        def.0.loc(self.ctx.db).expr,
                        args.as_closure().tupled_upvars_ty(),
                        lifetimes(args.as_closure().parent_args().as_slice()),
                    )),
                    TyKind::Coroutine(def, args) => {
                        let expr = def.0.loc(self.ctx.db).expr;
                        if let TyKind::Coroutine(_, typed) = written.kind() {
                            self.async_fn_output(
                                expr,
                                typed.as_coroutine().return_ty(),
                                args.as_coroutine().return_ty(),
                                span,
                            );
                        }
                        Some((
                            expr,
                            args.as_coroutine().tupled_upvars_ty(),
                            lifetimes(args.as_coroutine().parent_args().as_slice()),
                        ))
                    }
                    _ => None,
                };
                if let Some((expr, upvars, parents)) = made {
                    if let TyKind::Tuple(upvars) = upvars.kind() {
                        for (upvar, operand) in upvars.iter().zip(operands.iter()) {
                            if let Some(it) = self.operand_ty(operand) {
                                let at = At {
                                    category: Category::Boring,
                                    span: operand.span().unwrap_or(span),
                                };
                                self.relate(it, upvar, Variance::Covariant, at);
                            }
                        }
                    }
                    self.closure_requirements(expr, ty, parents, span);
                }
                Some(ty)
            }
            _ => None,
        }
    }

    /// rustc's `add_reborrow_constraint`: the loan of `*p` outlives nothing longer than the
    /// reference `p` is, up to the first shared reference.
    fn reborrow_constraints(&mut self, local: LocalId, projection: &[PlaceElem], loan: Rv, at: At) {
        for index in (0..projection.len()).rev() {
            if projection[index] != ProjectionElem::Deref {
                continue;
            }
            let base = self.place_ty(local, &projection[..index]).ty;
            match base.kind() {
                TyKind::Ref(region, _, mutability) => {
                    if let Some(region) = rv_of(region) {
                        self.outlives(region, loan, at);
                    }
                    if mutability == rustc_ast_ir::Mutability::Not {
                        return;
                    }
                }
                TyKind::RawPtr(..) => return,
                _ => (),
            }
        }
        // A capture by reference is rustc's `*(*env).i`, reborrowed through the closure's
        // reference to itself too: `|| &mut x` returns what lives only as long as the call.
        // A coroutine's body has itself by value, as rustc checks it.
        if projection.first() == Some(&ProjectionElem::Deref)
            && self.ctx.upvars.iter().any(|it| it.local == local && it.by_ref)
            && let Some(env) = self.ctx.closure_local
            && let TyKind::Ref(region, env, _) =
                self.regions.local_tys[env.into_raw().into_u32() as usize].kind()
            && !matches!(env.kind(), TyKind::Coroutine(..))
            && let Some(region) = rv_of(region)
        {
            self.outlives(region, loan, at);
        }
    }

    fn unsize(&mut self, source: Ty<'db>, target: Ty<'db>, at: At) {
        match (source.kind(), target.kind()) {
            (TyKind::Ref(ra, ta, _), TyKind::Ref(rb, tb, _)) => {
                self.relate_regions(ra, rb, Variance::Covariant, at);
                self.unsize_pointee(ta, tb, at);
            }
            (TyKind::RawPtr(ta, _), TyKind::RawPtr(tb, _)) => self.unsize_pointee(ta, tb, at),
            (TyKind::Adt(da, aa), TyKind::Adt(db, ab)) if da == db => {
                for (x, y) in aa.iter().zip(ab.iter()) {
                    match (x.kind(), y.kind()) {
                        // `Cell<T>: CoerceUnsized<Cell<U>>` where `T: CoerceUnsized<U>`: a
                        // pointer in it coerces as one, `&'a mut (dyn Trait + 'static)` to
                        // `&'a mut (dyn Trait + 'a)`.
                        (GenericArgKind::Type(x), GenericArgKind::Type(y)) => {
                            let pointer = |ty: Ty<'db>| {
                                matches!(
                                    ty.kind(),
                                    TyKind::Adt(..) | TyKind::Ref(..) | TyKind::RawPtr(..)
                                )
                            };
                            if pointer(x) && pointer(y) {
                                self.unsize(x, y, at);
                            } else {
                                self.unsize_pointee(x, y, at);
                            }
                        }
                        // The impls keep a lifetime as it is: `Ref<'b, T>` to `Ref<'b, U>`.
                        (GenericArgKind::Lifetime(x), GenericArgKind::Lifetime(y)) => {
                            self.relate_regions(x, y, Variance::Invariant, at)
                        }
                        _ => (),
                    }
                }
            }
            _ => (),
        }
    }

    /// What a pointer to `ty` is wide for, with its regions: its struct's last field, and so on.
    fn tail(&mut self, mut ty: Ty<'db>) -> Ty<'db> {
        for _ in 0..16 {
            match ty.kind() {
                TyKind::Adt(def, args) => {
                    let AdtId::StructId(id) = def.def_id() else { break };
                    let types = self.ctx.db.field_types(hir_def::VariantId::StructId(id));
                    let Some((_, last)) = types.iter().last() else { break };
                    let field = last.ty().instantiate(self.interner(), args).skip_norm_wip();
                    ty = match rustc_type_ir::TypeVisitableExt::has_aliases(&field) {
                        true => self.normalize(field).unwrap_or(field),
                        false => field,
                    };
                }
                TyKind::Tuple(tys) => match tys.last() {
                    Some(it) => ty = it,
                    None => break,
                },
                _ => break,
            }
        }
        ty
    }

    fn unsize_pointee(&mut self, source: Ty<'db>, target: Ty<'db>, at: At) {
        match (source.kind(), target.kind()) {
            // rustc's `[T; N]: Unsize<[T]>` is of one `T`: the slice's elements are the array's.
            (TyKind::Array(x, _), TyKind::Slice(y)) => self.relate(x, y, Variance::Invariant, at),
            (TyKind::Dynamic(..), TyKind::Dynamic(..)) => {
                self.relate(source, target, Variance::Covariant, at)
            }
            // rustc's struct unsizing, `RefCell<T>: Unsize<RefCell<dyn Trait>>`: the argument
            // its tail unsizes does so, and the others are the same.
            (TyKind::Adt(source_def, from), TyKind::Adt(target_def, to))
                if source_def == target_def =>
            {
                let interner = self.interner();
                let erase = |ty| {
                    rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_erased(interner))
                };
                for (x, y) in from.iter().zip(to.iter()) {
                    match (x.kind(), y.kind()) {
                        (GenericArgKind::Type(x), GenericArgKind::Type(y))
                            if erase(x) == erase(y) =>
                        {
                            self.relate(x, y, Variance::Invariant, at)
                        }
                        (GenericArgKind::Type(x), GenericArgKind::Type(y)) => {
                            self.unsize_pointee(x, y, at)
                        }
                        (GenericArgKind::Lifetime(x), GenericArgKind::Lifetime(y)) => {
                            self.relate_regions(x, y, Variance::Invariant, at)
                        }
                        _ => (),
                    }
                }
            }
            // The value implements the object's trait, with what the impl proving it needs:
            // `impl<T: 'static> Gettable<T> for S<T>` makes `&S<&'a u8> as &dyn Gettable<_>`
            // need `'a: 'static`; and what the trait says its types outlive, `T: 'static` of a
            // `T` as `dyn Any + 'a`.
            (_, TyKind::Dynamic(predicates, region)) => {
                self.type_outlives(source, region, at);
                for predicate in predicates.iter() {
                    if let rustc_type_ir::ExistentialPredicate::Trait(principal) =
                        predicate.skip_binder()
                    {
                        let trait_ref = principal.with_self_ty(self.interner(), source);
                        for region in self.trait_outlives(trait_ref) {
                            self.type_outlives(source, region, at);
                        }
                        self.prove(trait_ref, at);
                    }
                }
            }
            _ => self.relate(source, target, Variance::Covariant, at),
        }
    }

    fn terminator(&mut self, kind: &TerminatorKind, span: MirSpan) {
        kind.for_each_operand(&mut |operand| self.copy_bound(operand, span));
        match kind {
            TerminatorKind::Call { func, args, destination, .. } => {
                // A function item called through a variable, `let f = foo::<'a>; f()`, which
                // rust-analyzer lowers as a call of the constant, has the variable's type.
                // Through a reference to it too (`let f = &foo::<'a>; f()`).
                let peel = |mut ty: Ty<'db>| {
                    while let TyKind::Ref(_, inner, _) = ty.kind() {
                        ty = inner;
                    }
                    ty
                };
                let local = match (&func.kind, span) {
                    (
                        OperandKind::Constant { .. } | OperandKind::Allocation { .. },
                        MirSpan::ExprId(call),
                    ) => match self.ctx.store[call] {
                        hir_def::hir::Expr::Call { callee, .. } => self
                            .path_local(callee)
                            .filter(|&it| {
                                matches!(
                                    (peel(self.regions.local_tys[it.into_raw().into_u32() as usize]).kind(),
                                        self.operand_ty(func).map(|it| it.kind())),
                                    (TyKind::FnDef(a, _), Some(TyKind::FnDef(b, _))) if a == b
                                )
                            }),
                        _ => None,
                    },
                    _ => None,
                };
                if let (Some(local), Some(location)) = (local, self.location) {
                    self.regions.callee_reads.push((local, location));
                }
                let Some(func_ty) = local
                    .map(|it| peel(self.regions.local_tys[it.into_raw().into_u32() as usize]))
                    .or_else(|| self.written_callee(func, span))
                    .or_else(|| self.constant_ty(func, span))
                    .or_else(|| self.operand_ty(func))
                else {
                    return;
                };
                let (inputs, output) = match func_ty.kind() {
                    TyKind::FnDef(def, fn_args) => {
                        let def: CallableDefId = def.0;
                        let sig = self
                            .ctx
                            .db
                            .callable_item_signature(def)
                            .instantiate(self.interner(), fn_args)
                            .skip_norm_wip();
                        let sig = self.ctx.infcx.instantiate_binder_with_fresh_vars(
                            hir_ty::Span::Dummy,
                            hir_ty::next_solver::infer::BoundRegionConversionTime::FnCall,
                            sig,
                        );
                        self.callee_predicates(def, fn_args, span);
                        let wf = At { category: Category::Boring, span };
                        for ty in fn_args.types() {
                            self.well_formed(ty, wf, 0);
                        }
                        if let CallableDefId::FunctionId(function) = def
                            && let Some(expr) = self.callee_expr(func, span)
                        {
                            let container = function.lookup(self.ctx.db).container;
                            self.path_well_formed(expr, container, fn_args, span);
                        }
                        let (self_param, keep_static, returns_self, static_self) = match def {
                            CallableDefId::FunctionId(it) => {
                                let written = FunctionSignature::of(self.ctx.db, it);
                                let returns_self = written.ret_type.is_some_and(|it| {
                                    matches!(&written.store[it],
                                        hir_def::hir::type_ref::TypeRef::Path(path)
                                            if path.is_self_type())
                                });
                                (
                                    written.has_self_param(),
                                    writes_static_object(&written.store),
                                    returns_self,
                                    written.has_self_param() && self.impl_of_static_object(it),
                                )
                            }
                            _ => (false, true, false, false),
                        };
                        let function = match def {
                            CallableDefId::FunctionId(it) => Some(it),
                            _ => None,
                        };
                        let (inputs, output) = self.elide(
                            function,
                            sig.inputs().iter().copied().collect(),
                            sig.output(),
                            self_param,
                            keep_static,
                            returns_self,
                            static_self,
                        );
                        // The signature as written is well-formed, `&'a &'b ()` that `'b: 'a`
                        // even where it normalizes to `()`; and the where clauses of its
                        // associated types hold.
                        let wf = At { category: Category::Boring, span };
                        for ty in inputs.iter().copied().chain([output]) {
                            self.well_formed(ty, wf, 0);
                        }
                        let at = At { category: Category::Predicate, span };
                        for ty in inputs.iter().copied().chain([output]) {
                            for (def, args) in projections(ty) {
                                if let Ok(def) = GenericDefId::try_from(def) {
                                    self.predicates(def, args, at, false);
                                }
                            }
                        }
                        (inputs, output)
                    }
                    TyKind::FnPtr(sig_tys, header) => {
                        let sig = sig_tys.with(header);
                        let sig = self.ctx.infcx.instantiate_binder_with_fresh_vars(
                            hir_ty::Span::Dummy,
                            hir_ty::next_solver::infer::BoundRegionConversionTime::FnCall,
                            sig,
                        );
                        (sig.inputs().iter().copied().collect::<Vec<_>>(), sig.output())
                    }
                    _ => return,
                };
                // rustc builds a struct or a variant with a constructor's arguments as an
                // aggregate, whose fields are `Boring` to blame.
                let constructor = matches!(
                    func_ty.kind(),
                    TyKind::FnDef(def, _) if !matches!(def.0, CallableDefId::FunctionId(_))
                );
                let category = if constructor { Category::Boring } else { Category::CallArgument };
                // `Fn::call(&f, (x,))` that lowering passes as `Fn::call(&f, x)`: each argument
                // is its element of the signature's tuple.
                let lang = self.interner().lang_items();
                let calls = [lang.Fn_call, lang.FnMut_call_mut, lang.FnOnce_call_once];
                let inputs = match (func_ty.kind(), inputs.as_slice()) {
                    (TyKind::FnDef(def, _), &[receiver, tupled])
                        if let CallableDefId::FunctionId(function) = def.0
                            && calls.contains(&Some(function))
                            && let TyKind::Tuple(elements) = tupled.kind()
                            && args.len() == 1 + elements.len()
                            && !(args.len() == 2
                                && self
                                    .operand_ty(&args[1])
                                    .is_some_and(|it| matches!(it.kind(), TyKind::Tuple(_)))) =>
                    {
                        std::iter::once(receiver).chain(elements.iter()).collect()
                    }
                    _ => inputs,
                };
                for (arg, input) in args.iter().zip(inputs) {
                    let fn_item = arg.span().and_then(|it| self.fn_item(arg, it));
                    if let Some(ty) = fn_item.or_else(|| self.operand_ty(arg)) {
                        let at = At { category, span: arg.span().unwrap_or(span) };
                        self.relate(ty, input, Variance::Covariant, at);
                    }
                }
                // A closure called through `Fn::call`, `FnMut::call_mut` or `FnOnce::call_once`,
                // which lowering passes the arguments of one by one: proving it implements the
                // trait equates the arguments with its signature's inputs, whose regions are in
                // its type (`let f = |_| {}; f(&x)`).
                if let TyKind::FnDef(def, _) = func_ty.kind()
                    && let CallableDefId::FunctionId(function) = def.0
                    && calls.contains(&Some(function))
                    && let [receiver, given @ ..] = &args[..]
                    && let Some(receiver) = self.operand_ty(receiver)
                    && let TyKind::Closure(id, closure) = peel(receiver).kind()
                    && self.infers_signature(id.0)
                {
                    let sig = self.ctx.infcx.instantiate_binder_with_fresh_vars(
                        hir_ty::Span::Dummy,
                        hir_ty::next_solver::infer::BoundRegionConversionTime::FnCall,
                        closure.as_closure().sig(),
                    );
                    if let &[tupled] = sig.inputs().as_slice()
                        && let TyKind::Tuple(inputs) = tupled.kind()
                    {
                        for (arg, input) in given.iter().zip(inputs.iter()) {
                            if let Some(ty) = self.operand_ty(arg) {
                                let at = At {
                                    category: Category::CallArgument,
                                    span: arg.span().unwrap_or(span),
                                };
                                self.relate(ty, input, Variance::Covariant, at);
                            }
                        }
                    }
                    let projection = destination.projection.as_slice();
                    let dest = self.place_ty(destination.local, projection).ty;
                    let at = At {
                        category: self.category_of_assignment(destination.local, projection),
                        span,
                    };
                    self.relate(sig.output(), dest, Variance::Covariant, at);
                }
                let projection = destination.projection.as_slice();
                let dest = self.place_ty(destination.local, projection).ty;
                let at = At {
                    category: self.category_of_assignment(destination.local, projection),
                    span,
                };
                self.relate(output, dest, Variance::Covariant, at);
            }
            TerminatorKind::DropAndReplace { place, value, .. } => {
                let projection = place.projection.as_slice();
                let dest = self.place_ty(place.local, projection).ty;
                if let Some(ty) = self.operand_ty(value) {
                    let at =
                        At { category: self.category_of_assignment(place.local, projection), span };
                    self.relate(ty, dest, Variance::Covariant, at);
                }
            }
            _ => (),
        }
    }

    /// Whether `function` is of an impl for a `dyn Trait + 'static`.
    fn impl_of_static_object(&self, function: hir_def::FunctionId) -> bool {
        let ItemContainerId::ImplId(impl_) = function.lookup(self.ctx.db).container else {
            return false;
        };
        let ty = self.ctx.db.impl_self_ty(impl_).instantiate_identity().skip_norm_wip();
        matches!(ty.kind(), TyKind::Dynamic(_, region) if matches!(region.kind(), RegionKind::ReStatic))
    }

    /// Whether the closure `id` is of this body and writes no type of a parameter or of its
    /// return: the lifetimes rustc infers for its signature are in its type, the same for every
    /// call, where those a written `&T` elides are the call's own.
    fn infers_signature(&self, id: hir_ty::db::InternedClosureId<'db>) -> bool {
        let loc = id.loc(self.ctx.db);
        loc.owner == self.ctx.owner
            && matches!(
                &self.ctx.store[loc.expr],
                hir_def::hir::Expr::Closure { arg_types, ret_type: None, .. }
                    if arg_types.iter().all(Option::is_none)
            )
    }

    /// A callee's signature with the lifetimes it elides, which RA leaves as errors, made
    /// region variables by the elision rules: each elided input lifetime its own, an elided
    /// output lifetime that of `self` when `self` is a reference, else that of the only
    /// lifetime of the inputs. The default lifetime of a trait object is not one of them.
    /// `static_self`: `self` is of an impl for `dyn Trait`, whose lifetime the impl's header
    /// defaults to `'static` (`impl dyn Trait { fn f(&self) }`), not the reference's.
    /// An `impl Trait` `function` returns has, for each lifetime its parameters leave out, that
    /// lifetime's region.
    #[allow(clippy::too_many_arguments)]
    fn elide(
        &self,
        function: Option<hir_def::FunctionId>,
        inputs: Vec<Ty<'db>>,
        output: Ty<'db>,
        self_param: bool,
        keep_static: bool,
        returns_self: bool,
        static_self: bool,
    ) -> (Vec<Ty<'db>>, Ty<'db>) {
        let interner = self.interner();
        let mut defaults = ObjectDefaults { db: self.ctx.db, interner, keep_static, erase: false };
        let mut errors = vec![Vec::new(); inputs.len()];
        let inputs: Vec<Ty<'db>> = inputs
            .into_iter()
            .enumerate()
            .map(|(index, ty)| {
                let ty =
                    rustc_type_ir::fold_regions(interner, ty, |region, _| match region.kind() {
                        RegionKind::ReError(_) => {
                            let region = self.fresh();
                            errors[index].push(region);
                            region
                        }
                        _ => region,
                    });
                if static_self && index == 0 {
                    return ty;
                }
                rustc_type_ir::TypeFoldable::fold_with(ty, &mut defaults)
            })
            .collect();
        let self_region = match inputs.first().map(|it| it.kind()) {
            Some(TyKind::Ref(region, ..)) if self_param => Some(region),
            _ => None,
        };
        let elided = self_region.or_else(|| {
            let mut distinct = Vec::new();
            for ty in &inputs {
                rustc_type_ir::fold_regions(interner, *ty, |region, _| {
                    if !matches!(region.kind(), RegionKind::ReBound(..))
                        && !distinct.contains(&region)
                    {
                        distinct.push(region);
                    }
                    region
                });
            }
            match distinct.as_slice() {
                [only] => Some(*only),
                _ => None,
            }
        });
        let output = match function {
            Some(function) => {
                let regions = hir_ty::elided_lifetimes(self.ctx.db, function).regions(&errors);
                hir_ty::with_elided_regions(self.ctx.db, output, function, &regions)
            }
            None => output,
        };
        // A return of `Self` elides nothing: its anonymous lifetimes are the impl's,
        // `impl Foo<'_>`, which rust-analyzer leaves as errors too.
        let output =
            rustc_type_ir::fold_regions(interner, output, |region, _| match region.kind() {
                RegionKind::ReError(_) if returns_self => self.fresh(),
                RegionKind::ReError(_) => elided.unwrap_or_else(|| self.fresh()),
                _ => region,
            });
        (inputs, rustc_type_ir::TypeFoldable::fold_with(output, &mut defaults))
    }

    /// The where clauses of what a call calls, which the caller must prove.
    fn callee_predicates(&mut self, def: CallableDefId, args: GenericArgs<'db>, span: MirSpan) {
        let generic_def: GenericDefId = match def {
            CallableDefId::FunctionId(it) => it.into(),
            CallableDefId::StructId(it) => it.into(),
            CallableDefId::EnumVariantId(it) => it.lookup(self.ctx.db).parent.into(),
        };
        let at = At { category: Category::Predicate, span };
        self.predicates(generic_def, args, at, true);
    }

    /// The where clauses of `def` with `args`, which hold where it is used. Without
    /// `type_outlives`, only those between regions.
    fn predicates(
        &mut self,
        def: GenericDefId,
        args: GenericArgs<'db>,
        at: At,
        type_outlives: bool,
    ) {
        let interner = self.interner();
        let mut clauses: Vec<_> = GenericPredicates::query_all(self.ctx.db, def)
            .iter_instantiated(interner, args.as_slice())
            .map(|it| it.skip_norm_wip())
            .collect();
        // What an ADT's fields need of its lifetimes, rustc's `inferred_outlives_of`, holds
        // where it is used as its where clauses do.
        if let GenericDefId::AdtId(adt) = def
            && args.regions().next().is_some()
        {
            for predicate in self.inferred.of(self.ctx.db, adt) {
                let OutlivesPredicate(arg, region) = rustc_type_ir::EarlyBinder::bind(predicate)
                    .instantiate(interner, args)
                    .skip_norm_wip();
                use rustc_type_ir::Upcast as _;
                clauses.push(match arg.kind() {
                    GenericArgKind::Type(ty) => OutlivesPredicate(ty, region).upcast(interner),
                    GenericArgKind::Lifetime(longer) => {
                        OutlivesPredicate(longer, region).upcast(interner)
                    }
                    GenericArgKind::Const(_) => continue,
                });
            }
        }
        self.clauses(clauses, at, type_outlives);
    }

    /// `clauses`, which hold where `at` is.
    fn clauses(&mut self, clauses: Vec<Clause<'db>>, at: At, type_outlives: bool) {
        // Outliving every `'a` of the clause's own `for<'a>` is outliving `'static`.
        let forall = |region: Region<'db>| match region.kind() {
            RegionKind::ReBound(rustc_type_ir::BoundVarIndexKind::Bound(binder), _) => {
                binder == rustc_type_ir::INNERMOST
            }
            _ => false,
        };
        for clause in clauses {
            match clause.kind().skip_binder() {
                ClauseKind::RegionOutlives(OutlivesPredicate(longer, shorter)) => {
                    let shorter = match forall(shorter) {
                        true => self.static_region(),
                        false => shorter,
                    };
                    if let (Some(longer), Some(shorter)) =
                        (self.region_rv(longer), self.region_rv(shorter))
                    {
                        self.outlives(longer, shorter, at);
                    }
                }
                ClauseKind::TypeOutlives(OutlivesPredicate(ty, region))
                    if type_outlives
                        && !rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&ty) =>
                {
                    let original = region;
                    let region = match region.kind() {
                        RegionKind::ReStatic => self.static_region(),
                        _ if forall(region) => self.static_region(),
                        _ => region,
                    };
                    let tests = self.regions.type_tests.len();
                    self.type_outlives(ty, region, at);
                    if forall(original) {
                        for test in &mut self.regions.type_tests[tests..] {
                            test.forall = true;
                        }
                    }
                }
                // `I: IntoIterator<Item = T>`: what the projection normalizes to is `T`, so
                // `v.extend(d.iter())` makes the borrow of `d` the vector's element lifetime.
                ClauseKind::Projection(projection)
                    if type_outlives
                        && !rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(
                            &projection,
                        ) =>
                {
                    let interner = self.interner();
                    if let Some(term) = projection.term.as_type()
                        && let Some(alias) = projection.projection_term.to_term(interner).as_type()
                        && rustc_type_ir::TypeVisitableExt::has_free_regions(&alias)
                        && let Some(normalized) = self.normalize(alias)
                        && !matches!(normalized.kind(), TyKind::Alias(..) | TyKind::Infer(_))
                    {
                        self.relate(normalized, term, Variance::Invariant, at);
                    }
                }
                ClauseKind::Trait(predicate) if type_outlives => {
                    let trait_ref = predicate.trait_ref;
                    for region in self.trait_outlives(trait_ref) {
                        self.type_outlives(trait_ref.self_ty(), region, at);
                    }
                    self.caller_bound(trait_ref, at);
                    self.closure_bound(trait_ref, at);
                    let trait_ref = self.normalize_closure_args(trait_ref);
                    // `for<'a> G: Outlives<'a>` is proven for a placeholder `'a`, which a
                    // region outlives only by outliving `'static`.
                    let forall =
                        rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&trait_ref);
                    let interner = self.interner();
                    let instantiate = |with: &mut dyn FnMut(_) -> Region<'db>| {
                        rustc_type_ir::fold_regions(interner, trait_ref, |region, depth| {
                            match region.kind() {
                                RegionKind::ReBound(
                                    rustc_type_ir::BoundVarIndexKind::Bound(binder),
                                    bound,
                                ) if binder == depth => with(bound),
                                _ => region,
                            }
                        })
                    };
                    let placeholders = forall.then(|| {
                        let universe = self.ctx.infcx.create_next_universe();
                        let placeholders = instantiate(&mut |bound| {
                            Region::new_placeholder(
                                interner,
                                hir_ty::next_solver::PlaceholderRegion::new(universe, bound),
                            )
                        });
                        (universe, placeholders)
                    });
                    let trait_ref = match forall {
                        true => instantiate(&mut |_| Region::new_static(interner)),
                        false => trait_ref,
                    };
                    self.trait_args_outlive(trait_ref, at);
                    self.object_assoc_bounds(trait_ref, at);
                    // Proven for placeholders, what the trait's impls need of them is an error
                    // where it is more than they can be, `for<'a> Foo<&'a isize>` needing
                    // `'a: 'b`.
                    if let Some((universe, placeholders)) = placeholders {
                        self.forall_universe = Some(universe);
                        self.prove(placeholders, at);
                        self.forall_universe = None;
                    }
                    // Of a call written as one: `map[i] = v` calls `IndexMut::index_mut`, which
                    // is E0594 where only `Index` is implemented.
                    if self.prove(trait_ref, at)
                        && !forall
                        && !forall
                        && at.category == Category::Predicate
                        && let MirSpan::ExprId(expr) = at.span
                        && matches!(
                            self.ctx.store[expr],
                            hir_def::hir::Expr::Call { .. } | hir_def::hir::Expr::MethodCall { .. }
                        )
                    {
                        self.ctx.tainted.set(true);
                    }
                }
                _ => (),
            }
        }
    }

    /// The regions a type implementing `trait_ref` outlives, by the trait's `Self: 'r` bounds:
    /// its own, instantiated, and `'static` from its supertraits.
    fn trait_outlives(
        &mut self,
        trait_ref: rustc_type_ir::TraitRef<hir_ty::next_solver::DbInterner<'db>>,
    ) -> Vec<Region<'db>> {
        let db = self.ctx.db;
        let mut regions = Vec::new();
        if static_trait(db, trait_ref.def_id.0) {
            regions.push(self.static_region());
        }
        for clause in GenericPredicates::query_all(db, trait_ref.def_id.0.into())
            .iter_instantiated(self.interner(), trait_ref.args.as_slice())
        {
            // `trait Trait<'a>: 'a` of `Trait<'static>` too.
            if let ClauseKind::TypeOutlives(OutlivesPredicate(ty, region)) =
                clause.skip_norm_wip().kind().skip_binder()
                && ty == trait_ref.self_ty()
                && !matches!(region.kind(), RegionKind::ReBound(..))
            {
                regions.push(match region.kind() {
                    RegionKind::ReStatic => self.static_region(),
                    _ => region,
                });
            }
        }
        regions
    }

    /// The trait's where clauses on its arguments, which hold where `trait_ref` is well-formed:
    /// `trait Trait<ARG: 'static>` of `Trait<&'a str>` is `'a: 'static`.
    /// Of a trait object proving its own trait, what the trait's bounds on an associated type
    /// need of the type the object says it is, as rustc's object candidate checks them:
    /// `dyn for<'a> Tr<'a, A = &'b u8>: Tr<'static>` with `type A: 'a` needs `'b: 'static`.
    fn object_assoc_bounds(
        &mut self,
        trait_ref: rustc_type_ir::TraitRef<hir_ty::next_solver::DbInterner<'db>>,
        at: At,
    ) {
        use rustc_type_ir::inherent::Term as _;
        let TyKind::Dynamic(predicates, _) = trait_ref.self_ty().kind() else { return };
        if rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&trait_ref) {
            return;
        }
        let interner = self.interner();
        for predicate in predicates.iter() {
            let rustc_type_ir::ExistentialPredicate::Projection(projection) =
                predicate.skip_binder()
            else {
                continue;
            };
            let hir_ty::next_solver::TermId::TypeAliasId(assoc) = projection.def_id.0 else {
                continue;
            };
            if assoc.lookup(self.ctx.db).container != ItemContainerId::TraitId(trait_ref.def_id.0) {
                continue;
            }
            let Some(term) = projection.term.as_type() else { continue };
            if rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&term) {
                continue;
            }
            let assoc = hir_ty::next_solver::SolverDefId::TypeAliasId(assoc);
            let bounds = rustc_type_ir::Interner::item_bounds(interner, assoc);
            for clause in bounds.iter_instantiated(interner, trait_ref.args.as_slice()) {
                if let ClauseKind::TypeOutlives(OutlivesPredicate(bounded, region)) =
                    clause.kind().skip_binder()
                    && let TyKind::Alias(alias) = bounded.kind()
                    && alias.kind.def_id() == assoc
                    && !matches!(region.kind(), RegionKind::ReBound(..))
                {
                    let region = match region.kind() {
                        RegionKind::ReStatic => self.static_region(),
                        _ => region,
                    };
                    self.type_outlives(term, region, at);
                }
            }
        }
    }

    fn trait_args_outlive(
        &mut self,
        trait_ref: rustc_type_ir::TraitRef<hir_ty::next_solver::DbInterner<'db>>,
        at: At,
    ) {
        if rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&trait_ref) {
            return;
        }
        let db = self.ctx.db;
        for clause in GenericPredicates::query_all(db, trait_ref.def_id.0.into())
            .iter_instantiated(self.interner(), trait_ref.args.as_slice())
        {
            let region = |this: &mut Self, region: Region<'db>| match region.kind() {
                RegionKind::ReStatic => Some(this.static_region()),
                RegionKind::ReBound(..) => None,
                _ => Some(region),
            };
            match clause.skip_norm_wip().kind().skip_binder() {
                ClauseKind::TypeOutlives(OutlivesPredicate(ty, sub))
                    if ty != trait_ref.self_ty()
                        && !rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&ty) =>
                {
                    if let Some(sub) = region(self, sub) {
                        self.type_outlives(ty, sub, at);
                    }
                }
                ClauseKind::RegionOutlives(OutlivesPredicate(longer, shorter)) => {
                    if let Some(longer) = region(self, longer)
                        && let Some(shorter) = region(self, shorter)
                        && let (Some(longer), Some(shorter)) =
                            (self.region_rv(longer), self.region_rv(shorter))
                    {
                        self.outlives(longer, shorter, at);
                    }
                }
                _ => (),
            }
        }
    }

    /// `F: FnOnce(S, T)` of a closure of this body typed against an expected signature: rustc
    /// gives the closure that signature, so its parameters' types are the bound's arguments
    /// (`|z: &[i32]|` passed as `F` takes the `S` it is called with). A closure typed without
    /// one has a higher-ranked signature in rustc, which this closure's one signature cannot
    /// stand for. Of a bound with `for<'r>`, rustc's closure signature binds `'r` too, so
    /// only the bound's other regions are related: `impl FnOnce(&mut F<'a, 'b>)` makes the
    /// closure's `F<'1, '2>` that `F<'a, 'b>`, and leaves its `&'r` its own.
    fn closure_bound(
        &mut self,
        trait_ref: rustc_type_ir::TraitRef<hir_ty::next_solver::DbInterner<'db>>,
        at: At,
    ) {
        use rustc_type_ir::inherent::{GenericArg as _, GenericArgs as _};
        let TyKind::Closure(def, args) = trait_ref.self_ty().kind() else { return };
        let loc = def.0.loc(self.ctx.db);
        let lang = self.interner().lang_items();
        if loc.owner != self.ctx.owner
            || self.ctx.infer.closures_without_expectation.contains(&loc.expr)
            || ![lang.Fn, lang.FnMut, lang.FnOnce].contains(&Some(trait_ref.def_id.0))
        {
            return;
        }
        let sig = args.as_closure().sig();
        let &[inputs] = sig.skip_binder().inputs().as_slice() else { return };
        let Some(expected) = trait_ref.args.as_slice().get(1).and_then(|it| it.as_type()) else {
            return;
        };
        let higher_ranked = !sig.bound_vars().is_empty();
        if (higher_ranked || rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&expected))
            && !self.binds_alike(inputs, expected)
        {
            return;
        }
        // A region either binds is no region of the body, which relating leaves out.
        self.relate(inputs, expected, Variance::Invariant, at);
    }

    /// Whether `a` and `b`, each under a binder of its own, differ only in regions, and bind
    /// a region in the same places.
    fn binds_alike(&self, a: Ty<'db>, b: Ty<'db>) -> bool {
        let interner = self.interner();
        let shape = |ty: Ty<'db>| {
            let mut bound = Vec::new();
            let erased = rustc_type_ir::fold_regions(interner, ty, |region, depth| {
                bound.push(matches!(
                    region.kind(),
                    RegionKind::ReBound(rustc_type_ir::BoundVarIndexKind::Bound(binder), _)
                        if binder == depth
                ));
                Region::new_erased(interner)
            });
            (erased, bound)
        };
        shape(a) == shape(b)
    }

    /// `trait_ref` with an associated type of a closure's type normalized, to what does not
    /// name the closure: `Vec<&'a str>: FromIterator<<Map<I, F> as Iterator>::Item>` is about
    /// the `&'r str` the closure returns.
    fn normalize_closure_args(
        &mut self,
        trait_ref: rustc_type_ir::TraitRef<hir_ty::next_solver::DbInterner<'db>>,
    ) -> rustc_type_ir::TraitRef<hir_ty::next_solver::DbInterner<'db>> {
        use rustc_type_ir::inherent::GenericArg as _;
        if !trait_ref.args.iter().any(|arg| {
            arg.as_type()
                .is_some_and(|ty| matches!(ty.kind(), TyKind::Alias(..)) && names_closure(ty))
        }) {
            return trait_ref;
        }
        let args: Vec<hir_ty::next_solver::GenericArg<'db>> = trait_ref
            .args
            .iter()
            .map(|arg| match arg.as_type() {
                Some(ty) if matches!(ty.kind(), TyKind::Alias(..)) && names_closure(ty) => {
                    match self.normalize(ty) {
                        // Not one with a trait object, whose lifetime an impl may leave out,
                        // `impl Params for &[&dyn ToSql]`, which rust-analyzer makes `'static`.
                        Some(normalized)
                            if !names_closure(normalized)
                                && !normalized.walk().any(|it| {
                                    it.as_type()
                                        .is_some_and(|it| matches!(it.kind(), TyKind::Dynamic(..)))
                                })
                                && !matches!(
                                    normalized.kind(),
                                    TyKind::Alias(..) | TyKind::Infer(_)
                                ) =>
                        {
                            normalized.into()
                        }
                        _ => arg,
                    }
                }
                _ => arg,
            })
            .collect();
        rustc_type_ir::TraitRef::new(self.interner(), trait_ref.def_id, args)
    }

    /// A trait bound on a type parameter, proven by the one where clause of this body that
    /// names the same trait for it, whose arguments it then equals.
    fn caller_bound(
        &mut self,
        trait_ref: rustc_type_ir::TraitRef<hir_ty::next_solver::DbInterner<'db>>,
        at: At,
    ) {
        let TyKind::Param(param) = trait_ref.self_ty().kind() else { return };
        let mut found = None;
        for clause in self.ctx.env.caller_bounds().iter() {
            let kind = clause.kind();
            let ClauseKind::Trait(bound) = kind.skip_binder() else { continue };
            if bound.trait_ref.def_id != trait_ref.def_id
                || !matches!(bound.trait_ref.self_ty().kind(), TyKind::Param(it) if it.index == param.index)
            {
                continue;
            }
            if found.is_some() || !kind.bound_vars().is_empty() {
                return;
            }
            found = Some(bound.trait_ref);
        }
        let Some(bound) = found else { return };
        let args = rustc_type_ir::fold_regions(self.interner(), bound.args, |region, depth| {
            self.signature_region(region, depth).unwrap_or_else(|| self.fresh())
        });
        self.relate_args(trait_ref.args, args, None, Variance::Invariant, at);
    }

    /// Proves `trait_ref` with the trait solver, as rustc's `prove_predicate`, and takes the
    /// region constraints proving it gives: a where clause or an impl it matches relates the
    /// regions of the two (`&'min G: Get` proven by `&'max G: Get` equals them), and the
    /// impl's where clauses (`T: 'a`) hold. Nothing when it is not proven, and `true` when
    /// it does not hold.
    fn prove(
        &mut self,
        trait_ref: rustc_type_ir::TraitRef<hir_ty::next_solver::DbInterner<'db>>,
        at: At,
    ) -> bool {
        use hir_ty::next_solver::{
            infer::{
                region_constraints::Constraint,
                traits::{Obligation, ObligationCause},
            },
            obligation_ctxt::ObligationCtxt,
        };
        if rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&trait_ref)
            || rustc_type_ir::TypeVisitableExt::references_error(&trait_ref)
            || trait_ref.args.iter().any(|arg| {
                rustc_type_ir::inherent::GenericArg::as_type(&arg).is_some_and(names_closure)
            })
        {
            return false;
        }
        // Without regions of its own it can still need what a `for<'a>` in an impl's where
        // clauses says of the placeholders it is proven for: `&'static str: DeserializeOwned`.
        let regionless = !rustc_type_ir::TypeVisitableExt::has_free_regions(&trait_ref);
        let interner = self.interner();
        let (constraints_before, obligations_before) = {
            let mut inner = self.ctx.infcx.inner.borrow_mut();
            let obligations = inner.region_obligations().len();
            let constraints =
                inner.unwrap_region_constraints().region_constraint_data().constraints.len();
            (constraints, obligations)
        };
        let mut ocx = ObligationCtxt::new(&self.ctx.infcx);
        let predicate = rustc_type_ir::TraitPredicate {
            trait_ref,
            polarity: rustc_type_ir::PredicatePolarity::Positive,
        };
        ocx.register_obligation(Obligation::new(
            interner,
            ObligationCause::dummy(),
            self.ctx.env,
            predicate,
        ));
        let errors = ocx.evaluate_obligations_error_on_ambiguity();
        if !errors.is_empty() {
            return !regionless && errors.iter().any(|it| it.is_true_error());
        }
        let (constraints, obligations) = {
            let mut inner = self.ctx.infcx.inner.borrow_mut();
            let obligations: Vec<_> = inner.region_obligations()[obligations_before..]
                .iter()
                .map(|it| (it.sup_type, it.sub_region))
                .collect();
            let constraints: Vec<_> =
                inner.unwrap_region_constraints().region_constraint_data().constraints
                    [constraints_before..]
                    .iter()
                    .cloned()
                    .collect();
            (constraints, obligations)
        };
        if regionless && constraints.is_empty() {
            return false;
        }
        if crate::debugging(self.ctx.db, self.ctx.owner) {
            eprintln!("prove {trait_ref:?}: {constraints:?} {obligations:?}");
        }
        let rv = |this: &mut Self, region: Region<'db>| match region.kind() {
            RegionKind::ReVar(vid) => Some(vid.as_usize()),
            RegionKind::ReStatic => Some(this.regions.static_rv),
            RegionKind::ReEarlyParam(_) | RegionKind::ReLateParam(_) => {
                this.signature_region(region, rustc_type_ir::INNERMOST).and_then(rv_of)
            }
            RegionKind::RePlaceholder(_) => rv_of(this.placeholder(region)),
            _ => None,
        };
        let var = |vid: rustc_type_ir::RegionVid| Region::new_var(interner, vid);
        let pairs: Vec<(Region<'db>, Region<'db>)> = constraints
            .into_iter()
            .map(|constraint| match constraint {
                Constraint::VarSubVar(a, b) => (var(a), var(b)),
                Constraint::RegSubVar(a, b) => (a, var(b)),
                Constraint::VarSubReg(a, b) => (var(a), b),
                Constraint::RegSubReg(a, b) => (a, b),
            })
            .collect();
        // A placeholder equated with an error region stands for a lifetime rust-analyzer
        // elided in a signature, whose trait objects it gives `'static` rather than that
        // lifetime (`fn(&mut dyn Write)` for `for<'b> FnMut(&'b mut dyn Write)`), so nothing
        // else said of it holds.
        let unknown: FxHashSet<Region<'db>> = pairs
            .iter()
            .flat_map(|&(a, b)| [(a, b), (b, a)])
            .filter(|(it, other)| it.is_placeholder() && other.is_error())
            .map(|(it, _)| it)
            .collect();
        // `a <= b`: `b` outlives `a`.
        for (shorter, longer) in pairs {
            if unknown.contains(&shorter) || unknown.contains(&longer) {
                continue;
            }
            // A region of a universe that cannot name the placeholder outlives it only by
            // outliving every region, as rustc's `add_incompatible_universe`.
            let longer = match shorter.is_placeholder()
                && longer != shorter
                && self.ctx.infcx.universe_of_region(longer)
                    < self.ctx.infcx.universe_of_region(shorter)
            {
                true => Region::new_static(interner),
                false => longer,
            };
            if let (Some(shorter), Some(longer)) = (rv(self, shorter), rv(self, longer)) {
                self.outlives(longer, shorter, at);
            }
        }
        let forall = self.forall_universe;
        let ctx = self.ctx;
        let infcx = &ctx.infcx;
        let ours = |region: Region<'db>| {
            region.is_placeholder() && Some(infcx.universe_of_region(region)) == forall
        };
        for (ty, region) in obligations {
            let ty = rustc_type_ir::fold_regions(interner, ty, |it, _| match ours(it) {
                true => Region::new_static(interner),
                false => it,
            });
            let region = match ours(region) {
                true => Region::new_static(interner),
                false => region,
            };
            if region.is_placeholder() || rustc_type_ir::TypeVisitableExt::has_placeholders(&ty) {
                continue;
            }
            let Some(region) = rv(self, region) else { continue };
            let ty = rustc_type_ir::fold_regions(interner, ty, |it, _| match it.kind() {
                RegionKind::ReEarlyParam(_) | RegionKind::ReLateParam(_) | RegionKind::ReStatic => {
                    rv(self, it).map_or(it, |it| {
                        Region::new_var(interner, rustc_type_ir::RegionVid::from_usize(it))
                    })
                }
                _ => it,
            });
            self.type_outlives(
                ty,
                Region::new_var(interner, rustc_type_ir::RegionVid::from_usize(region)),
                at,
            );
        }
        false
    }

    /// `ty` of the signature with its lifetimes as universal regions.
    fn signature_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
        let interner = self.interner();
        rustc_type_ir::fold_regions(interner, ty, |region, depth| {
            self.signature_region(region, depth).unwrap_or(region)
        })
    }

    fn region_rv(&self, region: Region<'db>) -> Option<Rv> {
        match region.kind() {
            RegionKind::ReStatic => Some(self.regions.static_rv),
            _ => rv_of(region),
        }
    }
}

/// The lifetime of a trait object written without one, which rust-analyzer gives as
/// `'static`: in `&'r dyn Trait` it is `'r`, and elsewhere in a function body inferred, unless
/// the body writes `dyn Trait + 'static` somewhere, where it may be that, or the trait's
/// bounds say `'static`, as rustc's `compute_object_lifetime_bound`.
struct ObjectDefaults<'db> {
    db: &'db dyn hir_ty::db::HirDatabase,
    interner: hir_ty::next_solver::DbInterner<'db>,
    keep_static: bool,
    /// In a body, where the lifetime is inferred rather than `'static`.
    erase: bool,
}

impl<'db> rustc_type_ir::TypeFolder<hir_ty::next_solver::DbInterner<'db>> for ObjectDefaults<'db> {
    fn cx(&self) -> hir_ty::next_solver::DbInterner<'db> {
        self.interner
    }

    fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
        use rustc_type_ir::{
            TypeFoldable, TypeSuperFoldable, inherent::BoundExistentialPredicates as _,
        };
        let is_static = |region: Region<'db>| matches!(region.kind(), RegionKind::ReStatic);
        let defaulted = |this: &Self, predicates: <hir_ty::next_solver::DbInterner<'db> as rustc_type_ir::Interner>::BoundExistentialPredicates, object| {
            is_static(object)
                && !this.keep_static
                && !predicates.principal_def_id().is_some_and(|it| static_trait(this.db, it.0))
        };
        match ty.kind() {
            TyKind::Ref(region, inner, mutability) if !is_static(region) => {
                if let TyKind::Dynamic(predicates, object) = inner.kind()
                    && defaulted(self, predicates, object)
                {
                    let inner = Ty::new_dynamic(self.interner, predicates.fold_with(self), region);
                    return Ty::new_ref(self.interner, region, inner, mutability);
                }
                ty.super_fold_with(self)
            }
            TyKind::Dynamic(predicates, object)
                if self.erase && defaulted(self, predicates, object) =>
            {
                Ty::new_dynamic(
                    self.interner,
                    predicates.fold_with(self),
                    Region::new_erased(self.interner),
                )
            }
            _ => ty.super_fold_with(self),
        }
    }
}

/// Each occurrence of one type made another: an impl's self type the one with universal
/// regions for its anonymous lifetimes, an `impl Trait` the type it hides. The first may name
/// bound lifetimes, shifted under each binder; the second names none.
struct Replace<'db> {
    interner: hir_ty::next_solver::DbInterner<'db>,
    replace: (Ty<'db>, Ty<'db>),
    binders: u32,
}

impl<'db> Replace<'db> {
    fn new(interner: hir_ty::next_solver::DbInterner<'db>, replace: (Ty<'db>, Ty<'db>)) -> Self {
        Replace { interner, replace, binders: 0 }
    }
}

impl<'db> rustc_type_ir::TypeFolder<hir_ty::next_solver::DbInterner<'db>> for Replace<'db> {
    fn cx(&self) -> hir_ty::next_solver::DbInterner<'db> {
        self.interner
    }

    fn fold_binder<T: rustc_type_ir::TypeFoldable<hir_ty::next_solver::DbInterner<'db>>>(
        &mut self,
        binder: rustc_type_ir::Binder<hir_ty::next_solver::DbInterner<'db>, T>,
    ) -> rustc_type_ir::Binder<hir_ty::next_solver::DbInterner<'db>, T> {
        use rustc_type_ir::TypeSuperFoldable;
        self.binders += 1;
        let binder = binder.super_fold_with(self);
        self.binders -= 1;
        binder
    }

    fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
        use rustc_type_ir::TypeSuperFoldable;
        if ty == rustc_type_ir::shift_vars(self.interner, self.replace.0, self.binders) {
            self.replace.1
        } else {
            ty.super_fold_with(self)
        }
    }
}

/// Whether `trait_` or one of its supertraits is bounded by `'static`, so that its object
/// type's lifetime defaults to that and each type implementing it outlives it.
fn static_trait(db: &dyn hir_ty::db::HirDatabase, trait_: hir_def::TraitId) -> bool {
    hir_ty::utils::all_super_traits(db, trait_).iter().any(|&it| {
        GenericPredicates::query_all(db, it.into()).skip_binder().any(|clause| {
            matches!(clause.kind().skip_binder(),
                ClauseKind::TypeOutlives(OutlivesPredicate(ty, region))
                    if matches!(ty.kind(), TyKind::Param(param) if param.index == 0)
                        && matches!(region.kind(), RegionKind::ReStatic))
        })
    })
}

/// Whether the type `ty` in `store` writes `Self`.
fn writes_self(
    store: &hir_def::expr_store::ExpressionStore,
    ty: hir_def::hir::type_ref::TypeRefId,
) -> bool {
    use hir_def::{expr_store::path::GenericArg, hir::type_ref::TypeRef};
    match &store[ty] {
        TypeRef::Path(path) => {
            path.is_self_type()
                || path.qualifier().is_some_and(|it| it.is_self_type())
                || path.type_anchor().is_some_and(|it| writes_self(store, it))
                || path.generic_args().into_iter().flatten().flatten().any(|args| {
                    args.args
                        .iter()
                        .any(|arg| matches!(arg, GenericArg::Type(it) if writes_self(store, *it)))
                })
        }
        TypeRef::Tuple(tys) => tys.iter().any(|&it| writes_self(store, it)),
        TypeRef::RawPtr(it, _) | TypeRef::Slice(it) => writes_self(store, *it),
        TypeRef::Reference(it) => writes_self(store, it.ty),
        TypeRef::Array(it) => writes_self(store, it.ty),
        _ => false,
    }
}

/// Whether the type `ty` in `store` writes `&T` or `'_`, a lifetime that elision gives the
/// return type, outside a `fn` pointer and `Fn(..)`, which elide on their own.
fn writes_elided(
    store: &hir_def::expr_store::ExpressionStore,
    ty: hir_def::hir::type_ref::TypeRefId,
) -> bool {
    use hir_def::{
        expr_store::path::{GenericArg, GenericArgsParentheses, Path},
        hir::type_ref::{LifetimeRef, LifetimeRefId, TypeBound, TypeRef},
    };
    let lifetime = |it: LifetimeRefId| matches!(store[it], LifetimeRef::Placeholder);
    let path = |path: &Path| {
        path.generic_args().into_iter().flatten().flatten().any(|args| {
            matches!(args.parenthesized, GenericArgsParentheses::No)
                && args.args.iter().any(|arg| match arg {
                    GenericArg::Lifetime(it) => lifetime(*it),
                    GenericArg::Type(it) => writes_elided(store, *it),
                    _ => false,
                })
        })
    };
    match &store[ty] {
        TypeRef::Reference(it) => it.lifetime.is_none_or(lifetime) || writes_elided(store, it.ty),
        TypeRef::Path(it) => path(it),
        TypeRef::Tuple(tys) => tys.iter().any(|&it| writes_elided(store, it)),
        TypeRef::RawPtr(it, _) | TypeRef::Slice(it) => writes_elided(store, *it),
        TypeRef::Array(it) => writes_elided(store, it.ty),
        TypeRef::ImplTrait(bounds) | TypeRef::DynTrait(bounds) => {
            bounds.iter().any(|bound| match bound {
                TypeBound::Lifetime(it) => lifetime(*it),
                TypeBound::Path(it, _) => path(&store[*it]),
                _ => false,
            })
        }
        _ => false,
    }
}

/// How many lifetimes the type `ty` in `store` elides: `&T`, `'_`, and a path that writes
/// none of its lifetimes, one for `S2` of `type S2<'a> = S<'a, 'a>` though its type has it
/// twice. `None` where that is not known (`dyn Trait`, `impl Trait`, `Self`, associated types).
fn elided_lifetimes(
    db: &dyn hir_ty::db::HirDatabase,
    resolver: &hir_def::resolver::Resolver<'_>,
    store: &hir_def::expr_store::ExpressionStore,
    ty: hir_def::hir::type_ref::TypeRefId,
) -> Option<usize> {
    use hir_def::{
        expr_store::path::GenericArg,
        hir::{
            generics::GenericParams,
            type_ref::{LifetimeRef, TypeRef},
        },
        resolver::TypeNs,
    };
    let count = |it| elided_lifetimes(db, resolver, store, it);
    match &store[ty] {
        TypeRef::Reference(it) => {
            let elided =
                matches!(it.lifetime.map(|it| &store[it]), None | Some(LifetimeRef::Placeholder));
            Some(elided as usize + count(it.ty)?)
        }
        TypeRef::Tuple(tys) => tys.iter().map(|&it| count(it)).sum(),
        TypeRef::RawPtr(it, _) | TypeRef::Slice(it) => count(*it),
        TypeRef::Array(it) => count(it.ty),
        TypeRef::Never | TypeRef::TypeParam(_) => Some(0),
        TypeRef::Path(path) => {
            let params = match resolver.resolve_path_in_type_ns_fully(db, path)? {
                TypeNs::AdtId(it) => GenericParams::of(db, it.into()).len_lifetimes(),
                TypeNs::TypeAliasId(it) => GenericParams::of(db, it.into()).len_lifetimes(),
                TypeNs::BuiltinType(_) | TypeNs::GenericParam(_) => 0,
                _ => return None,
            };
            let (mut written, mut placeholders, mut inner) = (0, 0, 0);
            for args in path.generic_args().into_iter().flatten().flatten() {
                if !args.bindings.is_empty() {
                    return None;
                }
                for arg in &args.args {
                    match arg {
                        GenericArg::Lifetime(it) => {
                            written += 1;
                            placeholders += matches!(store[*it], LifetimeRef::Placeholder) as usize;
                        }
                        GenericArg::Type(it) => inner += count(*it)?,
                        GenericArg::Const(_) => (),
                    }
                }
            }
            Some(inner + if written == 0 { params } else { placeholders })
        }
        _ => None,
    }
}

/// Whether a type in `store` writes `dyn Trait + 'static`.
fn writes_static_object(store: &hir_def::expr_store::ExpressionStore) -> bool {
    use hir_def::hir::type_ref::{LifetimeRef, TypeBound, TypeRef};
    store.types.iter().any(|(_, it)| {
        match it {
        TypeRef::DynTrait(bounds) => bounds.iter().any(|bound| {
            matches!(bound, TypeBound::Lifetime(it) if matches!(store[*it], LifetimeRef::Static))
        }),
        _ => false,
    }
    })
}

/// Whether `path` writes generic arguments, `f::<T>` or `<T as Trait>::f`: a user type
/// annotation, whose lifetimes bind.
/// Whether `path` names the associated item `item` through a type, `Foo::f`, `<()>::f` or
/// `T::f`, not through its trait alone, `Default::default`: rustc gives it a user type
/// annotation whose generic arguments, and an inherent impl's self type, must be well-formed.
fn names_through_type(
    db: &dyn hir_ty::db::HirDatabase,
    path: &hir_def::expr_store::path::Path,
    container: ItemContainerId,
) -> bool {
    if path.type_anchor().is_some() {
        return true;
    }
    let segments = path.segments();
    let Some(qualifier) = segments.get(segments.len().wrapping_sub(2)) else { return false };
    match container {
        ItemContainerId::ImplId(_) => true,
        ItemContainerId::TraitId(trait_) => {
            *qualifier.name != hir_def::signatures::TraitSignature::of(db, trait_).name
        }
        _ => false,
    }
}

/// The `impl Trait` a function's return type `output` has, when it has one, and the part of
/// `ty`, the type its body returns, that hides it: `(impl Iterator, &'b str)` hides it in
/// the tuple's first field.
fn returned_opaque<'db>(
    output: Ty<'db>,
    ty: Ty<'db>,
) -> Option<(rustc_type_ir::AliasTy<hir_ty::next_solver::DbInterner<'db>>, Ty<'db>)> {
    fn find<'db>(
        output: Ty<'db>,
        ty: Ty<'db>,
    ) -> Option<(rustc_type_ir::AliasTy<hir_ty::next_solver::DbInterner<'db>>, Ty<'db>)> {
        match (output.kind(), ty.kind()) {
            (TyKind::Alias(alias), _)
                if matches!(alias.kind, rustc_type_ir::AliasTyKind::Opaque { .. }) =>
            {
                Some((alias, ty))
            }
            (TyKind::Tuple(outputs), TyKind::Tuple(tys)) if outputs.len() == tys.len() => {
                outputs.iter().zip(tys.iter()).find_map(|(output, ty)| find(output, ty))
            }
            (TyKind::Ref(_, output, _), TyKind::Ref(_, ty, _)) => find(output, ty),
            (TyKind::Adt(def, outputs), TyKind::Adt(other, tys))
                if def == other && outputs.len() == tys.len() =>
            {
                outputs.iter().zip(tys.iter()).find_map(|(output, ty)| find(output.ty()?, ty.ty()?))
            }
            _ => None,
        }
    }
    let opaques = output
        .walk()
        .filter_map(|it| rustc_type_ir::inherent::GenericArg::as_type(&it))
        .filter(|ty| {
            matches!(ty.kind(), TyKind::Alias(alias)
                if matches!(alias.kind, rustc_type_ir::AliasTyKind::Opaque { .. }))
        })
        .count();
    if opaques != 1 {
        return None;
    }
    find(output, ty)
}

fn spells_generics(path: &hir_def::expr_store::path::Path) -> bool {
    path.type_anchor().is_some()
        || path.generic_args().is_some_and(|args| args.iter().any(Option::is_some))
}

pub(super) fn adt_variants(
    db: &dyn hir_ty::db::HirDatabase,
    adt: AdtId,
) -> Vec<hir_def::VariantId> {
    match adt {
        AdtId::StructId(it) => vec![it.into()],
        AdtId::UnionId(it) => vec![it.into()],
        AdtId::EnumId(it) => {
            it.enum_variants(db).variants.values().map(|&(it, _)| it.into()).collect()
        }
    }
}

/// The regions and the type parameters in `ty` that must outlive a region for `ty` to: rustc's
/// outlives components.
pub(super) fn outlives_components<'db>(
    ty: Ty<'db>,
    regions: &mut Vec<Rv>,
    params: &mut Vec<Ty<'db>>,
) {
    match ty.kind() {
        TyKind::Param(_) => params.push(ty),
        TyKind::Alias(..) | TyKind::FnPtr(..) | TyKind::Placeholder(_) | TyKind::Bound(..) => (),
        TyKind::Ref(region, inner, _) => {
            regions.extend(rv_of(region));
            outlives_components(inner, regions, params);
        }
        TyKind::Dynamic(predicates, region) => {
            regions.extend(rv_of(region));
            for predicate in predicates.iter() {
                if let rustc_type_ir::ExistentialPredicate::Trait(it) = predicate.skip_binder() {
                    args_components(it.args, regions, params);
                }
            }
        }
        TyKind::Adt(_, args) | TyKind::FnDef(_, args) => args_components(args, regions, params),
        TyKind::Closure(_, args) => {
            outlives_components(args.as_closure().tupled_upvars_ty(), regions, params)
        }
        TyKind::Coroutine(_, args) => {
            outlives_components(args.as_coroutine().tupled_upvars_ty(), regions, params)
        }
        TyKind::Tuple(tys) => {
            for it in tys.iter() {
                outlives_components(it, regions, params);
            }
        }
        TyKind::Array(inner, _) | TyKind::Slice(inner) | TyKind::RawPtr(inner, _) => {
            outlives_components(inner, regions, params)
        }
        _ => (),
    }
}

/// The associated and opaque types among what `outlives_components` splits `ty` into, which
/// it leaves out.
pub(super) fn alias_components<'db>(ty: Ty<'db>, out: &mut Vec<Ty<'db>>) {
    let args = |args: GenericArgs<'db>, out: &mut Vec<Ty<'db>>| {
        for it in args.types() {
            alias_components(it, out);
        }
    };
    match ty.kind() {
        TyKind::Alias(alias)
            if matches!(
                alias.kind,
                rustc_type_ir::AliasTyKind::Projection { .. }
                    | rustc_type_ir::AliasTyKind::Opaque { .. }
            ) =>
        {
            out.push(ty)
        }
        TyKind::Ref(_, inner, _)
        | TyKind::Array(inner, _)
        | TyKind::Slice(inner)
        | TyKind::RawPtr(inner, _) => alias_components(inner, out),
        TyKind::Dynamic(predicates, _) => {
            for predicate in predicates.iter() {
                if let rustc_type_ir::ExistentialPredicate::Trait(it) = predicate.skip_binder() {
                    args(it.args, out);
                }
            }
        }
        TyKind::Adt(_, it) | TyKind::FnDef(_, it) => args(it, out),
        TyKind::Closure(_, it) => alias_components(it.as_closure().tupled_upvars_ty(), out),
        TyKind::Coroutine(_, it) => alias_components(it.as_coroutine().tupled_upvars_ty(), out),
        TyKind::Tuple(tys) => {
            for it in tys.iter() {
                alias_components(it, out);
            }
        }
        _ => (),
    }
}

fn args_components<'db>(args: GenericArgs<'db>, regions: &mut Vec<Rv>, params: &mut Vec<Ty<'db>>) {
    for arg in args.iter() {
        match arg.kind() {
            GenericArgKind::Lifetime(region) => regions.extend(rv_of(region)),
            GenericArgKind::Type(ty) => outlives_components(ty, regions, params),
            GenericArgKind::Const(_) => (),
        }
    }
}

/// The points each region holds at, and past them the universal regions it outlives.
pub(super) struct RegionValues {
    pub(super) points: usize,
    pub(super) values: Vec<BitSet>,
}

impl RegionValues {
    pub(super) fn contains_point(&self, rv: Rv, point: usize) -> bool {
        self.values[rv].contains(point)
    }

    pub(super) fn universals_of(&self, rv: Rv) -> impl Iterator<Item = usize> + '_ {
        let points = self.points;
        self.values[rv].iter().filter(move |&it| it >= points).map(move |it| it - points)
    }
}

/// Region inference: the values liveness gives grown along the outlives constraints until
/// each region holds wherever a region it outlives does.
pub(super) fn infer(
    regions: &Regions<'_>,
    count: usize,
    points: usize,
    mut values: Vec<BitSet>,
) -> RegionValues {
    let size = points + regions.universals.len();
    values.resize(count, BitSet::new(size));
    for (index, &(rv, _)) in regions.universals.iter().enumerate() {
        let value = &mut values[rv];
        for point in 0..points {
            value.insert(point);
        }
        value.insert(points + index);
    }
    let mut successors: Vec<Vec<Rv>> = vec![Vec::new(); count];
    for constraint in &regions.outlives {
        if constraint.sup < count && constraint.sub < count {
            successors[constraint.sub].push(constraint.sup);
        }
    }
    let mut queued = vec![true; count];
    let mut work: Vec<Rv> = (0..count).collect();
    while let Some(sub) = work.pop() {
        queued[sub] = false;
        if successors[sub].is_empty() {
            continue;
        }
        let value = values[sub].clone();
        for &sup in &successors[sub] {
            if values[sup].union(&value) && !queued[sup] {
                queued[sup] = true;
                work.push(sup);
            }
        }
    }
    RegionValues { points, values }
}

/// The constraints from `from` down to `to`, the shortest way, for the error that blames one
/// of them.
pub(super) fn constraint_path(
    regions: &Regions<'_>,
    count: usize,
    from: Rv,
    to: Rv,
) -> Vec<Outlives> {
    let mut by_sup: Vec<Vec<usize>> = vec![Vec::new(); count];
    for (index, constraint) in regions.outlives.iter().enumerate() {
        if constraint.sup < count {
            by_sup[constraint.sup].push(index);
        }
    }
    let mut came_from: Vec<Option<usize>> = vec![None; count];
    let mut seen = vec![false; count];
    let mut queue = std::collections::VecDeque::from([from]);
    seen[from] = true;
    while let Some(rv) = queue.pop_front() {
        if rv == to {
            let mut path = Vec::new();
            let mut at = to;
            while let Some(index) = came_from[at] {
                path.push(regions.outlives[index]);
                at = regions.outlives[index].sup;
            }
            path.reverse();
            return path;
        }
        for &index in &by_sup[rv] {
            let sub = regions.outlives[index].sub;
            if !seen[sub] {
                seen[sub] = true;
                came_from[sub] = Some(index);
                queue.push_back(sub);
            }
        }
    }
    Vec::new()
}

/// The constraint an error about `path` points at, as rustc's `best_blame_constraint`.
pub(super) fn blame(path: &[Outlives]) -> Option<Outlives> {
    blame_with(path, false)
}

/// [`blame`], where what is returned is an `impl Trait` that captures no lifetime: rustc's
/// `OpaqueType` constraint, less interesting than an assignment.
pub(super) fn blame_with(path: &[Outlives], opaque: bool) -> Option<Outlives> {
    // rustc's `best_blame_constraint` from a free region: the most interesting constraint,
    // and of equally interesting ones the closest to the region outlived.
    let interest = |it: &Outlives| match it.category {
        Category::Return if opaque => 3,
        Category::Return => 0,
        Category::TypeAnnotation
        | Category::Cast
        | Category::CallArgument
        | Category::Assignment
        | Category::Usage => 2,
        Category::Predicate => 4,
        Category::Boring | Category::Signature | Category::Opaque => 5,
        Category::Internal => 7,
    };
    let best = path.iter().map(interest).min()?;
    path.iter()
        .rev()
        .find(|it| interest(it) == best && it.span != MirSpan::Unknown)
        .or_else(|| path.iter().rev().find(|it| interest(it) == best))
        .copied()
}

/// `ty` an associated type normalized, with the lifetimes it takes from its arguments the same
/// regions: the solver gives the result regions of its own, related to those it came from only
/// by constraints it records.
pub(super) fn normalize<'db>(ctx: &Ctx<'_, 'db>, ty: Ty<'db>) -> Option<Ty<'db>> {
    use hir_ty::next_solver::{
        infer::{region_constraints::Constraint, traits::ObligationCause},
        obligation_ctxt::ObligationCtxt,
    };
    if rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&ty) {
        return None;
    }
    let infcx = &ctx.infcx;
    let constraints_before = infcx
        .inner
        .borrow_mut()
        .unwrap_region_constraints()
        .region_constraint_data()
        .constraints
        .len();
    let first_new = infcx.num_region_vars();
    let mut ocx = ObligationCtxt::new(infcx);
    let normalized = ocx.structurally_normalize_ty(&ObligationCause::dummy(), ctx.env, ty).ok()?;
    let mut same = FxHashMap::default();
    {
        let mut inner = infcx.inner.borrow_mut();
        let collector = inner.unwrap_region_constraints();
        let data = collector.region_constraint_data();
        for constraint in &data.constraints[constraints_before..] {
            if let Constraint::VarSubVar(a, b) = *constraint {
                let (new, old) = if a.index() >= first_new { (a, b) } else { (b, a) };
                if new.index() >= first_new && old.index() < first_new {
                    same.entry(new).or_insert(old);
                }
            }
        }
    }
    let mut unmapped = false;
    let normalized = rustc_type_ir::fold_regions(ctx.infcx.interner, normalized, |region, _| {
        match region.kind() {
            RegionKind::ReVar(vid) if vid.index() >= first_new => match same.get(&vid) {
                Some(&old) => Region::new_var(ctx.infcx.interner, old),
                None => {
                    unmapped = true;
                    region
                }
            },
            _ => region,
        }
    });
    (!unmapped).then_some(normalized)
}
