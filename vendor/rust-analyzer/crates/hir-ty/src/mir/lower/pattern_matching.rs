//! MIR lowering for patterns

use hir_def::{
    hir::{ExprId, RecordFieldPat},
    signatures::VariantFields,
};
use rustc_type_ir::inherent::{IntoKind, Ty as _};

use crate::{
    Adjust, Adjustment, BindingMode, ByRef,
    mir::{
        CastKind, FieldIndex, LocalId, MutBorrowKind, Operand, OperandKind, Place, Projection,
        lower::{
            AssignTarget, Assigned, BasicBlockId, BinOp, BindingId, BorrowKind, Expr, Idx,
            MemoryMap, MirLowerCtx, MirLowerError, MirSpan, Pat, PatId, PlaceElem, ProjectionElem,
            ResolveValueResult, Result, Rvalue, SwitchTargets, TerminatorKind, Ty, TyKind, ValueNs,
            VariantId,
        },
    },
};
use crate::{
    method_resolution::CandidateId,
    next_solver::{GenericArgs, infer::traits::ObligationCause, obligation_ctxt::ObligationCtxt},
};

macro_rules! not_supported {
    ($x: expr) => {
        return Err(MirLowerError::NotSupported(format!($x)))
    };
}

pub(super) enum AdtPatternShape<'a> {
    Tuple { args: &'a [PatId], ellipsis: Option<u32> },
    Record { args: &'a [RecordFieldPat] },
    Unit,
}

/// We need to do pattern matching in two phases: One to check if the pattern matches, and one to fill the bindings
/// of patterns. This is necessary to prevent double moves and similar problems. For example:
/// ```ignore
/// struct X;
/// match (X, 3) {
///     (b, 2) | (b, 3) => {},
///     _ => {}
/// }
/// ```
/// If we do everything in one pass, we will move `X` to the first `b`, then we see that the second field of tuple
/// doesn't match and we should move the `X` to the second `b` (which here is the same thing, but doesn't need to be) and
/// it might even doesn't match the second pattern and we may want to not move `X` at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchingMode {
    /// Check that if this pattern matches
    Check,
    /// Assume that this pattern matches, fill bindings
    Bind,
    /// Assume that this pattern matches, assign to existing variables.
    Assign,
}

