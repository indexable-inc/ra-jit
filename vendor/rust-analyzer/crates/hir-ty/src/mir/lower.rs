//! This module generates a polymorphic MIR from a hir body

use std::{borrow::Cow, iter, mem};

use arrayvec::ArrayVec;
use base_db::Crate;
use hir_def::{
    AdtId, DefWithBodyId, EnumVariantId, ExpressionStoreOwnerId, GenericParamId, HasModule,
    ItemContainerId, LocalFieldId, Lookup, TraitId,
    expr_store::{Body, ExpressionStore, HygieneId, body::Param, path::Path},
    hir::{
        ArithOp, Array, AsmOperand, BinaryOp, BindingAnnotation, BindingId, ClosureKind,
        CoroutineKind, CoroutineSource, ExprId, ExprOrPatId, LabelId, Literal, LoopSource,
        MatchArm, Pat, PatId, RecordLitField, RecordSpread,
    },
    item_tree::FieldsShape,
    lang_item::LangItems,
    resolver::{HasResolver, ResolveValueResult, Resolver, ValueNs},
    signatures::{ConstSignature, EnumSignature, FunctionSignature, StaticSignature},
};
use hir_expand::name::Name;
use itertools::{Either, EitherOrBoth, Itertools};
use la_arena::{ArenaMap, RawIdx};
use rustc_apfloat::Float;
use rustc_hash::{FxHashMap, FxHashSet};
use rustc_type_ir::{
    CoroutineArgs, CoroutineArgsParts, TypeVisitableExt,
    inherent::{Const as _, GenericArgs as _, IntoKind, Ty as _},
};
use salsa::SalsaValue;
use span::Edition;

use crate::{
    Adjust, Adjustment, AutoBorrow, CallableDefId, InferBodyId, ParamEnvAndCrate,
    consteval::{ConstEvalError, try_const_usize},
    db::{
        AnonConstId, AnonConstLoc, GeneralConstId, HirDatabase, InternedClosure, InternedClosureId,
        InternedCoroutineId,
    },
    generics::generics,
    infer::{
        AllowTwoPhase, AutoBorrowMutability, CaptureSourceStack, CapturedPlace, UpvarCapture,
        cast::CastTy,
        closure::analysis::expr_use_visitor::{
            Place as HirPlace, PlaceBase as HirPlaceBase, ProjectionKind as HirProjectionKind,
        },
    },
    inhabitedness::is_ty_uninhabited_from,
    layout::LayoutError,
    lower::LoweringMode,
    method_resolution::CandidateId,
    mir::{
        AggregateKind, Arena, BasicBlock, BasicBlockId, BinOp, BorrowKind, CastKind,
        CoroutineLocals, Expr, FieldIndex, GenericArgs, Idx, InferenceResult, Local, LocalId,
        MemoryMap, MirBody, MirPhase, MirSpan, Mutability, Operand, PlaceElem, PointerCast,
        Projection, ProjectionElem, Rvalue, Statement, StatementKind, StoredPlace, SwitchTargets,
        Terminator, TerminatorKind, Ty, UnOp, VariantId, return_slot,
    },
    next_solver::{
        Const, DbInterner, ParamConst, ParamEnv, Region, StoredEarlyBinder, StoredGenericArgs,
        StoredTy, TyKind, TypingMode, UnevaluatedConst,
        infer::{DbInternerInferExt, InferCtxt},
    },
};

use super::{OperandKind, Place};

mod as_place;
mod match_tree;
mod pattern_matching;

#[derive(Debug, Clone)]
struct LoopBlocks {
    begin: BasicBlockId,
    /// `None` for loops that are not terminating
    end: Option<BasicBlockId>,
    place: StoredPlace,
    drop_scope_index: usize,
}

#[derive(Debug, Clone, Default)]
struct DropScope {
    /// locals, in order of definition (so we should run drop glues in reverse order)
    locals: Vec<LocalId>,
    /// Whether this scope only holds the temporaries of a `let` initializer. The bindings the
    /// `let` declares outlive it.
    temporaries: bool,
    /// While the tail of a block before the 2024 edition is lowered: how many locals the block
    /// had declared before it. What the tail declares that is not an operand outlives the
    /// block's locals (`outlive_block`), also when the tail panics or leaves early.
    tail: Option<usize>,
    /// The blocks where leaving early (`break`, `continue`, `return`, a future dropped at an
    /// `await`) dropped this scope's locals. Those drops are written at the end of the scope
    /// once it is popped, as rustc writes every drop it schedules.
    exits: Vec<BasicBlockId>,
}

impl DropScope {
    /// The locals in the order they were declared, which is the reverse of the order they are
    /// dropped in. The temporaries of a tail before the 2024 edition come before the block's
    /// locals, as `outlive_block` leaves them once the tail is lowered.
    fn in_order(&self, operands: &FxHashSet<LocalId>) -> Cow<'_, [LocalId]> {
        match self.tail {
            Some(declared) if declared < self.locals.len() => {
                let (operands, temporaries): (Vec<LocalId>, Vec<LocalId>) =
                    self.locals[declared..].iter().partition(|it| operands.contains(it));
                Cow::Owned([temporaries, self.locals[..declared].to_vec(), operands].concat())
            }
            _ => Cow::Borrowed(&self.locals),
        }
    }
}

/// A binding of a guarded match arm that is moved into once its guard holds, and is live
/// only from then, as rustc binds what an arm binds by value after its guard.
struct GuardedMove<'db> {
    binding: BindingId,
    local: LocalId,
    place: Place<'db>,
    span: MirSpan,
}

/// What a destructuring assignment assigns once it has read every part: `target`, from the
/// temporary `part` holds the part in, as rustc's desugaring to a `let` of the parts and an
/// assignment of each after it does.
struct Assigned {
    target: AssignTarget,
    part: LocalId,
    span: MirSpan,
}

enum AssignTarget {
    Local(LocalId),
    Place(ExprId),
}

struct MirLowerCtx<'a, 'db> {
    result: MirBody<'db>,
    owner: InferBodyId<'db>,
    store_owner: ExpressionStoreOwnerId,
    current_loop_blocks: Option<LoopBlocks>,
    labeled_loop_blocks: FxHashMap<LabelId, LoopBlocks>,
    discr_temp: Option<StoredPlace>,
    db: &'db dyn HirDatabase,
    store: &'a ExpressionStore,
    infer: &'a InferenceResult<'db>,
    types: &'db crate::next_solver::DefaultAny<'db>,
    resolver: Resolver<'db>,
    drop_scopes: Vec<DropScope>,
    /// Expressions whose temporary lives as long as the `let` they initialize, with the index of
    /// the scope that `let` is in.
    extended_temporaries: FxHashMap<ExprId, usize>,
    /// Blocks whose `super let`s a `let` extends, with the index of the scope that `let` is in.
    extended_blocks: FxHashMap<ExprId, usize>,
    /// The scope that owns what the `super let` being lowered binds.
    super_let: Option<usize>,
    /// How many `const { .. }` blocks deep the expression being lowered is.
    inline_consts: u32,
    /// The expressions that end their own temporaries: the body, and from the 2024 edition the
    /// tail of a block. The operands of such an expression are in one scope with its
    /// temporaries, where those of any other expression are in a scope inside it.
    ends_temporaries: FxHashSet<ExprId>,
    /// The bindings of the body that have no local yet. rustc declares a binding where its
    /// pattern is lowered (a `let` before its initializer), so locals are numbered in that
    /// order, which decides the order of what an `async` block keeps in its future.
    undeclared: FxHashSet<BindingId>,
    /// rustc's unwind drop tree (`build/scope.rs`): a cleanup block per drop that scopes
    /// schedule, keyed by the block unwinding goes on to after it, the local, and whether it
    /// drops the value or ends the storage. Unwinding from anywhere goes through what is in
    /// scope there, innermost first, and ways out of sibling scopes share what they have in
    /// common, so what is in scope in one never meets what is in scope in another.
    unwind_nodes: FxHashMap<(BasicBlockId, LocalId, bool), BasicBlockId>,
    unwind_resume: Option<BasicBlockId>,
    /// rustc's exit drop trees: the way out of a `break`, `continue` or `return`, or of a
    /// future dropped at an `await`, drops each value it leaves in a block keyed by the block
    /// that comes after it, the local, and the locals whose storage ends between the two. Ways
    /// out to the same place share the drops of what they both leave: a struct literal whose
    /// fields each `?` drops what the fields before made once, not once for every field after
    /// them.
    exit_nodes: FxHashMap<(BasicBlockId, LocalId, Vec<LocalId>), BasicBlockId>,
    /// The block every `return` goes to once it has dropped what it leaves.
    returned: Option<BasicBlockId>,
    /// The block a future dropped at an `await` ends in once what is in scope is dropped.
    coroutine_dropped: Option<BasicBlockId>,
    /// The locals that hold an operand of an aggregate or a call lowered so far.
    operand_locals: FxHashSet<LocalId>,
    /// The temporaries an expression is being written to. rustc's `as_temp` schedules the drop
    /// of a temporary's value once the expression is done, so what unwinds out of it, the drops
    /// of its own temporaries after it wrote the value say, ends their storage only.
    unscheduled: FxHashSet<LocalId>,
    /// Set for the pattern that is lowered next: it is of a match arm that rustc builds no
    /// way into.
    arm_unreached: bool,
    /// The alternative to match for an or-pattern, while an arm is lowered once per
    /// combination of alternatives.
    or_choices: FxHashMap<PatId, usize>,
    /// While the pattern of a match arm that has a guard is lowered: what it binds by value,
    /// which is bound only once the guard holds. The guard reads each
    /// through a shared reference to the place it comes out of (`guard_refs`), as rustc lowers
    /// it, so a guard that reads the value matched on does not read a moved value.
    guarded_moves: Option<Vec<GuardedMove<'db>>>,
    /// The reference a guard reads each binding of `guarded_moves` through.
    guard_refs: FxHashMap<BindingId, LocalId>,
    /// While the pattern of a destructuring assignment is lowered: what it assigns, and the
    /// value it destructures, where a part that cannot be moved out of it is reported.
    assigned: Vec<Assigned>,
    assigned_from: MirSpan,
    /// Whether each type asked about is `Copy`. Every statement that may panic asks of every
    /// local in scope, which in a long body is the most of lowering it.
    copy: FxHashMap<Ty<'db>, bool>,
    env: ParamEnv<'db>,
    infcx: InferCtxt<'db>,
}

// FIXME: Make this smaller, its stored in database queries
#[derive(Debug, Clone, PartialEq, Eq, SalsaValue)]
pub enum MirLowerError<'db> {
    ConstEvalError(Box<str>, Box<ConstEvalError<'db>>),
    LayoutError(LayoutError),
    IncompleteExpr,
    IncompletePattern,
    /// Trying to lower a trait function, instead of an implementation
    TraitFunctionDefinition(TraitId, Name),
    UnresolvedName {
        owner: ExpressionStoreOwnerId,
        path: Path,
    },
    RecordLiteralWithoutPath,
    UnresolvedMethod(String),
    UnresolvedField,
    UnsizedTemporary(StoredTy),
    MissingFunctionDefinition(InferBodyId<'db>, ExprId),
    HasErrors,
    /// This should never happen. Type mismatch should catch everything.
    TypeError(&'static str),
    NotSupported(String),
    ContinueWithoutLoop,
    BreakWithoutLoop,
    Loop,
    /// Something that should never happen and is definitely a bug, but we don't want to panic if it happened
    ImplementationError(String),
    LangItemNotFound,
    MutatingRvalue,
    UnresolvedLabel,
    UnresolvedUpvar(StoredPlace),
    InaccessibleLocal,

    // monomorphization errors:
    GenericArgNotProvided(GenericParamId, StoredGenericArgs),
}

/// A token to ensuring that each drop scope is popped at most once, thanks to the compiler that checks moves.
struct DropScopeToken;
impl DropScopeToken {
    fn pop_and_drop<'db>(
        self,
        ctx: &mut MirLowerCtx<'_, 'db>,
        current: BasicBlockId,
        span: MirSpan,
    ) -> BasicBlockId {
        std::mem::forget(self);
        ctx.pop_drop_scope_internal(current, span)
    }

    /// It is useful when we want a drop scope is syntactically closed, but we don't want to execute any drop
    /// code. Either when the control flow is diverging (so drop code doesn't reached) or when drop is handled
    /// for us (for example a block that ended with a return statement. Return will drop everything, so the block shouldn't
    /// do anything)
    fn pop_assume_dropped(self, ctx: &mut MirLowerCtx<'_, '_>) {
        std::mem::forget(self);
        ctx.pop_drop_scope_assume_dropped_internal();
    }
}

impl Drop for DropScopeToken {
    fn drop(&mut self) {}
}

// Uncomment this to make `DropScopeToken` a drop bomb. Unfortunately we can't do this in release, since
// in cases that mir lowering fails, we don't handle (and don't need to handle) drop scopes so it will be
// actually reached. `pop_drop_scope_assert_finished` will also detect this case, but doesn't show useful
// stack trace.
//
// impl Drop for DropScopeToken {
//     fn drop(&mut self) {
//         never!("Drop scope doesn't popped");
//     }
// }

macro_rules! not_supported {
    ($it: expr) => {
        return Err(MirLowerError::NotSupported(format!($it)))
    };
}

macro_rules! implementation_error {
    ($it: expr) => {{
        ::stdx::never!("MIR lower implementation bug: {}", format!($it));
        return Err(MirLowerError::ImplementationError(format!($it)));
    }};
}

impl From<LayoutError> for MirLowerError<'_> {
    fn from(value: LayoutError) -> Self {
        MirLowerError::LayoutError(value)
    }
}

type Result<'db, T> = std::result::Result<T, MirLowerError<'db>>;

