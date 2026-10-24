use cases::*;

macro_rules! case {
    ($($e:expr),* $(,)?) => { $( println!("{} = {:?}", stringify!($e), $e); )* };
}

fn main() {
    use aggregates::*;
    use enums::*;
    for (a, b) in [(1u32, 2u32), (1000, 7), (77, 0), (4_000, 123_456)] {
        case!(scalars::arith(a, b), scalars::checked(a, b), scalars::checked(u32::MAX - a, b));
        case!(scalars::signed(a as i64, -(b as i64)), scalars::bits(a as u16, b as u8));
        case!(
            scalars::casts(-(a as i8), b as u64 * 1_000_003),
            scalars::wrapping(a as u64, !(b as u64))
        );
        case!(scalars::wide((a as u128) << 70, b as u64), scalars::compare(a as i32, b as i32));
        case!(scalars::counting((a as u64) << 9 | b as u64));
    }
    case!(scalars::wide_mul(u128::MAX / 3, 2, -5), scalars::wide_mul(1 << 100, 1 << 90, i128::MIN));
    case!(scalars::wide_mul(12345678901234567890, 98765432109876543210, 1 << 70));
    case!(control::collatz(27), control::nested(40), control::fib(15));
    case!(control::classify(-5), control::classify(0), control::classify(7), control::classify(99));

    case!(small(Small(7, 300)), pair(Pair { a: 5, b: 1 << 40 }, 9), odd(Odd(1, 2, 3)));
    let big_value = Big { a: 11, b: [1, 2, 3, 4, 5], c: -3 };
    case!(
        big(&big_value, 50),
        big_by_value(big_value),
        tuple((250, 9, 1 << 50)),
        array([1, 2, 3, 4])
    );
    case!(nested_field(&(Pair { a: 1, b: 2 }, Small(3, 4))));
    let mut p = Pair { a: 1, b: 21 };
    mutate(&mut p);
    case!(p);

    for n in 0..4 {
        case!(
            plain(n),
            plain_back(plain(n)),
            option(n),
            niche_bool(n as u8),
            niche_bool_back(niche_bool(n as u8))
        );
        case!(data(n * 5), data_back(data(n * 5)), result(n as i32 - 2), ordering(n, 2));
    }
    case!(explicit(Explicit::Low), explicit(Explicit::Mid), explicit(Explicit::High));
    case!(option_ref(&Some(4)), option_ref(&None), niche_ref(true, &5), niche_ref(false, &5));
    for c in *b"a7!?Z" {
        case!(nested_niche(c, 3), nested_niche_back(&nested_niche(c, 3)));
    }

    let xs = [3u32, 1, 4, 1, 5, 9, 2, 6];
    case!(
        slices::sum(&xs),
        slices::sum(&[]),
        slices::sum_iter(&xs),
        slices::tail(&xs),
        slices::window(&xs, 2, 5)
    );
    case!(slices::first_last(&[]), slices::first_last(&[4]), slices::first_last(&[4, 5, 6]));
    let mut rev = [1u16, 2, 3, 4, 5];
    slices::reverse(&mut rev);
    case!(
        rev,
        slices::str_len("héllo"),
        slices::bytes_hash("ra-jit"),
        slices::literal(0),
        slices::literal(1)
    );
    case!(
        slices::array_slice(&[1, 2, 3, 4, 5, 6]),
        slices::get(&[7, 8], 1),
        slices::get(&[7, 8], 2)
    );
    case!(slices::is_ascii_digit_run(b"123ab"));

    let bytes = *b"xyz";
    let mut cursor =
        pointers::Cursor { start: bytes.as_ptr(), end: unsafe { bytes.as_ptr().add(3) } };
    case!(pointers::peek(&cursor), pointers::advance(&mut cursor, 2), pointers::peek(&cursor));
    case!(pointers::advance(&mut cursor, 1), pointers::peek(&cursor));
    let words = [1u32, 2];
    case!(pointers::compare(&words[0], &words[1]), pointers::compare(&words[1], &words[1]));
    let mut slot = 5u64;
    case!(pointers::write(&mut slot, 6), slot);

    case!(
        consts::limit(50),
        consts::limit(5),
        consts::table(6),
        consts::name(1),
        consts::config(3)
    );
    case!(consts::local_array(4), consts::repeat(9));

    case!(generics::picks(5, 6), generics::swaps(1, 2), generics::inline(5), generics::methods(10));
    case!(generics::statics(3, 4), generics::option_map(Some(4)), generics::option_map(None));
    case!(generics::core_generics(3, 9));

    case!(closures::capture(10, &xs), closures::capture_mut(5), closures::iter_chain(&xs));
    case!(closures::fn_pointer(|x| x * 3, 2));

    case!(callables::reify(4), callables::reify_pair(7), callables::generic_closure(3, 5));
    case!(
        callables::generic_mut(6),
        callables::generic_once(9),
        callables::takes_fn(|a, b| a ^ b, 9)
    );

    let b3 = odd::rot3(odd::B3(1, 2, 3));
    case!((b3.0, b3.1, b3.2), odd::rot5(odd::B5([1, 2, 3, 4, 5])).0, odd::chain(odd::B3(7, 8, 9)));
    let b6 = odd::rot6(odd::B6(100, 200, 300));
    case!((b6.0, b6.1, b6.2), odd::rot7(odd::B7([1, 2, 3, 4, 5, 6, 7]), 99).0);

    case!(
        dynamic::call(&dynamic::Dog(4)),
        dynamic::call(&dynamic::Cat),
        dynamic::make(true),
        dynamic::make(false)
    );

    {
        use objects::*;
        let (mut one, mut two) = (One(5), Two(6, 7));
        case!(use_top(&mut one), use_top(&mut two), local(8));
        let (mut a, mut b) = (make_top(false), make_top(true));
        case!(
            a.base(),
            a.twice(),
            a.mid(2),
            a.top(),
            a.big(),
            a.name(),
            b.base(),
            b.twice(),
            b.mid(2),
            b.top(),
            b.big(),
            b.name()
        );
        let r = make_ref(&one);
        case!(r.base(), r.mid(9), r.big(), r.name());
        case!(
            upcast_first(&one).mid(1),
            upcast_first(&two).twice(),
            upcast_base(&two).base(),
            upcast_base(&one).twice()
        );
        case!(
            upcast_named(&one).name(),
            upcast_named(&two).name(),
            upcast_box(a).name(),
            use_named(&two),
            use_named(upcast_named(&*b))
        );
        case!(
            generic_trait(&one),
            boxed_sum(&[Box::new(One(1)), Box::new(Two(2, 0)), make_top(true)])
        );
        case!(display(12), show(&One(345)), show(&"hello"), show(&1.5f32));
        let log = std::cell::Cell::new(0u64);
        case!(make_thing(&log, true).id(), make_thing(&log, false).id(), log.get());
        case!(
            drop_thing(Box::new(Loud(&log, 6))),
            drop_thing(Box::new(3u8)),
            drop_thing(make_thing(&log, true)),
            log.get()
        );
        let k = 10;
        case!(call_fn(&|a, b| a * b + k, 5), call_fn(&u32::wrapping_sub, 5));
        let mut total = 0;
        case!(
            call_fn_mut(&mut |x| {
                total += x;
                total * 2
            }),
            total
        );
        let (f, mut g, h) = (make_fn(3), make_fn_mut(100), make_fn_once("abc".to_string()));
        case!(f(4), f(5), g(1), g(2), h(10), local_fn(&[1, 2, 3], 7));
    }
    let log = std::cell::Cell::new(0u64);
    let mut logged = |f: &dyn Fn(&std::cell::Cell<u64>)| {
        log.set(0);
        f(&log);
        log.get()
    };
    case!(
        logged(&|l| drops::scope(l)),
        logged(&|l| drop(drops::moved(l, true))),
        logged(&|l| drop(drops::moved(l, false)))
    );
    case!(
        logged(&|l| drops::fields(l)),
        logged(&|l| drops::variants(l, 0)),
        logged(&|l| drops::variants(l, 1)),
        logged(&|l| drops::variants(l, 2))
    );
    case!(
        logged(&|l| drops::arrays(l)),
        logged(&|l| drops::boxes(l)),
        logged(&|l| drops::param(drops::Noisy(l, 5), l))
    );
    case!(logged(&|l| drops::looped(l, 4)), drops::strings(3));
    case!(logged(&|l| drops::box_refilled(l)), logged(&|l| drops::left_behind(l)));
    case!(logged(&|l| drops::binding_order(l, true)), logged(&|l| drops::binding_order(l, false)));
    case!(logged(&|l| {
        drops::tail_temporaries(l);
    }));
    case!(logged(&|l| drops::arms(l, 2)), logged(&|l| drops::arms(l, 0)));
    case!(logged(&|l| drops::if_let(l, true)), logged(&|l| drops::if_let(l, false)));
    case!(
        logged(&|l| drop(drops::statement_temps(l))),
        drops::statement_temps(&std::cell::Cell::new(0))
    );
    case!(
        logged(&|l| drop(drops::tail_temp(l))),
        drops::tail_temp(&std::cell::Cell::new(0)),
        drops::tail_temp_block(&std::cell::Cell::new(0))
    );
    case!(drops::tail_temp_call(&std::cell::RefCell::new(Vec::new()), 3));
    case!(
        drops::condition_temp(&std::cell::Cell::new(0)),
        drops::match_temp(&std::cell::Cell::new(0))
    );
    case!(logged(&|l| drops::reassigned(l)), logged(&|l| drop(drops::partial_move(l))));
    case!(
        locations::tracked(),
        locations::unwraps(Some(1), Ok(2)),
        locations::borrows(&std::cell::RefCell::new(4))
    );
    case!(locations::through_pointers(Some(7)), locations::through_pointers(None));

    case!(
        heap::boxed(1),
        heap::vec(5),
        heap::string(3),
        heap::consume(vec![1, 2, 3]),
        heap::format(4, "x")
    );
    case!(heap::clears(3));
    let (shared, call) = heap::shared("abc");
    case!(std::sync::Arc::strong_count(&shared), call());
    drop(call);
    case!(std::sync::Arc::strong_count(&shared));
    let (mut map, mut maps) = (heap::hashmap(), heap::hashmaps(2));
    map.insert("a".into(), 1);
    maps[1].insert("b".into(), 2);
    case!(map.len(), map["a"], maps[0].len(), maps[1]["b"]);
    let (cell, lock) = (std::cell::RefCell::new([3u64; 14]), std::sync::Mutex::new((1u128, 2u8)));
    case!(heap::unsizable_tail(&cell, &lock), cell.borrow()[13], *lock.lock().unwrap());

    let long = "the quick brown fox jumps over the lazy dog, twice over and then some more";
    case!(
        simd::ascii(long),
        simd::ascii("caf\u{e9} au lait, s'il vous pla\u{ee}t, merci beaucoup"),
        simd::ascii("")
    );
    case!(simd::ascii_bytes(long.as_bytes()), simd::ascii_bytes(&[b'a', 200, b'c']));
    let hay = *b"hello, simd lane";
    let other = [3, 250, 17, 128, 127, 0, 255, 1, 90, 200, 44, 129, 5, 6, 7, 8];
    #[cfg(target_arch = "x86_64")]
    case!(
        simd::find(&hay, b'l'),
        simd::find(&hay, b'z'),
        simd::mix(&hay, &other),
        simd::mix(&other, &hay)
    );

    #[cfg(target_arch = "x86_64")]
    let shuffles = is_x86_feature_detected!("ssse3");
    #[cfg(target_arch = "aarch64")]
    let shuffles = std::arch::is_aarch64_feature_detected!("neon");
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    if shuffles {
        case!(unsafe { simd::shuffle(&hay, &other) }, unsafe { simd::shuffle(&other, &hay) });
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = (hay, other);
    case!(statics::bump(3), statics::bump(100), statics::once(), statics::once());
    case!(
        statics::lowest(4),
        statics::lowest(-9),
        statics::lowest(-2),
        statics::local(2),
        statics::local(77)
    );
    case!(statics::plain(0, 5), statics::plain(2, 7), statics::plain(0, 1));
    case!(
        floats::arith(1.5, -0.25),
        floats::arith(1e300, 1e-300),
        floats::arith(7.0, 0.0),
        floats::single(3.25, 2.0),
        floats::single(-1.0, -1.0)
    );
    case!(
        floats::compare(1.0, 2.0),
        floats::compare(2.0, 2.0),
        floats::compare(f64::NAN, 1.0),
        floats::compare(3.0, -3.0)
    );
    case!(
        floats::to_int(3.99),
        floats::to_int(-3.99),
        floats::to_int(1e30),
        floats::to_int(-1e30),
        floats::to_int(f64::NAN),
        floats::to_int(300.5)
    );
    case!(
        floats::from_int(-5, u64::MAX, 200, -300),
        floats::from_int(i64::MIN, 1 << 63, 0, 7),
        floats::widen(0.1, 0.1)
    );
    case!(floats::bits(1.0), floats::bits(-0.0), floats::bits(f64::NAN).2);
    let p = floats::point(floats::Point { x: 1.5, y: 2.5 }, 2.0);
    case!(p.x, p.y, floats::mixed(4, 0.5), floats::sum(&[0.1, 0.2, 0.3]), floats::sum(&[]));
    case!(
        misc::big_repeat(3),
        misc::big_repeat(255),
        misc::constructors(&[1, 2, 3]),
        misc::constructors(&[])
    );
    case!(misc::lines("a\nbb\r\nccc\n"), misc::lines(""));
    case!(
        misc::keyword("alpha"),
        misc::keyword("beta"),
        misc::keyword("gamma"),
        misc::keyword(""),
        misc::keyword("alphabet"),
        misc::keyword("alphx"),
        misc::keyword("delta")
    );
    case!(
        misc::barrier(14),
        misc::fn_item_cast(41),
        misc::float_math(-2.5, -7.75),
        misc::float_math(16.0, 0.3),
        misc::float_math(0.5, 1.0)
    );
    case!(misc::fn_statics(5), misc::fn_statics(7));
    let big = foreign::make_big(7);
    case!(big.a, big.b, big.c, foreign::big(foreign::Big { a: 1, b: 2, c: 3 }, 4));
    let mixed = foreign::mixed(foreign::Mixed { a: 1.5, b: 3 }, foreign::Mixed { a: 2.25, b: 5 });
    let small = foreign::small(foreign::Small { a: 1, b: 2, c: 3 }, true, -1);
    case!(mixed.a, mixed.b, small.a, small.b, small.c);
    case!(foreign::wide(1 << 100, -3), foreign::call_c(b"hello\0"));
    let floats = foreign::floats(foreign::Floats { a: 1.5, b: [2.5, 3.5] }, 2.0);
    let nested = foreign::nested(foreign::Nested { tag: 7, inner: (300, 9.0) });
    case!(floats.a, floats.b[0], floats.b[1], nested.tag, nested.inner.0, nested.inner.1);
    let two = foreign::spill(1, 2, 3, 4, 5, foreign::Two { a: 6, b: 7 }, 8);
    case!(two.a, two.b, foreign::through_pointer(foreign::floats, foreign::make_big));
    let quad = |x: f64| foreign::Quad { at: [x, x + 0.5, x * 2.0, x - 1.0] };
    let five = foreign::five(foreign::Five { at: [1.0, 2.0, 3.0, 4.0, 5.0] }, 1.5);
    case!(foreign::quads(quad(1.0), quad(2.0), quad(3.0), 0.25).at, five.at);
    case!(foreign::wides(1, 1 << 70, 2, -5, 3, 1 << 90, 4, 1 << 100));
    {
        let aligned = |a, b| foreign::Aligned { a, b };
        case!(foreign::aligned(
            1,
            aligned(2, 3),
            4,
            5,
            6,
            aligned(7, 8),
            9,
            aligned(10, 11),
            1 << 80,
            12
        ));
    }
    case!(foreign::wide_last([1, 2, 3, 4, 5, 6, 7], -(1 << 70), 8, 9, 1 << 100, 10));
    let map = shapes::byte_map();
    case!(map.map[b'a' as usize], map.map[10], map.map[0]);
    let (a, b) = (shapes::state(b"abc"), shapes::state(b"abd"));
    let names = shapes::names("first");
    case!(shapes::same(&a, &b), shapes::same(&a, &a.clone()), names.0, names.1.len(), a);
    let bits = [false, true, true, false, true];
    case!(shapes::SetBits { it: bits.iter().enumerate() }.collect::<Vec<_>>());
    case!(shapes::wide_pointers(b"abc", b"abd"));
    case!(unit_drops::count());
    println!("operand_order {:?}", cases::operand_order());
    println!("format_temporaries {}", cases::format_temporaries(4));
    println!("exclusive_ranges {:?} {:?}", cases::exclusive_ranges(7), cases::exclusive_ranges(6));
    println!("const_pointers {:?}", cases::const_pointers::run());
    println!("const_unsized_struct {:?}", cases::const_unsized_struct::run());
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    println!("inline_asm {:?}", cases::inline_asm::run());
    println!("evaluation_order {:?}", cases::evaluation_order::run());
    println!("promoted {:?}", cases::promoted::run());
    println!("unsized_tail {:?}", cases::unsized_tail::run());
    println!("const_in_const {:?}", cases::const_in_const::run());
    println!("upcast_generic {:?}", cases::upcast_generic::run());
    println!("packed_drop {:?}", cases::packed_drop::run());
    println!("array_move {:?}", cases::array_move::run());
    println!("const_overflow {:?}", cases::const_overflow::run());
    println!("const_overflow more {:?}", cases::const_overflow::more());
    #[cfg(panic = "unwind")]
    println!("unwind_cleanup {:?}", cases::unwind_cleanup::run());
    #[cfg(panic = "unwind")]
    println!("failing_drop {:?}", cases::unwind_cleanup::failing_drop());
    println!("capture_enum_field {:?}", cases::capture_enum_field::run());
    println!("autoref_promoted {:?}", cases::autoref_promoted::run());
    println!("promoted_self_variant {:?}", cases::promoted_self_variant::run());
    println!("static_cycle {:?}", cases::static_cycle::run());
    println!("async_frames {:?}", cases::async_frames::run());
    println!("boxed_fn_tuple {:?}", cases::boxed_fn_tuple::run());
    println!("builtin_location {:?}", cases::builtin_location::run());
    println!("builtin_location passed {:?}", cases::builtin_location::passed());
    println!("stringify_spacing {:?}", cases::stringify_spacing::run());
    println!(
        "pattern_alternatives {:?} {:?}",
        cases::pattern_alternatives::guard((3, 42)),
        cases::pattern_alternatives::slices([1, 2, 3, 4, 5])
    );
    println!("raw_identifiers {:?}", cases::raw_identifiers::run());
    println!("derive_ufcs {:?}", cases::derive_ufcs::run());
    println!("builtin_clone {:?}", cases::builtin_clone::run());
    println!("guard_moves {:?}", cases::guard_moves::run());
    #[cfg(panic = "unwind")]
    println!("division_panics {:?}", cases::division_panics::run());
    #[cfg(panic = "unwind")]
    println!("unwind_owned {:?}", cases::unwind_owned::run());
    println!("derive_copy_packed {:?}", cases::derive_copy_packed::run());
    println!("union_pointer {:?}", cases::union_pointer::run());
    println!("async_basic {:?}", cases::async_basic::run());
    println!("async_closures {:?}", cases::async_closures::run());
    println!("async_closures more {:?}", cases::async_closures::more());
    println!("async_items {:?}", cases::async_items::run());
    println!("stabilized spin_hint {:?}", cases::stabilized::spin_hint());
    println!("stabilized let_chains {:?}", cases::stabilized::let_chains());
    println!("stabilized if_let_guard {:?}", cases::stabilized::if_let_guard());
    println!("stabilized trait_upcasting {:?}", cases::stabilized::trait_upcasting());
    println!("stabilized cfg_select {:?}", cases::stabilized::cfg_select());
    println!("stabilized generic_arg_infer {:?}", cases::stabilized::generic_arg_infer());
    println!("stabilized repr128 {:?}", cases::stabilized::repr128());
    println!("stabilized precise_capturing {:?}", cases::stabilized::precise_capturing());
    #[cfg(panic = "unwind")]
    println!("stabilized never {:?}", cases::stabilized::never());
    println!("stabilized pc_trait {:?}", cases::stabilized::pc_trait());
    println!("stabilized pc_free {:?}", cases::stabilized::pc_free());
    println!("stabilized pc_plain {:?}", cases::stabilized::pc_plain());
    println!("stabilized pc_s2 {:?}", cases::stabilized::pc_s2());
    println!("stabilized pc_s3 {:?}", cases::stabilized::pc_s3());
    println!("stabilized pc_s4 {:?}", cases::stabilized::pc_s4());
    #[cfg(panic = "unwind")]
    println!("stabilized never_more {:?}", cases::stabilized::never_more());
    #[cfg(not(target_family = "wasm"))]
    println!("stabilized c_variadic {:?}", cases::stabilized::c_variadic());
    #[cfg(not(target_family = "wasm"))]
    println!("stabilized c_variadic_stack {:?}", cases::stabilized::c_variadic_stack());
    #[cfg(target_vendor = "apple")]
    println!(
        "stabilized c_variadic_named_wide {:?}",
        cases::stabilized::c_variadic_named_wide()
    );
    #[cfg(target_vendor = "apple")]
    println!(
        "stabilized c_variadic_named_stack {:?}",
        cases::stabilized::c_variadic_named_stack()
    );
    #[cfg(all(feature = "nightly", not(target_family = "wasm")))]
    println!("stabilized c_variadic_wide {:?}", cases::stabilized::c_variadic_wide());
    #[cfg(not(target_family = "wasm"))]
    println!("stabilized c_variadic_named {:?}", cases::stabilized::c_variadic_named());
    #[cfg(windows)]
    println!("stabilized by_reference {:?}", cases::stabilized::by_reference::run());
    #[cfg(any(unix, windows))]
    println!("stabilized variadic_floats {:?}", cases::stabilized::variadic_floats());
    #[cfg(all(target_arch = "x86_64", any(unix, windows)))]
    println!("stabilized naked_functions {:?}", cases::stabilized::x86::naked_functions());
    #[cfg(all(target_arch = "x86_64", any(unix, windows)))]
    println!("stabilized asm_goto {:?}", cases::stabilized::x86::asm_goto());
    #[cfg(all(target_arch = "x86_64", any(unix, windows)))]
    println!("stabilized asm_cfg {:?}", cases::stabilized::x86::asm_cfg());
    #[cfg(all(target_arch = "x86_64", any(unix, windows)))]
    println!("stabilized target_feature_11 {:?}", cases::stabilized::x86::target_feature_11());
    #[cfg(all(target_arch = "x86_64", any(unix, windows)))]
    println!("stabilized varargs_abi {:?}", cases::stabilized::x86::varargs_abi());
    #[cfg(all(target_arch = "x86_64", any(unix, windows)))]
    println!("stabilized naked_more {:?}", cases::stabilized::x86::naked_more());
    #[cfg(all(target_arch = "x86_64", any(unix, windows)))]
    println!("stabilized naked_sym {:?}", cases::stabilized::x86::naked_sym());
    #[cfg(all(target_arch = "x86_64", any(unix, windows)))]
    println!("stabilized asm_goto_more {:?}", cases::stabilized::x86::asm_goto_more());
    #[cfg(all(target_arch = "aarch64", target_vendor = "apple"))]
    println!("stabilized arm naked_sym {:?}", cases::stabilized::arm::naked_sym());
    #[cfg(target_arch = "aarch64")]
    println!("stabilized arm naked_functions {:?}", cases::stabilized::arm::naked_functions());
    #[cfg(target_arch = "aarch64")]
    println!("stabilized arm asm_goto {:?}", cases::stabilized::arm::asm_goto());
    #[cfg(target_arch = "aarch64")]
    println!("stabilized arm asm_cfg {:?}", cases::stabilized::arm::asm_cfg());
    #[cfg(target_arch = "aarch64")]
    println!("stabilized arm target_feature_11 {:?}", cases::stabilized::arm::target_feature_11());
    #[cfg(target_arch = "aarch64")]
    println!("stabilized arm naked_more {:?}", cases::stabilized::arm::naked_more());
    #[cfg(target_arch = "aarch64")]
    println!("stabilized arm asm_goto_more {:?}", cases::stabilized::arm::asm_goto_more());
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    {
        use cases::stabilized::arm::symbols;
        let (jumped, loaded) = (symbols::naked_sym(6), symbols::naked_sym_data(8));
        println!("stabilized arm naked_sym {:?} {jumped} {loaded}", symbols::naked_sym_operands());
    }
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    {
        use cases::stabilized::x86::symbols;
        let (jumped, loaded) = (symbols::naked_sym(6), symbols::naked_sym_data(8));
        println!("stabilized naked_sym {:?} {jumped} {loaded}", symbols::naked_sym_operands());
    }
    #[cfg(target_arch = "aarch64")]
    {
        use cases::stabilized::arm::{self, Aligned};
        let (x, y) = (Aligned { a: 30, b: 40 }, Aligned { a: 50, b: 60 });
        println!(
            "stabilized arm wide_stack {:?} {} {} {}",
            arm::wide_stack(),
            arm::wide_late(9, 8, 7, 6, 5, 4, 3, 1 << 77 | 5, 2, -1 << 100),
            arm::aligned_late(9, 8, 7, 6, 5, 4, 3, x, 2, y),
            arm::wide_fits(9, 8, 7, 6, 5, 1 << 99, 4),
        );
        println!("stabilized arm va_named {:?} {}", arm::va_named_args(), unsafe {
            arm::va_named(
                9, 8, 7, 6, 5, 4, 3, 2, 1, 8.5, 7.5, 6.5, 5.5, 4.5, 3.5, 2.5, 1.5, 0.5,
                7u128 << 64 | 3, 4, 100i64, 0.125f64, 80u32, -7i64,
            )
        });
        #[cfg(feature = "nightly")]
        println!("stabilized arm va_wide {:?}", arm::va_wide_args());
        #[cfg(feature = "nightly")]
        println!("stabilized arm va_wide runner {:?}", unsafe {
            (
                arm::va_wide(5, 3u128 << 90, 77u64, 1.25f64, -9i128, 1u128),
                arm::va_wide(9, 1u128, 2u64, 3.0f64, 4i128, 5u128, 6u64, 7.0f64, 8i128, 9u128),
                arm::va_named_wide(7, 6, 5, 4, 3, 2, 1, 1i128 << 100, -2i64, -3i128 << 70, 4.0f64),
            )
        });
    }
    #[cfg(panic = "unwind")]
    println!("async_more {:?}", cases::async_more::run());
    println!("const_patterns {:?}", cases::const_patterns::run());
    println!("array_map {:?}", cases::array_map::run());
    #[cfg(feature = "nightly")]
    println!("box_patterns {:?}", cases::box_patterns::run());
    println!("promoted_call {:?}", cases::promoted_call::run());
    println!("type_ids {:?}", cases::type_ids::run());
    #[cfg(panic = "unwind")]
    println!("unwind_drops {:?}", cases::unwind_drops::run());
    #[cfg(panic = "unwind")]
    println!("async_drops {:?}", cases::async_drops::run());
    println!("promoted_assoc_const {:?}", cases::promoted_assoc_const::run());
    println!("offsets {:?}", cases::offsets::run());
    println!("backtraces {:?}", cases::backtraces::run());
    println!("inline_const_bindings {:?}", cases::inline_const_bindings::run());
    println!("closure_clone {:?}", cases::closure_clone::run());
    println!("type_ids primitives {:?}", cases::type_id_hashes::primitives());
    println!("type_ids compound {:?}", cases::type_id_hashes::compound());
    println!("type_ids local_structs {:?}", cases::type_id_hashes::local_structs());
    println!("type_ids local_reprs {:?}", cases::type_id_hashes::local_reprs());
    println!("type_ids local_enums {:?}", cases::type_id_hashes::local_enums());
    println!("type_ids local_generics {:?}", cases::type_id_hashes::local_generics());
    println!("type_ids std_types {:?}", cases::type_id_hashes::std_types());
    println!("type_ids trait_objects {:?}", cases::type_id_hashes::trait_objects());
    println!("type_ids functions {:?}", cases::type_id_hashes::functions());
    println!("type_ids elided {:?}", cases::type_id_hashes::elided());
    println!("type_ids function_items {:?}", cases::type_id_hashes::function_items());
    println!("type_ids closures {:?}", cases::type_id_hashes::closures());
    println!("type_ids futures {:?}", cases::type_id_hashes::futures());
    println!("type_names {:?}", cases::type_id_hashes::type_names());
    println!("type_ids projections {:?}", cases::type_id_hashes::projections());
    println!("type_ids more_std_types {:?}", cases::type_id_hashes::more_std_types());
    println!("type_ids in_bodies {:?}", cases::type_id_hashes::in_bodies());
    println!("type_names {:?}", cases::type_names::run());
    println!("type_names nested {:#?}", cases::type_names::nested());
    println!("repeated statics {:?}", cases::repeated_statics::twice());
    println!("repeated statics {:?}", cases::repeated_statics::twice());
    println!("async_frames shared {:?}", cases::async_frames::shared());
    println!("raw_newline {:?}", cases::raw_newline::run());
    println!("tail_temporaries {:?}", cases::tail_temporaries::run(true));
    println!("tail_temporaries {:?}", cases::tail_temporaries::run(false));
    println!("const_field_names {:?}", cases::const_field_names::run());
    println!("object_lifetime_default {:?}", cases::object_lifetime_default::run());
    println!("cargo_borrows {:?}", cases::cargo_borrows::run());
    println!("shared_map {:?}", cases::shared_map::build());
    println!("lazy statics {:?}", cases::repeated_statics::lazy());
    println!("repeated statics in_closure {:?}", cases::repeated_statics::in_closure());
    println!("repeated statics in_async {:?}", cases::repeated_statics::in_async());
    println!("type_ids downcasts {:?}", cases::type_id_hashes::downcasts());
    println!("promoted_slices {:?}", cases::promoted_slices::run());
    println!("reified {:?}", cases::reified::run());
    println!("overloaded_unary {}", cases::overloaded_unary::run());
    println!("closure_layout {}", cases::closure_layout::run());
    println!("in_place_collect {}", cases::in_place_collect::run());
    println!("dyn_statics {}", cases::dyn_statics::run());
    println!("included_files {:?}", cases::included_files::run());
    println!("nested_macro_self {:?}", cases::nested_macro_self::run());
    println!("shadowed_item_macro {:?}", cases::shadowed_item_macro::run());
    println!("wide_float_casts {}", cases::wide_float_casts::run());
    println!("const_floats {}", cases::const_floats::run());
    println!("static_identity {}", cases::static_identity::run());
    println!("lifetime_impl_consts {}", cases::lifetime_impl_consts::run());
    #[cfg(target_arch = "x86_64")]
    println!("simd_x86 {}", cases::simd_x86::run());
    #[cfg(target_arch = "aarch64")]
    println!("simd_aarch64 {}", cases::simd_aarch64::run());
    // Threads.
    #[cfg(not(target_family = "wasm"))]
    println!("boxed_fn_once {:?}", cases::boxed_fn_once::run());
    println!("module_paths {}", cases::module_paths::run());
    // Threads.
    #[cfg(not(target_family = "wasm"))]
    println!("fn_thread_local {:?}", cases::fn_thread_local::run());
    println!("shadowed_items {}", cases::shadowed_items::run());
    println!("hrtb_type_ids {:?}", cases::hrtb_type_ids::run());
    println!("projected_patterns {:?}", cases::projected_patterns::run());
    println!("projected_fields {:?}", cases::projected_fields::run());
    println!("anonymous_consts {:?}", cases::anonymous_consts::run());
    println!("specialized_items {:?}", cases::specialized_items::run());
    println!("hidden_derives {:?}", cases::hidden_derives::run());
    println!("moved_in_break {:?}", cases::moved_in_break::run());
    for it in cases::stringified::run() {
        println!("stringified {it:?}");
    }
    println!("fn_pointers {:?}", cases::fn_pointers::run());
    println!("closure_in_bounded_field {:?}", cases::closure_in_bounded_field::run());
    println!("unstable_name_collision {:?}", cases::unstable_name_collision::run());
    println!("closure_in_block_tail {:?}", cases::closure_in_block_tail::run());
    println!(
        "build_cfgs {:?} {}",
        cases::build_cfgs::flags(),
        cases::build_cfgs::debug_assert_runs()
    );
    println!("trivial_bounds {}", cases::trivial_bounds::default_len());
    println!(
        "variadic {} {} {}",
        cases::variadic::none(),
        cases::variadic::two(-3, u64::MAX),
        cases::variadic::string(c"text")
    );
    println!("const_pointer_into_const {:?}", cases::const_pointer_into_const::run());
    let words: [&[u8]; 8] = [b"do", b"if", b"of", b"for", b"lex", b"while", b"", b"in"];
    println!("const_tables {}", cases::const_tables::run(&words));
    println!("enum_ordering {:?}", cases::enum_ordering::run(7));
    {
        use cases::added_strings::Segment;
        let all = [Segment::Literal("a".into()), Segment::Literal("b".into())];
        println!(
            "added_strings {:?} {:?}",
            cases::added_strings::run(&all),
            cases::added_strings::run(&[Segment::Other(1)])
        );
    }
    println!("joined {:?}", cases::added_strings::joined(vec![1, 2, 3]));
    println!("joined_words {:?}", cases::added_strings::joined_words(vec![1, 2, 3]));
    println!("macro_type_arguments {:?}", cases::macro_type_arguments::run("x"));
    println!("cfg_macro_arguments {:?}", cases::cfg_macro_arguments::run("x"));
    {
        use cases::move_closure_fields::{run, Separator};
        println!(
            "move_closure_fields {:?} {:?} {:?} {:?}",
            run(vec![1, 2, 3, 4], Separator::Undecided),
            run(vec![9], Separator::Comma),
            run(vec![1, 2, 3], Separator::Comma),
            run(vec![1, 2, 3, 4, 5], Separator::Comma)
        );
    }
    println!("moved_out_field {:?}", cases::moved_out_field::run());
    println!("local_type_names {:?}", cases::local_type_names::run());
    println!("unsized_in_struct {:?}", cases::unsized_in_struct::run());
    println!("promoted_fn_cast {:?}", cases::promoted_fn_cast::run());
    println!("same_name_callable {:?}", cases::same_name_callable::run());
    println!("same_name_statics {:?}", cases::same_name_statics::run());
    println!("track_caller_in_vtable {:?}", cases::track_caller_in_vtable::run());
    println!("static_in_defaulted_impl {:?}", cases::static_in_defaulted_impl::run());
    println!("downstream_add {}", String::from("a") + Tail(3));
    {
        use cases::niche_values::*;
        // The depths as this crate reads what `cases` made, then as `cases` reads what this
        // crate made.
        macro_rules! probes {
            ($($value:ident $make:ident $depth:ident;)*) => {$(
                println!(
                    "niche_values {} {:?} {:?}",
                    stringify!($value),
                    [0, 1, 2].map(|it| depth(&$make(it))),
                    [0, 1, 2].map(|it| $depth(&nest(it, $value()))),
                );
            )*};
        }
        probes! {
        parsed_err make_parsed_err depth_parsed_err;
        parsed_ok make_parsed_ok depth_parsed_ok;
        error make_error depth_error;
        kind make_kind depth_kind;
        signed make_signed depth_signed;
        wrap make_wrap depth_wrap;
        high make_high depth_high;
        gap make_gap depth_gap;
        two make_two depth_two;
        ends make_ends depth_ends;
        data make_data depth_data;
        nested make_nested depth_nested;
        nested_data make_nested_data depth_nested_data;
        pair make_pair depth_pair;
        triple make_triple depth_triple;
        letter make_letter depth_letter;
        ordering make_ordering depth_ordering;
        nonzero make_nonzero depth_nonzero;
        flag make_flag depth_flag;
        text make_text depth_text;
        boxed make_boxed depth_boxed;
        }
        println!(
            "niche_values is_ok {:?}",
            (is_ok(&parsed_ok()), is_ok(&parsed_err()), parsed_ok().is_ok(), parsed_err().is_ok())
        );
    }
    for n in [0u32, 1, 6, 255] {
        let text = ["", "text"].get(n as usize).copied();
        case!(inlined::constants(Some(n).filter(|it| it % 2 == 0), text, None));
        case!(inlined::constants(None, None, char::from_u32(n + 65)));
        case!(inlined::pointers(&[3, 1 << 40, u64::MAX, n as u64], n as usize));
        case!(inlined::unwinds(n), inlined::shapes(n as u64), inlined::shapes(!(n as u64)));
        case!(inlined::instances(n as u8, -(n as i64), "moved"));
    }
    case!(cases::opaque_of_trait_method::run(4), cases::opaque_of_trait_method::run(90));
    case!(cases::pointer_relations::run(7));
    case!(cases::static_places::run());
    case!(cases::associated_type_returns::run(&["a".into(), "bcd".into()]));
    case!(cases::constructor_extends_temporaries::run(), cases::statement_borrows_temporary::run(true, &None));
    case!(cases::undecided_alias_drop::run(3), cases::undecided_alias_drop::run(40));
    let text = std::hint::black_box(b"a // b /** c */ \\\" d <!-- e \"\"\" f /".to_vec());
    case!(
        cases::byte_compares::run(&text),
        cases::byte_compares::order(*b"abc", *b"abd", b"zy", b"za"),
        cases::byte_compares::order(*b"abd", *b"abc", b"za", b"zy"),
        cases::byte_compares::order(*b"abc", *b"abc", b"zy", b"zy"),
    );
    println!("crypto_lanes {}", cases::crypto_lanes::run());
    case!(cases::dyn_static_suites::run(), cases::narrow_constructors::aead::run());
    case!(cases::provided_futures::run(7));
    case!(cases::optional_repeats_per_field::run(5));
    case!(cases::deep_send_cycle::run(3));
    case!(cases::returned_in_place::run());
    case!(cases::elided_captures::run());
    case!(cases::destructuring_assignment::run());
    case!(cases::higher_ranked::run());
}

fn nest<T>(depth: u8, value: T) -> Option<Option<T>> {
    match depth {
        0 => None,
        1 => Some(None),
        _ => Some(Some(value)),
    }
}

fn depth<T>(value: &Option<Option<T>>) -> u8 {
    match value {
        None => 0,
        Some(None) => 1,
        Some(Some(_)) => 2,
    }
}

/// An `Add` impl for `String` that the crates below this one cannot see: what they add to
/// a `String` still has only the one impl to fit.
struct Tail(u8);

impl std::ops::Add<Tail> for String {
    type Output = String;
    fn add(mut self, rhs: Tail) -> String {
        self.push_str(&rhs.0.to_string());
        self
    }
}
