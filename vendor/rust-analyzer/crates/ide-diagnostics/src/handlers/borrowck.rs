use syntax::{TextRange, TextSize};

use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};

// Diagnostic: borrowck
//
// An error of rustc's borrow checker: a use of a moved or uninitialized value, a mutation of
// what is not mutable, a borrow that conflicts with another or does not live long enough. Also
// rustc's `unused_mut` lint, which its borrow checker gives: a variable declared `mut` that is
// never mutated.
pub(crate) fn borrowck_error(
    ctx: &DiagnosticsContext<'_, '_>,
    d: &hir::BorrowckError,
) -> Diagnostic {
    // rustc gives some errors no code, as "lifetime may not live long enough".
    let code = match d.code {
        "" => DiagnosticCode::Ra("borrowck", crate::Severity::Error),
        "unused_mut" => DiagnosticCode::RustcLint("unused_mut"),
        code => DiagnosticCode::RustcHardError(code),
    };
    let mut diagnostic = Diagnostic::new_with_syntax_node_ptr(ctx, code, d.message.clone(), d.node);
    // rustc's span of an expression a macro writes, `&$e`, is in the macro's definition, where
    // its first token is; one the caller writes is the caller's. A definition in another file
    // stays at the call.
    if let Some(file) = d.node.file_id.macro_file() {
        let start = d.node.value.text_range().start();
        let (origin, context) = hir::InMacroFile::new(file, start).original_file_range(ctx.sema.db);
        if !context.is_root() && origin.file_id.file_id(ctx.sema.db) == diagnostic.range.file_id {
            diagnostic.range.range = origin.range;
        }
    }
    if d.at_end {
        let range = diagnostic.range.range;
        if let Some(last) = range.end().checked_sub(TextSize::new(1))
            && last >= range.start()
        {
            diagnostic.range.range = TextRange::new(last, range.end());
        }
    }
    diagnostic.stable()
}

// Diagnostic: borrowck-skipped
//
// The borrow checker did not check a body, because MIR lowering does not support something
// in it. Reported only for the borrow checker's test harness.
pub(crate) fn borrowck_skipped(
    ctx: &DiagnosticsContext<'_, '_>,
    d: &hir::BorrowckSkipped,
) -> Diagnostic {
    Diagnostic::new_with_syntax_node_ptr(
        ctx,
        DiagnosticCode::Ra("borrowck-skipped", crate::Severity::WeakWarning),
        format!("not borrow checked: {}", d.reason),
        d.node,
    )
}

#[cfg(test)]
mod tests {
    use crate::tests::check_diagnostics;

    #[test]
    fn impl_trait_captures_elided_lifetime_2024() {
        check_diagnostics(
            r#"
//- minicore: sized, copy
//- /main.rs edition:2024
struct S(i32);
impl S {
    fn get(&self) -> impl Sized { &self.0 }
    fn kept(&self) -> impl Sized + use<> {}
    fn set(&mut self) {}
}
fn consume<T>(_: T) {}
fn user(mut s: S) {
    let it = s.get();
    s.set();
  //^^^^^^^ error: cannot borrow `s` as mutable because it is also borrowed as immutable
    consume(it);
    let kept = s.kept();
    s.set();
    consume(kept);
}
"#,
        );
    }

    #[test]
    fn impl_trait_captures_elided_lifetime_2021() {
        check_diagnostics(
            r#"
//- minicore: sized, copy
//- /main.rs edition:2021
struct S(i32);
impl S {
    fn get(&self) -> impl Sized + '_ { &self.0 }
    fn copied(&self) -> impl Sized { self.0 }
    fn set(&mut self) {}
}
fn consume<T>(_: T) {}
fn user(mut s: S) {
    let it = s.get();
    s.set();
  //^^^^^^^ error: cannot borrow `s` as mutable because it is also borrowed as immutable
    consume(it);
    let copied = s.copied();
    s.set();
    consume(copied);
}
"#,
        );
    }

    #[test]
    fn higher_ranked_bound_holds_for_any_lifetime() {
        check_diagnostics(
            r#"
//- minicore: sized, fn
trait Deserialize<'de> {}
trait DeserializeOwned: for<'de> Deserialize<'de> {}
impl<T> DeserializeOwned for T where T: for<'de> Deserialize<'de> {}
impl<'de: 'a, 'a> Deserialize<'de> for &'a str {}
impl<'de> Deserialize<'de> for u32 {}
fn owned<T: DeserializeOwned>() {}
trait Write {}
fn handler(_: &u32, _: &mut dyn Write) {}
fn run(_: impl FnMut(&u32, &mut dyn Write)) {}
fn main() {
    owned::<u32>();
    owned::<&'static str>();
  //^^^^^^^^^^^^^^^^^^^^^^^ error: higher-ranked lifetime error
    run(handler);
}
"#,
        );
    }