impl<'a, 'db> MirLowerCtx<'a, 'db> {
    fn new(
        db: &'db dyn HirDatabase,
        owner: InferBodyId<'db>,
        store: &'a ExpressionStore,
        infer: &'a InferenceResult<'db>,
    ) -> Self {
        let mut basic_blocks = Arena::new();
        let start_block = basic_blocks.alloc(BasicBlock {
            statements: vec![],
            terminator: None,
            is_cleanup: false,
        });
        let locals = Arena::new();
        let binding_locals: ArenaMap<BindingId, LocalId> = ArenaMap::new();
        let mir = MirBody {
            basic_blocks,
            locals,
            start_block,
            binding_locals,
            upvar_locals: FxHashMap::default(),
            param_locals: vec![],
            promoted_locals: vec![],
            owner,
            closures: vec![],
            coroutine: None,
            unreached: vec![],
            scope_ends: vec![],
            inherits_overflow_checks: match owner {
                InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)) => {
                    hir_def::attrs::AttrFlags::query(db, func.into())
                        .contains(hir_def::attrs::AttrFlags::RUSTC_INHERIT_OVERFLOW_CHECKS)
                }
                _ => false,
            },
            phase: MirPhase::Built,
            upvars_in_place: Vec::new(),
            panic_unwinds: Vec::new(),
            promoted_constants: Vec::new(),
        };
        let store_owner = owner.expression_store_owner(db);
        let resolver = owner.resolver(db);
        let env = db.trait_environment(owner.generic_def(db));
        let interner = DbInterner::new_with(db, resolver.krate());
        // FIXME(next-solver): Is `non_body_analysis()` correct here? Don't we want to reveal opaque types defined by this body?
        let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());

        MirLowerCtx {
            result: mir,
            db,
            infer,
            store,
            types: crate::next_solver::default_types(),
            owner,
            store_owner,
            resolver,
            current_loop_blocks: None,
            labeled_loop_blocks: Default::default(),
            discr_temp: None,
            drop_scopes: vec![DropScope::default()],
            extended_temporaries: FxHashMap::default(),
            extended_blocks: FxHashMap::default(),
            super_let: None,
            inline_consts: 0,
            ends_temporaries: FxHashSet::default(),
            undeclared: FxHashSet::default(),
            unwind_nodes: FxHashMap::default(),
            unwind_resume: None,
            exit_nodes: FxHashMap::default(),
            returned: None,
            coroutine_dropped: None,
            operand_locals: FxHashSet::default(),
            unscheduled: FxHashSet::default(),
            arm_unreached: false,
            or_choices: FxHashMap::default(),
            guarded_moves: None,
            guard_refs: FxHashMap::default(),
            assigned: Vec::new(),
            assigned_from: MirSpan::Unknown,
            copy: FxHashMap::default(),
            env,
            infcx,
        }
    }

    #[inline]
    fn interner(&self) -> DbInterner<'db> {
        self.infcx.interner
    }

    #[inline]
    fn lang_items(&self) -> &'db LangItems {
        self.infcx.interner.lang_items()
    }

    fn temp(&mut self, ty: Ty<'db>, current: BasicBlockId, span: MirSpan) -> Result<'db, LocalId> {
        if matches!(ty.kind(), TyKind::Slice(_) | TyKind::Dynamic(..)) {
            return Err(MirLowerError::UnsizedTemporary(ty.store()));
        }
        let l = self.result.locals.alloc(Local { ty: ty.store() });
        self.push_storage_live_for_local(l, current, span)?;
        Ok(l)
    }

    fn lower_expr_to_some_operand(
        &mut self,
        expr_id: ExprId,
        current: BasicBlockId,
    ) -> Result<'db, Option<(Operand, BasicBlockId)>> {
        if !self.has_adjustments(expr_id)
            && let Expr::Literal(l) = &self.store[expr_id]
        {
            let ty = self.expr_ty_without_adjust(expr_id);
            let mut operand = self.lower_literal_to_operand(ty, l)?;
            operand.span = Some(expr_id.into());
            return Ok(Some((operand, current)));
        }
        let Some((p, current)) = self.lower_expr_to_fresh_place(expr_id, current)? else {
            return Ok(None);
        };
        // rustc moves out of a temporary, as it reads it once. Only a `Copy` one here: what
        // a temporary with drop glue moves out of is how drops are elaborated.
        let kind = if self.is_copy(self.result.locals[p.local].ty.as_ref()) {
            OperandKind::Move(p.store())
        } else {
            OperandKind::Copy(p.store())
        };
        Ok(Some((Operand { kind, span: Some(expr_id.into()) }, current)))
    }

    /// The value of an expression in a place nothing else can write to. A place expression is
    /// read now, into a temporary: in `x + { x = 2; 1 }` and `x = x` the value is the one `x`
    /// had when the operand was evaluated.
    fn lower_expr_to_fresh_place(
        &mut self,
        expr_id: ExprId,
        current: BasicBlockId,
    ) -> Result<'db, Option<(Place<'db>, BasicBlockId)>> {
        let locals = self.result.locals.len();
        let Some((p, current)) = self.lower_expr_as_place(current, expr_id, true)? else {
            return Ok(None);
        };
        let fresh =
            p.projection.as_slice().is_empty() && p.local.into_raw().into_u32() as usize >= locals;
        if fresh {
            return Ok(Some((p, current)));
        }
        let temp: Place<'db> =
            self.temp(self.expr_ty_after_adjustments(expr_id), current, expr_id.into())?.into();
        self.push_assignment(
            current,
            temp,
            Operand { kind: OperandKind::Copy(p.store()), span: None }.into(),
            expr_id.into(),
        );
        Ok(Some((temp, current)))
    }

    fn lower_expr_to_place_with_adjust(
        &mut self,
        expr_id: ExprId,
        place: Place<'db>,
        current: BasicBlockId,
        adjustments: &[Adjustment],
    ) -> Result<'db, Option<BasicBlockId>> {
        match adjustments.split_last() {
            Some((last, rest)) => match &last.kind {
                Adjust::NeverToAny => {
                    let temp = self.temp(self.types.types.never, current, MirSpan::Unknown)?;
                    self.lower_expr_to_place_with_adjust(expr_id, temp.into(), current, rest)
                }
                Adjust::Deref(_) => {
                    let Some((p, current)) =
                        self.lower_expr_as_place_with_adjust(current, expr_id, true, adjustments)?
                    else {
                        return Ok(None);
                    };
                    self.push_assignment(
                        current,
                        place,
                        Operand { kind: OperandKind::Copy(p.store()), span: None }.into(),
                        expr_id.into(),
                    );
                    Ok(Some(current))
                }
                Adjust::Borrow(AutoBorrow::Ref(m)) => {
                    let bk = match m {
                        AutoBorrowMutability::Mut {
                            allow_two_phase_borrow: AllowTwoPhase::Yes,
                        } => BorrowKind::Mut { kind: super::MutBorrowKind::TwoPhasedBorrow },
                        _ => BorrowKind::from_rustc_mutability((*m).into()),
                    };
                    self.lower_expr_to_place_with_borrow_adjust(expr_id, place, current, rest, bk)
                }
                Adjust::Borrow(AutoBorrow::RawPtr(m)) => {
                    let bk = BorrowKind::from_rustc_mutability(*m);
                    self.lower_expr_to_place_with_borrow_adjust(expr_id, place, current, rest, bk)
                }
                Adjust::Pointer(cast) => {
                    let Some((p, current)) =
                        self.lower_expr_as_place_with_adjust(current, expr_id, true, rest)?
                    else {
                        return Ok(None);
                    };
                    self.push_assignment(
                        current,
                        place,
                        Rvalue::Cast(
                            CastKind::PointerCoercion(*cast),
                            Operand { kind: OperandKind::Copy(p.store()), span: None },
                            last.target.clone(),
                        ),
                        expr_id.into(),
                    );
                    Ok(Some(current))
                }
            },
            None => self.lower_expr_to_place_without_adjust(expr_id, place, current),
        }
    }

    fn lower_expr_to_place_with_borrow_adjust(
        &mut self,
        expr_id: ExprId,
        place: Place<'db>,
        current: BasicBlockId,
        rest: &[Adjustment],
        bk: BorrowKind,
    ) -> Result<'db, Option<BasicBlockId>> {
        let locals = self.result.locals.len();
        let Some((p, current)) =
            self.lower_expr_as_place_with_adjust(current, expr_id, true, rest)?
        else {
            return Ok(None);
        };
        // `CONST.len()` borrows a promoted constant just as `(&CONST).len()` does.
        if bk == BorrowKind::Shared
            && rest.is_empty()
            && p.local.into_raw().into_u32() as usize >= locals
            && p.projection.as_slice().is_empty()
            && self.is_promotable_unadjusted(expr_id)
        {
            self.result.promoted_locals.push((p.local, expr_id));
        }
        self.push_assignment(current, place, Rvalue::Ref(bk, p.store()), expr_id.into());
        Ok(Some(current))
    }

    fn lower_expr_to_place(
        &mut self,
        expr_id: ExprId,
        place: Place<'db>,
        prev_block: BasicBlockId,
    ) -> Result<'db, Option<BasicBlockId>> {
        // An expression is lowered within the one it is in: rustc's 5,204-arm `else if`.
        crate::next_solver::ensure_sufficient_stack(|| {
            if let Some(adjustments) = self.infer.expr_adjustments.get(&expr_id) {
                return self.lower_expr_to_place_with_adjust(
                    expr_id,
                    place,
                    prev_block,
                    adjustments,
                );
            }
            self.lower_expr_to_place_without_adjust(expr_id, place, prev_block)
        })
    }

    fn lower_expr_to_place_without_adjust(
        &mut self,
        expr_id: ExprId,
        place: Place<'db>,
        mut current: BasicBlockId,
    ) -> Result<'db, Option<BasicBlockId>> {
        match &self.store[expr_id] {
            Expr::OffsetOf(offset) => {
                let field = self.offset_of_place(offset, current, expr_id.into())?;
                // The field of a value at address 0 is at its offset.
                self.push_assignment(
                    current,
                    place,
                    Rvalue::Ref(BorrowKind::Shared, field.store()),
                    expr_id.into(),
                );
                Ok(Some(current))
            }
            // A register only written to has no value the block could leave in it, so such an
            // operand is lowered as assembly would be.
            Expr::InlineAsm(asm)
                if asm.comment_only
                    && !asm
                        .operands
                        .iter()
                        .any(|(_, it)| matches!(it, AsmOperand::Out { expr: Some(_), .. })) =>
            {
                // The block emits nothing, so a register it reads and writes keeps the value it
                // was given: the borrow checker sees the write to the variable as a mutation.
                for (_, operand) in &asm.operands {
                    let (input, output) = match *operand {
                        AsmOperand::InOut { expr, .. } => (expr, expr),
                        AsmOperand::SplitInOut { in_expr, out_expr: Some(out_expr), .. } => {
                            (in_expr, out_expr)
                        }
                        _ => continue,
                    };
                    let Some((value, next)) = self.lower_expr_to_some_operand(input, current)?
                    else {
                        return Ok(None);
                    };
                    let Some((place, next)) = self.lower_expr_as_place(next, output, true)? else {
                        return Ok(None);
                    };
                    current = next;
                    self.push_assignment(current, place, value.into(), expr_id.into());
                }
                Ok(Some(current))
            }
            Expr::InlineAsm(asm) => {
                let Some(template) = &asm.template else {
                    not_supported!("inline assembly whose template is not known");
                };
                let naked = match asm.kind {
                    hir_def::hir::InlineAsmKind::Asm => false,
                    hir_def::hir::InlineAsmKind::NakedAsm => true,
                    hir_def::hir::InlineAsmKind::GlobalAsm => {
                        not_supported!("`global_asm!` in a body")
                    }
                };
                let mut labels = Vec::new();
                let mut operands = Vec::with_capacity(asm.operands.len());
                for (_, operand) in &asm.operands {
                    let (reg, input, output) = match operand {
                        AsmOperand::In { reg, expr } => (reg, Some(*expr), None),
                        AsmOperand::Out { reg, expr, late: _ } => (reg, None, *expr),
                        AsmOperand::InOut { reg, expr, late: _ } => (reg, Some(*expr), Some(*expr)),
                        AsmOperand::SplitInOut { reg, in_expr, out_expr, late: _ } => {
                            (reg, Some(*in_expr), *out_expr)
                        }
                        AsmOperand::Const(value) => {
                            let Some((value, next)) =
                                self.lower_expr_to_some_operand(*value, current)?
                            else {
                                return Ok(None);
                            };
                            current = next;
                            operands.push(super::AsmOperand::Const(value));
                            continue;
                        }
                        AsmOperand::Label(block) => {
                            labels.push(*block);
                            operands.push(super::AsmOperand::Label);
                            continue;
                        }
                        AsmOperand::Sym(path) => {
                            let guard = self.resolver.update_to_inner_scope(
                                self.db,
                                self.store_owner,
                                expr_id,
                            );
                            let named = self.resolver.resolve_path_in_value_ns_fully(
                                self.db,
                                path,
                                HygieneId::ROOT,
                            );
                            self.resolver.reset_to_guard(guard);
                            let named = match named {
                                Some(ValueNs::StaticId(id)) => {
                                    Operand { kind: OperandKind::Static(id), span: None }
                                }
                                // FIXME: one with parameters, whose arguments inference
                                // does not record for a `sym`.
                                Some(ValueNs::FunctionId(func))
                                    if generics(self.db, func.into()).len(true) == 0 =>
                                {
                                    Operand::const_zst(Ty::new_fn_def(
                                        self.interner(),
                                        CallableDefId::FunctionId(func).into(),
                                        GenericArgs::empty(),
                                    ))
                                }
                                _ => not_supported!(
                                    "inline assembly with a `sym` that is not a static or a                                      function without parameters"
                                ),
                            };
                            operands.push(super::AsmOperand::Sym(named));
                            continue;
                        }
                    };
                    let input = match input {
                        Some(input) => {
                            let Some((input, next)) =
                                self.lower_expr_to_some_operand(input, current)?
                            else {
                                return Ok(None);
                            };
                            current = next;
                            Some(input)
                        }
                        None => None,
                    };
                    let output = match output {
                        Some(output) => {
                            let Some((output, next)) =
                                self.lower_expr_as_place(current, output, true)?
                            else {
                                return Ok(None);
                            };
                            current = next;
                            Some(output.store())
                        }
                        None => None,
                    };
                    operands.push(super::AsmOperand::Reg { reg: reg.clone(), input, output });
                }
                let taken: Option<Place<'db>> = if labels.is_empty() {
                    None
                } else {
                    let ty = Ty::new_uint(self.interner(), rustc_type_ir::UintTy::U64);
                    Some(self.temp(ty, current, expr_id.into())?.into())
                };
                self.push_statement(
                    current,
                    StatementKind::InlineAsm(Box::new(super::InlineAsm {
                        template: template.clone(),
                        operands: operands.into(),
                        options: asm.options,
                        taken: taken.map(|it| it.store()),
                        naked,
                    }))
                    .with_span(expr_id.into()),
                );
                let noreturn = naked || asm.options.contains(hir_def::hir::AsmOptions::NORETURN);
                let Some(taken) = taken else {
                    if noreturn {
                        self.set_terminator(current, TerminatorKind::Unreachable, expr_id.into());
                        return Ok(None);
                    }
                    return Ok(Some(current));
                };
                let end = self.new_basic_block();
                if noreturn {
                    self.set_terminator(end, TerminatorKind::Unreachable, expr_id.into());
                }
                let mut ends = (!noreturn).then_some(end);
                let mut targets = Vec::with_capacity(labels.len());
                for (at, label) in labels.into_iter().enumerate() {
                    let start = self.new_basic_block();
                    targets.push((at as u128 + 1, start));
                    let unit = self.temp(Ty::new_unit(self.interner()), start, label.into())?;
                    let label_end = self.lower_expr_to_place(label, unit.into(), start)?;
                    ends = self.merge_blocks(ends, label_end, expr_id.into());
                }
                self.set_terminator(
                    current,
                    TerminatorKind::SwitchInt {
                        discr: Operand {
                            kind: OperandKind::Copy(taken.store()),
                            span: Some(expr_id.into()),
                        },
                        targets: SwitchTargets::new(targets.into_iter(), end),
                    },
                    expr_id.into(),
                );
                Ok(ends)
            }
            Expr::Missing => {
                if let Some(f) = self.owner.as_function() {
                    let assoc = f.lookup(self.db);
                    if let ItemContainerId::TraitId(t) = assoc.container {
                        let name = &FunctionSignature::of(self.db, f).name;
                        return Err(MirLowerError::TraitFunctionDefinition(t, name.clone()));
                    }
                }
                Err(MirLowerError::IncompleteExpr)
            }
            Expr::Path(p) => {
                let pr =
                    if let Some((assoc, subst)) = self.infer.assoc_resolutions_for_expr(expr_id) {
                        match assoc {
                            CandidateId::ConstId(c) => {
                                self.lower_const(c.into(), current, place, subst, expr_id.into())?;
                                return Ok(Some(current));
                            }
                            CandidateId::FunctionId(_) => {
                                self.push_fn_item(current, place, expr_id);
                                return Ok(Some(current));
                            }
                        }
                    } else if let Some(variant) = self.infer.variant_resolution_for_expr(expr_id) {
                        match variant {
                            VariantId::EnumVariantId(e) => ValueNs::EnumVariantId(e),
                            VariantId::StructId(s) => ValueNs::StructId(s),
                            VariantId::UnionId(_) => implementation_error!("Union variant as path"),
                        }
                    } else {
                        let resolver_guard =
                            self.resolver.update_to_inner_scope(self.db, self.store_owner, expr_id);
                        let hygiene = self.store.expr_path_hygiene(expr_id);
                        let result = self
                            .resolver
                            .resolve_path_in_value_ns_fully(self.db, p, hygiene)
                            .ok_or_else(|| MirLowerError::UnresolvedName {
                                owner: self.owner.expression_store_owner(self.db),
                                path: p.clone(),
                            })?;
                        self.resolver.reset_to_guard(resolver_guard);
                        result
                    };
                match pr {
                    ValueNs::LocalBinding(_) | ValueNs::StaticId(_) => {
                        let Some((temp, current)) =
                            self.lower_expr_as_place_without_adjust(current, expr_id, false)?
                        else {
                            return Ok(None);
                        };
                        self.push_assignment(
                            current,
                            place,
                            Operand { kind: OperandKind::Copy(temp.store()), span: None }.into(),
                            expr_id.into(),
                        );
                        Ok(Some(current))
                    }
                    ValueNs::ConstId(const_id) => {
                        self.lower_const(
                            const_id.into(),
                            current,
                            place,
                            GenericArgs::empty(),
                            expr_id.into(),
                        )?;
                        Ok(Some(current))
                    }
                    ValueNs::EnumVariantId(variant_id) => {
                        let variant_fields = variant_id.fields(self.db);
                        if variant_fields.shape == FieldsShape::Unit {
                            let ty = self.infer.expr_ty(expr_id);
                            current = self.lower_enum_variant(
                                variant_id,
                                current,
                                place,
                                ty,
                                Box::new([]),
                                expr_id.into(),
                            )?;
                        } else {
                            // A tuple variant's constructor, a function item.
                            self.push_fn_item(current, place, expr_id);
                        }
                        Ok(Some(current))
                    }
                    ValueNs::GenericParam(p) => {
                        let def = self.owner.generic_def(self.db);
                        let generics = generics(self.db, def);
                        let index = generics.type_or_const_param_idx(p.into());
                        self.push_assignment(
                            current,
                            place,
                            Rvalue::from(Operand {
                                kind: OperandKind::Constant {
                                    konst: Const::new_param(
                                        self.interner(),
                                        ParamConst { id: p, index },
                                    )
                                    .store(),
                                    ty: self.db.const_param_ty(p).store(),
                                },
                                span: None,
                            }),
                            expr_id.into(),
                        );
                        Ok(Some(current))
                    }
                    ValueNs::FunctionId(_) | ValueNs::StructId(_) | ValueNs::ImplSelf(_) => {
                        // A unit struct is a value that has come to exist, and may have to be
                        // dropped.
                        self.push_fn_item(current, place, expr_id);
                        if let TyKind::Adt(adt, subst) = self.infer.expr_ty(expr_id).kind()
                            && let AdtId::StructId(id) = adt.def_id()
                        {
                            self.push_assignment(
                                current,
                                place,
                                Rvalue::Aggregate(
                                    AggregateKind::Adt(id.into(), subst.store()),
                                    Box::new([]),
                                ),
                                expr_id.into(),
                            );
                        }
                        Ok(Some(current))
                    }
                }
            }
            Expr::If { condition, then_branch, else_branch } => {
                // A literal condition, as in `if cfg!(..)`, stays a constant operand so that a
                // backend can leave out the branch that cannot run.
                let plain =
                    self.has_let(*condition) || matches!(self.store[*condition], Expr::Literal(_));
                // What `if let` binds, and the temporaries of what it matches on, are dropped
                // when the branch that was taken ends.
                let bindings = self.has_let(*condition).then(|| self.push_drop_scope());
                let branches = match self.store[*condition] {
                    Expr::Let { pat, expr } => {
                        Some(self.lower_if_let(*condition, pat, expr, current, false)?)
                    }
                    Expr::BinaryOp { .. } if self.has_let(*condition) => {
                        Some(self.lower_let_chain(*condition, current, false)?)
                    }
                    _ => None,
                };
                let condition_value = if let Some(branches) = branches {
                    branches.map(Either::Right)
                } else if plain {
                    self.lower_expr_to_some_operand(*condition, current)?.map(Either::Left)
                } else {
                    // The condition's temporaries are dropped before either branch runs.
                    let ty = Ty::new_bool(self.interner());
                    let result: Place<'db> = self.temp(ty, current, MirSpan::Unknown)?.into();
                    let scope = self.push_drop_scope();
                    let Some(c) = self.lower_expr_to_place(*condition, result, current)? else {
                        scope.pop_assume_dropped(self);
                        return Ok(None);
                    };
                    let current = scope.pop_and_drop(self, c, (*condition).into());
                    let discr = Operand {
                        kind: OperandKind::Copy(result.store()),
                        span: Some((*condition).into()),
                    };
                    Some(Either::Left((discr, current)))
                };
                let Some(condition_value) = condition_value else {
                    if let Some(bindings) = bindings {
                        bindings.pop_assume_dropped(self);
                    }
                    return Ok(None);
                };
                let (start_of_then, start_of_else) = match condition_value {
                    Either::Right(branches) => branches,
                    Either::Left((discr, current)) => {
                        let (start_of_then, start_of_else) =
                            (self.new_basic_block(), self.new_basic_block());
                        self.set_terminator(
                            current,
                            TerminatorKind::SwitchInt {
                                discr,
                                targets: SwitchTargets::static_if(1, start_of_then, start_of_else),
                            },
                            expr_id.into(),
                        );
                        (start_of_then, start_of_else)
                    }
                };
                let bound = bindings.is_some().then(|| self.drop_scopes.last().cloned()).flatten();
                let drop_bound = |this: &mut Self, mut block: BasicBlockId| {
                    if let Some(bound) = &bound {
                        this.emit_drop_and_storage_dead_for_scope(
                            bound,
                            &mut block,
                            (*condition).into(),
                        );
                    }
                    block
                };
                let end_of_then = self
                    .lower_branch_to_place(*then_branch, place, start_of_then)?
                    .map(|block| drop_bound(self, block));
                // From the 2024 edition on, the `else` branch runs after the drops.
                let early = self.edition().at_least_2024();
                let else_block =
                    if early { drop_bound(self, start_of_else) } else { start_of_else };
                let end_of_else = if let Some(else_branch) = else_branch {
                    self.lower_branch_to_place(*else_branch, place, else_block)?
                } else {
                    self.push_assign_unit(else_block, place, expr_id.into());
                    Some(else_block)
                };
                let end_of_else = if early {
                    end_of_else
                } else {
                    end_of_else.map(|block| drop_bound(self, block))
                };
                if let Some(bindings) = bindings {
                    bindings.pop_assume_dropped(self);
                }
                Ok(self.merge_blocks(end_of_then, end_of_else, expr_id.into()))
            }
            Expr::Let { pat, expr } => {
                let Some((cond_place, current)) = self.lower_expr_as_place(current, *expr, true)?
                else {
                    return Ok(None);
                };
                self.push_fake_read(current, cond_place, expr_id.into());
                let resolver_guard =
                    self.resolver.update_to_inner_scope(self.db, self.store_owner, expr_id);
                let (then_target, else_target) =
                    self.pattern_match(current, None, cond_place, *pat)?;
                self.resolver.reset_to_guard(resolver_guard);
                self.write_bytes_to_place(
                    then_target,
                    place,
                    Box::new([1]),
                    Ty::new_bool(self.interner()),
                    MirSpan::Unknown,
                )?;
                if let Some(else_target) = else_target {
                    self.write_bytes_to_place(
                        else_target,
                        place,
                        Box::new([0]),
                        Ty::new_bool(self.interner()),
                        MirSpan::Unknown,
                    )?;
                }
                Ok(self.merge_blocks(Some(then_target), else_target, expr_id.into()))
            }
            Expr::Block { id: _, statements, tail, label, unsafe_: _ } => {
                if let Some(label) = label {
                    self.lower_loop(current, place, Some(*label), expr_id.into(), |this, begin| {
                        if let Some(current) =
                            this.lower_block_to_place(statements, begin, *tail, place, expr_id)?
                        {
                            let end = this.current_loop_end()?;
                            this.set_goto(current, end, expr_id.into());
                        }
                        Ok(())
                    })
                } else {
                    self.lower_block_to_place(statements, current, *tail, place, expr_id)
                }
            }
            Expr::Loop { body, label, source: _ } => {
                self.lower_loop(current, place, *label, expr_id.into(), |this, begin| {
                    let scope = this.push_drop_scope();
                    if let Some((_, mut current)) = this.lower_expr_as_place(begin, *body, true)? {
                        current = scope.pop_and_drop(this, current, body.into());
                        this.set_goto(current, begin, expr_id.into());
                    } else {
                        scope.pop_assume_dropped(this);
                    }
                    Ok(())
                })
            }
            Expr::Call { callee, args, .. } => {
                if let Some((func_id, generic_args)) = self.infer.method_resolution(expr_id) {
                    let ty = Ty::new_fn_def(
                        self.interner(),
                        CallableDefId::FunctionId(func_id).into(),
                        generic_args,
                    );
                    let func = Operand::from_bytes(Box::default(), ty);
                    return self.lower_call_and_args(
                        func,
                        iter::once(*callee).chain(args.iter().copied()),
                        place,
                        current,
                        self.is_uninhabited(expr_id),
                        expr_id,
                    );
                }
                let callee_ty = self.expr_ty_after_adjustments(*callee);
                match callee_ty.kind() {
                    TyKind::FnDef(def, _)
                        if let CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) =
                            def.0 =>
                    {
                        self.lower_constructor(def.0, args, place, current, expr_id)
                    }
                    TyKind::FnDef(def, _) => {
                        let func = Operand::from_bytes(Box::default(), callee_ty);
                        // `#[rustc_legacy_const_generics]`: those are generic arguments.
                        let consts = match def.0 {
                            CallableDefId::FunctionId(func) => FunctionSignature::of(self.db, func)
                                .legacy_const_generics_indices(self.db, func)
                                .filter(|consts| args.len() > consts.len()),
                            _ => None,
                        }
                        .unwrap_or_default();
                        let args = args
                            .iter()
                            .enumerate()
                            .filter(|(idx, _)| !consts.contains(&(*idx as u32)))
                            .map(|(_, &arg)| arg);
                        self.lower_call_and_args(
                            func,
                            args,
                            place,
                            current,
                            self.is_uninhabited(expr_id),
                            expr_id,
                        )
                    }
                    TyKind::FnPtr(..) => {
                        let Some((func, current)) =
                            self.lower_expr_to_some_operand(*callee, current)?
                        else {
                            return Ok(None);
                        };
                        self.lower_call_and_args(
                            func,
                            args.iter().copied(),
                            place,
                            current,
                            self.is_uninhabited(expr_id),
                            expr_id,
                        )
                    }
                    TyKind::Closure(_, _) => {
                        not_supported!(
                            "method resolution not emitted for closure (Are Fn traits available?)"
                        );
                    }
                    TyKind::Error(_) => {
                        Err(MirLowerError::MissingFunctionDefinition(self.owner, expr_id))
                    }
                    _ => Err(MirLowerError::TypeError("function call on bad type")),
                }
            }
            Expr::MethodCall { receiver, args, method_name, .. } => {
                let (func_id, generic_args) =
                    self.infer.method_resolution(expr_id).ok_or_else(|| {
                        MirLowerError::UnresolvedMethod(
                            method_name.display(self.db, self.edition()).to_string(),
                        )
                    })?;
                let func = Operand::from_fn(self.db, func_id, generic_args);
                self.lower_call_and_args(
                    func,
                    iter::once(*receiver).chain(args.iter().copied()),
                    place,
                    current,
                    self.is_uninhabited(expr_id),
                    expr_id,
                )
            }
            Expr::Match { expr, arms } => {
                // The temporaries of what a `for` iterates over are dropped where the loop
                // ends, wherever the loop is written.
                let temporaries = self.is_for_loop(arms).then(|| self.push_drop_scope());
                let Some((cond_place, mut current)) =
                    self.lower_expr_as_place(current, *expr, true)?
                else {
                    if let Some(temporaries) = temporaries {
                        temporaries.pop_assume_dropped(self);
                    }
                    return Ok(None);
                };
                self.push_fake_read(current, cond_place, expr_id.into());
                let mut end = None;
                let resolver_guard =
                    self.resolver.update_to_inner_scope(self.db, self.store_owner, expr_id);
                let reached = self.reached_arms(arms);
                for (index, (MatchArm { pat, guard, expr }, reached)) in
                    arms.iter().zip(reached).enumerate()
                {
                    // Each way an arm can match is linked to the next by a false edge, but for
                    // the last way of the last arm.
                    let last_arm = index + 1 == arms.len();
                    // When the guard fails, rustc goes on to the next way the or-patterns of
                    // the arm can match, and runs the guard again for it.
                    let expansions = match guard {
                        Some(_) => self.or_expansions(*pat),
                        None => Vec::new(),
                    };
                    // What the arm binds is dropped at the end of the arm.
                    let arm = self.push_drop_scope();
                    if let &Some(guard) = guard
                        && (2..=64).contains(&expansions.len())
                    {
                        let body = self.new_basic_block();
                        let count = expansions.len();
                        for (expansion, choice) in expansions.into_iter().enumerate() {
                            self.or_choices = choice.into_iter().collect();
                            let outer = self.guarded_moves.replace(Vec::new());
                            self.arm_unreached = !reached;
                            let matched = match last_arm && expansion + 1 == count {
                                true => self.pattern_match(current, None, cond_place, *pat),
                                false => self
                                    .pattern_match_linked(current, cond_place, *pat)
                                    .map(|(then, otherwise)| (then, Some(otherwise))),
                            };
                            let moves = std::mem::replace(&mut self.guarded_moves, outer);
                            self.or_choices.clear();
                            let (then, otherwise) = matched?;
                            let otherwise = otherwise.unwrap_or_else(|| self.new_basic_block());
                            let bound = self.drop_scopes.last().map_or(0, |it| it.locals.len());
                            let guarded = self.lower_guard(guard, then, expr_id.into());
                            self.guard_refs.clear();
                            if let Some((held, failed)) = guarded? {
                                let failed = self.guard_failed(failed, bound, guard.into());
                                self.set_goto(failed, otherwise, expr_id.into());
                                self.guard_held(held, moves.unwrap_or_default(), bound)?;
                                self.set_goto(held, body, expr_id.into());
                            }
                            current = otherwise;
                        }
                        if let Some(block) = self.lower_expr_to_place(*expr, place, body)? {
                            let block = arm.pop_and_drop(self, block, expr_id.into());
                            let r = end.get_or_insert_with(|| self.new_basic_block());
                            self.set_goto(block, *r, expr_id.into());
                        } else {
                            arm.pop_assume_dropped(self);
                        }
                        continue;
                    }
                    let outer = match guard {
                        Some(_) => self.guarded_moves.replace(Vec::new()),
                        None => self.guarded_moves.take(),
                    };
                    self.arm_unreached = !reached;
                    let matched = match last_arm {
                        true => self.pattern_match(current, None, cond_place, *pat),
                        false => self
                            .pattern_match_linked(current, cond_place, *pat)
                            .map(|(then, otherwise)| (then, Some(otherwise))),
                    };
                    let moves = std::mem::replace(&mut self.guarded_moves, outer);
                    let (then, mut otherwise) = matched?;
                    let then = if let &Some(guard) = guard {
                        let next = self.new_basic_block();
                        let o = *otherwise.get_or_insert_with(|| self.new_basic_block());
                        let bound = self.drop_scopes.last().map_or(0, |it| it.locals.len());
                        let guarded = self.lower_guard(guard, then, expr_id.into());
                        self.guard_refs.clear();
                        if let Some((held, failed)) = guarded? {
                            let failed = self.guard_failed(failed, bound, guard.into());
                            self.set_goto(failed, o, expr_id.into());
                            self.guard_held(held, moves.unwrap_or_default(), bound)?;
                            self.set_goto(held, next, expr_id.into());
                        }
                        next
                    } else {
                        then
                    };
                    if let Some(block) = self.lower_expr_to_place(*expr, place, then)? {
                        let block = arm.pop_and_drop(self, block, expr_id.into());
                        let r = end.get_or_insert_with(|| self.new_basic_block());
                        self.set_goto(block, *r, expr_id.into());
                    } else {
                        arm.pop_assume_dropped(self);
                    }
                    match otherwise {
                        Some(o) => current = o,
                        // The last arm, which cannot fail.
                        None => break,
                    }
                }
                self.resolver.reset_to_guard(resolver_guard);
                if self.is_unterminated(current) {
                    self.set_terminator(current, TerminatorKind::Unreachable, expr_id.into());
                }
                match (temporaries, end) {
                    (Some(temporaries), Some(end)) => {
                        Ok(Some(temporaries.pop_and_drop(self, end, expr_id.into())))
                    }
                    (Some(temporaries), None) => {
                        temporaries.pop_assume_dropped(self);
                        Ok(None)
                    }
                    (None, end) => Ok(end),
                }
            }
            Expr::Continue { label } => {
                let loop_data = match label {
                    Some(l) => {
                        self.labeled_loop_blocks.get(l).ok_or(MirLowerError::UnresolvedLabel)?
                    }
                    None => self
                        .current_loop_blocks
                        .as_ref()
                        .ok_or(MirLowerError::ContinueWithoutLoop)?,
                };
                let (begin, scope) = (loop_data.begin, loop_data.drop_scope_index);
                self.exit_to(scope, current, begin, expr_id.into());
                Ok(None)
            }
            &Expr::Break { expr, label } => {
                let loop_data = match label {
                    Some(l) => {
                        self.labeled_loop_blocks.get(&l).ok_or(MirLowerError::UnresolvedLabel)?
                    }
                    None => {
                        self.current_loop_blocks.as_ref().ok_or(MirLowerError::BreakWithoutLoop)?
                    }
                };
                let loop_place = loop_data.place.as_ref();
                match expr {
                    Some(expr) => {
                        let Some(c) = self.lower_expr_to_place(expr, loop_place, current)? else {
                            return Ok(None);
                        };
                        current = c;
                    }
                    None => self.push_assign_unit(current, loop_place, expr_id.into()),
                }
                let (end, drop_scope) = match label {
                    Some(l) => {
                        let loop_blocks = self
                            .labeled_loop_blocks
                            .get(&l)
                            .ok_or(MirLowerError::UnresolvedLabel)?;
                        (
                            loop_blocks.end.expect("We always generate end for labeled loops"),
                            loop_blocks.drop_scope_index,
                        )
                    }
                    None => (
                        self.current_loop_end()?,
                        self.current_loop_blocks.as_ref().unwrap().drop_scope_index,
                    ),
                };
                self.exit_to(drop_scope, current, end, expr_id.into());
                Ok(None)
            }
            Expr::Return { expr } => {
                if let Some(expr) = expr {
                    if let Some(c) =
                        self.lower_expr_to_place(*expr, return_slot().into(), current)?
                    {
                        current = c;
                    } else {
                        return Ok(None);
                    }
                } else {
                    self.push_assign_unit(current, return_slot().into(), expr_id.into());
                }
                let returned = match self.returned {
                    Some(it) => it,
                    None => {
                        let block = self.new_basic_block();
                        self.set_terminator(block, TerminatorKind::Return, expr_id.into());
                        *self.returned.insert(block)
                    }
                };
                self.exit_to(0, current, returned, expr_id.into());
                Ok(None)
            }
            Expr::Become { .. } => not_supported!("tail-calls"),
            Expr::Yield { .. } => not_supported!("yield"),
            Expr::RecordLit { fields, path, spread, .. } => {
                let variant_id =
                    self.infer.variant_resolution_for_expr(expr_id).ok_or_else(|| {
                        MirLowerError::UnresolvedName {
                            owner: self.owner.expression_store_owner(self.db),
                            path: path.clone(),
                        }
                    })?;
                let subst = match self.expr_ty_without_adjust(expr_id).kind() {
                    TyKind::Adt(_, s) => s,
                    _ => not_supported!("Non ADT record literal"),
                };
                let variant_fields = variant_id.fields(self.db);
                match variant_id {
                    VariantId::EnumVariantId(_) | VariantId::StructId(_) => {
                        let mut operands = vec![None; variant_fields.fields().len()];
                        let declared = self.declared_locals();
                        for RecordLitField { name, expr } in fields.iter() {
                            let field_id =
                                variant_fields.field(name).ok_or(MirLowerError::UnresolvedField)?;
                            let Some((op, c)) = self.lower_expr_to_some_operand(*expr, current)?
                            else {
                                return Ok(None);
                            };
                            current = c;
                            // What unwinds from the next field drops this one first.
                            self.declare_operands_last(expr_id, declared, std::iter::once(&op));
                            operands[u32::from(field_id.into_raw()) as usize] = Some(op);
                        }
                        self.declare_operands_last(expr_id, declared, operands.iter().flatten());
                        // The fields are evaluated in the order written, and the base after
                        // them.
                        let spread_place = match *spread {
                            RecordSpread::Expr(it) => {
                                let Some((p, c)) = self.lower_expr_as_place(current, it, true)?
                                else {
                                    return Ok(None);
                                };
                                current = c;
                                Some(p)
                            }
                            RecordSpread::None => None,
                            RecordSpread::FieldDefaults => not_supported!("empty record spread"),
                        };
                        let rvalue = Rvalue::Aggregate(
                            AggregateKind::Adt(variant_id, subst.store()),
                            match spread_place {
                                Some(sp) if let VariantId::StructId(_) = variant_id => operands
                                    .into_iter()
                                    .enumerate()
                                    .map(|(i, it)| match it {
                                        Some(it) => it,
                                        None => {
                                            let p = sp.project(ProjectionElem::Field(FieldIndex(
                                                i as u32,
                                            )));
                                            Operand {
                                                kind: OperandKind::Copy(p.store()),
                                                span: None,
                                            }
                                        }
                                    })
                                    .collect(),
                                Some(_) => {
                                    return Err(MirLowerError::TypeError(
                                        "functional record update syntax requires a struct",
                                    ));
                                }
                                None => operands.into_iter().collect::<Option<_>>().ok_or(
                                    MirLowerError::TypeError("missing field in record literal"),
                                )?,
                            },
                        );
                        self.push_assignment(current, place, rvalue, expr_id.into());
                        Ok(Some(current))
                    }
                    VariantId::UnionId(_union_id) => {
                        let [RecordLitField { name, expr }] = fields.as_ref() else {
                            not_supported!("Union record literal with more than one field");
                        };
                        let local_id =
                            variant_fields.field(name).ok_or(MirLowerError::UnresolvedField)?;
                        let place = place.project(PlaceElem::Field(local_id.into()));
                        self.lower_expr_to_place(*expr, place, current)
                    }
                }
            }
            &Expr::Await { expr } => self.lower_await(expr, expr_id, place, current),
            Expr::Yeet { .. } => not_supported!("yeet"),
            Expr::Const(_) if let Some(konst) = self.inline_const(expr_id) => {
                let subst = GenericArgs::identity_for_item(
                    self.interner(),
                    self.owner.generic_def(self.db).into(),
                );
                self.lower_const(konst.into(), current, place, subst, expr_id.into())?;
                Ok(Some(current))
            }
            &Expr::Const(id) => {
                // Inline const blocks (`const { .. }`) are stored with their inner expression in
                // the same body (see inference, which infers the inner expression directly), so we
                // lower that expression in place. Const-ness is irrelevant here: MIR evaluation
                // already runs in a const context.
                // The block owns its bindings, so the enclosing body did not pick them: they
                // are declared where the block binds them.
                for (binding, _) in self.store.bindings() {
                    if self.store.binding_owner(binding) == Some(expr_id)
                        && !self.result.binding_locals.contains_idx(binding)
                    {
                        self.undeclared.insert(binding);
                    }
                }
                self.inline_consts += 1;
                let lowered = self.lower_expr_to_place(id, place, current);
                self.inline_consts -= 1;
                lowered
            }
            Expr::Cast { expr, type_ref: _ } => {
                // Since we don't have THIR, this is the "zipped" version of [rustc's HIR lowering](https://github.com/rust-lang/rust/blob/e71f9529121ca8f687e4b725e3c9adc3f1ebab4d/compiler/rustc_mir_build/src/thir/cx/expr.rs#L165-L178)
                // and [THIR lowering as RValue](https://github.com/rust-lang/rust/blob/a4601859ae3875732797873612d424976d9e3dd0/compiler/rustc_mir_build/src/build/expr/as_rvalue.rs#L193-L313)
                let (rvalue, current) = if self.infer.coercion_casts.contains(expr) {
                    let Some((it, current)) = self.lower_expr_to_some_operand(*expr, current)?
                    else {
                        return Ok(None);
                    };
                    (Rvalue::Use(it), current)
                } else {
                    let source_ty = self.infer.expr_ty(*expr);
                    let target_ty = self.infer.expr_ty(expr_id);
                    // A variant named as a value casts to its discriminant; one named as
                    // its constructor function is a function like any other.
                    let (it, source_ty, current) = if let Some(VariantId::EnumVariantId(variant_id)) =
                        self.infer.variant_resolution_for_expr(*expr)
                        && !matches!(source_ty.kind(), TyKind::FnDef(..))
                    {
                        self.lower_variant_discriminant(current, variant_id)?
                    } else if let TyKind::Adt(adt, _) = source_ty.kind()
                        && adt.is_enum()
                    {
                        let Some((enum_place, current)) =
                            self.lower_expr_as_place(current, *expr, true)?
                        else {
                            return Ok(None);
                        };
                        let discr_ty = Ty::new_int(self.interner(), rustc_type_ir::IntTy::I128);
                        let discr_place: Place<'db> =
                            self.temp(discr_ty, current, expr_id.into())?.into();

                        self.push_assignment(
                            current,
                            discr_place,
                            Rvalue::Discriminant(enum_place.store()),
                            expr_id.into(),
                        );
                        (
                            Operand {
                                kind: OperandKind::Copy(discr_place.store()),
                                span: Some(expr_id.into()),
                            },
                            discr_ty,
                            current,
                        )
                    } else {
                        let Some((it, current)) =
                            self.lower_expr_to_some_operand(*expr, current)?
                        else {
                            return Ok(None);
                        };
                        // The operand carries the cast's coercion, e.g. fn item to pointer.
                        (it, self.expr_ty_after_adjustments(*expr), current)
                    };
                    // `f as *const ()` and `f as usize` go through a function pointer.
                    let (it, source_ty) = if let TyKind::FnDef(..) = source_ty.kind()
                        && !matches!(target_ty.kind(), TyKind::FnPtr(..))
                    {
                        let pointer_ty =
                            Ty::new_fn_ptr(self.interner(), source_ty.fn_sig(self.interner()));
                        let pointer: Place<'db> =
                            self.temp(pointer_ty, current, expr_id.into())?.into();
                        self.push_assignment(
                            current,
                            pointer,
                            Rvalue::Cast(
                                CastKind::PointerCoercion(PointerCast::ReifyFnPointer),
                                it,
                                pointer_ty.store(),
                            ),
                            expr_id.into(),
                        );
                        let operand = Operand {
                            kind: OperandKind::Copy(pointer.store()),
                            span: Some(expr_id.into()),
                        };
                        (operand, pointer_ty)
                    } else {
                        (it, source_ty)
                    };
                    let cast_kind = if source_ty.as_reference().is_some() {
                        CastKind::PointerCoercion(PointerCast::ArrayToPointer)
                    } else {
                        cast_kind(self.db, source_ty, target_ty)?
                    };
                    (Rvalue::Cast(cast_kind, it, target_ty.store()), current)
                };
                self.push_assignment(current, place, rvalue, expr_id.into());
                Ok(Some(current))
            }
            Expr::Ref { expr, rawness: _, mutability } => {
                let locals = self.result.locals.len();
                let Some((p, current)) = self.lower_expr_as_place(current, *expr, true)? else {
                    return Ok(None);
                };
                // A mutable borrow is promoted only of an empty array, as rustc's `&mut []`.
                if (mutability.is_shared() || self.is_empty_array(*expr))
                    && p.local.into_raw().into_u32() as usize >= locals
                    // Behind a deref the local is a pointer, an overloaded index's result say,
                    // and not the value.
                    && !p.projection.as_slice().iter().any(|it| matches!(it, ProjectionElem::Deref))
                    // What a `const { .. }` block borrows is a constant whatever it is made of.
                    && (self.inline_consts > 0 || self.is_promotable_place(*expr))
                {
                    // The local holds what the fields and elements are taken out of.
                    let mut constant = *expr;
                    while let Expr::Field { expr: base, .. } | Expr::Index { base, .. } =
                        self.store[constant]
                    {
                        constant = base;
                    }
                    self.result.promoted_locals.push((p.local, constant));
                }
                let bk = BorrowKind::from_hir_mutability(*mutability);
                self.push_assignment(current, place, Rvalue::Ref(bk, p.store()), expr_id.into());
                Ok(Some(current))
            }
            Expr::Field { .. }
            | Expr::Index { .. }
            | Expr::UnaryOp { op: hir_def::hir::UnaryOp::Deref, .. } => {
                let Some((p, current)) =
                    self.lower_expr_as_place_without_adjust(current, expr_id, true)?
                else {
                    return Ok(None);
                };
                self.push_assignment(
                    current,
                    place,
                    Operand { kind: OperandKind::Copy(p.store()), span: None }.into(),
                    expr_id.into(),
                );
                Ok(Some(current))
            }
            Expr::UnaryOp {
                expr,
                op: op @ (hir_def::hir::UnaryOp::Not | hir_def::hir::UnaryOp::Neg),
            } => {
                let is_builtin = matches!(
                    self.expr_ty_without_adjust(*expr).kind(),
                    TyKind::Bool | TyKind::Int(_) | TyKind::Uint(_) | TyKind::Float(_)
                );
                if !is_builtin
                    && let Some((func_id, generic_args)) = self.infer.method_resolution(expr_id)
                {
                    let func = Operand::from_fn(self.db, func_id, generic_args);
                    return self.lower_call_and_args(
                        func,
                        [*expr].into_iter(),
                        place,
                        current,
                        self.is_uninhabited(expr_id),
                        expr_id,
                    );
                }
                // `-128i8` is that number, not the negation of one `i8` does not have.
                if let hir_def::hir::UnaryOp::Neg = op
                    && !self.has_adjustments(*expr)
                    && let Expr::Literal(literal @ hir_def::hir::Literal::Int(..)) =
                        &self.store[*expr]
                    && let Some(negated) = literal.clone().negate()
                {
                    let ty = self.expr_ty_without_adjust(*expr);
                    let mut operand = self.lower_literal_to_operand(ty, &negated)?;
                    operand.span = Some(expr_id.into());
                    self.push_assignment(current, place, Rvalue::Use(operand), expr_id.into());
                    return Ok(Some(current));
                }
                let Some((operand, current)) = self.lower_expr_to_some_operand(*expr, current)?
                else {
                    return Ok(None);
                };
                let operation = match op {
                    hir_def::hir::UnaryOp::Not => UnOp::Not,
                    hir_def::hir::UnaryOp::Neg => UnOp::Neg,
                    _ => unreachable!(),
                };
                self.push_assignment(
                    current,
                    place,
                    Rvalue::UnaryOp(operation, operand),
                    expr_id.into(),
                );
                Ok(Some(current))
            }
            Expr::BinaryOp { lhs, rhs, op } => {
                let op: BinaryOp = op.ok_or(MirLowerError::IncompleteExpr)?;
                let is_builtin = 'b: {
                    // Without adjust here is a hack. We assume that we know every possible adjustment
                    // for binary operator, and use without adjust to simplify our conditions.
                    let lhs_ty = self.expr_ty_without_adjust(*lhs);
                    let rhs_ty = self.expr_ty_without_adjust(*rhs);
                    // Every comparison of raw pointers is builtin. Treating `<` as a method call
                    // makes core's `impl PartialOrd for *const T` call itself.
                    if matches!(op, BinaryOp::CmpOp(_))
                        && matches!(lhs_ty.kind(), TyKind::RawPtr(..))
                        && matches!(rhs_ty.kind(), TyKind::RawPtr(..))
                    {
                        break 'b true;
                    }
                    let builtin_inequal_impls = matches!(
                        op,
                        BinaryOp::ArithOp(ArithOp::Shl | ArithOp::Shr)
                            | BinaryOp::Assignment { op: Some(ArithOp::Shl | ArithOp::Shr) }
                    );
                    matches!(
                        lhs_ty.kind(),
                        TyKind::Bool
                            | TyKind::Char
                            | TyKind::Int(_)
                            | TyKind::Uint(_)
                            | TyKind::Float(_)
                    ) && matches!(
                        rhs_ty.kind(),
                        TyKind::Bool
                            | TyKind::Char
                            | TyKind::Int(_)
                            | TyKind::Uint(_)
                            | TyKind::Float(_)
                    ) && (lhs_ty == rhs_ty || builtin_inequal_impls)
                };
                // rustc's typeck drops the autoref it records for the operands of a comparison
                // of scalars (writeback's `fix_scalar_builtin_expr`): the values are compared.
                let compares_scalars = is_builtin && matches!(op, BinaryOp::CmpOp(_));
                if !is_builtin
                    && let Some((func_id, generic_args)) = self.infer.method_resolution(expr_id)
                {
                    let func = Operand::from_fn(self.db, func_id, generic_args);
                    return self.lower_call_and_args(
                        func,
                        [*lhs, *rhs].into_iter(),
                        place,
                        current,
                        self.is_uninhabited(expr_id),
                        expr_id,
                    );
                }
                if let hir_def::hir::BinaryOp::Assignment { op: Some(op) } = op {
                    // last adjustment is `&mut` which we don't want it.
                    let adjusts = match self.infer.expr_adjustments.get(lhs) {
                        Some(it) if let Some((_, rest)) = it.split_last() => rest,
                        // A scalar's operator is not looked up while its type is still an
                        // integer variable (`let mut n = 0; n += i;`), so there is no `&mut`
                        // to take off: the place is the operand.
                        _ if is_builtin => &[],
                        _ => {
                            return Err(MirLowerError::TypeError(
                                "adjustment of binary op was missing",
                            ));
                        }
                    };
                    // rustc evaluates the right-hand side of a built-in compound assignment
                    // before the place it assigns to.
                    let Some((rhs_op, current)) = self.lower_expr_to_some_operand(*rhs, current)?
                    else {
                        return Ok(None);
                    };
                    let Some((lhs_place, current)) =
                        self.lower_expr_as_place_with_adjust(current, *lhs, false, adjusts)?
                    else {
                        return Ok(None);
                    };
                    let r_value = Rvalue::CheckedBinaryOp(
                        op.into(),
                        Operand { kind: OperandKind::Copy(lhs_place.store()), span: None },
                        rhs_op,
                    );
                    self.push_assignment(current, lhs_place, r_value, expr_id.into());
                    self.push_assign_unit(current, place, expr_id.into());
                    return Ok(Some(current));
                }
                // Each side of `&&` and `||` drops its temporaries when it is done, unless it
                // is a `let`, whose temporaries live as long as what it binds.
                let lazy = matches!(op, hir_def::hir::BinaryOp::LogicOp(_));
                let (lhs_op, current) = if lazy && !self.has_let(*lhs) {
                    let ty = Ty::new_bool(self.interner());
                    let result: Place<'db> = self.temp(ty, current, MirSpan::Unknown)?.into();
                    let scope = self.push_drop_scope();
                    self.drop_scopes.last_mut().unwrap().temporaries = true;
                    let Some(c) = self.lower_expr_to_place(*lhs, result, current)? else {
                        scope.pop_assume_dropped(self);
                        return Ok(None);
                    };
                    let current = scope.pop_and_drop(self, c, (*lhs).into());
                    let operand = Operand {
                        kind: OperandKind::Copy(result.store()),
                        span: Some((*lhs).into()),
                    };
                    (operand, current)
                } else if compares_scalars {
                    let Some(it) = self.lower_scalar_operand(*lhs, current)? else {
                        return Ok(None);
                    };
                    it
                } else {
                    let Some(it) = self.lower_expr_to_some_operand(*lhs, current)? else {
                        return Ok(None);
                    };
                    it
                };
                if let hir_def::hir::BinaryOp::LogicOp(op) = op {
                    let value_to_short = match op {
                        syntax::ast::LogicOp::And => 0,
                        syntax::ast::LogicOp::Or => 1,
                    };
                    let start_of_then = self.new_basic_block();
                    self.push_assignment(
                        start_of_then,
                        place,
                        lhs_op.clone().into(),
                        expr_id.into(),
                    );
                    let end_of_then = Some(start_of_then);
                    let start_of_else = self.new_basic_block();
                    let end_of_else = if self.has_let(*rhs) {
                        self.lower_expr_to_place(*rhs, place, start_of_else)?
                    } else {
                        let scope = self.push_drop_scope();
                        self.drop_scopes.last_mut().unwrap().temporaries = true;
                        match self.lower_expr_to_place(*rhs, place, start_of_else)? {
                            Some(block) => Some(scope.pop_and_drop(self, block, (*rhs).into())),
                            None => {
                                scope.pop_assume_dropped(self);
                                None
                            }
                        }
                    };
                    self.set_terminator(
                        current,
                        TerminatorKind::SwitchInt {
                            discr: lhs_op,
                            targets: SwitchTargets::static_if(
                                value_to_short,
                                start_of_then,
                                start_of_else,
                            ),
                        },
                        expr_id.into(),
                    );
                    return Ok(self.merge_blocks(end_of_then, end_of_else, expr_id.into()));
                }
                let rhs_op = match compares_scalars {
                    true => self.lower_scalar_operand(*rhs, current)?,
                    false => self.lower_expr_to_some_operand(*rhs, current)?,
                };
                let Some((rhs_op, current)) = rhs_op else {
                    return Ok(None);
                };
                self.push_assignment(
                    current,
                    place,
                    Rvalue::CheckedBinaryOp(
                        match op {
                            hir_def::hir::BinaryOp::LogicOp(op) => match op {
                                hir_def::hir::LogicOp::And => BinOp::BitAnd, // FIXME: make these short circuit
                                hir_def::hir::LogicOp::Or => BinOp::BitOr,
                            },
                            hir_def::hir::BinaryOp::ArithOp(op) => BinOp::from(op),
                            hir_def::hir::BinaryOp::CmpOp(op) => BinOp::from(op),
                            hir_def::hir::BinaryOp::Assignment { .. } => unreachable!(), // handled above
                        },
                        lhs_op,
                        rhs_op,
                    ),
                    expr_id.into(),
                );
                Ok(Some(current))
            }
            &Expr::Assignment { target, value: rhs } => {
                // What the pattern takes of a place is all that is moved out of it, as a
                // `let` of the pattern moves: `_ = place` moves nothing.
                let Some((value, mut current)) = self.lower_expr_as_place(current, rhs, true)?
                else {
                    return Ok(None);
                };
                self.push_fake_read(current, value, expr_id.into());
                let resolver_guard =
                    self.resolver.update_to_inner_scope(self.db, self.store_owner, expr_id);
                current = self.pattern_match_assignment(current, value, rhs.into(), target)?;
                self.resolver.reset_to_guard(resolver_guard);
                self.push_assign_unit(current, place, expr_id.into());
                Ok(Some(current))
            }
            Expr::Closure {
                closure_kind:
                    ClosureKind::Closure
                    | ClosureKind::Coroutine { kind: CoroutineKind::Async, .. }
                    | ClosureKind::CoroutineClosure(CoroutineKind::Async),
                ..
            } => {
                let ty = self.expr_ty_without_adjust(expr_id);
                match ty.kind() {
                    TyKind::Closure(id, _) => self.result.closures.push(id.0),
                    TyKind::CoroutineClosure(id, _) => {
                        self.result.closures.push(id.0.as_closure(self.db))
                    }
                    TyKind::Coroutine(..) => (),
                    _ => not_supported!("closure with non closure type"),
                }
                let Some(closure_data) = self.infer.closures_data.get(&expr_id) else {
                    not_supported!("closure without capture analysis");
                };

                let span = |sources: &[CaptureSourceStack]| match sources
                    .first()
                    .map(|it| it.final_source().unpack())
                {
                    Some(ExprOrPatId::ExprId(it)) => it.into(),
                    Some(ExprOrPatId::PatId(it)) => it.into(),
                    None => MirSpan::Unknown,
                };
                let convert_place = |this: &mut Self, place: &HirPlace| {
                    let (HirPlaceBase::Local(local) | HirPlaceBase::Upvar { var_id: local, .. }) =
                        place.base
                    else {
                        not_supported!("non-local capture");
                    };
                    // A binding a guard reads through a reference is captured through it.
                    let (local, deref) = match this.guard_refs.get(&local) {
                        Some(&reference) => (reference, Some(ProjectionElem::Deref)),
                        None => (this.binding_local(local)?, None),
                    };
                    Ok(StoredPlace {
                        local,
                        projection: Projection::new_from_iter(
                            deref
                                .into_iter()
                                .chain(convert_closure_capture_projections(self.db, place)),
                        )
                        .store(),
                    })
                };

                for (place, _, sources) in &closure_data.fake_reads {
                    let p = convert_place(self, place)?;
                    self.push_fake_read(current, p.as_ref(), span(sources));
                }

                let captures = closure_data.min_captures.values().flatten();
                let mut operands = vec![];
                for capture in captures {
                    let p = convert_place(self, &capture.place)?;
                    match capture.info.capture_kind {
                        UpvarCapture::ByRef(bk) => {
                            let tmp_ty = capture.captured_ty(self.db);
                            // FIXME: Handle more than one span.
                            let capture_span = span(&capture.info.sources);
                            let tmp = self.temp(tmp_ty, current, capture_span)?.into();
                            self.push_assignment(
                                current,
                                tmp,
                                Rvalue::Ref(BorrowKind::from_hir(bk), p),
                                capture_span,
                            );
                            operands
                                .push(Operand { kind: OperandKind::Move(tmp.store()), span: None });
                        }
                        UpvarCapture::ByValue => {
                            operands.push(Operand { kind: OperandKind::Move(p), span: None })
                        }
                        UpvarCapture::ByUse => not_supported!("capture by use"),
                    }
                }
                self.push_assignment(
                    current,
                    place,
                    Rvalue::Aggregate(AggregateKind::Closure(ty.store()), operands.into()),
                    expr_id.into(),
                );
                Ok(Some(current))
            }
            Expr::Closure { closure_kind, .. } => not_supported!("{closure_kind:?} closure"),
            Expr::Tuple { exprs } => {
                let declared = self.declared_locals();
                let Some(values): Option<Box<[Operand]>> = exprs
                    .iter()
                    .map(|it| {
                        let Some((o, c)) = self.lower_expr_to_some_operand(*it, current)? else {
                            return Ok(None);
                        };
                        current = c;
                        // What unwinds from the next operand drops this one first.
                        self.declare_operands_last(expr_id, declared, std::iter::once(&o));
                        Ok(Some(o))
                    })
                    .collect::<Result<'_, Option<_>>>()?
                else {
                    return Ok(None);
                };
                self.declare_operands_last(expr_id, declared, values.iter());
                let r = Rvalue::Aggregate(
                    AggregateKind::Tuple(self.expr_ty_without_adjust(expr_id).store()),
                    values,
                );
                self.push_assignment(current, place, r, expr_id.into());
                Ok(Some(current))
            }
            Expr::Array(l) => match l {
                Array::ElementList { elements, .. } => {
                    let elem_ty = match self.expr_ty_without_adjust(expr_id).kind() {
                        TyKind::Array(ty, _) => ty,
                        _ => {
                            return Err(MirLowerError::TypeError(
                                "Array expression with non array type",
                            ));
                        }
                    };
                    let declared = self.declared_locals();
                    let Some(values): Option<Box<[Operand]>> = elements
                        .iter()
                        .map(|it| {
                            let Some((o, c)) = self.lower_expr_to_some_operand(*it, current)?
                            else {
                                return Ok(None);
                            };
                            current = c;
                            // What unwinds from the next element drops this one first.
                            self.declare_operands_last(expr_id, declared, std::iter::once(&o));
                            Ok(Some(o))
                        })
                        .collect::<Result<'_, Option<_>>>()?
                    else {
                        return Ok(None);
                    };
                    self.declare_operands_last(expr_id, declared, values.iter());
                    let r = Rvalue::Aggregate(AggregateKind::Array(elem_ty.store()), values);
                    self.push_assignment(current, place, r, expr_id.into());
                    Ok(Some(current))
                }
                Array::Repeat { initializer, .. } => {
                    let TyKind::Array(element, len) = self.expr_ty_without_adjust(expr_id).kind()
                    else {
                        return Err(MirLowerError::TypeError(
                            "Array repeat expression with non array type",
                        ));
                    };
                    if let Some(constant) = self.constant_operand(*initializer)? {
                        let r = Rvalue::Repeat(constant, len.store());
                        self.push_assignment(current, place, r, expr_id.into());
                        return Ok(Some(current));
                    }
                    let Some((init, current)) =
                        self.lower_expr_to_some_operand(*initializer, current)?
                    else {
                        return Ok(None);
                    };
                    // An inline `const { .. }` is lowered in place, into a temporary: each element
                    // is a copy of its bytes and the temporary is used up, `[const { .. }; 0]` too.
                    if self.names_constant(*initializer) {
                        let r = Rvalue::Repeat(self.moved(init), len.store());
                        self.push_assignment(current, place, r, expr_id.into());
                        return Ok(Some(current));
                    }
                    let span = expr_id.into();
                    match (&init.kind, try_const_usize(self.db, len)) {
                        // No element takes the value, so it ends here.
                        (OperandKind::Copy(value), Some(0)) => {
                            let value = value.as_ref();
                            let current = self.drop_assigned(value, element, current, span, None);
                            let empty = AggregateKind::Array(element.store());
                            let r = Rvalue::Aggregate(empty, Box::new([]));
                            self.push_assignment(current, place, r, span);
                            Ok(Some(current))
                        }
                        _ => {
                            let r = Rvalue::Repeat(self.moved(init), len.store());
                            self.push_assignment(current, place, r, span);
                            Ok(Some(current))
                        }
                    }
                }
            },
            Expr::Literal(l) => {
                let ty = self.expr_ty_without_adjust(expr_id);
                let mut op = self.lower_literal_to_operand(ty, l)?;
                op.span = Some(expr_id.into());
                self.push_assignment(current, place, op.into(), expr_id.into());
                Ok(Some(current))
            }
            Expr::Underscore => Ok(Some(current)),
            Expr::IncludeBytes => not_supported!("include_bytes!()"),
        }
    }

    /// The field `offset_of!` names, of a value at address 0.
    fn offset_of_place(
        &mut self,
        offset: &hir_def::hir::OffsetOf,
        current: BasicBlockId,
        span: MirSpan,
    ) -> Result<'db, Place<'db>> {
        let interner = self.interner();
        let mut ty = self
            .infer
            .type_of_type_placeholder(offset.container)
            .ok_or(MirLowerError::TypeError("unknown type in offset_of"))?;
        let base = self.temp(Ty::new_ptr(interner, ty, Mutability::Not), current, span)?;
        let null = Operand::from_bytes(word_bytes(0), self.result.locals[base].ty.as_ref());
        self.push_assignment(current, base.into(), null.into(), span);
        let mut place = Place::from(base).project(ProjectionElem::Deref);
        let mut variant = None;
        for name in &offset.fields {
            let no_field = MirLowerError::TypeError("no such field in offset_of");
            let (field, field_ty) = match ty.kind() {
                TyKind::Tuple(tys) => {
                    let index = name.as_tuple_index().ok_or(no_field.clone())?;
                    (FieldIndex(index as u32), *tys.get(index).ok_or(no_field)?)
                }
                TyKind::Adt(adt, args) => {
                    let of = match (adt.def_id(), variant.take()) {
                        (_, Some(variant)) => VariantId::EnumVariantId(variant),
                        (AdtId::EnumId(it), None) => {
                            let id = it.enum_variants(self.db).variant(name).ok_or(no_field)?;
                            place = place.project(ProjectionElem::Downcast(id.into()));
                            variant = Some(id);
                            continue;
                        }
                        (AdtId::StructId(it), None) => it.into(),
                        (AdtId::UnionId(it), None) => it.into(),
                    };
                    let field = of.fields(self.db).field(name).ok_or(no_field)?;
                    let field_ty = self.db.field_types(of)[field]
                        .ty()
                        .instantiate(interner, args)
                        .skip_norm_wip();
                    (FieldIndex::from(field), field_ty)
                }
                _ => return Err(no_field),
            };
            place = place.project(ProjectionElem::Field(field));
            ty = field_ty;
        }
        Ok(place)
    }

    fn push_field_projection(
        &mut self,
        place: &mut Place<'db>,
        expr_id: ExprId,
    ) -> Result<'db, ()> {
        if let Expr::Field { expr, name } = &self.store[expr_id] {
            if let TyKind::Tuple(tys) = self.expr_ty_after_adjustments(*expr).kind() {
                let index =
                    name.as_tuple_index().ok_or(MirLowerError::TypeError("named field on tuple"))?
                        as u32;
                if tys.get(index as usize).is_none() {
                    return Err(MirLowerError::TypeError("tuple field index out of range"));
                }
                *place = place.project(ProjectionElem::Field(FieldIndex(index)));
            } else {
                let field = self
                    .infer
                    .field_resolution(expr_id)
                    .ok_or(MirLowerError::UnresolvedField)?
                    .either(|f| f.local_id.into(), |t| FieldIndex(t.index));
                *place = place.project(ProjectionElem::Field(field));
            }
        } else {
            not_supported!("")
        }
        Ok(())
    }

    fn lower_literal_or_const_to_operand(
        &mut self,
        ty: Ty<'db>,
        loc: &ExprId,
    ) -> Result<'db, Operand> {
        let mut operand = self.lower_literal_or_const(ty, loc)?;
        operand.span = Some((*loc).into());
        Ok(operand)
    }

    fn lower_literal_or_const(&mut self, ty: Ty<'db>, loc: &ExprId) -> Result<'db, Operand> {
        match &self.store[*loc] {
            Expr::Literal(l) => self.lower_literal_to_operand(ty, l),
            Expr::Path(c) => {
                if let Some((CandidateId::ConstId(c), subst)) =
                    self.infer.assoc_resolutions_for_expr(*loc)
                {
                    return self.lower_const_to_operand(subst, c.into());
                }
                let unresolved_name = || MirLowerError::UnresolvedName {
                    owner: self.owner.expression_store_owner(self.db),
                    path: c.clone(),
                };
                let pr = self
                    .resolver
                    .resolve_path_in_value_ns(self.db, c, HygieneId::ROOT)
                    .ok_or_else(unresolved_name)?;
                match pr {
                    ResolveValueResult::ValueNs(v) => {
                        if let ValueNs::ConstId(c) = v {
                            self.lower_const_to_operand(GenericArgs::empty(), c.into())
                        } else {
                            not_supported!("bad path in range pattern");
                        }
                    }
                    ResolveValueResult::Partial(_, _) => {
                        not_supported!("associated constants in range pattern")
                    }
                }
            }
            _ => {
                not_supported!("only `char` and numeric types are allowed in range patterns");
            }
        }
    }

    fn lower_literal_to_operand(&mut self, ty: Ty<'db>, l: &Literal) -> Result<'db, Operand> {
        let size = || {
            self.db
                .layout_of_ty(
                    ty.store(),
                    ParamEnvAndCrate { param_env: self.env, krate: self.krate() }.store(),
                )
                .map(|it| it.size.bytes_usize())
        };
        // A reference to `len` bytes, whose address the memory map gives: two words of the
        // target.
        let fat = |len: usize| -> Box<[u8]> { [word_bytes(0), word_bytes(len)].concat().into() };
        let bytes: Box<[_]> = match l {
            hir_def::hir::Literal::String(b) => {
                let b = b.as_str();
                let data = fat(b.len());
                let mm = MemoryMap::simple(b.as_bytes().into());
                return Ok(Operand::from_concrete_const(data, mm, ty));
            }
            hir_def::hir::Literal::CString(b) => {
                let bytes = b.iter().copied().chain(iter::once(0)).collect::<Box<_>>();

                let data = fat(bytes.len());
                let mm = MemoryMap::simple(bytes);
                return Ok(Operand::from_concrete_const(data, mm, ty));
            }
            hir_def::hir::Literal::ByteString(b) => {
                // A reference to an array is the address alone.
                let mut data = fat(b.len()).into_vec();
                data.truncate(size()?);
                let data = data.into_boxed_slice();
                let mm = MemoryMap::simple(b.clone());
                return Ok(Operand::from_concrete_const(data, mm, ty));
            }
            hir_def::hir::Literal::Char(c) => Box::new(u32::from(*c).to_le_bytes()),
            hir_def::hir::Literal::Bool(b) => Box::new([*b as u8]),
            hir_def::hir::Literal::Int(it, _) => Box::from(&it.to_le_bytes()[0..size()?]),
            hir_def::hir::Literal::Uint(it, _) => Box::from(&it.to_le_bytes()[0..size()?]),
            hir_def::hir::Literal::Float(f, _) => match size()? {
                16 => Box::new(f.to_f128().to_bits().to_le_bytes()),
                8 => Box::new(f.to_f64().to_bits().to_le_bytes()),
                4 => Box::new(f.to_f32().to_bits().to_le_bytes()),
                2 => Box::new(u16::try_from(f.to_f16().to_bits()).unwrap().to_le_bytes()),
                _ => {
                    return Err(MirLowerError::TypeError(
                        "float with size other than 2, 4, 8 or 16 bytes",
                    ));
                }
            },
        };
        Ok(Operand::from_concrete_const(bytes, MemoryMap::default(), ty))
    }

    fn new_basic_block(&mut self) -> BasicBlockId {
        self.result.basic_blocks.alloc(BasicBlock::default())
    }

    fn lower_const(
        &mut self,
        const_id: GeneralConstId<'db>,
        prev_block: BasicBlockId,
        place: Place<'db>,
        subst: GenericArgs<'db>,
        span: MirSpan,
    ) -> Result<'db, ()> {
        let c = self.lower_const_to_operand(subst, const_id)?;
        self.push_assignment(prev_block, place, c.into(), span);
        Ok(())
    }

    /// A function's `const { .. }` block as the constant it is, evaluated for each instance of
    /// the function as rustc's inline consts are: what only runs at compile time
    /// (`#[rustc_comptime]`, `type_info`'s intrinsics) is then never code of the function. The
    /// block of a constant's own body is lowered in place, as it is already evaluated. The
    /// constant is the `const { .. }` expression, which owns the block's bindings.
    fn inline_const(&self, expr: ExprId) -> Option<AnonConstId<'db>> {
        let InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(_)) = self.owner else {
            return None;
        };
        let ty = self.expr_ty_without_adjust(expr);
        if ty.references_non_lt_error() || ty.has_infer() {
            return None;
        }
        let token = LoweringMode::Analysis.allow_tracked_structs()?;
        Some(AnonConstId::new(
            self.db,
            AnonConstLoc {
                owner: self.store_owner,
                expr,
                ty: StoredEarlyBinder::bind(ty.store()),
                allow_using_generic_params: true,
            },
            token,
        ))
    }

    fn lower_const_to_operand(
        &mut self,
        subst: GenericArgs<'db>,
        const_id: GeneralConstId<'db>,
    ) -> Result<'db, Operand> {
        let konst = Const::new_unevaluated(
            self.interner(),
            UnevaluatedConst { def: const_id.into(), args: subst },
        );
        let ty = match const_id {
            GeneralConstId::ConstId(id) => self.db.value_ty(id.into()).unwrap(),
            GeneralConstId::StaticId(id) => self.db.value_ty(id.into()).unwrap(),
            GeneralConstId::AnonConstId(id) => id.loc(self.db).ty.get(),
        };
        let ty = ty.instantiate(self.interner(), subst).skip_norm_wip();
        Ok(Operand {
            kind: OperandKind::Constant { konst: konst.store(), ty: ty.store() },
            span: None,
        })
    }

    fn write_bytes_to_place(
        &mut self,
        prev_block: BasicBlockId,
        place: Place<'db>,
        cv: Box<[u8]>,
        ty: Ty<'db>,
        span: MirSpan,
    ) -> Result<'db, ()> {
        self.push_assignment(prev_block, place, Operand::from_bytes(cv, ty).into(), span);
        Ok(())
    }

    /// `S(a, b)` or `E::V(a, b)`: rustc builds the value from its fields as it does a record
    /// literal (THIR's `ExprKind::Adt`). Nothing is called, so nothing unwinds out of it once
    /// the fields are moved in.
    fn lower_constructor(
        &mut self,
        def: CallableDefId,
        args: &[ExprId],
        place: Place<'db>,
        mut current: BasicBlockId,
        expr_id: ExprId,
    ) -> Result<'db, Option<BasicBlockId>> {
        let variant: VariantId = match def {
            CallableDefId::StructId(id) => id.into(),
            CallableDefId::EnumVariantId(id) => id.into(),
            CallableDefId::FunctionId(_) => implementation_error!("constructor of a function"),
        };
        let subst = match self.expr_ty_without_adjust(expr_id).kind() {
            TyKind::Adt(_, subst) => subst,
            _ => implementation_error!("constructor of a type that is not an ADT"),
        };
        let declared = self.declared_locals();
        let mut operands = Vec::with_capacity(args.len());
        for &arg in args {
            let Some((operand, c)) = self.lower_expr_to_some_operand(arg, current)? else {
                return Ok(None);
            };
            current = c;
            // What unwinds from the next field drops this one first.
            self.declare_operands_last(expr_id, declared, std::iter::once(&operand));
            operands.push(operand);
        }
        self.declare_operands_last(expr_id, declared, operands.iter());
        self.push_assignment(
            current,
            place,
            Rvalue::Aggregate(AggregateKind::Adt(variant, subst.store()), operands.into()),
            expr_id.into(),
        );
        Ok(Some(current))
    }

    fn lower_enum_variant(
        &mut self,
        variant_id: EnumVariantId,
        prev_block: BasicBlockId,
        place: Place<'db>,
        ty: Ty<'db>,
        fields: Box<[Operand]>,
        span: MirSpan,
    ) -> Result<'db, BasicBlockId> {
        let subst = match ty.kind() {
            TyKind::Adt(_, subst) => subst,
            _ => implementation_error!("Non ADT enum"),
        };
        self.push_assignment(
            prev_block,
            place,
            Rvalue::Aggregate(AggregateKind::Adt(variant_id.into(), subst.store()), fields),
            span,
        );
        Ok(prev_block)
    }

    fn lower_variant_discriminant(
        &mut self,
        current: BasicBlockId,
        variant_id: EnumVariantId,
    ) -> Result<'db, (Operand, Ty<'db>, BasicBlockId)> {
        let discriminant = self.const_eval_discriminant(variant_id)?;
        let discr_ty = Ty::new_int(self.interner(), rustc_type_ir::IntTy::I128);
        let operand = Operand::from_bytes(Box::new(discriminant.to_le_bytes()), discr_ty);
        Ok((operand, discr_ty, current))
    }

    /// Lowers `let pat = scrutinee` where it is the whole condition of an `if`, in the scope
    /// the `if` has made for it, and gives the blocks to go on in when the pattern matches and
    /// when it does not. No block is on both ways, so what is known of the value where the
    /// pattern did not match stays known.
    fn lower_if_let(
        &mut self,
        condition: ExprId,
        pat: PatId,
        scrutinee: ExprId,
        current: BasicBlockId,
        guard: bool,
    ) -> Result<'db, Option<(BasicBlockId, BasicBlockId)>> {
        let Some((place, current)) = self.lower_expr_as_place(current, scrutinee, true)? else {
            return Ok(None);
        };
        // A match guard's temporaries are the arm's in every edition, dropped where the arm
        // ends or before the next arm is tried.
        if !guard && !self.edition().at_least_2024() {
            self.outlive_innermost_scope();
        }
        self.push_fake_read(current, place, condition.into());
        let resolver_guard =
            self.resolver.update_to_inner_scope(self.db, self.store_owner, condition);
        let (matched, unmatched) = self.pattern_match_linked(current, place, pat)?;
        self.resolver.reset_to_guard(resolver_guard);
        Ok(Some((matched, unmatched)))
    }

    /// Lowers a condition that binds names, `a && let P = e && ..`, to the blocks where it
    /// holds and where it does not, as rustc's `then_else_break`: each operand branches on its
    /// own, so what a `let` binds is initialized where the condition holds. `guard` if it is
    /// a match arm's guard.
    fn lower_let_chain(
        &mut self,
        condition: ExprId,
        current: BasicBlockId,
        guard: bool,
    ) -> Result<'db, Option<(BasicBlockId, BasicBlockId)>> {
        match self.store[condition] {
            Expr::Let { pat, expr } => self.lower_if_let(condition, pat, expr, current, guard),
            Expr::BinaryOp {
                lhs,
                rhs,
                op: Some(hir_def::hir::BinaryOp::LogicOp(syntax::ast::LogicOp::And)),
            } => {
                let Some((holds, fails)) = self.lower_let_chain(lhs, current, guard)? else {
                    return Ok(None);
                };
                let (holds, also_fails) = match self.lower_let_chain(rhs, holds, guard)? {
                    Some((holds, fails)) => (holds, Some(fails)),
                    None => (self.new_basic_block(), None),
                };
                let Some(also_fails) = also_fails else { return Ok(Some((holds, fails))) };
                let joined = self.new_basic_block();
                self.set_goto(fails, joined, condition.into());
                self.set_goto(also_fails, joined, condition.into());
                Ok(Some((holds, joined)))
            }
            _ => {
                // The operand's temporaries are dropped before the next one runs.
                let ty = Ty::new_bool(self.interner());
                let result: Place<'db> = self.temp(ty, current, MirSpan::Unknown)?.into();
                let scope = self.push_drop_scope();
                let Some(end) = self.lower_expr_to_place(condition, result, current)? else {
                    scope.pop_assume_dropped(self);
                    return Ok(None);
                };
                let end = scope.pop_and_drop(self, end, condition.into());
                let discr = Operand {
                    kind: OperandKind::Copy(result.store()),
                    span: Some(condition.into()),
                };
                let (holds, fails) = (self.new_basic_block(), self.new_basic_block());
                self.set_terminator(
                    end,
                    TerminatorKind::SwitchInt {
                        discr,
                        targets: SwitchTargets::static_if(1, holds, fails),
                    },
                    condition.into(),
                );
                Ok(Some((holds, fails)))
            }
        }
    }

    /// Hands what the innermost scope has declared to the scope around it. Before the 2024
    /// edition the temporaries of what an `if let` matches on live as long as those of the
    /// expression the `if` is in, and only what the pattern binds ends with the branch.
    fn outlive_innermost_scope(&mut self) {
        if let [.., outer, scope] = &mut self.drop_scopes[..] {
            outer.locals.append(&mut scope.locals);
        }
    }

    /// Whether these are the arms of the `match` that a `for` loop is written as.
    fn is_for_loop(&self, arms: &[MatchArm]) -> bool {
        matches!(arms, [arm] if matches!(
            self.store[arm.expr],
            Expr::Loop { source: LoopSource::ForLoop, .. }
        ))
    }

    /// Lowers a branch of an `if`. Its temporaries are dropped where the branch ends, also
    /// those of the tail of a block that would otherwise live as long as the statement the
    /// `if` is in.
    fn lower_branch_to_place(
        &mut self,
        branch: ExprId,
        place: Place<'db>,
        current: BasicBlockId,
    ) -> Result<'db, Option<BasicBlockId>> {
        let temporaries = self.push_drop_scope();
        let Some(current) = self.lower_expr_to_place(branch, place, current)? else {
            temporaries.pop_assume_dropped(self);
            return Ok(None);
        };
        Ok(Some(temporaries.pop_and_drop(self, current, branch.into())))
    }

    /// How many locals the innermost scope has declared: where the operands of an expression
    /// that is about to be lowered will start.
    fn declared_locals(&self) -> usize {
        self.drop_scopes.last().map_or(0, |it| it.locals.len())
    }

    /// Declares the operands of `owner`, an aggregate or a call, after everything else that
    /// was declared since `declared`, so that they are dropped first. rustc keeps the operands
    /// in the scope of the expression they are operands of: when a later operand panics, the
    /// earlier ones are dropped before the temporaries that live to the end of the statement.
    /// An expression that ends its own temporaries has them in that same scope, in the order
    /// they were made.
    fn declare_operands_last<'o>(
        &mut self,
        owner: ExprId,
        declared: usize,
        operands: impl Iterator<Item = &'o Operand>,
    ) {
        if self.ends_temporaries.contains(&owner) {
            return;
        }
        for operand in operands {
            if let (OperandKind::Copy(place) | OperandKind::Move(place)) = &operand.kind
                && place.projection.as_slice().is_empty()
            {
                self.operand_locals.insert(place.local);
            }
        }
        let Some(scope) = self.drop_scopes.last_mut() else { return };
        let Some(since) = scope.locals.get(declared..) else { return };
        // The operands of an expression inside this one stay behind its own, in the order
        // they were made: they were made later and are dropped earlier.
        let (operands, temporaries): (Vec<LocalId>, Vec<LocalId>) =
            since.iter().partition(|it| self.operand_locals.contains(it));
        scope.locals.truncate(declared);
        scope.locals.extend(temporaries);
        scope.locals.extend(operands);
    }

    fn lower_call_and_args(
        &mut self,
        func: Operand,
        args: impl Iterator<Item = ExprId>,
        place: Place<'db>,
        mut current: BasicBlockId,
        is_uninhabited: bool,
        call: ExprId,
    ) -> Result<'db, Option<BasicBlockId>> {
        let declared = self.declared_locals();
        let Some(args) = args
            .map(|arg| {
                if let Some((temp, c)) = self.lower_expr_to_some_operand(arg, current)? {
                    current = c;
                    // What unwinds from the next argument drops this one first.
                    self.declare_operands_last(call, declared, std::iter::once(&temp));
                    Ok(Some(temp))
                } else {
                    Ok(None)
                }
            })
            .collect::<Result<'_, Option<Vec<_>>>>()?
        else {
            return Ok(None);
        };
        self.declare_operands_last(call, declared, args.iter());
        let temps: Vec<LocalId> = args
            .iter()
            .filter_map(|arg| match &arg.kind {
                OperandKind::Copy(place) | OperandKind::Move(place)
                    if place.projection.as_slice().is_empty() =>
                {
                    Some(place.local)
                }
                _ => None,
            })
            .collect();
        let after =
            self.lower_call(func, args.into(), place, current, is_uninhabited, call.into())?;
        if let Some(after) = after {
            self.end_arguments(declared, &temps, after, call.into());
        }
        Ok(after)
    }

    /// Ends the storage of the temporaries a call that returned took as arguments, as rustc
    /// ends the scope of the call's operands there. The call moved or copied what they held,
    /// so a way out after it has nothing of theirs to drop: a struct literal whose fields each
    /// `?` a call would otherwise leave through every argument of the fields before.
    fn end_arguments(
        &mut self,
        declared: usize,
        temps: &[LocalId],
        after: BasicBlockId,
        span: MirSpan,
    ) {
        let Some(scope) = self.drop_scopes.last_mut() else { return };
        let Some(since) = scope.locals.get(declared..) else { return };
        let (ended, kept): (Vec<LocalId>, Vec<LocalId>) =
            since.iter().partition(|it| temps.contains(it));
        if ended.is_empty() {
            return;
        }
        scope.locals.truncate(declared);
        scope.locals.extend(kept);
        for local in ended.into_iter().rev() {
            self.push_statement(after, StatementKind::StorageDead(local).with_span(span));
        }
    }

    fn lower_call(
        &mut self,
        func: Operand,
        args: Box<[Operand]>,
        place: Place<'db>,
        current: BasicBlockId,
        is_uninhabited: bool,
        span: MirSpan,
    ) -> Result<'db, Option<BasicBlockId>> {
        let b = if is_uninhabited { None } else { Some(self.new_basic_block()) };
        let cleanup = self.unwind_here();
        self.set_terminator(
            current,
            TerminatorKind::Call {
                func,
                args,
                destination: place.store(),
                target: b,
                cleanup,
                from_hir_call: true,
            },
            span,
        );
        Ok(b)
    }

    fn is_unterminated(&mut self, source: BasicBlockId) -> bool {
        self.result.basic_blocks[source].terminator.is_none()
    }

    fn set_terminator(&mut self, source: BasicBlockId, terminator: TerminatorKind, span: MirSpan) {
        self.result.basic_blocks[source].terminator = Some(Terminator { span, kind: terminator });
    }

    fn set_goto(&mut self, source: BasicBlockId, target: BasicBlockId, span: MirSpan) {
        self.set_terminator(source, TerminatorKind::Goto { target }, span);
    }

    /// Whether `&expr` is a borrow of a constant that rustc gives the `'static` lifetime.
    /// This looks at the syntax only; whether the type allows it is left to the user of
    /// `MirBody::promoted_locals`.
    /// Whether the place `expr` is, borrowed, is of a promoted constant: rustc promotes the array
    /// an element is borrowed out of whatever the index, which the borrow does not read
    /// (`&ALL[i]`), but not an element read as a value (`&(ALL[i] + 1)`).
    fn is_promotable_place(&mut self, expr: ExprId) -> bool {
        match self.store[expr] {
            Expr::Index { base, .. }
                if matches!(self.expr_ty_without_adjust(base).kind(), TyKind::Array(..)) =>
            {
                self.is_promotable_unadjusted(base)
            }
            Expr::Field { expr: inner, .. } if !self.has_adjustments(expr) => {
                self.is_promotable_place(inner)
            }
            _ => self.is_promotable(expr),
        }
    }

    fn is_promotable(&mut self, expr: ExprId) -> bool {
        if let &Expr::Index { base, index } = &self.store[expr]
            && let TyKind::Array(_, len) = self.expr_ty_without_adjust(base).kind()
        {
            // Indexing an array is built in, whatever adjustments inference left on it. rustc
            // promotes it only by a constant it can tell is in bounds: a literal less than the
            // length, or a constant item, but not a `const { .. }` block, which it does not
            // evaluate there.
            let literal = match self.store[index] {
                Expr::Literal(Literal::Int(it, _)) => u128::try_from(it).ok(),
                Expr::Literal(Literal::Uint(it, _)) => Some(it),
                _ => None,
            };
            let in_bounds = match literal {
                Some(it) => {
                    crate::consteval::try_const_usize(self.db, len).is_some_and(|len| it < len)
                }
                None => matches!(self.store[index], Expr::Path(_)) && self.names_constant(index),
            };
            return in_bounds && self.is_promotable_unadjusted(base);
        }
        let is_fn = matches!(self.expr_ty_without_adjust(expr).kind(), TyKind::FnDef(..));
        let adjustments = self.infer.expr_adjustments.get(&expr).map_or(&[][..], |it| &**it);
        // Unsizing a reference to a constant, `&[1, 2]` to a slice say, leaves it one: rustc's
        // `promote_consts::Validator::validate_rvalue` lets every cast through that is not of
        // a pointer to an integer.
        let adjustments = match adjustments {
            [rest @ .., Adjustment { kind: Adjust::Pointer(PointerCast::Unsize), .. }] => rest,
            all => all,
        };
        match adjustments {
            [] => self.is_promotable_unadjusted(expr),
            // A function's address is a constant.
            [Adjustment { kind: Adjust::Pointer(PointerCast::ReifyFnPointer), .. }]
            | [
                Adjustment { kind: Adjust::Pointer(PointerCast::ReifyFnPointer), .. },
                Adjustment { kind: Adjust::Pointer(PointerCast::UnsafeFnPointer), .. },
            ] => is_fn && matches!(self.store[expr], Expr::Path(_)),
            // A reborrow `&*expr` of a shared reference is a copy of the reference, as in
            // rustc's `promote_consts::Validator::validate_rvalue`.
            [
                Adjustment { kind: Adjust::Deref(None), .. },
                Adjustment { kind: Adjust::Borrow(AutoBorrow::Ref(mutability)), .. },
            ] => {
                let shared = matches!(Mutability::from(*mutability), Mutability::Not);
                let reference = self.expr_ty_without_adjust(expr).as_reference().is_some();
                shared && reference && self.is_promotable_unadjusted(expr)
            }
            _ => false,
        }
    }

    /// Whether an expression is a constant item or a `const` block, which an array repeats by
    /// making it once per element, rather than a value the array takes.
    /// The constant a path names, as an operand, as rustc's MIR has it in `[CONSTANT; N]`.
    fn constant_operand(&mut self, expr: ExprId) -> Result<'db, Option<Operand>> {
        let Expr::Path(path) = &self.store[expr] else { return Ok(None) };
        if self.has_adjustments(expr) {
            return Ok(None);
        }
        let (konst, subst) =
            if let Some((candidate, subst)) = self.infer.assoc_resolutions_for_expr(expr) {
                let CandidateId::ConstId(konst) = candidate else { return Ok(None) };
                (konst, subst)
            } else {
                let guard = self.resolver.update_to_inner_scope(self.db, self.store_owner, expr);
                let hygiene = self.store.expr_path_hygiene(expr);
                let resolved = self.resolver.resolve_path_in_value_ns_fully(self.db, path, hygiene);
                self.resolver.reset_to_guard(guard);
                let Some(ValueNs::ConstId(konst)) = resolved else { return Ok(None) };
                (konst, GenericArgs::empty())
            };
        let mut operand = self.lower_const_to_operand(subst, konst.into())?;
        operand.span = Some(expr.into());
        Ok(Some(operand))
    }

    fn names_constant(&mut self, expr: ExprId) -> bool {
        let Expr::Path(path) = &self.store[expr] else {
            return matches!(self.store[expr], Expr::Const(_));
        };
        if let Some((candidate, _)) = self.infer.assoc_resolutions_for_expr(expr) {
            return matches!(candidate, CandidateId::ConstId(_));
        }
        let guard = self.resolver.update_to_inner_scope(self.db, self.store_owner, expr);
        let hygiene = self.store.expr_path_hygiene(expr);
        let resolved = self.resolver.resolve_path_in_value_ns_fully(self.db, path, hygiene);
        self.resolver.reset_to_guard(guard);
        matches!(resolved, Some(ValueNs::ConstId(_)))
    }

    /// The operand that takes the value of the place another operand reads.
    fn moved(&self, operand: Operand) -> Operand {
        match operand.kind {
            OperandKind::Copy(place) => {
                Operand { kind: OperandKind::Move(place), span: operand.span }
            }
            _ => operand,
        }
    }

    fn is_promotable_unadjusted(&mut self, expr: ExprId) -> bool {
        let scalar = |this: &Self, expr| {
            matches!(
                this.expr_ty_without_adjust(expr).kind(),
                TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Uint(_) | TyKind::Float(_)
            )
        };
        match &self.store[expr] {
            Expr::Literal(_) => true,
            Expr::Path(path) => {
                // `Type::CONST` is resolved by inference, not by name.
                if let Some((CandidateId::ConstId(_), _)) =
                    self.infer.assoc_resolutions_for_expr(expr)
                {
                    return true;
                }
                // `Self::Variant` likewise.
                if self.infer.variant_resolution_for_expr(expr).is_some() {
                    return !matches!(self.expr_ty_without_adjust(expr).kind(), TyKind::FnDef(..));
                }
                let guard = self.resolver.update_to_inner_scope(self.db, self.store_owner, expr);
                let hygiene = self.store.expr_path_hygiene(expr);
                let resolved = self.resolver.resolve_path_in_value_ns_fully(self.db, path, hygiene);
                self.resolver.reset_to_guard(guard);
                match resolved {
                    Some(
                        ValueNs::ConstId(_) | ValueNs::EnumVariantId(_) | ValueNs::GenericParam(_),
                    ) => true,
                    Some(ValueNs::StructId(_)) => {
                        !matches!(self.expr_ty_without_adjust(expr).kind(), TyKind::FnDef(..))
                    }
                    _ => false,
                }
            }
            Expr::Tuple { exprs } | Expr::Array(Array::ElementList { elements: exprs }) => {
                exprs.clone().iter().all(|&it| self.is_promotable(it))
            }
            // The length is a constant already.
            &Expr::Array(Array::Repeat { initializer, .. }) => self.is_promotable(initializer),
            Expr::RecordLit { fields, spread: RecordSpread::None, .. } => {
                fields.clone().iter().all(|it| self.is_promotable(it.expr))
            }
            &Expr::Field { expr, .. } => self.is_promotable(expr),
            Expr::Call { callee, args } => {
                let TyKind::FnDef(def, _) = self.expr_ty_without_adjust(*callee).kind() else {
                    return false;
                };
                // A tuple struct or variant is made of its fields, as a record literal is.
                let makes_value = match def.0 {
                    CallableDefId::FunctionId(func) => {
                        hir_def::attrs::AttrFlags::query(self.db, func.into())
                            .contains(hir_def::attrs::AttrFlags::RUSTC_PROMOTABLE)
                    }
                    CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) => true,
                };
                makes_value && args.clone().iter().all(|&it| self.is_promotable(it))
            }
            &Expr::Ref { expr, mutability, .. } => {
                (mutability.is_shared() || self.is_empty_array(expr)) && self.is_promotable(expr)
            }
            &Expr::UnaryOp {
                expr,
                op: hir_def::hir::UnaryOp::Not | hir_def::hir::UnaryOp::Neg,
            } => scalar(self, expr) && self.is_promotable(expr),
            &Expr::Cast { expr: inner, .. } => {
                let from = self.expr_ty_without_adjust(inner);
                let to = self.expr_ty_without_adjust(expr);
                // `function as fn()` is the function's address, as the coercion to it is.
                let is_fn_address = matches!(self.store[inner], Expr::Path(_))
                    && matches!(from.kind(), TyKind::FnDef(..))
                    && matches!(to.kind(), TyKind::FnPtr(..));
                // rustc's `promote_consts::Validator::validate_rvalue` lets every cast through
                // but of a pointer to an integer, `b"node" as &[u8]` as much as `1 as char`.
                let exposes_address = matches!(
                    from.kind(),
                    TyKind::RawPtr(..) | TyKind::FnPtr(..) | TyKind::FnDef(..)
                ) && matches!(to.kind(), TyKind::Int(_) | TyKind::Uint(_));
                is_fn_address || !exposes_address && self.is_promotable(inner)
            }
            &Expr::BinaryOp { lhs, rhs, op: Some(op) } => {
                let promoted = match op {
                    BinaryOp::ArithOp(ArithOp::Div | ArithOp::Rem) => {
                        self.division_promotable(lhs, rhs)
                    }
                    BinaryOp::Assignment { .. } => false,
                    BinaryOp::ArithOp(_) | BinaryOp::CmpOp(_) | BinaryOp::LogicOp(_) => true,
                };
                promoted
                    && scalar(self, lhs)
                    && scalar(self, rhs)
                    && self.is_promotable(lhs)
                    && self.is_promotable(rhs)
            }
            _ => false,
        }
    }

    fn is_empty_array(&self, expr: ExprId) -> bool {
        match self.expr_ty_without_adjust(expr).kind() {
            TyKind::Array(_, len) => crate::consteval::try_const_usize(self.db, len) == Some(0),
            _ => false,
        }
    }

    /// rustc's promotion of an integer division or remainder: the divisor is a constant other
    /// than 0, and for a signed one other than -1 unless the dividend is a constant other than
    /// the type's minimum. A `const { .. }` block is not evaluated to tell.
    fn division_promotable(&self, lhs: ExprId, rhs: ExprId) -> bool {
        let (signed, bits) = match self.expr_ty_without_adjust(lhs).kind() {
            TyKind::Int(it) => (true, it.bit_width()),
            TyKind::Uint(it) => (false, it.bit_width()),
            _ => return true,
        };
        let literal = |expr: ExprId| match &self.store[expr] {
            &Expr::Literal(Literal::Int(it, _)) => Some(it),
            &Expr::Literal(Literal::Uint(it, _)) => i128::try_from(it).ok(),
            &Expr::UnaryOp { expr, op: hir_def::hir::UnaryOp::Neg } => match self.store[expr] {
                Expr::Literal(Literal::Int(it, _)) => Some(-it),
                Expr::Literal(Literal::Uint(it, _)) => i128::try_from(it).ok().map(|it| -it),
                _ => None,
            },
            _ => None,
        };
        match literal(rhs) {
            None | Some(0) => false,
            Some(-1) if signed => {
                let bits = bits.unwrap_or(64);
                let min = -(1i128 << (bits - 1));
                literal(lhs).is_some_and(|it| it != min)
            }
            Some(_) => true,
        }
    }

    fn expr_ty_without_adjust(&self, e: ExprId) -> Ty<'db> {
        self.infer.expr_ty(e)
    }

    fn expr_ty_after_adjustments(&self, e: ExprId) -> Ty<'db> {
        let mut ty = None;
        if let Some(it) = self.infer.expr_adjustments.get(&e)
            && let Some(it) = it.last()
        {
            ty = Some(it.target.as_ref());
        }
        ty.unwrap_or_else(|| self.expr_ty_without_adjust(e))
    }

    fn push_statement(&mut self, block: BasicBlockId, statement: Statement) {
        // A statement that panics unwinds as the `Assert` rustc has before it would.
        if statement.kind.may_panic() && !self.result.basic_blocks[block].is_cleanup {
            let at = self.result.basic_blocks[block].statements.len();
            if let Some(unwind) = self.unwind_here() {
                self.result.panic_unwinds.push((block, at, unwind));
            }
        }
        self.result.basic_blocks[block].statements.push(statement);
    }

    /// The unwind drop tree's way out from where lowering is: every scope's drops.
    fn unwind_here(&mut self) -> Option<BasicBlockId> {
        let in_scope: Vec<LocalId> =
            (0..self.drop_scopes.len()).flat_map(|index| self.leaving_order(index)).collect();
        self.unwind_through(&in_scope, None)
    }

    /// The locals of the scope at `index` in the reverse of the order leaving it early drops
    /// them in. rustc keeps the operands of an expression being lowered in that expression's
    /// own scope, inside the temporaries that live to the end of the statement: in
    /// `boom().extend(&String::from(..))` a panic in the deref drops `boom()` first, and a
    /// `?` in a field of a struct literal drops the fields before it first.
    fn leaving_order(&self, index: usize) -> Vec<LocalId> {
        let operands = &self.operand_locals;
        let (pending, mut rest): (Vec<LocalId>, Vec<LocalId>) =
            self.drop_scopes[index].in_order(operands).iter().partition(|it| operands.contains(it));
        rest.extend(pending);
        rest
    }

    /// What every scope has declared, outermost first, in the order it was declared.
    fn in_scope(&self) -> Vec<LocalId> {
        let operands = &self.operand_locals;
        self.drop_scopes.iter().flat_map(|it| it.in_order(operands).into_owned()).collect()
    }

    /// The way out of a drop of `locals[at]` of a scope that drops `locals`: what is declared
    /// before it, then, in a coroutine, its own storage.
    fn unwind_from_drop(&mut self, locals: &[LocalId], at: usize) -> Option<BasicBlockId> {
        let local = locals[at];
        let mut in_scope = self.in_scope();
        match in_scope.iter().position(|&it| it == local) {
            Some(position) => in_scope.truncate(position),
            None => in_scope.extend_from_slice(&locals[..at]),
        }
        self.unwind_through(&in_scope, Some(local))
    }

    /// The node of the unwind drop tree that drops `locals` last first, and before them ends
    /// the storage of `storage_of`. A local's storage is ended on unwinding only in a coroutine,
    /// as in rustc, where the coroutine transform reads storage liveness; `None` if there is
    /// nothing to do on the way out.
    fn unwind_through(
        &mut self,
        locals: &[LocalId],
        storage_of: Option<LocalId>,
    ) -> Option<BasicBlockId> {
        let storage = self.result.coroutine.is_some();
        let mut at = None;
        for &local in locals {
            if storage {
                at = Some(self.unwind_node(at, local, false));
            }
            if !self.is_copy(self.result.locals[local].ty.as_ref())
                && !self.unscheduled.contains(&local)
            {
                at = Some(self.unwind_node(at, local, true));
            }
        }
        if let (true, Some(local)) = (storage, storage_of) {
            at = Some(self.unwind_node(at, local, false));
        }
        at
    }

    /// Whether a value of `ty` is `Copy`, which nothing drops.
    pub(super) fn is_copy(&mut self, ty: Ty<'db>) -> bool {
        let (infcx, env) = (&self.infcx, self.env);
        *self.copy.entry(ty).or_insert_with(|| infcx.type_is_copy_modulo_regions(env, ty))
    }

    fn unwind_node(
        &mut self,
        parent: Option<BasicBlockId>,
        local: LocalId,
        value: bool,
    ) -> BasicBlockId {
        let parent = match parent {
            Some(it) => it,
            None => self.resume_block(),
        };
        if let Some(&node) = self.unwind_nodes.get(&(parent, local, value)) {
            return node;
        }
        let (statements, kind) = match value {
            true => (
                Vec::new(),
                TerminatorKind::Drop {
                    place: Place::from(local).store(),
                    target: parent,
                    unwind: None,
                },
            ),
            false => (
                vec![StatementKind::StorageDead(local).with_span(MirSpan::Unknown)],
                TerminatorKind::Goto { target: parent },
            ),
        };
        let node = self.result.basic_blocks.alloc(BasicBlock {
            statements,
            terminator: Some(Terminator { span: MirSpan::Unknown, kind }),
            is_cleanup: true,
        });
        self.unwind_nodes.insert((parent, local, value), node);
        node
    }

    fn resume_block(&mut self) -> BasicBlockId {
        if let Some(resume) = self.unwind_resume {
            return resume;
        }
        let resume = self.result.basic_blocks.alloc(BasicBlock {
            statements: Vec::new(),
            terminator: Some(Terminator {
                span: MirSpan::Unknown,
                kind: TerminatorKind::UnwindResume,
            }),
            is_cleanup: true,
        });
        self.unwind_resume = Some(resume);
        resume
    }

    /// rustc's `push_assign_unit`: what an expression of type `()` that computes nothing gives
    /// its place, as an assignment, a block without a tail or a `break` without a value. Zero
    /// sized, but assigned, so that the borrow checker sees the place initialized and a second
    /// write to it as one that needs its `mut`.
    fn push_assign_unit(&mut self, block: BasicBlockId, place: Place<'db>, span: MirSpan) {
        let unit = Ty::new_unit(self.interner());
        let r = Rvalue::Aggregate(AggregateKind::Tuple(unit.store()), Box::default());
        self.push_assignment(block, place, r, span);
    }

    /// A function item, or a tuple struct's or variant's constructor, written as a value: zero
    /// sized, but assigned as rustc does, so that the borrow checker sees it initialized.
    fn push_fn_item(&mut self, block: BasicBlockId, place: Place<'db>, expr: ExprId) {
        let ty = self.expr_ty_without_adjust(expr);
        if let TyKind::FnDef(..) = ty.kind() {
            self.push_assignment(block, place, Operand::const_zst(ty).into(), expr.into());
        }
    }

    fn push_fake_read(&mut self, block: BasicBlockId, p: Place<'db>, span: MirSpan) {
        self.push_statement(block, StatementKind::FakeRead(p.store()).with_span(span));
    }

    fn push_assignment(
        &mut self,
        block: BasicBlockId,
        place: Place<'db>,
        rvalue: Rvalue,
        span: MirSpan,
    ) {
        self.push_statement(block, StatementKind::Assign(place.store(), rvalue).with_span(span));
    }

    fn discr_temp_place(&mut self, current: BasicBlockId) -> Place<'db> {
        match &self.discr_temp {
            Some(it) => it.as_ref(),
            None => {
                // FIXME: rustc's ty is dependent on the adt type, maybe we need to do that as well
                let discr_ty = Ty::new_int(self.interner(), rustc_type_ir::IntTy::I128);
                let tmp: Place<'_> = self
                    .temp(discr_ty, current, MirSpan::Unknown)
                    .expect("discr_ty is never unsized")
                    .into();
                self.discr_temp = Some(tmp.store());
                tmp
            }
        }
    }

    fn lower_loop(
        &mut self,
        prev_block: BasicBlockId,
        place: Place<'db>,
        label: Option<LabelId>,
        span: MirSpan,
        f: impl FnOnce(&mut MirLowerCtx<'_, 'db>, BasicBlockId) -> Result<'db, ()>,
    ) -> Result<'db, Option<BasicBlockId>> {
        let begin = self.new_basic_block();
        let prev = self.current_loop_blocks.replace(LoopBlocks {
            begin,
            end: None,
            place: place.store(),
            drop_scope_index: self.drop_scopes.len(),
        });
        let prev_label = if let Some(label) = label {
            // We should generate the end now, to make sure that it wouldn't change later. It is
            // bad as we may emit end (unnecessary unreachable block) for unterminating loop, but
            // it should not affect correctness.
            self.current_loop_end()?;
            self.labeled_loop_blocks
                .insert(label, self.current_loop_blocks.as_ref().unwrap().clone())
        } else {
            None
        };
        self.set_goto(prev_block, begin, span);
        f(self, begin)?;
        let my = mem::replace(&mut self.current_loop_blocks, prev).ok_or(
            MirLowerError::ImplementationError("current_loop_blocks is corrupt".to_owned()),
        )?;
        if let Some(prev) = prev_label {
            self.labeled_loop_blocks.insert(label.unwrap(), prev);
        }
        Ok(my.end)
    }

    /// An operand of a comparison of scalars: the value, without the autoref inference records
    /// for `PartialEq::eq(&a, &b)`, as rustc has it after writeback.
    fn lower_scalar_operand(
        &mut self,
        expr_id: ExprId,
        current: BasicBlockId,
    ) -> Result<'db, Option<(Operand, BasicBlockId)>> {
        let adjustments = self.infer.expr_adjustments.get(&expr_id).cloned().unwrap_or_default();
        let Some((Adjustment { kind: Adjust::Borrow(AutoBorrow::Ref(_)), .. }, rest)) =
            adjustments.split_last()
        else {
            return self.lower_expr_to_some_operand(expr_id, current);
        };
        if rest.is_empty()
            && let Expr::Literal(l) = &self.store[expr_id]
        {
            let ty = self.expr_ty_without_adjust(expr_id);
            let mut operand = self.lower_literal_to_operand(ty, l)?;
            operand.span = Some(expr_id.into());
            return Ok(Some((operand, current)));
        }
        let ty = match rest.last() {
            Some(it) => it.target.as_ref(),
            None => self.expr_ty_without_adjust(expr_id),
        };
        let temp: Place<'db> = self.temp(ty, current, expr_id.into())?.into();
        let Some(current) = self.lower_expr_to_place_with_adjust(expr_id, temp, current, rest)?
        else {
            return Ok(None);
        };
        let operand = Operand { kind: OperandKind::Copy(temp.store()), span: Some(expr_id.into()) };
        Ok(Some((operand, current)))
    }

    fn has_adjustments(&self, expr_id: ExprId) -> bool {
        !self.infer.expr_adjustments.get(&expr_id).map(|it| it.is_empty()).unwrap_or(true)
    }

    fn merge_blocks(
        &mut self,
        b1: Option<BasicBlockId>,
        b2: Option<BasicBlockId>,
        span: MirSpan,
    ) -> Option<BasicBlockId> {
        match (b1, b2) {
            (None, None) => None,
            (None, Some(b)) | (Some(b), None) => Some(b),
            (Some(b1), Some(b2)) => {
                let bm = self.new_basic_block();
                self.set_goto(b1, bm, span);
                self.set_goto(b2, bm, span);
                Some(bm)
            }
        }
    }

    fn current_loop_end(&mut self) -> Result<'db, BasicBlockId> {
        let r = match self
            .current_loop_blocks
            .as_mut()
            .ok_or(MirLowerError::ImplementationError(
                "Current loop access out of loop".to_owned(),
            ))?
            .end
        {
            Some(it) => it,
            None => {
                let s = self.new_basic_block();
                self.current_loop_blocks
                    .as_mut()
                    .ok_or(MirLowerError::ImplementationError(
                        "Current loop access out of loop".to_owned(),
                    ))?
                    .end = Some(s);
                s
            }
        };
        Ok(r)
    }

    fn is_uninhabited(&self, expr_id: ExprId) -> bool {
        is_ty_uninhabited_from(
            &self.infcx,
            self.infer.expr_ty(expr_id),
            self.owner.module(self.db),
            self.env,
        )
    }

    /// This function push `StorageLive` statement for the binding, and applies changes to add `StorageDead` and
    /// `Drop` in the appropriated places.
    fn push_storage_live(&mut self, b: BindingId, current: BasicBlockId) -> Result<'db, ()> {
        let l = self.binding_local(b)?;
        self.push_storage_live_for_local(l, current, MirSpan::BindingId(b))
    }

    fn push_storage_live_for_local(
        &mut self,
        l: LocalId,
        current: BasicBlockId,
        span: MirSpan,
    ) -> Result<'db, ()> {
        let scope = match span {
            MirSpan::ExprId(expr) => self.extended_temporaries.get(&expr).copied(),
            MirSpan::BindingId(_) => {
                self.super_let.or_else(|| self.drop_scopes.iter().rposition(|it| !it.temporaries))
            }
            _ => None,
        };
        let scope = match scope.and_then(|it| self.drop_scopes.get_mut(it)) {
            Some(scope) => scope,
            None => self.drop_scopes.last_mut().unwrap(),
        };
        if !scope.locals.contains(&l) {
            scope.locals.push(l);
        }
        self.push_statement(current, StatementKind::StorageLive(l).with_span(span));
        Ok(())
    }

    /// Records which temporaries of a `let` initializer live to the end of the enclosing block
    /// rather than to the end of the statement: to the scope `scope`, the one the `let` is in.
    fn extend_temporaries(&mut self, initializer: ExprId, pat: PatId, scope: usize) {
        let mut by_ref = false;
        self.store.walk_bindings_in_pat(pat, |b| {
            by_ref |=
                matches!(self.store[b].mode, BindingAnnotation::Ref | BindingAnnotation::RefMut);
        });
        if by_ref {
            self.extend_operand(initializer, scope);
        }
        self.extend_in(initializer, scope);
    }

    fn extend_in(&mut self, expr: ExprId, scope: usize) {
        match &self.store[expr] {
            &Expr::Ref { expr, .. } => {
                self.extend_operand(expr, scope);
                self.extend_in(expr, scope);
            }
            &Expr::Cast { expr, .. } => self.extend_in(expr, scope),
            Expr::Tuple { exprs } | Expr::Array(Array::ElementList { elements: exprs }) => {
                exprs.iter().for_each(|&it| self.extend_in(it, scope))
            }
            Expr::RecordLit { fields, .. } => {
                fields.iter().for_each(|it| self.extend_in(it.expr, scope))
            }
            // A tuple struct's or variant's constructor named as such, `Some(&temp())` or
            // `Self(&temp())`, not through a variable (`let some = Some; some(&temp())`).
            Expr::Call { callee, args } if self.names_constructor(*callee) => {
                args.iter().for_each(|&it| self.extend_in(it, scope))
            }
            &Expr::Block { tail, .. } => {
                self.extended_blocks.insert(expr, scope);
                if let Some(tail) = tail {
                    self.extend_in(tail, scope);
                }
            }
            &Expr::If { then_branch, else_branch, .. } => {
                self.extend_in(then_branch, scope);
                if let Some(else_branch) = else_branch {
                    self.extend_in(else_branch, scope);
                }
            }
            Expr::Match { arms, .. } => arms.iter().for_each(|it| self.extend_in(it.expr, scope)),
            _ => (),
        }
    }

    fn names_constructor(&self, callee: ExprId) -> bool {
        self.infer.variant_resolution_for_expr(callee).is_some()
            || matches!(&self.store[callee], Expr::Path(path) if path.is_self_type())
                && matches!(
                    self.infer.expr_ty(callee).kind(),
                    TyKind::FnDef(def, _) if !matches!(def.0, CallableDefId::FunctionId(_))
                )
    }

    fn extend_operand(&mut self, mut expr: ExprId, scope: usize) {
        loop {
            self.extended_temporaries.insert(expr, scope);
            expr = match &self.store[expr] {
                &Expr::Ref { expr, .. }
                | &Expr::UnaryOp { expr, op: hir_def::hir::UnaryOp::Deref }
                | &Expr::Field { expr, .. } => expr,
                &Expr::Index { base, .. } => base,
                _ => return,
            };
        }
    }

    /// Whether an `if` condition binds names, in which case its temporaries are needed in the
    /// branches.
    fn has_let(&self, expr: ExprId) -> bool {
        match &self.store[expr] {
            Expr::Let { .. } => true,
            &Expr::BinaryOp { lhs, rhs, .. } => self.has_let(lhs) || self.has_let(rhs),
            _ => false,
        }
    }

    /// A literal statement among them is an argument `format_args!` put in its template,
    /// kept for inference and nothing else.
    fn is_super_let_block(&self, statements: &[hir_def::hir::Statement]) -> bool {
        let literal = |mut expr: ExprId| loop {
            match &self.store[expr] {
                Expr::Ref { expr: inner, .. } => expr = *inner,
                Expr::Literal(_) => return true,
                _ => return false,
            }
        };
        statements.iter().any(|it| matches!(it, hir_def::hir::Statement::Let { .. }))
            && statements.iter().all(|statement| match statement {
                hir_def::hir::Statement::Let { pat, initializer: Some(_), is_super, .. } => {
                    *is_super
                        || matches!(self.store[*pat], Pat::Bind { id, subpat: None } if self.store[id].name.is_generated())
                }
                hir_def::hir::Statement::Expr { expr, .. } => literal(*expr),
                _ => false,
            })
    }

    fn lower_block_to_place(
        &mut self,
        statements: &[hir_def::hir::Statement],
        mut current: BasicBlockId,
        tail: Option<ExprId>,
        place: Place<'db>,
        block: ExprId,
    ) -> Result<'db, Option<Idx<BasicBlock>>> {
        let span = MirSpan::from(block);
        if self.is_super_let_block(statements)
            && let Some(tail) = tail
        {
            // `format_args!` and `pin!` expand to a block of `super let`s: what they bind lives
            // as long as the block's own value does. A `let` that extends the block extends
            // the temporaries of their initializers too; otherwise those are dropped as they
            // would be anywhere, a block's tail's at the end of the block from 2024 on
            // (rust-lang/rust#145784).
            let extended = self.extended_blocks.remove(&block);
            for statement in statements {
                let hir_def::hir::Statement::Let { pat, initializer: Some(initializer), .. } =
                    statement
                else {
                    continue;
                };
                if let Some(scope) = extended {
                    self.extend_temporaries(*initializer, *pat, scope);
                }
                self.declare_bindings(*pat);
                let Some((init_place, c)) =
                    self.lower_expr_as_place(current, *initializer, true)?
                else {
                    return Ok(None);
                };
                let resolver_guard =
                    self.resolver.update_to_inner_scope(self.db, self.store_owner, *initializer);
                self.super_let = Some(extended.unwrap_or(self.drop_scopes.len() - 1));
                let matched = self.pattern_match(c, None, init_place, *pat);
                self.super_let = None;
                (current, _) = matched?;
                self.resolver.reset_to_guard(resolver_guard);
            }
            return self.lower_expr_to_place(tail, place, current);
        }
        let enclosing = self.drop_scopes.iter().rposition(|it| !it.temporaries);
        let scope = self.push_drop_scope();
        for statement in statements.iter() {
            match statement {
                hir_def::hir::Statement::Let {
                    pat, initializer, else_branch, is_super, ..
                } => {
                    // rustc declares what the pattern binds before it lowers the initializer.
                    self.declare_bindings(*pat);
                    // `let x = e` with `x` bound by value: rustc lowers `e` into `x` itself
                    // (`expr_into_pattern`), so `x` holds the value before the temporaries of
                    // `e` are dropped, which is what a coroutine's storage conflicts see. A
                    // value of a type without a size can't be a local, so that goes the other
                    // way, which says so.
                    if let (Some(expr_id), None, false, Pat::Bind { id, subpat: None }) =
                        (initializer, else_branch, *is_super, &self.store[*pat])
                        && matches!(
                            self.store[*id].mode,
                            BindingAnnotation::Unannotated | BindingAnnotation::Mutable
                        )
                        && !matches!(
                            self.infer.binding_ty(*id).kind(),
                            TyKind::Slice(_) | TyKind::Str | TyKind::Dynamic(..)
                        )
                    {
                        let binding = *id;
                        self.extend_temporaries(*expr_id, *pat, self.drop_scopes.len() - 1);
                        let local = self.binding_local(binding)?;
                        // rustc's storage_live_binding: the storage starts here, and the drop of
                        // the value is scheduled only once it holds one, after the initializer
                        // and the temporaries it extends, so that nothing unwinding out of the
                        // initializer drops it.
                        let at = MirSpan::BindingId(binding);
                        self.push_statement(
                            current,
                            StatementKind::StorageLive(local).with_span(at),
                        );
                        let temporaries = self.push_drop_scope();
                        self.drop_scopes.last_mut().unwrap().temporaries = true;
                        let Some(c) = self.lower_expr_to_place(*expr_id, local.into(), current)?
                        else {
                            temporaries.pop_assume_dropped(self);
                            scope.pop_assume_dropped(self);
                            return Ok(None);
                        };
                        // rustc's `FakeReadCause::ForLet` of `x`, before the initializer's
                        // temporaries are dropped: what the value borrows from a `match` arm's
                        // bindings, dropped as the arm ends, must outlive them.
                        self.push_fake_read(c, local.into(), MirSpan::PatId(*pat));
                        current = temporaries.pop_and_drop(self, c, span);
                        if let Some(scope) =
                            self.drop_scopes.iter_mut().rev().find(|it| !it.temporaries)
                            && !scope.locals.contains(&local)
                        {
                            scope.locals.push(local);
                        }
                        continue;
                    }
                    if let Some(expr_id) = initializer {
                        let else_block;
                        let declared = self.drop_scopes.last().map_or(0, |it| it.locals.len());
                        self.extend_temporaries(*expr_id, *pat, self.drop_scopes.len() - 1);
                        let temporaries = self.push_drop_scope();
                        self.drop_scopes.last_mut().unwrap().temporaries = true;
                        let Some((init_place, c)) =
                            self.lower_expr_as_place(current, *expr_id, true)?
                        else {
                            temporaries.pop_assume_dropped(self);
                            scope.pop_assume_dropped(self);
                            return Ok(None);
                        };
                        current = c;
                        // rustc's `FakeReadCause::ForLet`, at the pattern, where the borrow
                        // checker finds the place a `let _: T` ascribes.
                        self.push_fake_read(current, init_place, MirSpan::PatId(*pat));
                        // Using the initializer for the resolver scope is good enough for us, as it cannot create new declarations
                        // and has all declarations of the `let`.
                        let resolver_guard = self.resolver.update_to_inner_scope(
                            self.db,
                            self.store_owner,
                            *expr_id,
                        );
                        self.super_let = enclosing.filter(|_| *is_super);
                        let matched = match else_branch {
                            Some(_) => self
                                .pattern_match_linked(current, init_place, *pat)
                                .map(|(then, otherwise)| (then, Some(otherwise))),
                            None => self.pattern_match(current, None, init_place, *pat),
                        };
                        self.super_let = None;
                        (current, else_block) = matched?;
                        self.resolver.reset_to_guard(resolver_guard);
                        match (else_block, else_branch) {
                            (None, _) => (),
                            (Some(else_block), None) => {
                                self.set_terminator(else_block, TerminatorKind::Unreachable, span);
                            }
                            (Some(else_block), Some(else_branch)) => {
                                self.lower_let_else(else_block, *else_branch, declared, span)?;
                            }
                        }
                        current = temporaries.pop_and_drop(self, current, span);
                    } else {
                        let mut err = None;
                        self.store.walk_bindings_in_pat(*pat, |b| {
                            if let Err(e) = self.push_storage_live(b, current) {
                                err = Some(e);
                            }
                        });
                        if let Some(e) = err {
                            return Err(e);
                        }
                    }
                }
                &hir_def::hir::Statement::Expr { expr, has_semi: _ } => {
                    let scope2 = self.push_drop_scope();
                    // A place used as a statement (`*x;`) is moved out of, and the value
                    // dropped at the end of the statement.
                    let unsized_place = matches!(
                        self.expr_ty_after_adjustments(expr).kind(),
                        TyKind::Slice(_) | TyKind::Str | TyKind::Dynamic(..)
                    );
                    let lowered = if unsized_place {
                        self.lower_expr_as_place(current, expr, true)?
                    } else {
                        self.lower_expr_to_fresh_place(expr, current)?
                    };
                    let Some((p, c)) = lowered else {
                        scope2.pop_assume_dropped(self);
                        scope.pop_assume_dropped(self);
                        return Ok(None);
                    };
                    self.push_fake_read(c, p, expr.into());
                    current = scope2.pop_and_drop(self, c, expr.into());
                }
                hir_def::hir::Statement::Item(_) => (),
            }
        }
        if let Some(tail) = tail {
            // Before the 2024 edition the temporaries of a block's tail live as long as those
            // of the expression the block is in. The edition is the one the block was written
            // in, which for a macro's block is the macro's.
            let edition = self.store.block_edition(block).unwrap_or_else(|| self.edition());
            if edition.at_least_2024() {
                self.ends_temporaries.insert(tail);
            }
            let declared = self.drop_scopes.last().map_or(0, |it| it.locals.len());
            if !edition.at_least_2024()
                && let Some(scope) = self.drop_scopes.last_mut()
            {
                scope.tail = Some(declared);
            }
            let Some(c) = self.lower_expr_to_place(tail, place, current)? else {
                scope.pop_assume_dropped(self);
                return Ok(None);
            };
            current = c;
            if !edition.at_least_2024() {
                self.outlive_block(declared);
            }
        } else if self.expr_ty_without_adjust(block).is_unit() {
            self.push_assign_unit(current, place, span);
        }
        current = scope.pop_and_drop(self, current, span);
        Ok(Some(current))
    }

    /// Hands the temporaries of a block's tail, the locals of the block from `declared` on, to
    /// the scope the block is in, or puts them before the block's own locals when it is in
    /// none. The operands of the tail stay where they are: when a later operand panics they
    /// are dropped before the locals of the block.
    fn outlive_block(&mut self, declared: usize) {
        let Some((scope, outer)) = self.drop_scopes.split_last_mut() else { return };
        scope.tail = None;
        if declared > scope.locals.len() {
            return;
        }
        let (operands, temporaries): (Vec<LocalId>, Vec<LocalId>) =
            scope.locals.drain(declared..).partition(|it| self.operand_locals.contains(it));
        match outer.last_mut() {
            Some(outer) => outer.locals.extend(temporaries),
            None => {
                scope.locals.splice(0..0, temporaries);
            }
        }
        scope.locals.extend(operands);
    }

    /// Lowers the `else` of a `let`, which runs when the pattern did not match. The temporaries
    /// of the initializer are dropped before it runs: the value that was matched, and the ones
    /// that would have lived on to the end of the block, which are the locals of the block from
    /// `declared` on.
    fn lower_let_else(
        &mut self,
        mut else_block: BasicBlockId,
        else_branch: ExprId,
        declared: usize,
        span: MirSpan,
    ) -> Result<'db, ()> {
        let [.., block, temporaries] = &mut self.drop_scopes[..] else {
            return Err(MirLowerError::ImplementationError("`let` outside a block".into()));
        };
        let extended = block.locals.split_off(declared.min(block.locals.len()));
        let held = [
            DropScope { locals: extended, temporaries: true, ..Default::default() },
            DropScope {
                locals: std::mem::take(&mut temporaries.locals),
                temporaries: true,
                ..Default::default()
            },
        ];
        for scope in held.iter().rev() {
            self.emit_drop_and_storage_dead_for_scope(scope, &mut else_block, span);
        }
        let lowered = self.lower_expr_as_place(else_block, else_branch, true);
        let [extended, statement] = held;
        if let [.., block, temporaries] = &mut self.drop_scopes[..] {
            block.locals.extend(extended.locals);
            temporaries.locals = statement.locals;
        }
        if let Some((_, end)) = lowered? {
            self.set_terminator(end, TerminatorKind::Unreachable, span);
        }
        Ok(())
    }

    fn lower_params_and_bindings(
        &mut self,
        params: impl Iterator<Item = (PatId, Ty<'db>)> + Clone,
        self_binding: Option<(BindingId, Ty<'db>)>,
        pick_binding: impl Fn(BindingId) -> bool,
    ) -> Result<'db, BasicBlockId> {
        let base_param_count = self.result.param_locals.len();
        let self_binding = match self_binding {
            Some((self_binding, ty)) => {
                let local_id = self.result.locals.alloc(Local { ty: ty.store() });
                self.drop_scopes.last_mut().unwrap().locals.push(local_id);
                self.result.binding_locals.insert(self_binding, local_id);
                self.result.param_locals.push(local_id);
                Some(self_binding)
            }
            None => None,
        };
        self.result.param_locals.extend(params.clone().map(|(it, ty)| {
            let local_id = self.result.locals.alloc(Local { ty: ty.store() });
            self.drop_scopes.last_mut().unwrap().locals.push(local_id);
            if let Pat::Bind { id, subpat: None } = self.store[it]
                && matches!(
                    self.store[id].mode,
                    BindingAnnotation::Unannotated | BindingAnnotation::Mutable
                )
            {
                self.result.binding_locals.insert(id, local_id);
            }
            local_id
        }));
        // The rest of the bindings get a local where they are declared.
        for (id, _) in self.store.bindings() {
            if pick_binding(id) && !self.result.binding_locals.contains_idx(id) {
                self.undeclared.insert(id);
            }
        }
        let mut current = self.result.start_block;
        if let Some(self_binding) = self_binding {
            let local = self.result.param_locals.clone()[base_param_count];
            if local != self.binding_local(self_binding)? {
                let r = self.match_self_param(self_binding, current, local)?;
                if let Some(b) = r.1 {
                    self.set_terminator(b, TerminatorKind::Unreachable, MirSpan::SelfParam);
                }
                current = r.0;
            }
        }
        let local_params = self
            .result
            .param_locals
            .clone()
            .into_iter()
            .skip(base_param_count + self_binding.is_some() as usize);
        for ((param, _), local) in params.zip(local_params) {
            if let Pat::Bind { id, .. } = self.store[param]
                && local == self.binding_local(id)?
            {
                continue;
            }
            let scope = self.drop_scopes.iter().rposition(|it| !it.temporaries);
            let declared = scope.map_or(0, |it| self.drop_scopes[it].locals.len());
            let r = self.pattern_match(current, None, local.into(), param)?;
            // What a parameter's pattern binds is dropped before the parameters ahead of it.
            if let Some(scope) = scope.map(|it| &mut self.drop_scopes[it].locals)
                && let Some(param) = scope.iter().position(|&it| it == local)
                && param < declared
            {
                let bound = scope.split_off(declared);
                scope.splice(param + 1..param + 1, bound);
            }
            if let Some(b) = r.1 {
                self.set_terminator(b, TerminatorKind::Unreachable, param.into());
            }
            current = r.0;
        }
        Ok(current)
    }

    /// Gives each binding of `pat` that has none a local, in the order of the pattern.
    fn declare_bindings(&mut self, pat: PatId) {
        let mut bindings = Vec::new();
        self.store.walk_bindings_in_pat(pat, |b| bindings.push(b));
        for b in bindings {
            self.declare_binding(b);
        }
    }

    fn declare_binding(&mut self, b: BindingId) {
        if self.undeclared.remove(&b) {
            let local = self.result.locals.alloc(Local { ty: self.infer.binding_ty(b).store() });
            self.result.binding_locals.insert(b, local);
        }
    }

    fn binding_local(&mut self, b: BindingId) -> Result<'db, LocalId> {
        // A binding not declared by its `let` is declared where it is first bound or used:
        // a match arm's when the arm binds, as rustc declares it.
        self.declare_binding(b);
        match self.result.binding_locals.get(b) {
            Some(it) => Ok(*it),
            None => {
                // FIXME: It should never happens, but currently it will happen in some cases, not sure when exactly.
                // never!("Using inaccessible local for binding is always a bug");
                Err(MirLowerError::InaccessibleLocal)
            }
        }
    }

    fn const_eval_discriminant(&self, variant: EnumVariantId) -> Result<'db, i128> {
        let r = self.db.const_eval_discriminant(variant);
        match r {
            Ok(r) => Ok(r),
            Err(e) => {
                let edition = self.edition();
                let db = self.db;
                let loc = variant.lookup(db);
                let name = format!(
                    "{}::{}",
                    EnumSignature::of(db, loc.parent).name.display(db, edition),
                    loc.parent
                        .enum_variants(self.db)
                        .variant_name_by_id(variant)
                        .unwrap()
                        .display(db, edition),
                );
                Err(MirLowerError::ConstEvalError(name.into(), Box::new(e)))
            }
        }
    }

    fn edition(&self) -> Edition {
        self.krate().data(self.db).edition
    }

    /// `Poll::Ready` or `Poll::Pending` of the `async` body being lowered.
    fn poll_variant(&self, ready: bool) -> Result<'db, AggregateKind> {
        let lang_items = self.lang_items();
        let variant = if ready { lang_items.PollReady } else { lang_items.PollPending };
        let variant = variant.ok_or(MirLowerError::LangItemNotFound)?;
        let output = self.result.locals[return_slot()].ty.as_ref();
        let args = GenericArgs::new_from_slice(&[output.into()]);
        Ok(AggregateKind::Adt(variant.into(), args.store()))
    }

    /// `expr.await`: turns `expr` into a future and polls it until it is ready, handing
    /// `Poll::Pending` to whoever polled this body each time it is not.
    fn lower_await(
        &mut self,
        awaitee: ExprId,
        expr_id: ExprId,
        place: Place<'db>,
        current: BasicBlockId,
    ) -> Result<'db, Option<BasicBlockId>> {
        let Some(CoroutineLocals { poll, context }) = self.result.coroutine else {
            not_supported!("await outside of an async block");
        };
        let lang_items = self.lang_items();
        let (Some(into_future), Some(future_poll), Some(pin), Some(poll_enum), Some(trait_)) = (
            lang_items.IntoFutureIntoFuture,
            lang_items.FuturePoll,
            lang_items.Pin,
            lang_items.Poll,
            lang_items.IntoFuture,
        ) else {
            return Err(MirLowerError::LangItemNotFound);
        };
        let Some(future_alias) = trait_
            .trait_items(self.db)
            .associated_type_by_name(&Name::new_symbol_root(intern::sym::IntoFuture))
        else {
            return Err(MirLowerError::LangItemNotFound);
        };
        let interner = self.interner();
        let span = MirSpan::from(expr_id);
        let awaitee_ty = self.expr_ty_after_adjustments(awaitee);
        let future_ty = Ty::new_projection(interner, future_alias.into(), [awaitee_ty]);
        // rustc's `into_future(e)` writes to a temporary made before what `e` makes.
        let future = self.result.locals.alloc(Local { ty: future_ty.store() });
        let Some((awaitee_value, current)) = self.lower_expr_to_some_operand(awaitee, current)?
        else {
            return Ok(None);
        };
        let output_ty = self.expr_ty_without_adjust(expr_id);
        let erased = Region::new_erased(interner);
        let future_ref_ty = Ty::new_ref(interner, erased, future_ty, Mutability::Mut);
        let pin_args = GenericArgs::new_from_slice(&[future_ref_ty.into()]);
        let pin_ty = Ty::new_adt(interner, AdtId::StructId(pin), pin_args);
        let poll_ty = Ty::new_adt(
            interner,
            AdtId::EnumId(poll_enum),
            GenericArgs::new_from_slice(&[output_ty.into()]),
        );
        let fn_operand = |func, self_ty: Ty<'db>| {
            Operand::const_zst(Ty::new_fn_def(
                interner,
                CallableDefId::FunctionId(func).into(),
                GenericArgs::new_from_slice(&[self_ty.into()]),
            ))
        };
        let moved =
            |place: Place<'db>| Operand { kind: OperandKind::Move(place.store()), span: None };
        let mutable = BorrowKind::Mut { kind: super::MutBorrowKind::Default };

        // The future is dropped as soon as it is ready, before the rest of the statement runs.
        let scope = self.push_drop_scope();
        self.push_storage_live_for_local(future, current, MirSpan::Unknown)?;
        let future: Place<'db> = future.into();
        let Some(current) = self.lower_call(
            fn_operand(into_future, awaitee_ty),
            Box::new([awaitee_value]),
            future,
            current,
            false,
            span,
        )?
        else {
            scope.pop_assume_dropped(self);
            return Ok(None);
        };
        // rustc's `match into_future(e) { mut __awaitee => loop { .. } }`: the future polled is
        // a binding of its own, which takes it from the temporary `into_future` returned into.
        let awaitee: Place<'db> = self.temp(future_ty, current, MirSpan::Unknown)?.into();
        self.push_assignment(current, awaitee, moved(future).into(), span);
        let again = self.new_basic_block();
        self.set_goto(current, again, span);

        // rustc's `loop { match poll(..) { Ready(result) => break result, Pending => {} } yield }`:
        // what one poll makes belongs to the `match` statement and is gone before the body
        // yields, so the future does not keep it.
        let polling = self.push_drop_scope();
        let future_ref: Place<'db> = self.temp(future_ref_ty, again, MirSpan::Unknown)?.into();
        self.push_assignment(again, future_ref, Rvalue::Ref(mutable, awaitee.store()), span);
        let pinned: Place<'db> = self.temp(pin_ty, again, MirSpan::Unknown)?.into();
        self.push_assignment(
            again,
            pinned,
            Rvalue::Aggregate(
                AggregateKind::Adt(pin.into(), pin_args.store()),
                Box::new([moved(future_ref)]),
            ),
            span,
        );
        let context_ty = self.result.locals[context].ty.as_ref();
        let reborrowed: Place<'db> = self.temp(context_ty, again, MirSpan::Unknown)?.into();
        self.push_assignment(
            again,
            reborrowed,
            Rvalue::Ref(mutable, Place::from(context).project(ProjectionElem::Deref).store()),
            span,
        );
        let polled: Place<'db> = self.temp(poll_ty, again, MirSpan::Unknown)?.into();
        let Some(current) = self.lower_call(
            fn_operand(future_poll, future_ty),
            Box::new([moved(pinned), moved(reborrowed)]),
            polled,
            again,
            false,
            span,
        )?
        else {
            polling.pop_assume_dropped(self);
            scope.pop_assume_dropped(self);
            return Ok(None);
        };
        let discriminant = self.discr_temp_place(current);
        self.push_assignment(current, discriminant, Rvalue::Discriminant(polled.store()), span);
        let (ready, pending) = (self.new_basic_block(), self.new_basic_block());
        self.set_terminator(
            current,
            TerminatorKind::SwitchInt {
                discr: Operand { kind: OperandKind::Move(discriminant.store()), span: None },
                targets: SwitchTargets::static_if(0, ready, pending),
            },
            span,
        );

        let Some(ready_variant) = lang_items.PollReady else {
            return Err(MirLowerError::LangItemNotFound);
        };
        let value = polled
            .project(ProjectionElem::Downcast(ready_variant.into()))
            .project(ProjectionElem::Field(FieldIndex(0)));
        self.push_assignment(ready, place, moved(value).into(), span);
        let ready = self.drop_until_scope(self.drop_scopes.len() - 1, ready, span);
        let pending = polling.pop_and_drop(self, pending, span);

        let poll = Place::from(poll);
        self.push_assignment(
            pending,
            poll,
            Rvalue::Aggregate(self.poll_variant(false)?, Box::new([])),
            span,
        );
        // A future dropped while it waits here drops what is in scope, innermost first, as
        // rustc's coroutine drop does.
        let dropped = self.new_basic_block();
        let end = match self.coroutine_dropped {
            Some(it) => it,
            None => {
                let block = self.new_basic_block();
                self.set_terminator(block, TerminatorKind::CoroutineDrop, span);
                *self.coroutine_dropped.insert(block)
            }
        };
        self.exit_to(0, dropped, end, span);
        self.set_terminator(
            pending,
            TerminatorKind::Yield {
                value: Operand { kind: OperandKind::Copy(poll.store()), span: None },
                resume: again,
                resume_arg: Place::from(context).store(),
                drop: Some(dropped),
            },
            span,
        );
        Ok(Some(scope.pop_and_drop(self, ready, span)))
    }

    fn krate(&self) -> Crate {
        self.owner.krate(self.db)
    }

    fn drop_until_scope(
        &mut self,
        scope_index: usize,
        mut current: BasicBlockId,
        span: MirSpan,
    ) -> BasicBlockId {
        for index in (scope_index..self.drop_scopes.len()).rev() {
            let scope = self.drop_scopes[index].clone();
            let written = self.emit_drop_and_storage_dead_for_scope(&scope, &mut current, span);
            self.drop_scopes[index].exits.extend(written);
        }
        current
    }

    /// Has `current` leave the scopes from `scope_index` in, dropping what they hold innermost
    /// first and ending its storage, and go on to `target`. As in rustc's drop tree, a drop of a
    /// value is a block shared with every other way out to `target` that leaves the same
    /// locals after it; the storage this way out alone ends is ended in `current`.
    fn exit_to(
        &mut self,
        scope_index: usize,
        current: BasicBlockId,
        target: BasicBlockId,
        span: MirSpan,
    ) {
        let mut at = target;
        // The locals whose storage ends after the drop that comes next and before `at`, in the
        // order they were declared, with their scope.
        let mut storage: Vec<(usize, LocalId)> = Vec::new();
        for index in scope_index..self.drop_scopes.len() {
            let locals = self.leaving_order(index);
            for (position, &local) in locals.iter().enumerate() {
                if self.is_copy(self.result.locals[local].ty.as_ref()) {
                    storage.push((index, local));
                    continue;
                }
                let ended: Vec<LocalId> = storage.iter().map(|it| it.1).collect();
                let key = (at, local, ended);
                if let Some(&node) = self.exit_nodes.get(&key) {
                    at = node;
                    storage.clear();
                    continue;
                }
                let (node, dropped) = (self.new_basic_block(), self.new_basic_block());
                let mut before: Vec<LocalId> =
                    (0..index).flat_map(|outer| self.leaving_order(outer)).collect();
                before.extend_from_slice(&locals[..position]);
                let unwind = self.unwind_through(&before, Some(local));
                self.set_terminator(
                    node,
                    TerminatorKind::Drop {
                        place: Place::from(local).store(),
                        target: dropped,
                        unwind,
                    },
                    span,
                );
                self.push_statement(dropped, StatementKind::StorageDead(local).with_span(span));
                self.drop_scopes[index].exits.extend([node, dropped]);
                self.end_storage(dropped, &mut storage, span);
                self.set_goto(dropped, at, span);
                self.exit_nodes.insert(key, node);
                at = node;
            }
        }
        self.end_storage(current, &mut storage, span);
        self.set_goto(current, at, span);
    }

    /// Ends the storage of `storage`, the locals of an exit's scopes with their scope, in
    /// `block`, the last declared first, and empties it.
    fn end_storage(
        &mut self,
        block: BasicBlockId,
        storage: &mut Vec<(usize, LocalId)>,
        span: MirSpan,
    ) {
        let mut last_scope = None;
        for (scope, local) in storage.drain(..).rev() {
            self.push_statement(block, StatementKind::StorageDead(local).with_span(span));
            if last_scope != Some(scope) {
                self.drop_scopes[scope].exits.push(block);
                last_scope = Some(scope);
            }
        }
    }

    /// Writes the drops that left `scope` early at its `end`.
    fn exits_end_at(&mut self, scope: &DropScope, end: MirSpan) {
        if scope.exits.is_empty() {
            return;
        }
        let locals: FxHashSet<LocalId> = scope.locals.iter().copied().collect();
        let mut exits = scope.exits.clone();
        exits.sort_unstable_by_key(|it| it.into_raw());
        exits.dedup();
        for block in exits {
            let block = &mut self.result.basic_blocks[block];
            for statement in &mut block.statements {
                if let StatementKind::StorageDead(local) = statement.kind
                    && locals.contains(&local)
                {
                    statement.span = end;
                }
            }
            if let Some(Terminator { span, kind: TerminatorKind::Drop { place, .. } }) =
                &mut block.terminator
                && locals.contains(&place.local)
            {
                *span = end;
            }
        }
    }

    /// The way out of a match arm whose guard was false. What the guard bound is dropped, and
    /// what the pattern took out of the matched value goes back, for the arms that follow and
    /// for whoever drops that value.
    fn guard_failed(
        &mut self,
        mut current: BasicBlockId,
        bound_by_pattern: usize,
        span: MirSpan,
    ) -> BasicBlockId {
        if let Some(arm) = self.drop_scopes.last() {
            let by_guard = DropScope {
                locals: arm.locals.get(bound_by_pattern..).unwrap_or_default().to_vec(),
                ..Default::default()
            };
            self.emit_drop_and_storage_dead_for_scope(&by_guard, &mut current, span);
        }
        current
    }

    /// Where the guard of an arm held: what the arm binds by value is moved into its bindings.
    /// They are declared there, but dropped after what the guard binds, as the pattern's other
    /// bindings are: in the arm's scope they go at `bound`, before what the guard added.
    fn guard_held(
        &mut self,
        current: BasicBlockId,
        moves: Vec<GuardedMove<'db>>,
        bound: usize,
    ) -> Result<'db, ()> {
        let mut at = bound;
        for GuardedMove { binding, local, place, span } in moves {
            let known = self.drop_scopes.last().is_some_and(|it| it.locals.contains(&local));
            self.push_storage_live(binding, current)?;
            if let Some(arm) = self.drop_scopes.last_mut()
                && !known
                && arm.locals.last() == Some(&local)
            {
                arm.locals.pop();
                arm.locals.insert(at.min(arm.locals.len()), local);
                at += 1;
            }
            let mode = crate::BindingMode(crate::ByRef::No, rustc_ast_ir::Mutability::Not);
            self.push_match_assignment(current, local, mode, place, span);
        }
        Ok(())
    }

    /// Lowers a match arm's guard to the blocks where it holds and where it fails, switching
    /// with the span `span`.
    fn lower_guard(
        &mut self,
        guard: ExprId,
        current: BasicBlockId,
        span: MirSpan,
    ) -> Result<'db, Option<(BasicBlockId, BasicBlockId)>> {
        if self.has_let(guard) {
            // What a `let` binds lives to the end of the arm, and so do the temporaries of
            // what it matched on. Each operand of a chain branches on its own, as in `if`, so
            // what a `let` binds is initialized where the guard holds.
            return self.lower_let_chain(guard, current, true);
        }
        let temporaries = self.push_drop_scope();
        self.drop_scopes.last_mut().unwrap().temporaries = true;
        let Some((discr, current)) = self.lower_expr_to_some_operand(guard, current)? else {
            temporaries.pop_assume_dropped(self);
            return Ok(None);
        };
        // The arm switches on the guard's value after its temporaries are gone, so the
        // value outlives them.
        if let OperandKind::Copy(place) | OperandKind::Move(place) = &discr.kind
            && place.projection.as_slice().is_empty()
        {
            let local = place.local;
            self.drop_scopes.last_mut().unwrap().locals.retain(|&it| it != local);
        }
        let current = temporaries.pop_and_drop(self, current, guard.into());
        let (held, failed) = (self.new_basic_block(), self.new_basic_block());
        self.set_terminator(
            current,
            TerminatorKind::SwitchInt { discr, targets: SwitchTargets::static_if(1, held, failed) },
            span,
        );
        Ok(Some((held, failed)))
    }

    fn push_drop_scope(&mut self) -> DropScopeToken {
        self.drop_scopes.push(DropScope::default());
        DropScopeToken
    }

    /// Don't call directly
    fn pop_drop_scope_assume_dropped_internal(&mut self) {
        if let Some(scope) = self.drop_scopes.pop() {
            self.result.scope_ends.extend(scope.locals.iter().rev());
        }
    }

    /// Don't call directly
    fn pop_drop_scope_internal(
        &mut self,
        mut current: BasicBlockId,
        span: MirSpan,
    ) -> BasicBlockId {
        let scope = self.drop_scopes.pop().unwrap();
        self.result.scope_ends.extend(scope.locals.iter().rev());
        self.exits_end_at(&scope, span.end());
        self.emit_drop_and_storage_dead_for_scope(&scope, &mut current, span.end());
        current
    }

    fn pop_drop_scope_assert_finished(
        &mut self,
        mut current: BasicBlockId,
        span: MirSpan,
    ) -> Result<'db, BasicBlockId> {
        current = self.pop_drop_scope_internal(current, span);
        if !self.drop_scopes.is_empty() {
            implementation_error!("Mismatched count between drop scope push and pops");
        }
        Ok(current)
    }

    /// Drops the value an assignment of `value` is about to overwrite. If that drop unwinds,
    /// the assignment still happens before the scopes' drops, so that the place is not dropped
    /// twice: rustc's drop-and-replace. With no `value`, the place just goes.
    fn drop_assigned(
        &mut self,
        place: Place<'db>,
        ty: Ty<'db>,
        current: BasicBlockId,
        span: MirSpan,
        value: Option<Rvalue>,
    ) -> BasicBlockId {
        if self.is_copy(ty) {
            return current;
        }
        let next = self.new_basic_block();
        let mut unwind = self.unwind_here();
        if let Some(value) = value {
            let target = match unwind {
                Some(it) => it,
                None => self.resume_block(),
            };
            unwind = Some(self.result.basic_blocks.alloc(BasicBlock {
                statements: vec![StatementKind::Assign(place.store(), value).with_span(span)],
                terminator: Some(Terminator { span, kind: TerminatorKind::Goto { target } }),
                is_cleanup: true,
            }));
        }
        self.set_terminator(
            current,
            TerminatorKind::Drop { place: place.store(), target: next, unwind },
            span,
        );
        next
    }

    fn emit_drop_and_storage_dead_for_scope(
        &mut self,
        scope: &DropScope,
        current: &mut Idx<BasicBlock>,
        span: MirSpan,
    ) -> Vec<BasicBlockId> {
        let locals = scope.in_order(&self.operand_locals).into_owned();
        let mut written = vec![*current];
        for (at, &l) in locals.iter().enumerate().rev() {
            if !self.is_copy(self.result.locals[l].ty.as_ref()) {
                let unwind = self.unwind_from_drop(&locals, at);
                let prev = std::mem::replace(current, self.new_basic_block());
                written.push(*current);
                self.set_terminator(
                    prev,
                    TerminatorKind::Drop {
                        place: Place::from(l).store(),
                        target: *current,
                        unwind,
                    },
                    span,
                );
            }
            self.push_statement(*current, StatementKind::StorageDead(l).with_span(span));
        }
        written
    }
}