impl<'db> MirLowerCtx<'_, 'db> {
    /// It gets a `current` unterminated block, appends some statements and possibly a terminator to it to check if
    /// the pattern matches and write bindings, and returns two unterminated blocks, one for the matched path (which
    /// can be the `current` block) and one for the mismatched path. If the input pattern is irrefutable, the
    /// mismatched path block is `None`.
    ///
    /// By default, it will create a new block for mismatched path. If you already have one, you can provide it with
    /// `current_else` argument to save an unnecessary jump. If `current_else` isn't `None`, the result mismatched path
    /// wouldn't be `None` as well. Note that this function will add jumps to the beginning of the `current_else` block,
    /// so it should be an empty block.
    pub(super) fn pattern_match(
        &mut self,
        current: BasicBlockId,
        current_else: Option<BasicBlockId>,
        cond_place: Place<'db>,
        pattern: PatId,
    ) -> Result<'db, (BasicBlockId, Option<BasicBlockId>)> {
        self.pattern_match_then_bind(current, current_else, cond_place, pattern, false)
    }

    /// [`Self::pattern_match`] for a candidate rustc links to the one it tries next, a later
    /// arm or what runs when a refutable `let` does not match: a false edge from where the
    /// pattern has matched, before it binds, to the mismatched path, which the borrow checker
    /// follows and nothing runs. So the borrow checker does not know which patterns cannot
    /// fail, and the mismatched path is there even where this one cannot.
    pub(super) fn pattern_match_linked(
        &mut self,
        current: BasicBlockId,
        cond_place: Place<'db>,
        pattern: PatId,
    ) -> Result<'db, (BasicBlockId, BasicBlockId)> {
        let (then, otherwise) =
            self.pattern_match_then_bind(current, None, cond_place, pattern, true)?;
        let otherwise = otherwise.ok_or(MirLowerError::ImplementationError(
            "a linked pattern without a mismatched path".into(),
        ))?;
        Ok((then, otherwise))
    }

    /// A false edge from `current`, rustc's `false_edges`: on to the block returned, which
    /// runs, and to `imaginary`, which only the borrow checker follows.
    fn false_edge(
        &mut self,
        current: BasicBlockId,
        imaginary: BasicBlockId,
        span: MirSpan,
    ) -> BasicBlockId {
        let real = self.new_basic_block();
        self.set_terminator(
            current,
            TerminatorKind::FalseEdge { real_target: real, imaginary_target: imaginary },
            span,
        );
        real
    }

    fn pattern_match_then_bind(
        &mut self,
        current: BasicBlockId,
        current_else: Option<BasicBlockId>,
        cond_place: Place<'db>,
        pattern: PatId,
        linked: bool,
    ) -> Result<'db, (BasicBlockId, Option<BasicBlockId>)> {
        let (mut current, mut current_else) = self.pattern_match_inner(
            current,
            current_else,
            cond_place,
            pattern,
            MatchingMode::Check,
        )?;
        if linked {
            let next = *current_else.get_or_insert_with(|| self.new_basic_block());
            current = self.false_edge(current, next, pattern.into());
        }
        let current = match std::mem::take(&mut self.arm_unreached) {
            true => self.unreached_block(current, pattern.into()),
            false => current,
        };
        // Bindings are dropped in the reverse of the order the pattern names them, whichever
        // alternative of an or-pattern ends up binding them. Of a guarded arm, what the first
        // alternative binds by value is declared once the guard holds, after the rest.
        let mut bindings = Vec::new();
        let mut alternatives = false;
        self.store.walk_pats(pattern, &mut |pat| match &self.store[pat] {
            Pat::Or(_) => alternatives = true,
            &Pat::Bind { id, .. } if !bindings.contains(&id) => bindings.push(id),
            _ => (),
        });
        if alternatives {
            let mut by_value = Vec::new();
            if self.guarded_moves.is_some() {
                self.first_alternative_by_value(pattern, &mut by_value);
            }
            for binding in bindings {
                if !by_value.contains(&binding) {
                    self.push_storage_live(binding, current)?;
                }
            }
        }
        let (current, current_else) = self.pattern_match_inner(
            current,
            current_else,
            cond_place,
            pattern,
            MatchingMode::Bind,
        )?;
        Ok((current, current_else))
    }

    /// The bindings `pattern` binds by value in the first alternative of each of its
    /// or-patterns, which rustc's binding modes for a guarded arm follow.
    fn first_alternative_by_value(&self, pattern: PatId, found: &mut Vec<BindingId>) {
        match &self.store[pattern] {
            Pat::Or(alternatives) => {
                if let Some(&first) = alternatives.first() {
                    self.first_alternative_by_value(first, found);
                }
            }
            &Pat::Bind { id, .. } => {
                if let BindingMode(ByRef::No, _) = self.infer.binding_modes[pattern] {
                    found.push(id);
                }
                self.store
                    .walk_pats_shallow(pattern, |it| self.first_alternative_by_value(it, found));
            }
            _ => self
                .store
                .walk_pats_shallow(pattern, |it| self.first_alternative_by_value(it, found)),
        }
    }

    /// Goes on from `current` in a block that rustc has no way into: what is moved out there
    /// is not counted where a drop asks what may be gone.
    fn unreached_block(&mut self, current: BasicBlockId, span: MirSpan) -> BasicBlockId {
        let block = self.new_basic_block();
        self.set_goto(current, block, span);
        self.result.unreached.push(block);
        block
    }

    /// Every way of choosing one alternative from each or-pattern in `pattern`, in the order
    /// rustc tries them. A choice maps an or-pattern to the index of its alternative.
    pub(super) fn or_expansions(&self, pattern: PatId) -> Vec<Vec<(PatId, usize)>> {
        if let Pat::Or(alternatives) = &self.store[pattern] {
            return alternatives
                .iter()
                .enumerate()
                .flat_map(|(i, &alternative)| {
                    self.or_expansions(alternative).into_iter().map(move |mut choice| {
                        choice.push((pattern, i));
                        choice
                    })
                })
                .collect();
        }
        let mut expansions = vec![Vec::new()];
        self.store[pattern].walk_child_pats(|child| {
            let children = self.or_expansions(child);
            if children.len() == 1 && children[0].is_empty() {
                return;
            }
            expansions = expansions
                .iter()
                .flat_map(|choice| {
                    children.iter().map(move |child| [&choice[..], &child[..]].concat())
                })
                .collect();
        });
        expansions
    }

    pub(super) fn pattern_match_assignment(
        &mut self,
        current: BasicBlockId,
        value: Place<'db>,
        value_span: MirSpan,
        pattern: PatId,
    ) -> Result<'db, BasicBlockId> {
        // Every part is read before any is assigned, so that `(t.1, t.0) = t` swaps.
        let outer = std::mem::take(&mut self.assigned);
        let outer_from = std::mem::replace(&mut self.assigned_from, value_span);
        let matched = self.pattern_match_inner(current, None, value, pattern, MatchingMode::Assign);
        let assigned = std::mem::replace(&mut self.assigned, outer);
        self.assigned_from = outer_from;
        let (mut current, _) = matched?;
        for Assigned { target, part, span } in assigned {
            let (place, ty) = match target {
                AssignTarget::Local(local) => (local.into(), self.result.locals[local].ty.as_ref()),
                AssignTarget::Place(expr) => {
                    let Some((place, next)) =
                        self.lower_expr_as_place_without_adjust(current, expr, false)?
                    else {
                        continue;
                    };
                    current = next;
                    (place, self.expr_ty_without_adjust(expr))
                }
            };
            let value: Rvalue =
                Operand { kind: OperandKind::Move(Place::from(part).store()), span: None }.into();
            current = self.drop_assigned(place, ty, current, span, Some(value.clone()));
            self.push_assignment(current, place, value, span);
        }
        Ok(current)
    }

    /// Reads `value`, a part of what a destructuring assignment destructures, into a
    /// temporary of `ty`, for `target` to be assigned from once every part is read.
    fn read_assigned(
        &mut self,
        current: BasicBlockId,
        target: AssignTarget,
        ty: Ty<'db>,
        value: Rvalue,
        span: MirSpan,
    ) -> Result<'db, ()> {
        let part = self.temp(ty, current, span)?;
        self.push_assignment(current, part.into(), value, self.assigned_from);
        self.assigned.push(Assigned { target, part, span });
        Ok(())
    }

    pub(super) fn match_self_param(
        &mut self,
        id: BindingId,
        current: BasicBlockId,
        local: LocalId,
    ) -> Result<'db, (BasicBlockId, Option<BasicBlockId>)> {
        self.pattern_match_binding(
            id,
            BindingMode(ByRef::No, rustc_ast_ir::Mutability::Not),
            local.into(),
            MirSpan::SelfParam,
            current,
            None,
        )
    }

    fn pattern_match_inner(
        &mut self,
        mut current: BasicBlockId,
        mut current_else: Option<BasicBlockId>,
        mut cond_place: Place<'db>,
        pattern: PatId,
        mode: MatchingMode,
    ) -> Result<'db, (BasicBlockId, Option<BasicBlockId>)> {
        let cnt = self.infer.pat_adjustments.get(&pattern).map(|x| x.len()).unwrap_or_default();
        cond_place.projection = Projection::new_from_iter(
            cond_place
                .projection
                .as_slice()
                .iter()
                .cloned()
                .chain((0..cnt).map(|_| ProjectionElem::Deref)),
        );
        Ok(match &self.store[pattern] {
            Pat::Missing | Pat::Rest | Pat::NotNull => {
                return Err(MirLowerError::IncompletePattern);
            }
            Pat::Wild => (current, current_else),
            Pat::Tuple { args, ellipsis } => {
                let place_ty = self.place_ty(&cond_place);
                let subst = match place_ty.kind() {
                    TyKind::Tuple(s) => s,
                    _ => {
                        return Err(MirLowerError::TypeError(
                            "non tuple type matched with tuple pattern",
                        ));
                    }
                };
                self.pattern_match_tuple_like(
                    current,
                    current_else,
                    args,
                    *ellipsis,
                    (0..subst.len()).map(|i| PlaceElem::Field(FieldIndex(i as u32))),
                    &cond_place,
                    mode,
                )?
            }
            Pat::Or(pats) if let Some(&chosen) = self.or_choices.get(&pattern) => {
                self.pattern_match_inner(current, current_else, cond_place, pats[chosen], mode)?
            }
            Pat::Or(pats) => {
                let then_target = self.new_basic_block();
                let mut finished = false;
                // After an alternative that cannot fail, the rest are reached only by the
                // false edge, and what they bind is not what a guard reads or moves after.
                let mut imaginary = false;
                for (index, pat) in pats.iter().enumerate() {
                    let held = imaginary.then(|| {
                        (self.guarded_moves.as_ref().map(Vec::len), self.guard_refs.clone())
                    });
                    let (mut next, mut next_else) = self.pattern_match_inner(
                        current,
                        None,
                        cond_place,
                        *pat,
                        MatchingMode::Check,
                    )?;
                    // rustc binds each alternative as a candidate of its own, linked to the
                    // next by a false edge, so what any of them binds may have been bound.
                    if mode == MatchingMode::Bind && index + 1 < pats.len() {
                        imaginary |= next_else.is_none();
                        let start = *next_else.get_or_insert_with(|| self.new_basic_block());
                        next = self.false_edge(next, start, pattern.into());
                    }
                    if mode != MatchingMode::Check {
                        (next, _) = self.pattern_match_inner(next, None, cond_place, *pat, mode)?;
                    }
                    if let Some((moves, refs)) = held {
                        if let (Some(len), Some(guarded)) = (moves, &mut self.guarded_moves) {
                            guarded.truncate(len);
                        }
                        self.guard_refs = refs;
                    }
                    self.set_goto(next, then_target, pattern.into());
                    match next_else {
                        Some(t) => {
                            current = t;
                        }
                        None => {
                            finished = true;
                            break;
                        }
                    }
                }
                if !finished {
                    if mode == MatchingMode::Check {
                        let ce = *current_else.get_or_insert_with(|| self.new_basic_block());
                        self.set_goto(current, ce, pattern.into());
                    } else {
                        self.set_terminator(current, TerminatorKind::Unreachable, pattern.into());
                    }
                }
                (then_target, current_else)
            }
            Pat::Record { args, .. } => {
                let Some(variant) = self.infer.variant_resolution_for_pat(pattern) else {
                    not_supported!("unresolved variant for record");
                };
                self.pattern_matching_variant(
                    cond_place,
                    variant,
                    current,
                    pattern.into(),
                    current_else,
                    AdtPatternShape::Record { args },
                    mode,
                )?
            }
            Pat::Range { start, end, range_type } => {
                let mut add_check = |l: &ExprId, binop| -> Result<'db, ()> {
                    let lv =
                        self.lower_literal_or_const_to_operand(self.infer.pat_ty(pattern), l)?;
                    let else_target = *current_else.get_or_insert_with(|| self.new_basic_block());
                    let next = self.new_basic_block();
                    let discr =
                        self.temp(Ty::new_bool(self.interner()), current, pattern.into())?.into();
                    self.push_assignment(
                        current,
                        discr,
                        Rvalue::CheckedBinaryOp(
                            binop,
                            lv,
                            Operand { kind: OperandKind::Copy(cond_place.store()), span: None },
                        ),
                        pattern.into(),
                    );
                    let discr = Operand { kind: OperandKind::Move(discr.store()), span: None };
                    self.set_terminator(
                        current,
                        TerminatorKind::SwitchInt {
                            discr,
                            targets: SwitchTargets::static_if(1, next, else_target),
                        },
                        pattern.into(),
                    );
                    current = next;
                    Ok(())
                };
                if mode == MatchingMode::Check {
                    if let Some(start) = start {
                        add_check(start, BinOp::Le)?;
                    }
                    if let Some(end) = end {
                        let op = match range_type {
                            syntax::ast::RangeOp::Exclusive => BinOp::Gt,
                            syntax::ast::RangeOp::Inclusive => BinOp::Ge,
                        };
                        add_check(end, op)?;
                    }
                }
                (current, current_else)
            }
            Pat::Slice { prefix, slice, suffix } => {
                let pat_ty = self.infer.pat_ty(pattern);
                // FIXME: MIR lowering should be skipped for bodies with inference errors. Once
                // that happens, this recovery for invalid slice patterns can be removed.
                if !matches!(pat_ty.kind(), TyKind::Array(..) | TyKind::Slice(_)) {
                    return Err(MirLowerError::TypeError(
                        "non array or slice type matched with slice pattern",
                    ));
                }

                if mode == MatchingMode::Check {
                    // emit runtime length check for slice
                    if let TyKind::Slice(_) = pat_ty.kind() {
                        let pattern_len = prefix.len() + suffix.len();
                        let place_len = self
                            .temp(Ty::new_usize(self.interner()), current, pattern.into())?
                            .into();
                        self.push_assignment(
                            current,
                            place_len,
                            Rvalue::Len(cond_place.store()),
                            pattern.into(),
                        );
                        let else_target =
                            *current_else.get_or_insert_with(|| self.new_basic_block());
                        let next = self.new_basic_block();
                        if slice.is_none() {
                            self.set_terminator(
                                current,
                                TerminatorKind::SwitchInt {
                                    discr: Operand {
                                        kind: OperandKind::Copy(place_len.store()),
                                        span: None,
                                    },
                                    targets: SwitchTargets::static_if(
                                        pattern_len as u128,
                                        next,
                                        else_target,
                                    ),
                                },
                                pattern.into(),
                            );
                        } else {
                            let c = Operand::from_concrete_const(
                                crate::mir::lower::word_bytes(pattern_len),
                                MemoryMap::default(),
                                Ty::new_usize(self.interner()),
                            );
                            let discr = self
                                .temp(Ty::new_bool(self.interner()), current, pattern.into())?
                                .into();
                            self.push_assignment(
                                current,
                                discr,
                                Rvalue::CheckedBinaryOp(
                                    BinOp::Le,
                                    c,
                                    Operand {
                                        kind: OperandKind::Copy(place_len.store()),
                                        span: None,
                                    },
                                ),
                                pattern.into(),
                            );
                            let discr =
                                Operand { kind: OperandKind::Move(discr.store()), span: None };
                            self.set_terminator(
                                current,
                                TerminatorKind::SwitchInt {
                                    discr,
                                    targets: SwitchTargets::static_if(1, next, else_target),
                                },
                                pattern.into(),
                            );
                        }
                        current = next;
                    }
                }
                for (i, &pat) in prefix.iter().enumerate() {
                    let next_place = cond_place.project(ProjectionElem::ConstantIndex {
                        offset: i as u64,
                        min_length: (prefix.len() + suffix.len()) as u64,
                        from_end: false,
                    });
                    (current, current_else) =
                        self.pattern_match_inner(current, current_else, next_place, pat, mode)?;
                }
                if let &Some(slice) = slice
                    && mode != MatchingMode::Check
                    && let Pat::Bind { id, subpat: _ } = self.store[slice]
                {
                    let next_place = cond_place.project(ProjectionElem::Subslice {
                        from: prefix.len() as u64,
                        to: suffix.len() as u64,
                    });
                    let mode = self.infer.binding_modes[slice];
                    (current, current_else) = self.pattern_match_binding(
                        id,
                        mode,
                        next_place,
                        (slice).into(),
                        current,
                        current_else,
                    )?;
                }
                for (i, &pat) in suffix.iter().enumerate() {
                    // Offset 1 from the end is the last element, as in rustc's MIR.
                    let next_place = cond_place.project(ProjectionElem::ConstantIndex {
                        offset: (suffix.len() - i) as u64,
                        min_length: (prefix.len() + suffix.len()) as u64,
                        from_end: true,
                    });
                    (current, current_else) =
                        self.pattern_match_inner(current, current_else, next_place, pat, mode)?;
                }
                (current, current_else)
            }
            Pat::Path(p) => match self.infer.variant_resolution_for_pat(pattern) {
                Some(variant) => self.pattern_matching_variant(
                    cond_place,
                    variant,
                    current,
                    pattern.into(),
                    current_else,
                    AdtPatternShape::Unit,
                    mode,
                )?,
                None => {
                    let unresolved_name = || MirLowerError::UnresolvedName {
                        owner: self.owner.expression_store_owner(self.db),
                        path: p.clone(),
                    };
                    // A path that starts at a type, `<T>::C`, is one only inference resolves.
                    let associated = match self.infer.assoc_resolutions_for_pat(pattern) {
                        Some((CandidateId::ConstId(c), subst)) => Some((c, subst)),
                        _ => None,
                    };
                    let hygiene = self.store.pat_path_hygiene(pattern);
                    let pr = self.resolver.resolve_path_in_value_ns(self.db, p, hygiene);
                    let pr = match (pr, associated) {
                        (Some(pr), _) => Some(pr),
                        (None, Some(_)) => None,
                        (None, None) => return Err(unresolved_name()),
                    };

                    if let (
                        MatchingMode::Assign,
                        Some(ResolveValueResult::ValueNs(ValueNs::LocalBinding(binding))),
                    ) = (mode, pr.as_ref())
                    {
                        let local = self.binding_local(*binding)?;
                        let ty = self.result.locals[local].ty.as_ref();
                        let value =
                            Operand { kind: OperandKind::Copy(cond_place.store()), span: None };
                        let target = AssignTarget::Local(local);
                        self.read_assigned(current, target, ty, value.into(), pattern.into())?;
                        return Ok((current, current_else));
                    }

                    // A unit struct, by its name or as `Self`, is all there is of its type.
                    if let Some(ResolveValueResult::ValueNs(
                        ValueNs::StructId(_) | ValueNs::ImplSelf(_),
                    )) = pr
                    {
                        return Ok((current, current_else));
                    }

                    // The path is not a variant or a local, so it is a const
                    if mode != MatchingMode::Check {
                        // A const don't bind anything. Only needs check.
                        return Ok((current, current_else));
                    }
                    let (c, subst) = 'b: {
                        if let Some(found) = associated {
                            break 'b found;
                        }
                        if let Some(ResolveValueResult::ValueNs(ValueNs::ConstId(c))) = pr {
                            break 'b (c, GenericArgs::empty());
                        }
                        not_supported!("path in pattern position that is not const or variant")
                    };
                    let tmp =
                        self.temp(self.infer.pat_ty(pattern), current, pattern.into())?.into();
                    let span = pattern.into();
                    self.lower_const(c.into(), current, tmp, subst, span)?;
                    let tmp2 =
                        self.temp(Ty::new_bool(self.interner()), current, pattern.into())?.into();
                    self.push_assignment(
                        current,
                        tmp2,
                        Rvalue::CheckedBinaryOp(
                            BinOp::Eq,
                            Operand { kind: OperandKind::Copy(tmp.store()), span: None },
                            Operand { kind: OperandKind::Copy(cond_place.store()), span: None },
                        ),
                        span,
                    );
                    let next = self.new_basic_block();
                    let else_target = current_else.unwrap_or_else(|| self.new_basic_block());
                    self.set_terminator(
                        current,
                        TerminatorKind::SwitchInt {
                            discr: Operand { kind: OperandKind::Move(tmp2.store()), span: None },
                            targets: SwitchTargets::static_if(1, next, else_target),
                        },
                        span,
                    );
                    (next, Some(else_target))
                }
            },
            Pat::Lit(l) => match &self.store[*l] {
                Expr::Literal(l) => {
                    if mode == MatchingMode::Check {
                        let c = self.lower_literal_to_operand(self.infer.pat_ty(pattern), l)?;
                        self.pattern_match_const(current_else, current, c, cond_place, pattern)?
                    } else {
                        (current, current_else)
                    }
                }
                _ => not_supported!("expression path literal"),
            },
            Pat::Bind { id, subpat } => {
                if let Some(subpat) = subpat {
                    (current, current_else) =
                        self.pattern_match_inner(current, current_else, cond_place, *subpat, mode)?
                }
                if mode != MatchingMode::Check {
                    let mode = self.infer.binding_modes[pattern];
                    self.pattern_match_binding(
                        *id,
                        mode,
                        cond_place,
                        pattern.into(),
                        current,
                        current_else,
                    )?
                } else {
                    (current, current_else)
                }
            }
            Pat::TupleStruct { path: _, args, ellipsis } => {
                let Some(variant) = self.infer.variant_resolution_for_pat(pattern) else {
                    not_supported!("unresolved variant");
                };
                self.pattern_matching_variant(
                    cond_place,
                    variant,
                    current,
                    pattern.into(),
                    current_else,
                    AdtPatternShape::Tuple { args, ellipsis: *ellipsis },
                    mode,
                )?
            }
            Pat::Ref { pat, mutability: _ } => {
                let ty = self.place_ty(&cond_place);
                if !ty.is_ref() {
                    return Err(MirLowerError::TypeError(
                        "non reference type matched with reference pattern",
                    ));
                }
                let cond_place = cond_place.project(ProjectionElem::Deref);
                self.pattern_match_inner(current, current_else, cond_place, *pat, mode)?
            }
            &Pat::Expr(expr) => {
                stdx::always!(
                    mode == MatchingMode::Assign,
                    "Pat::Expr can only come in destructuring assignments"
                );
                let ty = self.expr_ty_without_adjust(expr);
                let value = self.assigned_part(expr, cond_place)?;
                self.read_assigned(current, AssignTarget::Place(expr), ty, value, expr.into())?;
                (current, current_else)
            }
            &Pat::Box { inner } => {
                let cond_place = cond_place.project(ProjectionElem::Deref);
                self.pattern_match_inner(current, current_else, cond_place, inner, mode)?
            }
            Pat::Deref { .. } => not_supported!("deref pattern"),
        })
    }

    /// The part of a destructured value at `part` as it is assigned to `target`. The
    /// adjustments of the target are the coercion of the part to the type of the target.
    fn assigned_part(&self, target: ExprId, part: Place<'db>) -> Result<'db, Rvalue> {
        let part = Operand { kind: OperandKind::Copy(part.store()), span: None };
        let adjustments = self.infer.expr_adjustments.get(&target).map_or(&[][..], |it| &**it);
        // A reborrow is the pointer it is of.
        let adjustments = match adjustments {
            [
                Adjustment { kind: Adjust::Deref(None), .. },
                Adjustment { kind: Adjust::Borrow(_), .. },
                rest @ ..,
            ] => rest,
            all => all,
        };
        match adjustments {
            [] => Ok(part.into()),
            [Adjustment { kind: Adjust::Pointer(cast), target }] => {
                Ok(Rvalue::Cast(CastKind::PointerCoercion(*cast), part, target.clone()))
            }
            _ => not_supported!("a coercion of a part of a destructuring assignment"),
        }
    }

    fn pattern_match_binding(
        &mut self,
        id: BindingId,
        mode: BindingMode,
        cond_place: Place<'db>,
        span: MirSpan,
        current: BasicBlockId,
        current_else: Option<BasicBlockId>,
    ) -> Result<'db, (BasicBlockId, Option<BasicBlockId>)> {
        let target_place = self.binding_local(id)?;
        let ty = self.result.locals[target_place].ty.as_ref();
        if let BindingMode(ByRef::No, _) = mode
            && self.guarded_moves.is_some()
        {
            let interner = self.interner();
            let erased = crate::next_solver::Region::new_erased(interner);
            let ref_ty = Ty::new_ref(interner, erased, ty, rustc_ast_ir::Mutability::Not);
            let reference = self.temp(ref_ty, current, span)?;
            self.push_assignment(
                current,
                reference.into(),
                Rvalue::Ref(BorrowKind::Shared, cond_place.store()),
                span,
            );
            self.guard_refs.insert(id, reference);
            if let Some(moves) = &mut self.guarded_moves {
                moves.push(super::GuardedMove {
                    binding: id,
                    local: target_place,
                    place: cond_place,
                    span,
                });
            }
            return Ok((current, current_else));
        }
        self.push_storage_live(id, current)?;
        self.push_match_assignment(current, target_place, mode, cond_place, span);
        Ok((current, current_else))
    }

    pub(super) fn push_match_assignment(
        &mut self,
        current: BasicBlockId,
        target_place: LocalId,
        mode: BindingMode,
        cond_place: Place<'db>,
        span: MirSpan,
    ) {
        self.push_assignment(
            current,
            target_place.into(),
            match mode {
                BindingMode(ByRef::No, _) => {
                    Operand { kind: OperandKind::Copy(cond_place.store()), span: None }.into()
                }
                BindingMode(ByRef::Yes(rustc_ast_ir::Mutability::Not), _) => {
                    Rvalue::Ref(BorrowKind::Shared, cond_place.store())
                }
                BindingMode(ByRef::Yes(rustc_ast_ir::Mutability::Mut), _) => Rvalue::Ref(
                    BorrowKind::Mut { kind: MutBorrowKind::Default },
                    cond_place.store(),
                ),
            },
            span,
        );
    }

    fn pattern_match_const(
        &mut self,
        current_else: Option<BasicBlockId>,
        current: BasicBlockId,
        c: Operand,
        cond_place: Place<'db>,
        pattern: Idx<Pat>,
    ) -> Result<'db, (BasicBlockId, Option<BasicBlockId>)> {
        let then_target = self.new_basic_block();
        let else_target = current_else.unwrap_or_else(|| self.new_basic_block());
        let discr = self.temp(Ty::new_bool(self.interner()), current, pattern.into())?.into();
        self.push_assignment(
            current,
            discr,
            Rvalue::CheckedBinaryOp(
                BinOp::Eq,
                c,
                Operand { kind: OperandKind::Copy(cond_place.store()), span: None },
            ),
            pattern.into(),
        );
        let discr = Operand { kind: OperandKind::Move(discr.store()), span: None };
        self.set_terminator(
            current,
            TerminatorKind::SwitchInt {
                discr,
                targets: SwitchTargets::static_if(1, then_target, else_target),
            },
            pattern.into(),
        );
        Ok((then_target, Some(else_target)))
    }

    /// The type of a place, normalized: the local of a `?` has the type `<R as Try>::Residual`.
    fn place_ty(&self, place: &Place<'db>) -> Ty<'db> {
        let ty = place.ty(&self.result, &self.infcx, self.env).ty;
        if !matches!(ty.kind(), TyKind::Alias(..)) {
            return ty;
        }
        let mut ocx = ObligationCtxt::new(&self.infcx);
        ocx.structurally_normalize_ty(&ObligationCause::dummy(), self.env, ty).unwrap_or(ty)
    }

    fn pattern_matching_variant(
        &mut self,
        cond_place: Place<'db>,
        variant: VariantId,
        mut current: BasicBlockId,
        span: MirSpan,
        mut current_else: Option<BasicBlockId>,
        shape: AdtPatternShape<'_>,
        mode: MatchingMode,
    ) -> Result<'db, (BasicBlockId, Option<BasicBlockId>)> {
        let place_ty = self.place_ty(&cond_place);
        let Some((place_adt, _)) = place_ty.as_adt() else {
            return Err(MirLowerError::TypeError("non ADT type matched with ADT pattern"));
        };
        if place_adt != variant.adt_id(self.db) {
            return Err(MirLowerError::TypeError("ADT pattern does not match place type"));
        }

        Ok(match variant {
            VariantId::EnumVariantId(v) => {
                // A pattern for the only variant of an enum always matches, and a closure that
                // captures one field of such an enum has no discriminant to read.
                let only_variant = hir_def::Lookup::lookup(&v, self.db)
                    .parent
                    .enum_variants(self.db)
                    .variants
                    .len()
                    == 1;
                if mode == MatchingMode::Check && !only_variant {
                    let e = self.const_eval_discriminant(v)? as u128;
                    let tmp = self.discr_temp_place(current);
                    self.push_assignment(
                        current,
                        tmp,
                        Rvalue::Discriminant(cond_place.store()),
                        span,
                    );
                    let next = self.new_basic_block();
                    let else_target = current_else.get_or_insert_with(|| self.new_basic_block());
                    self.set_terminator(
                        current,
                        TerminatorKind::SwitchInt {
                            discr: Operand { kind: OperandKind::Move(tmp.store()), span: None },
                            targets: SwitchTargets::static_if(e, next, *else_target),
                        },
                        span,
                    );
                    current = next;
                }
                self.pattern_matching_variant_fields(
                    shape,
                    v.fields(self.db),
                    variant,
                    current,
                    current_else,
                    &cond_place,
                    mode,
                )?
            }
            VariantId::StructId(s) => self.pattern_matching_variant_fields(
                shape,
                s.fields(self.db),
                variant,
                current,
                current_else,
                &cond_place,
                mode,
            )?,
            VariantId::UnionId(u) => self.pattern_matching_variant_fields(
                shape,
                u.fields(self.db),
                variant,
                current,
                current_else,
                &cond_place,
                mode,
            )?,
        })
    }

    fn pattern_matching_variant_fields(
        &mut self,
        shape: AdtPatternShape<'_>,
        variant_data: &VariantFields,
        v: VariantId,
        current: BasicBlockId,
        current_else: Option<BasicBlockId>,
        cond_place: &Place<'db>,
        mode: MatchingMode,
    ) -> Result<'db, (BasicBlockId, Option<BasicBlockId>)> {
        let downcast_place = if matches!(v, VariantId::EnumVariantId(_)) {
            cond_place.project(ProjectionElem::Downcast(v))
        } else {
            *cond_place
        };
        Ok(match shape {
            AdtPatternShape::Record { args } => {
                let it = args
                    .iter()
                    .map(|x| {
                        let field_id =
                            variant_data.field(&x.name).ok_or(MirLowerError::UnresolvedField)?;
                        Ok((PlaceElem::Field(field_id.into()), x.pat))
                    })
                    .collect::<Result<'db, Vec<_>>>()?;
                self.pattern_match_adt(
                    current,
                    current_else,
                    it.into_iter(),
                    &downcast_place,
                    mode,
                )?
            }
            AdtPatternShape::Tuple { args, ellipsis } => {
                let fields = variant_data.fields().iter().map(|(x, _)| PlaceElem::Field(x.into()));
                self.pattern_match_tuple_like(
                    current,
                    current_else,
                    args,
                    ellipsis,
                    fields,
                    &downcast_place,
                    mode,
                )?
            }
            AdtPatternShape::Unit => (current, current_else),
        })
    }

    fn pattern_match_adt(
        &mut self,
        mut current: BasicBlockId,
        mut current_else: Option<BasicBlockId>,
        args: impl Iterator<Item = (PlaceElem, PatId)>,
        cond_place: &Place<'db>,
        mode: MatchingMode,
    ) -> Result<'db, (BasicBlockId, Option<BasicBlockId>)> {
        for (proj, arg) in args {
            let cond_place = cond_place.project(proj);
            (current, current_else) =
                self.pattern_match_inner(current, current_else, cond_place, arg, mode)?;
        }
        Ok((current, current_else))
    }

    fn pattern_match_tuple_like(
        &mut self,
        current: BasicBlockId,
        current_else: Option<BasicBlockId>,
        args: &[PatId],
        ellipsis: Option<u32>,
        fields: impl DoubleEndedIterator<Item = PlaceElem> + Clone,
        cond_place: &Place<'db>,
        mode: MatchingMode,
    ) -> Result<'db, (BasicBlockId, Option<BasicBlockId>)> {
        let (al, ar) = args.split_at(ellipsis.map_or(args.len(), |it| it as usize));
        let it = al
            .iter()
            .zip(fields.clone())
            .chain(ar.iter().rev().zip(fields.rev()))
            .map(|(x, y)| (y, *x));
        self.pattern_match_adt(current, current_else, it, cond_place, mode)
    }
}