    #[test]
    fn destructuring_assignment_moves_what_it_takes() {
        check_diagnostics(
            r#"
//- minicore: copy
struct NC;
struct S { a: u8, b: NC }
union U { field: u8 }
fn copies(r: &S, u: &U, mut a: u8, mut field: u8) {
    S { a, .. } = *r;
    S { a: _, .. } = *r;
    unsafe { U { field } = *u };
    let _ = (a, field);
}
fn moves(r: &S, mut b: NC) {
    S { b, .. } = *r;
                //^^ error: cannot move out of `r.b` which is behind a shared reference
    let _ = b;
}
"#,
        );
    }

    #[test]
    fn a_body_with_an_error_is_not_borrow_checked() {
        check_diagnostics(
            r#"
//- minicore: copy
struct NC;
fn tainted(x: NC) -> u32 {
    let _y = x;
    let _z = x;
    NC as u32
  //^^^^^^^^^ error: non-primitive cast: `NC` as `u32`
}
"#,
        );
    }

    // rustc's tests/ui/expr/if/expr-stack-overflow.rs: each expression's syntax was found by
    // walking down from the body's root, quadratic in the chain, and lowering it recursed
    // past the stack.
    #[test]
    fn a_5204_arm_else_if_chain_is_diagnosed_in_linear_time() {
        let mut fixture = String::from(
            "//- minicore: copy\npub fn banana(v: u32) -> u32 {\n    if v == 0 { 0 }\n",
        );
        for arm in 1..5204 {
            fixture.push_str(&format!("    else if v == {arm} {{ {arm} }}\n"));
        }
        fixture.push_str("    else { 5204 }\n}\n");
        let started = std::time::Instant::now();
        check_diagnostics(&fixture);
        let took = started.elapsed();
        assert!(took < std::time::Duration::from_secs(60), "took {took:?}");
    }

    // rustc's tests/ui/associated-type-bounds/issue-83017.rs.
    #[test]
    fn bound_of_an_associated_type_in_a_for_binds_the_outer_lifetimes() {
        check_diagnostics(
            r#"
//- minicore: sized
trait TraitA<'a> { type AsA; }
trait TraitB<'a, 'b> { type AsB; }
trait TraitC<'a, 'b, 'c> {}
struct X;
impl<'a, 'b, 'c> TraitC<'a, 'b, 'c> for X {}
struct Y;
impl<'a, 'b> TraitB<'a, 'b> for Y { type AsB = X; }
struct Z;
impl<'a> TraitA<'a> for Z { type AsA = Y; }
fn foo<T>()
where
    for<'a> T: TraitA<'a, AsA: for<'b> TraitB<'a, 'b, AsB: for<'c> TraitC<'a, 'b, 'c>>>,
{
}
fn main() {
    foo::<Z>();
}
"#,
        );
    }

    // rustc's tests/ui/dyn-compatibility/multiple-supers-should-work.rs.
    #[test]
    fn a_trait_object_keeps_projections_that_become_the_same() {
        check_diagnostics(
            r#"
//- minicore: sized, unsize, coerce_unsized, dispatch_from_dyn
trait Sup<T> { type Assoc; }
impl<T> Sup<T> for () { type Assoc = T; }
trait Trait<A, B>: Sup<A, Assoc = A> + Sup<B, Assoc = B> {}
impl<T, U> Trait<T, U> for () {}
fn main() {
    let x: &dyn Trait<(), _> = &();
    let _y: &dyn Trait<_, ()> = x;
}
"#,
        );
    }

    // rustc's tests/ui/traits/trait-upcasting/higher-ranked-upcasting-ok.rs.
    #[test]
    fn upcasting_a_higher_ranked_trait_object() {
        check_diagnostics(
            r#"
//- minicore: sized, unsize, coerce_unsized, dispatch_from_dyn
trait Supertrait<'a, 'b> {}
trait Subtrait<'a, 'b>: Supertrait<'a, 'b> {}
impl Supertrait<'_, '_> for () {}
impl Subtrait<'_, '_> for () {}
fn ok(x: &dyn for<'a, 'b> Subtrait<'a, 'b>) -> &dyn for<'a> Supertrait<'a, 'a> {
    x
}
fn main() {
    ok(&());
}
"#,
        );
    }

    #[test]
    fn unit_expression_initializes_its_place() {
        check_diagnostics(
            r#"
//- minicore: copy
#![deny(unused_mut)]
fn f() -> () {
    let mut x: i32;
    let z: () = x = 10;
    let w = z;
    let mut q: () = x = 11;
    q = x += 1;
    let mut b: () = { x += 1; };
    b = ();
    let mut l: () = loop { break; };
    l = ();
    let mut i: () = if x > 0 { x = 1 };
    i = ();
    let _ = (q, b, l, i, x);
    w
}
"#,
        );
    }

    #[test]
    fn unused_mut_self() {
        check_diagnostics(
            r#"
#![deny(unused_mut)]
struct S(u8);
impl S {
    fn read(mut self) -> u8 { self.0 }
          //^^^^^^^^ error: variable does not need to be mutable
    fn write(mut self) -> u8 { self.0 = 1; self.0 }
}
"#,
        );
    }
}