fn convert_closure_capture_projections(
    _db: &dyn HirDatabase,
    place: &HirPlace,
) -> impl Iterator<Item = PlaceElem> {
    place.projections.iter().enumerate().flat_map(|(i, proj)| {
        let mut projections = ArrayVec::<PlaceElem, 2>::new();
        match proj.kind {
            HirProjectionKind::Deref => projections.push(ProjectionElem::Deref),
            HirProjectionKind::Field { field_idx, variant_idx } => {
                let ty = place.ty_before_projection(i);
                match ty.kind() {
                    TyKind::Tuple(_) => {
                        projections.push(ProjectionElem::Field(FieldIndex(field_idx)))
                    }
                    TyKind::Adt(adt_def, _) => {
                        if let AdtId::EnumId(enum_id) = adt_def.def_id() {
                            let variant =
                                enum_id.enum_variants(_db).variants[variant_idx as usize].0.into();
                            projections.push(ProjectionElem::Downcast(variant));
                        }
                        let local_field_id = LocalFieldId::from_raw(RawIdx::from_u32(field_idx));
                        projections.push(ProjectionElem::Field(local_field_id.into()));
                    }
                    _ => panic!("unexpected type"),
                }
            }
            _ => panic!("unexpected projection"),
        }
        projections.into_iter()
    })
}

fn cast_kind<'db>(
    db: &'db dyn HirDatabase,
    source_ty: Ty<'db>,
    target_ty: Ty<'db>,
) -> Result<'db, CastKind> {
    let from = CastTy::from_ty(db, source_ty);
    let cast = CastTy::from_ty(db, target_ty);
    Ok(match (from, cast) {
        (Some(CastTy::Ptr(..) | CastTy::FnPtr), Some(CastTy::Int(_))) => {
            CastKind::PointerExposeAddress
        }
        (Some(CastTy::Int(_)), Some(CastTy::Ptr(..))) => CastKind::PointerFromExposedAddress,
        (Some(CastTy::Int(_)), Some(CastTy::Int(_))) => CastKind::IntToInt,
        (Some(CastTy::FnPtr), Some(CastTy::Ptr(..))) => CastKind::FnPtrToPtr,
        (Some(CastTy::Float), Some(CastTy::Int(_))) => CastKind::FloatToInt,
        (Some(CastTy::Int(_)), Some(CastTy::Float)) => CastKind::IntToFloat,
        (Some(CastTy::Float), Some(CastTy::Float)) => CastKind::FloatToFloat,
        (Some(CastTy::Ptr(..)), Some(CastTy::Ptr(..))) => CastKind::PtrToPtr,
        _ => not_supported!("Unknown cast between {source_ty:?} and {target_ty:?}"),
    })
}

#[salsa::tracked(returns(as_ref), cycle_result = mir_body_for_closure_cycle_result)]
pub fn mir_body_for_closure_query<'db>(
    db: &'db dyn HirDatabase,
    closure: InternedClosureId<'db>,
) -> Result<'db, MirBody<'db>> {
    lower_closure_body(db, closure.loc(db), CoroutineCaptures::Own)
}

/// The body of an `async` block, as the `Future::poll` of the block's type.
#[salsa::tracked(returns(as_ref), cycle_result = mir_body_for_coroutine_cycle_result)]
pub fn mir_body_for_coroutine_query<'db>(
    db: &'db dyn HirDatabase,
    coroutine: InternedCoroutineId<'db>,
) -> Result<'db, MirBody<'db>> {
    lower_closure_body(db, coroutine.loc(db), CoroutineCaptures::Own)
}

/// The body of the `async` block of an `async` closure that is called by value though it could
/// be called again: the future that makes owns what the closure captured, where the block
/// itself borrows from the closure.
#[salsa::tracked(returns(as_ref), cycle_result = mir_body_for_coroutine_cycle_result)]
pub fn mir_body_for_coroutine_by_move_query<'db>(
    db: &'db dyn HirDatabase,
    coroutine: InternedCoroutineId<'db>,
) -> Result<'db, MirBody<'db>> {
    lower_closure_body(db, coroutine.loc(db), CoroutineCaptures::OfClosure)
}

/// Whether a future of this type, which the `async` block `coroutine` gives, is one that owns
/// what its `async` closure captured.
pub fn is_by_move_coroutine<'db>(
    db: &'db dyn HirDatabase,
    coroutine: InternedCoroutineId<'db>,
    args: CoroutineArgs<DbInterner<'db>>,
) -> bool {
    let InternedClosure { owner, expr, .. } = coroutine.loc(db);
    let store = ExpressionStore::of(db, owner.expression_store_owner(db));
    let Expr::Closure {
        closure_kind: ClosureKind::Coroutine { source: CoroutineSource::Closure, .. },
        ..
    } = &store[expr]
    else {
        return false;
    };
    let closure = ExpressionStore::closure_for_coroutine(expr);
    let TyKind::CoroutineClosure(_, closure_args) =
        InferenceResult::of(db, owner).expr_ty(closure).kind()
    else {
        return false;
    };
    let once = rustc_type_ir::ClosureKind::FnOnce;
    args.kind_ty().to_opt_closure_kind() == Some(once)
        && closure_args.as_coroutine_closure().kind() != once
}

fn mir_body_for_coroutine_cycle_result<'db>(
    _db: &'db dyn HirDatabase,
    _: salsa::Id,
    _def: InternedCoroutineId<'db>,
) -> Result<'db, MirBody<'db>> {
    Err(MirLowerError::Loop)
}

/// Refuses a body with a macro call that failed to expand: what stands in for the expansion
/// is a guess, where rustc stops.
fn check_expansions<'db>(db: &'db dyn HirDatabase, owner: InferBodyId<'db>) -> Result<'db, ()> {
    if let Some(message) = failed_expansion(db, owner) {
        not_supported!("macro call that failed to expand: {message}");
    }
    Ok(())
}

/// Why the first macro call of the body that failed to expand did. A query of its own: the
/// source map it reads changes with every edit above the body, and what it finds does not.
#[salsa::tracked(returns(ref))]
fn failed_expansion<'db>(db: &'db dyn HirDatabase, owner: InferBodyId<'db>) -> Option<String> {
    let source_map = match owner.expression_store_owner(db) {
        ExpressionStoreOwnerId::Body(def) => &Body::unmoved_source_map(db, def).store,
        owner => ExpressionStore::with_source_map(db, owner).1,
    };
    source_map.macro_calls().find_map(|(_, call)| {
        let error = call.parse_macro_expansion(db).err.as_ref()?;
        Some(error.render_to_string(db).message)
    })
}

/// How the body of a closure gets at what the closure captured.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Receiver {
    /// The closure is consumed by the call.
    Owned,
    /// The closure is called again, so a capture it owns has to be used in place.
    Borrowed,
    /// An `async` body is entered once per `poll` through a reference, and takes its captures
    /// out of the future the first time.
    Coroutine,
}

/// What the future of an `async` block holds of its surroundings.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CoroutineCaptures {
    /// What the block itself captures.
    Own,
    /// The arguments of the `async` closure the block is the body of, then what that closure
    /// captures.
    OfClosure,
}

fn lower_closure_body<'db>(
    db: &'db dyn HirDatabase,
    InternedClosure { owner: body_owner, expr, .. }: InternedClosure<'db>,
    held: CoroutineCaptures,
) -> Result<'db, MirBody<'db>> {
    let store = ExpressionStore::of(db, body_owner.expression_store_owner(db));
    let infer = InferenceResult::of(db, body_owner);
    check_expansions(db, body_owner)?;
    let Expr::Closure { args, body: root, .. } = &store[expr] else {
        implementation_error!("closure expression is not closure");
    };
    let Some(closure_data) = infer.closures_data.get(&expr) else {
        not_supported!("closure without capture analysis");
    };
    let captures: Vec<&CapturedPlace> = match held {
        CoroutineCaptures::Own => closure_data.min_captures.values().flatten().collect(),
        CoroutineCaptures::OfClosure => {
            let closure = ExpressionStore::closure_for_coroutine(expr);
            let (Expr::Closure { args, .. }, Some(of_closure)) =
                (&store[closure], infer.closures_data.get(&closure))
            else {
                implementation_error!("async block of a closure that is not one");
            };
            let arguments = closure_data.min_captures.values().flatten().take(args.len());
            arguments.chain(of_closure.min_captures.values().flatten()).collect()
        }
    };
    let mut ctx = MirLowerCtx::new(db, body_owner, store, infer);
    ctx.result.inherits_overflow_checks = store.inherits_overflow_checks(expr);
    let interner = ctx.interner();
    let by_ref = |mutability| {
        Ty::new_ref(interner, Region::new_erased(interner), infer.expr_ty(expr), mutability)
    };

    // 0 is return local
    let (receiver, receiver_ty, inputs) = match infer.expr_ty(expr).kind() {
        crate::next_solver::TyKind::Closure(_, substs) => {
            // The body's own type can still be waiting for a coercion to the declared return
            // type.
            let sig = closure_data.liberated_sig.get();
            ctx.result.locals.alloc(Local { ty: sig.output().store() });
            let (receiver, ty) = match substs.as_closure().kind() {
                rustc_type_ir::ClosureKind::FnOnce => (Receiver::Owned, infer.expr_ty(expr)),
                rustc_type_ir::ClosureKind::FnMut => (Receiver::Borrowed, by_ref(Mutability::Mut)),
                rustc_type_ir::ClosureKind::Fn => (Receiver::Borrowed, by_ref(Mutability::Not)),
            };
            (receiver, ty, sig.inputs())
        }
        crate::next_solver::TyKind::CoroutineClosure(_, substs) => {
            // Returns the future of its `async` block.
            let sig = closure_data.liberated_sig.get();
            ctx.result.locals.alloc(Local { ty: sig.output().store() });
            let (receiver, ty) = match substs.as_coroutine_closure().kind() {
                rustc_type_ir::ClosureKind::FnOnce => (Receiver::Owned, infer.expr_ty(expr)),
                rustc_type_ir::ClosureKind::FnMut => (Receiver::Borrowed, by_ref(Mutability::Mut)),
                rustc_type_ir::ClosureKind::Fn => (Receiver::Borrowed, by_ref(Mutability::Not)),
            };
            (receiver, ty, sig.inputs())
        }
        crate::next_solver::TyKind::Coroutine(id, substs) => {
            let own = substs.as_coroutine();
            ctx.result.locals.alloc(Local { ty: own.return_ty().store() });
            let future = match held {
                CoroutineCaptures::Own => infer.expr_ty(expr),
                CoroutineCaptures::OfClosure => {
                    let once = rustc_type_ir::ClosureKind::FnOnce;
                    let parts = CoroutineArgsParts {
                        parent_args: own.parent_args(),
                        kind_ty: Ty::from_coroutine_closure_kind(interner, once),
                        resume_ty: own.resume_ty(),
                        yield_ty: own.yield_ty(),
                        return_ty: own.return_ty(),
                        tupled_upvars_ty: Ty::new_tup_from_iter(
                            interner,
                            captures.iter().map(|it| it.captured_ty(db)),
                        ),
                    };
                    Ty::new_coroutine(interner, id, CoroutineArgs::new(interner, parts).args)
                }
            };
            let future =
                Ty::new_ref(interner, Region::new_erased(interner), future, Mutability::Mut);
            (Receiver::Coroutine, future, Default::default())
        }
        _ => implementation_error!("closure expression is not closure"),
    };
    let closure_local = ctx.result.locals.alloc(Local { ty: receiver_ty.store() });
    ctx.result.param_locals.push(closure_local);
    if receiver == Receiver::Coroutine {
        let lang_items = ctx.lang_items();
        let (Some(context), Some(poll)) = (lang_items.Context, lang_items.Poll) else {
            return Err(MirLowerError::LangItemNotFound);
        };
        let erased = Region::new_erased(interner);
        let context = Ty::new_adt(
            interner,
            AdtId::StructId(context),
            GenericArgs::new_from_slice(&[erased.into()]),
        );
        let context = ctx
            .result
            .locals
            .alloc(Local { ty: Ty::new_ref(interner, erased, context, Mutability::Mut).store() });
        ctx.result.param_locals.push(context);
        let output = ctx.result.locals[return_slot()].ty.as_ref();
        let poll = Ty::new_adt(
            interner,
            AdtId::EnumId(poll),
            GenericArgs::new_from_slice(&[output.into()]),
        );
        let poll = ctx.result.locals.alloc(Local { ty: poll.store() });
        ctx.result.coroutine = Some(CoroutineLocals { poll, context });
    }
    let resolver_guard = ctx.resolver.update_to_inner_scope(db, ctx.store_owner, expr);
    let current = ctx.lower_params_and_bindings(
        args.iter().zip(inputs.iter()).map(|(it, y)| (*it, *y)),
        None,
        |_| true,
    )?;

    // Push local for every upvar in the closure. rustc doesn't do that, but we have to so we have locals
    // to associate with upvars for borrowck.
    let is_by_ref_closure = receiver == Receiver::Borrowed;
    let mut upvar_map: FxHashMap<LocalId, Vec<Capture<'_>>> = FxHashMap::default();
    for (capture_idx, capture) in captures.into_iter().enumerate() {
        let capture_local = ctx.result.locals.alloc(Local { ty: capture.captured_ty(db).store() });
        ctx.push_storage_live_for_local(capture_local, current, MirSpan::Unknown)?;
        let mut projections = Vec::with_capacity(2);
        if receiver != Receiver::Owned {
            projections.push(ProjectionElem::Deref);
        }
        projections.push(ProjectionElem::Field(FieldIndex(capture_idx as u32)));
        let capture_param_place = StoredPlace {
            local: closure_local,
            projection: Projection::new_from_slice(&projections).store(),
        };
        let capture_local_place = StoredPlace {
            local: capture_local,
            projection: Projection::new_from_slice(&[]).store(),
        };
        let capture_local_rvalue =
            Rvalue::Use(Operand { kind: OperandKind::Move(capture_param_place), span: None });
        // A closure that is only borrowed uses the captures it owns in place (see below), so
        // moving them into the local would drop them on every call.
        if !is_by_ref_closure || capture.is_by_ref() {
            ctx.push_assignment(
                current,
                capture_local_place.as_ref(),
                capture_local_rvalue,
                MirSpan::Unknown,
            );
        }

        let local = capture.captured_local();
        let local = ctx.binding_local(local)?;
        upvar_map.entry(local).or_default().push((capture, capture_local, capture_idx));

        ctx.result
            .upvar_locals
            .entry(capture.captured_local())
            .or_default()
            .push((capture_local, capture.place.clone()));
    }

    ctx.resolver.reset_to_guard(resolver_guard);
    ctx.ends_temporaries.insert(*root);
    if let Some(current) = ctx.lower_expr_to_place(*root, return_slot().into(), current)? {
        let current = ctx.pop_drop_scope_assert_finished(current, root.into())?;
        ctx.set_terminator(current, TerminatorKind::Return, MirSpan::from(*root).end());
    }
    if let Some(CoroutineLocals { poll, .. }) = ctx.result.coroutine {
        let ready = ctx.poll_variant(true)?;
        for (_, block) in ctx.result.basic_blocks.iter_mut() {
            if let Some(Terminator { kind: TerminatorKind::Return, span }) = &block.terminator {
                let output = Operand {
                    kind: OperandKind::Move(Place::from(return_slot()).store()),
                    span: None,
                };
                block.statements.push(
                    StatementKind::Assign(
                        Place::from(poll).store(),
                        Rvalue::Aggregate(ready.clone(), Box::new([output])),
                    )
                    .with_span(*span),
                );
            }
        }
    }
    // What a pattern without bindings matches on is not read, so it is not captured either.
    // The statement becomes a `Nop`, so that `panic_unwinds` still finds the others.
    for (_, block) in ctx.result.basic_blocks.iter_mut() {
        for statement in &mut block.statements {
            if let StatementKind::FakeRead(place) = &statement.kind
                && upvar_map
                    .get(&place.local)
                    .is_some_and(|captures| captured_place(db, captures, place).is_none())
            {
                statement.kind = StatementKind::Nop;
            }
        }
    }
    let mut err = None;
    ctx.result.walk_places(|mir_place| {
        if let Some(hir_places) = upvar_map.get(&mir_place.local) {
            let projections = captured_place(db, hir_places, mir_place);
            let mir_projections = mir_place.projection.as_slice();
            match projections {
                Some((skip_projections_up_to, (hir_place, upvar_local, capture_idx))) => {
                    let in_closure = [
                        ProjectionElem::Deref,
                        ProjectionElem::Field(FieldIndex(capture_idx as u32)),
                    ];
                    let maybe_deref: &[PlaceElem] = if hir_place.is_by_ref() {
                        mir_place.local = upvar_local;
                        &[ProjectionElem::Deref]
                    } else if is_by_ref_closure {
                        // The closure is called again, so a capture it owns has to be used in
                        // place: the local only holds a copy.
                        mir_place.local = closure_local;
                        &in_closure
                    } else {
                        mir_place.local = upvar_local;
                        &[]
                    };
                    mir_place.projection = Projection::new_from_iter(
                        maybe_deref
                            .iter()
                            .copied()
                            .chain(mir_projections[skip_projections_up_to..].iter().copied()),
                    )
                    .store();
                }
                None => err = Some(mir_place.clone()),
            }
        }
    });
    ctx.result.binding_locals = ctx
        .result
        .binding_locals
        .into_iter()
        .filter(|it| ctx.store.binding_owner(it.0) == Some(expr))
        .collect();
    if let Some(err) = err {
        return Err(MirLowerError::UnresolvedUpvar(err));
    }
    ctx.result.mark_moves(&ctx.infcx, ctx.env, &mut ctx.copy);
    ctx.result.shrink_to_fit();
    Ok(ctx.result)
}

/// A capture of a closure, the local it is in while the closure runs, and which capture it is.
type Capture<'a> = (&'a CapturedPlace, LocalId, usize);

/// The capture that `place` is, or is inside of, or holds, and how many of the projections
/// of `place` the capture stands for.
fn captured_place<'a>(
    db: &dyn HirDatabase,
    captures: &[Capture<'a>],
    place: &StoredPlace,
) -> Option<(usize, Capture<'a>)> {
    let mir_projections = place.projection.as_slice();
    captures.iter().find_map(|hir_place| {
        let iter = mir_projections
            .iter()
            .cloned()
            .zip_longest(convert_closure_capture_projections(db, &hir_place.0.place))
            .enumerate();

        for (idx, item) in iter {
            match item {
                EitherOrBoth::Both(mir, hir) => {
                    if mir != hir {
                        // Not this place.
                        return None;
                    }
                }
                EitherOrBoth::Right(_) => {
                    // FIXME: This can happen in fake reads. I believe this is a bug. So we change the fake read's meaning.
                    return Some((mir_projections.len(), *hir_place));
                }
                // This place, but truncated.
                EitherOrBoth::Left(_) => return Some((idx, *hir_place)),
            }
        }
        // Exactly this place.
        Some((mir_projections.len(), *hir_place))
    })
}

#[salsa::tracked(returns(as_ref), cycle_result = mir_body_cycle_result)]
pub fn mir_body_query<'db>(
    db: &'db dyn HirDatabase,
    def: InferBodyId<'db>,
) -> Result<'db, MirBody<'db>> {
    let krate = def.krate(db);
    let edition = krate.data(db).edition;
    let detail = match def {
        InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(it)) => {
            FunctionSignature::of(db, it).name.display(db, edition).to_string()
        }
        InferBodyId::DefWithBodyId(DefWithBodyId::StaticId(it)) => {
            StaticSignature::of(db, it).name.display(db, edition).to_string()
        }
        InferBodyId::DefWithBodyId(DefWithBodyId::ConstId(it)) => ConstSignature::of(db, it)
            .name
            .clone()
            .unwrap_or_else(Name::missing)
            .display(db, edition)
            .to_string(),
        InferBodyId::DefWithBodyId(DefWithBodyId::VariantId(it)) => {
            let loc = it.lookup(db);
            loc.name.display(db, edition).to_string()
        }
        InferBodyId::AnonConstId(_) => "{const}".to_owned(),
    };
    let _p = tracing::info_span!("mir_body_query", ?detail).entered();
    let (store, root_expr, self_param, params) = match def {
        InferBodyId::DefWithBodyId(def) => {
            let body = Body::of(db, def);
            (&**body, body.root_expr(), body.self_param.map(|param| param.formal), &*body.params)
        }
        InferBodyId::AnonConstId(def) => {
            let loc = def.loc(db);
            let store = ExpressionStore::of(db, loc.owner);
            (store, loc.expr, None, &[][..])
        }
    };
    let infer = InferenceResult::of(db, def);
    let mut result = lower_body_to_mir(db, def, store, infer, root_expr, self_param, params)?;
    result.shrink_to_fit();
    Ok(result)
}

/// The body of the constant behind a promoted `&expr` of `def`, which evaluates `expr` alone.
#[salsa::tracked(returns(as_ref))]
pub fn promoted_mir_body_query<'db>(
    db: &'db dyn HirDatabase,
    def: InferBodyId<'db>,
    expr: ExprId,
) -> Result<'db, MirBody<'db>> {
    let store = match def {
        InferBodyId::DefWithBodyId(def) => &**Body::of(db, def),
        InferBodyId::AnonConstId(def) => ExpressionStore::of(db, def.loc(db).owner),
    };
    let infer = InferenceResult::of(db, def);
    if infer.has_type_mismatches() || infer.is_erroneous() {
        return Err(MirLowerError::HasErrors);
    }
    check_expansions(db, def)?;
    let mut ctx = MirLowerCtx::new(db, def, store, infer);
    // The value itself, before what borrowing or indexing it adjusts it to.
    ctx.result.locals.alloc(Local { ty: ctx.expr_ty_without_adjust(expr).store() });
    let current = ctx.lower_params_and_bindings(iter::empty(), None, |_| false)?;
    if let Some(current) =
        ctx.lower_expr_to_place_without_adjust(expr, return_slot().into(), current)?
    {
        let current = ctx.pop_drop_scope_assert_finished(current, expr.into())?;
        ctx.set_terminator(current, TerminatorKind::Return, MirSpan::from(expr).end());
    }
    ctx.result.mark_moves(&ctx.infcx, ctx.env, &mut ctx.copy);
    ctx.result.shrink_to_fit();
    Ok(ctx.result)
}

fn mir_body_cycle_result<'db>(
    _db: &'db dyn HirDatabase,
    _: salsa::Id,
    _def: InferBodyId<'db>,
) -> Result<'db, MirBody<'db>> {
    Err(MirLowerError::Loop)
}

fn mir_body_for_closure_cycle_result<'db>(
    _db: &'db dyn HirDatabase,
    _: salsa::Id,
    _def: InternedClosureId<'db>,
) -> Result<'db, MirBody<'db>> {
    Err(MirLowerError::Loop)
}

/// Extracts params from `body.params`/`body.self_param` and the callable signature,
/// then delegates to [`lower_to_mir_with_store`].
pub fn lower_body_to_mir<'db>(
    db: &'db dyn HirDatabase,
    owner: InferBodyId<'db>,
    store: &ExpressionStore,
    infer: &InferenceResult<'db>,
    root_expr: ExprId,
    self_param: Option<BindingId>,
    params: &[Param<PatId>],
) -> Result<'db, MirBody<'db>> {
    // Extract params and self_param only when lowering the body's root expression for a function.
    if let Some(fid) = owner.as_function() {
        let callable_sig = {
            let resolver = owner.resolver(db);
            let interner = DbInterner::new_with(db, resolver.krate());
            interner.liberate_late_bound_regions(
                fid.into(),
                db.callable_item_signature(fid.into()).instantiate_identity().skip_norm_wip(),
            )
        };
        let mut param_tys = callable_sig.inputs().iter().copied();
        let self_param = self_param.and_then(|id| Some((id, param_tys.next()?)));
        // The signature has no type for `...`, whose pattern is the last and binds the list.
        let list = params.last().filter(|_| callable_sig.c_variadic());
        let param_tys = param_tys.chain(list.map(|list| infer.pat_ty(list.formal)));

        lower_to_mir_with_store(
            db,
            owner,
            store,
            infer,
            root_expr,
            params.iter().map(|param| param.formal).zip(param_tys),
            self_param,
        )
    } else {
        lower_to_mir_with_store(db, owner, store, infer, root_expr, iter::empty(), None)
    }
}

/// # Parameters
/// - `is_root`: `true` when `root_expr` is the body's top-level expression (picks
///   bindings with no owner); `false` when lowering an inline const or anonymous
///   const (picks bindings owned by `root_expr`).
pub fn lower_to_mir_with_store<'db>(
    db: &'db dyn HirDatabase,
    owner: InferBodyId<'db>,
    store: &ExpressionStore,
    infer: &InferenceResult<'db>,
    root_expr: ExprId,
    params: impl Iterator<Item = (PatId, Ty<'db>)> + Clone,
    self_param: Option<(BindingId, Ty<'db>)>,
) -> Result<'db, MirBody<'db>> {
    check_expansions(db, owner)?;
    if infer.has_type_mismatches() || infer.is_erroneous() {
        if std::env::var_os("RUSTD_DUMP_ERRORS").is_some() {
            for (expr, data) in store.exprs() {
                if infer.expr_has_type_mismatch(expr) {
                    eprintln!("mismatch at {data:?}: {:?}", infer.expr_ty(expr));
                }
            }
            for (pat, data) in store.pats() {
                if infer.pat_has_type_mismatch(pat) {
                    eprintln!("mismatch at {data:?}: {:?}", infer.pat_ty(pat));
                }
            }
            eprintln!("diagnostics: {:?}", infer.diagnostics());
        }
        return Err(MirLowerError::HasErrors);
    }
    // A call with too many or too few arguments still has a type for everything in it.
    let miscounted = |it: &crate::infer::InferenceDiagnostic| {
        matches!(it, crate::infer::InferenceDiagnostic::MismatchedArgCount { .. })
    };
    if infer.diagnostics().iter().any(miscounted) {
        return Err(MirLowerError::HasErrors);
    }
    let mut ctx = MirLowerCtx::new(db, owner, store, infer);
    // 0 is return local
    ctx.result.locals.alloc(Local { ty: ctx.expr_ty_after_adjustments(root_expr).store() });
    let expected_binding_owner =
        if matches!(owner, InferBodyId::DefWithBodyId(_)) { None } else { Some(root_expr) };
    let binding_picker = |b: BindingId| ctx.store.binding_owner(b) == expected_binding_owner;
    let current = ctx.lower_params_and_bindings(params, self_param, binding_picker)?;
    ctx.ends_temporaries.insert(root_expr);
    if let Some(current) = ctx.lower_expr_to_place(root_expr, return_slot().into(), current)? {
        let current = ctx.pop_drop_scope_assert_finished(current, root_expr.into())?;
        ctx.set_terminator(current, TerminatorKind::Return, MirSpan::from(root_expr).end());
    }
    ctx.result.mark_moves(&ctx.infcx, ctx.env, &mut ctx.copy);
    Ok(ctx.result)
}

/// `value` as a `usize` of the target.
fn word_bytes(value: usize) -> Box<[u8]> {
    let word = crate::mir::target::word();
    value.to_le_bytes()[..word].into()
}

/// Whether `base[index]` reads an array or a slice in place by a `usize`, and is not a call
/// of `Index::index`.
pub fn is_builtin_index(infer: &InferenceResult, base: ExprId, index: ExprId) -> bool {
    let adjusted = |expr: ExprId| {
        let adjustments = infer.expr_adjustments.get(&expr);
        adjustments
            .and_then(|it| it.last())
            .map_or_else(|| infer.expr_ty(expr), |it| it.target.as_ref())
    };
    matches!(adjusted(index).kind(), TyKind::Uint(rustc_ast_ir::UintTy::Usize))
        && matches!(adjusted(base).strip_reference().kind(), TyKind::Array(..) | TyKind::Slice(..))
}
