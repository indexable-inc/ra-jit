//! Differential-test corpus. Every `pub fn` here is compiled by ra-jit and called from the
//! rustc-built `runner`; the output must match an all-rustc build.
//!
//! One module per backend feature area, so a bisected culprit names the area.
// `box` patterns and 128-bit variable arguments are unstable: the cases are for a nightly
// toolchain only.
#![cfg_attr(feature = "nightly", feature(box_patterns, c_variadic_int128))]

pub mod stabilized;

pub mod scalars {
    pub fn wide_mul(a: u128, b: u128, c: i128) -> (u128, bool, Option<i128>, Option<u128>) {
        let (product, overflow) = a.overflowing_mul(b);
        (product, overflow, c.checked_mul(a as i128), (a >> 64).checked_mul(b >> 64))
    }
    pub fn arith(a: u32, b: u32) -> u32 {
        (a + b) * 3 - (a / (b | 1)) % 7
    }
    pub fn signed(a: i64, b: i64) -> i64 {
        (a - b) / 3 + (a % 5) - (b >> 2) + (a << 3)
    }
    pub fn bits(a: u16, b: u8) -> u16 {
        (a ^ 0x5555) & !(b as u16) | (a >> (b & 7))
    }
    pub fn casts(a: i8, b: u64) -> (i64, u8, i32, u16) {
        (a as i64, b as u8, a as i32 + b as i32, a as u16)
    }
    pub fn wrapping(a: u64, b: u64) -> u64 {
        a.wrapping_mul(b).wrapping_add(a.rotate_left(7)) ^ b.wrapping_sub(a)
    }
    pub fn checked(a: u32, b: u32) -> (Option<u32>, Option<u32>, u32) {
        (a.checked_add(b), a.checked_sub(b), a.saturating_sub(b))
    }
    pub fn wide(a: u128, b: u64) -> u128 {
        a.wrapping_mul(b as u128) + 1
    }
    pub fn compare(a: i32, b: i32) -> (bool, bool, bool, bool) {
        (a < b, a == b, a >= b, a != b)
    }
    pub fn counting(a: u64) -> (u32, u32, u32) {
        (a.count_ones(), a.leading_zeros(), a.trailing_zeros())
    }
}

pub mod control {
    pub fn collatz(mut n: u64) -> u32 {
        let mut steps = 0;
        while n != 1 {
            n = if n % 2 == 0 { n / 2 } else { 3 * n + 1 };
            steps += 1;
        }
        steps
    }
    pub fn classify(n: i32) -> &'static str {
        match n {
            i32::MIN..=-1 => "negative",
            0 => "zero",
            1..=9 => "small",
            _ => "large",
        }
    }
    pub fn nested(n: u32) -> u32 {
        let mut acc = 0;
        for i in 0..n {
            for j in 0..i {
                if (i + j) % 3 == 0 {
                    continue;
                }
                acc += i * j;
                if acc > 10_000 {
                    return acc;
                }
            }
        }
        acc
    }
    pub fn fib(n: u32) -> u64 {
        if n < 2 { n as u64 } else { fib(n - 1) + fib(n - 2) }
    }
}

pub mod aggregates {
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct Small(pub u8, pub u16);
    #[derive(Debug, Clone, Copy)]
    pub struct Pair {
        pub a: u32,
        pub b: u64,
    }
    #[derive(Debug, Clone, Copy)]
    pub struct Big {
        pub a: u64,
        pub b: [u32; 5],
        pub c: i8,
    }
    #[derive(Debug, Clone, Copy)]
    pub struct Odd(pub u8, pub u8, pub u8);

    pub fn small(s: Small) -> Small {
        Small(s.0 + 1, s.1 * 2)
    }
    pub fn pair(p: Pair, k: u32) -> Pair {
        Pair { a: p.a ^ k, b: p.b + k as u64 }
    }
    pub fn big(b: &Big, k: u32) -> Big {
        Big { a: b.a + 1, b: [b.b[0], k, b.b[2] + k, b.b[4], b.b[1]], c: -b.c }
    }
    pub fn big_by_value(mut b: Big) -> Big {
        b.b[3] = b.a as u32;
        b
    }
    pub fn odd(o: Odd) -> Odd {
        Odd(o.2, o.0, o.1)
    }
    pub fn tuple(t: (u8, u32, u64)) -> (u64, u8, u32) {
        (t.2 + 1, t.0 + 1, t.1 + 1)
    }
    pub fn array(a: [u16; 4]) -> [u16; 4] {
        [a[3], a[2], a[1], a[0] + 1]
    }
    pub fn nested_field(p: &(Pair, Small)) -> u64 {
        p.0.b + p.1.1 as u64
    }
    pub fn mutate(p: &mut Pair) {
        p.a += 1;
        p.b *= 2;
    }
}

pub mod enums {
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub enum Plain {
        A,
        B,
        C,
    }
    #[derive(Debug, Clone, Copy)]
    pub enum Explicit {
        Low = 3,
        Mid = 70,
        High = 200,
    }
    #[derive(Debug, Clone, Copy)]
    pub enum Data {
        Unit,
        One(u32),
        Two { a: u8, b: u64 },
    }
    #[derive(Debug)]
    pub enum HexError {
        Invalid { c: char, index: usize },
        OddLength,
        BadLength,
    }

    pub fn plain(n: u32) -> Plain {
        match n % 3 {
            0 => Plain::A,
            1 => Plain::B,
            _ => Plain::C,
        }
    }
    pub fn plain_back(p: Plain) -> u32 {
        match p {
            Plain::A => 10,
            Plain::B => 20,
            Plain::C => 30,
        }
    }
    pub fn explicit(e: Explicit) -> u8 {
        e as u8
    }
    pub fn data(n: u32) -> Data {
        match n {
            0 => Data::Unit,
            1..=9 => Data::One(n * 2),
            _ => Data::Two { a: n as u8, b: n as u64 * 3 },
        }
    }
    pub fn data_back(d: Data) -> u64 {
        match d {
            Data::Unit => 0,
            Data::One(x) => x as u64,
            Data::Two { a, b } => a as u64 + b,
        }
    }
    pub fn option(n: u32) -> Option<u32> {
        if n % 2 == 0 { Some(n / 2) } else { None }
    }
    pub fn option_ref(v: &Option<u64>) -> u64 {
        match v {
            Some(x) => *x + 1,
            None => 0,
        }
    }
    pub fn niche_ref<'a>(flag: bool, v: &'a u32) -> Option<&'a u32> {
        if flag { Some(v) } else { None }
    }
    pub fn niche_bool(n: u8) -> Option<bool> {
        match n {
            0 => None,
            1 => Some(false),
            _ => Some(true),
        }
    }
    pub fn niche_bool_back(v: Option<bool>) -> u8 {
        match v {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        }
    }
    pub fn nested_niche(c: u8, index: usize) -> Result<u8, HexError> {
        match c {
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'0'..=b'9' => Ok(c - b'0'),
            b'!' => Err(HexError::OddLength),
            b'?' => Err(HexError::BadLength),
            _ => Err(HexError::Invalid { c: c as char, index }),
        }
    }
    pub fn nested_niche_back(r: &Result<u8, HexError>) -> u64 {
        match r {
            Ok(v) => *v as u64,
            Err(HexError::OddLength) => 1000,
            Err(HexError::BadLength) => 2000,
            Err(HexError::Invalid { c, index }) => *c as u64 * 10_000 + *index as u64,
        }
    }
    pub fn result(n: i32) -> Result<u32, i64> {
        if n >= 0 { Ok(n as u32) } else { Err(n as i64 * 2) }
    }
    pub fn ordering(a: u32, b: u32) -> core::cmp::Ordering {
        a.cmp(&b)
    }
}

pub mod slices {
    pub fn sum(xs: &[u32]) -> u64 {
        let mut total = 0;
        let mut i = 0;
        while i < xs.len() {
            total += xs[i] as u64;
            i += 1;
        }
        total
    }
    pub fn sum_iter(xs: &[u32]) -> u64 {
        let mut total = 0;
        for x in xs {
            total += *x as u64;
        }
        total
    }
    pub fn first_last(xs: &[u8]) -> Option<(u8, u8)> {
        match xs {
            [] => None,
            [only] => Some((*only, *only)),
            [first, .., last] => Some((*first, *last)),
        }
    }
    pub fn reverse(xs: &mut [u16]) {
        let n = xs.len();
        let mut i = 0;
        while i < n / 2 {
            let tmp = xs[i];
            xs[i] = xs[n - 1 - i];
            xs[n - 1 - i] = tmp;
            i += 1;
        }
    }
    pub fn tail(xs: &[u32]) -> &[u32] {
        &xs[1..]
    }
    pub fn window(xs: &[u32], from: usize, to: usize) -> u64 {
        sum(&xs[from..to])
    }
    pub fn str_len(s: &str) -> usize {
        s.len()
    }
    pub fn bytes_hash(s: &str) -> u32 {
        let mut hash = 5381u32;
        for b in s.as_bytes() {
            hash = hash.wrapping_mul(33) ^ *b as u32;
        }
        hash
    }
    pub fn literal(n: u8) -> &'static str {
        if n == 0 { "zero" } else { "nonzero" }
    }
    pub fn array_slice(a: &[u32; 6]) -> u64 {
        sum(a) + sum(&a[2..4])
    }
    pub fn get(xs: &[u64], i: usize) -> Option<u64> {
        xs.get(i).copied()
    }
    pub fn is_ascii_digit_run(s: &[u8]) -> usize {
        s.iter().take_while(|b| b.is_ascii_digit()).count()
    }
}

pub mod pointers {
    pub struct Cursor {
        pub start: *const u8,
        pub end: *const u8,
    }

    /// Ordering on raw pointers: RA used to lower this to a self-recursive `PartialOrd::lt`.
    pub fn peek(c: &Cursor) -> Option<u8> {
        if c.start < c.end { Some(unsafe { *c.start }) } else { None }
    }
    pub fn compare(a: *const u32, b: *const u32) -> (bool, bool, bool, bool) {
        (a < b, a >= b, a == b, a != b)
    }
    pub fn advance(c: &mut Cursor, n: usize) -> usize {
        c.start = unsafe { c.start.add(n) };
        unsafe { c.end.offset_from(c.start) as usize }
    }
    pub fn write(p: *mut u64, v: u64) -> u64 {
        unsafe {
            let old = *p;
            *p = v;
            old
        }
    }
}

pub mod consts {
    pub const LIMIT: u32 = 40 + 2;
    pub static TABLE: [u8; 8] = [1, 1, 2, 3, 5, 8, 13, 21];
    const NAMES: [&str; 3] = ["alpha", "beta", "gamma"];
    pub struct Config {
        pub scale: u32,
        pub name: &'static str,
    }
    const CONFIG: Config = Config { scale: 9, name: "cfg" };

    pub fn limit(n: u32) -> u32 {
        n.min(LIMIT)
    }
    pub fn table(i: usize) -> u8 {
        TABLE[i % TABLE.len()]
    }
    pub fn name(i: usize) -> &'static str {
        NAMES[i % 3]
    }
    pub fn config(n: u32) -> (u32, &'static str) {
        (n * CONFIG.scale, CONFIG.name)
    }
    pub fn local_array(i: usize) -> u32 {
        let squares = [0u32, 1, 4, 9, 16, 25];
        squares[i % 6]
    }
    pub fn repeat(v: u16) -> [u16; 5] {
        [v; 5]
    }
}

pub mod generics {
    fn pick<T>(first: bool, a: T, b: T) -> T {
        if first { a } else { b }
    }
    fn swap<A: Copy, B: Copy>(pair: (A, B)) -> (B, A) {
        (pair.1, pair.0)
    }
    #[inline]
    fn add_one(x: u32) -> u32 {
        x + 1
    }
    pub struct Counter {
        n: u32,
    }
    impl Counter {
        pub fn new(n: u32) -> Counter {
            Counter { n }
        }
        pub fn bump(&mut self, by: u32) -> u32 {
            self.n += by;
            self.n
        }
        pub fn get(&self) -> u32 {
            self.n
        }
    }
    pub trait Shape {
        fn area(&self) -> u64;
        fn double(&self) -> u64 {
            self.area() * 2
        }
    }
    pub struct Rect(pub u32, pub u32);
    pub struct Square(pub u32);
    impl Shape for Rect {
        fn area(&self) -> u64 {
            self.0 as u64 * self.1 as u64
        }
    }
    impl Shape for Square {
        fn area(&self) -> u64 {
            self.0 as u64 * self.0 as u64
        }
        fn double(&self) -> u64 {
            7
        }
    }
    fn total<S: Shape>(s: &S) -> u64 {
        s.area() + s.double()
    }

    pub fn picks(a: u64, b: u64) -> (u64, u8, (u16, u16)) {
        (pick(true, a, b), pick(false, 1u8, 2u8), pick(a > b, (1, 2), (3, 4)))
    }
    pub fn swaps(a: u8, b: u64) -> (u64, u8) {
        swap((a, b))
    }
    pub fn inline(x: u32) -> u32 {
        add_one(add_one(x))
    }
    pub fn methods(n: u32) -> u32 {
        let mut c = Counter::new(n);
        c.bump(3);
        c.bump(4) + c.get()
    }
    pub fn statics(w: u32, h: u32) -> u64 {
        total(&Rect(w, h)) + total(&Square(w))
    }
    pub fn option_map(v: Option<u32>) -> u32 {
        v.map(|x| x * 2).unwrap_or(7)
    }
    pub fn core_generics(a: u32, b: u32) -> (u32, u32) {
        (core::cmp::max(a, b), core::mem::replace(&mut { a }, b))
    }
}

pub mod closures {
    pub fn capture(k: u32, xs: &[u32]) -> u32 {
        let add = |x: u32| x + k;
        let mut total = 0;
        for x in xs {
            total += add(*x);
        }
        total
    }
    pub fn capture_mut(n: u32) -> u32 {
        let mut count = 0;
        let mut bump = |by: u32| count += by;
        bump(n);
        bump(2);
        count
    }
    pub fn iter_chain(xs: &[u32]) -> u32 {
        xs.iter().filter(|x| **x % 2 == 1).map(|x| x * 3).sum()
    }
    pub fn fn_pointer(f: fn(u32) -> u32, x: u32) -> u32 {
        f(f(x))
    }
}

pub mod callables {
    fn double(x: u32) -> u32 {
        x * 2
    }
    fn pair(x: u32) -> (u64, u64) {
        (x as u64, x as u64 + 1)
    }
    fn apply<F: Fn(u32) -> u32>(f: F, x: u32) -> u32 {
        f(x)
    }
    fn apply_mut<F: FnMut(u32)>(mut f: F, x: u32) {
        f(x);
        f(x + 1)
    }
    fn apply_once<F: FnOnce() -> [u64; 3]>(f: F) -> [u64; 3] {
        f()
    }

    pub fn reify(x: u32) -> u32 {
        let f: fn(u32) -> u32 = double;
        f(x) + apply(double, x)
    }
    pub fn reify_pair(x: u32) -> (u64, u64) {
        let f: fn(u32) -> (u64, u64) = pair;
        f(x)
    }
    pub fn generic_closure(k: u32, x: u32) -> u32 {
        apply(|v| v * k, x) + apply(&|v| v + k, x)
    }
    pub fn generic_mut(x: u32) -> u32 {
        let mut total = 0;
        apply_mut(|v| total += v, x);
        total
    }
    pub fn generic_once(a: u64) -> [u64; 3] {
        let big = [a, a + 1, a + 2];
        apply_once(move || big)
    }
    pub fn takes_fn(f: fn(u8, u8) -> u8, a: u8) -> u8 {
        f(a, 3)
    }
}

pub mod odd {
    #[derive(Clone, Copy)]
    pub struct B3(pub u8, pub u8, pub u8);
    #[derive(Clone, Copy)]
    pub struct B5(pub [u8; 5]);
    #[derive(Clone, Copy)]
    pub struct B6(pub u16, pub u16, pub u16);
    #[derive(Clone, Copy)]
    pub struct B7(pub [u8; 7]);

    pub fn rot3(b: B3) -> B3 {
        B3(b.1, b.2, b.0)
    }
    pub fn rot5(b: B5) -> B5 {
        B5([b.0[4], b.0[0], b.0[1], b.0[2], b.0[3]])
    }
    pub fn rot6(b: B6) -> B6 {
        B6(b.2, b.0, b.1)
    }
    pub fn rot7(b: B7, x: u8) -> B7 {
        B7([x, b.0[0], b.0[1], b.0[2], b.0[3], b.0[4], b.0[5]])
    }
    pub fn chain(b: B3) -> u32 {
        let r = rot3(rot3(b));
        r.0 as u32 * 10000 + r.1 as u32 * 100 + r.2 as u32
    }
}

pub mod dynamic {
    pub trait Speak {
        fn speak(&self) -> u32;
    }
    pub struct Dog(pub u32);
    pub struct Cat;
    impl Speak for Dog {
        fn speak(&self) -> u32 {
            self.0
        }
    }
    impl Speak for Cat {
        fn speak(&self) -> u32 {
            9
        }
    }

    pub fn call(s: &dyn Speak) -> u32 {
        s.speak() + 1
    }
    pub fn make(dog: bool) -> u32 {
        let d = Dog(40);
        let s: &dyn Speak = if dog { &d } else { &Cat };
        s.speak()
    }
}

pub mod objects {
    use std::cell::Cell;
    use std::fmt::{self, Display};

    pub trait Base {
        fn base(&self) -> u32;
        fn twice(&self) -> u32 {
            self.base() * 2
        }
    }
    pub trait Named {
        fn name(&self) -> &'static str;
    }
    pub trait Sized2: Sized {
        fn nope(&self) -> u32 {
            0
        }
    }
    pub trait Mid: Base {
        fn mid(&self, x: u32) -> u32;
    }
    /// Two supertraits with methods, a marker in between, and a method that is not in the vtable.
    pub trait Top: Mid + Send + Named {
        fn top(&mut self) -> u64;
        fn skipped(&self) -> u32
        where
            Self: Sized,
        {
            77
        }
        fn big(&self) -> [u64; 3];
    }
    pub trait Conv<T> {
        fn conv(&self, x: T) -> T;
    }

    pub struct One(pub u32);
    pub struct Two(pub u32, pub u64);
    impl Base for One {
        fn base(&self) -> u32 {
            self.0
        }
    }
    impl Named for One {
        fn name(&self) -> &'static str {
            "one"
        }
    }
    impl Mid for One {
        fn mid(&self, x: u32) -> u32 {
            self.0 + x
        }
    }
    impl Top for One {
        fn top(&mut self) -> u64 {
            self.0 += 1;
            self.0 as u64
        }
        fn big(&self) -> [u64; 3] {
            [self.0 as u64, 1, 2]
        }
    }
    impl Base for Two {
        fn base(&self) -> u32 {
            self.0
        }
        fn twice(&self) -> u32 {
            5
        }
    }
    impl Named for Two {
        fn name(&self) -> &'static str {
            "two"
        }
    }
    impl Mid for Two {
        fn mid(&self, x: u32) -> u32 {
            self.0 * x
        }
    }
    impl Top for Two {
        fn top(&mut self) -> u64 {
            self.1 += 10;
            self.1
        }
        fn big(&self) -> [u64; 3] {
            [self.1, self.0 as u64, 0]
        }
    }
    impl Conv<u8> for One {
        fn conv(&self, x: u8) -> u8 {
            x.wrapping_add(self.0 as u8)
        }
    }
    impl Conv<u64> for One {
        fn conv(&self, x: u64) -> u64 {
            x * self.0 as u64
        }
    }
    impl Display for One {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "One({})", self.0)
        }
    }

    /// Calls every kind of slot on an object somebody else made.
    pub fn use_top(t: &mut dyn Top) -> (u32, u32, u32, u64, u64, usize) {
        let big = t.big();
        (t.base(), t.twice(), t.mid(3), t.top(), big[0] + big[1] + big[2], t.name().len())
    }
    /// Makes the objects for somebody else to call.
    pub fn make_top(two: bool) -> Box<dyn Top> {
        if two { Box::new(Two(3, 4)) } else { Box::new(One(7)) }
    }
    pub fn make_ref(one: &One) -> &dyn Top {
        one
    }
    pub fn local(x: u32) -> u64 {
        let mut one = One(x);
        let t: &mut dyn Top = &mut one;
        t.top() + t.mid(1) as u64 + t.twice() as u64
    }
    pub fn upcast_first(t: &dyn Top) -> &dyn Mid {
        t
    }
    pub fn upcast_base(t: &dyn Top) -> &dyn Base {
        t
    }
    pub fn upcast_named(t: &dyn Top) -> &dyn Named {
        t
    }
    pub fn upcast_box(t: Box<dyn Top>) -> Box<dyn Named> {
        t
    }
    pub fn use_named(n: &dyn Named) -> usize {
        n.name().len()
    }
    pub fn generic_trait(one: &One) -> (u8, u64) {
        let a: &dyn Conv<u8> = one;
        let b: &dyn Conv<u64> = one;
        (a.conv(250), b.conv(6))
    }
    pub fn boxed_sum(xs: &[Box<dyn Base>]) -> u32 {
        let mut sum = 0;
        for x in xs {
            sum += x.base();
        }
        sum
    }
    pub fn display(x: u32) -> String {
        let d: &dyn Display = &One(x);
        d.to_string()
    }
    pub fn show(d: &dyn Display) -> usize {
        d.to_string().len()
    }

    pub struct Loud<'a>(pub &'a Cell<u64>, pub u64);
    impl Drop for Loud<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() * 10 + self.1);
        }
    }
    pub trait Thing {
        fn id(&self) -> u64;
    }
    impl Thing for Loud<'_> {
        fn id(&self) -> u64 {
            self.1
        }
    }
    impl Thing for u8 {
        fn id(&self) -> u64 {
            *self as u64
        }
    }
    pub fn make_thing<'a>(log: &'a Cell<u64>, loud: bool) -> Box<dyn Thing + 'a> {
        if loud { Box::new(Loud(log, 4)) } else { Box::new(9u8) }
    }
    pub fn drop_thing(t: Box<dyn Thing + '_>) -> u64 {
        t.id()
    }

    pub fn call_fn(f: &dyn Fn(u32, u32) -> u32, x: u32) -> u32 {
        f(x, 2) + f(1, x)
    }
    pub fn call_fn_mut(f: &mut dyn FnMut(u64) -> u64) -> u64 {
        f(1);
        f(2);
        f(3)
    }
    pub fn make_fn(k: u32) -> Box<dyn Fn(u32) -> u32> {
        Box::new(move |x| x * k + 1)
    }
    pub fn make_fn_mut(mut acc: u64) -> Box<dyn FnMut(u64) -> u64> {
        Box::new(move |x| {
            acc += x;
            acc
        })
    }
    pub fn make_fn_once(s: String) -> Box<dyn FnOnce(usize) -> usize> {
        Box::new(move |x| s.len() + x)
    }
    pub fn local_fn(xs: &[u32], k: u32) -> u32 {
        let f: &dyn Fn(u32) -> u32 = &|x| x + k;
        let mut sum = 0;
        for &x in xs {
            sum += f(x);
        }
        sum
    }
}

pub mod drops {
    use std::cell::Cell;

    /// Appends its digit to the log when dropped.
    pub struct Noisy<'a>(pub &'a Cell<u64>, pub u64);
    impl Drop for Noisy<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() * 10 + self.1);
        }
    }
    pub struct Pair<'a> {
        a: Noisy<'a>,
        b: Noisy<'a>,
    }
    pub enum Either<'a> {
        Left(Noisy<'a>),
        Right(Noisy<'a>, Noisy<'a>),
        Neither,
    }

    pub fn scope(log: &Cell<u64>) {
        let _a = Noisy(log, 1);
        let _b = Noisy(log, 2);
        {
            let _c = Noisy(log, 3);
        }
    }
    pub fn moved(log: &Cell<u64>, keep: bool) -> Option<Noisy<'_>> {
        let a = Noisy(log, 1);
        let b = Noisy(log, 2);
        if keep {
            Some(a)
        } else {
            drop(b);
            None
        }
    }
    pub fn fields(log: &Cell<u64>) {
        let _p = Pair { a: Noisy(log, 4), b: Noisy(log, 5) };
        let _t = (Noisy(log, 6), 0u8, Noisy(log, 7));
    }
    pub fn variants(log: &Cell<u64>, which: u8) {
        let _e = match which {
            0 => Either::Left(Noisy(log, 1)),
            1 => Either::Right(Noisy(log, 2), Noisy(log, 3)),
            _ => Either::Neither,
        };
    }
    pub fn arrays(log: &Cell<u64>) {
        let _xs = [Noisy(log, 1), Noisy(log, 2), Noisy(log, 3)];
    }
    pub fn boxes(log: &Cell<u64>) {
        let _b = Box::new(Noisy(log, 8));
        let _v = vec![Noisy(log, 1), Noisy(log, 2)];
    }
    pub fn param(_n: Noisy<'_>, log: &Cell<u64>) {
        let _late = Noisy(log, 9);
    }
    pub fn looped(log: &Cell<u64>, n: u64) {
        for i in 0..n {
            let _x = Noisy(log, i + 1);
        }
    }
    fn make(log: &Cell<u64>, n: u64) -> Noisy<'_> {
        Noisy(log, n)
    }
    /// Temporaries die at the end of their statement, before the next one runs.
    pub fn statement_temps(log: &Cell<u64>) -> u64 {
        let a = make(log, 1).1;
        let seen = log.get();
        make(log, 2).1;
        let _keep = make(log, 3);
        a + seen * 100 + log.get() * 10000
    }
    /// A temporary in the tail expression dies as the function returns.
    pub fn tail_temp(log: &Cell<u64>) -> u64 {
        make(log, 4).1 + log.get()
    }
    pub fn tail_temp_block(log: &Cell<u64>) -> u64 {
        let n = { make(log, 5).1 };
        n + log.get() * 10
    }
    pub fn tail_temp_call(cell: &std::cell::RefCell<Vec<u64>>, n: u64) -> usize {
        fn add(cell: &std::cell::RefCell<Vec<u64>>, n: u64) -> usize {
            let id = cell.borrow().len();
            cell.borrow_mut().push(n);
            id
        }
        fn add2(cell: &std::cell::RefCell<Vec<u64>>, n: u64) -> usize {
            cell.borrow_mut().len() + n as usize
        }
        add(cell, n) + add(cell, n + 1) + add2(cell, n) + add2(cell, n)
    }
    pub fn condition_temp(log: &Cell<u64>) -> u64 {
        if make(log, 6).1 == 6 { log.get() + 1 } else { 0 }
    }
    pub fn match_temp(log: &Cell<u64>) -> u64 {
        match make(log, 7).1 {
            7 => log.get() + 1,
            _ => 0,
        }
    }
    pub fn reassigned(log: &Cell<u64>) {
        let mut a = make(log, 1);
        a = make(log, 2);
        let _b = make(log, 3);
        a.1 += 0;
    }
    pub fn partial_move(log: &Cell<u64>) -> Noisy<'_> {
        let p = Pair { a: make(log, 4), b: make(log, 5) };
        let _late = make(log, 6);
        p.a
    }
    /// `*b;` moves the contents out and drops them, and the box can be filled again.
    pub fn box_refilled(log: &Cell<u64>) {
        let mut b = Box::new(make(log, 1));
        *b;
        let _mid = make(log, 2);
        *b = make(log, 3);
    }
    /// What a pattern leaves behind in an enum or an array is still dropped.
    pub fn left_behind(log: &Cell<u64>) {
        let pair = Some((make(log, 1), make(log, 2)));
        match pair {
            Some((_taken, _)) => {}
            None => {}
        }
        let [_first, ..] = [make(log, 3), make(log, 4), make(log, 5)];
    }
    /// Bindings drop in the reverse of the order the pattern names them, whichever
    /// alternative matched, and those of a later parameter go first.
    pub fn binding_order(log: &Cell<u64>, first: bool) {
        let ((true, _x, _y) | (false, _y, _x)) = (first, make(log, 1), make(log, 2));
        (|_a, (Ok(_b) | Err(_b)), _c| {})(
            make(log, 3),
            Ok::<_, Noisy<'_>>(make(log, 4)),
            make(log, 5),
        );
    }
    fn next<'a>(n: &Noisy<'a>) -> Noisy<'a> {
        make(n.0, n.1 + 1)
    }
    /// Before the 2024 edition the temporaries in a block's tail outlive its locals.
    pub fn tail_temporaries(log: &Cell<u64>) -> u64 {
        let a = make(log, 1);
        let b = {
            let _local = make(log, 5);
            next(&next(&a)).1
        };
        b + next(&a).1
    }
    /// An arm's bindings are dropped when the arm ends, and a guard's temporaries before the
    /// arm is entered. A guard that fails gives the value back for the next arm.
    pub fn arms(log: &Cell<u64>, pick: u64) {
        let pair = Some((make(log, 1), make(log, 2)));
        let _late = make(log, 3);
        match pair {
            Some((taken, _)) if next(&taken).1 == pick => log.set(log.get() * 10 + 7),
            Some(whole) => log.set(log.get() * 10 + whole.1.1 + 6),
            None => {}
        }
    }
    /// What `if let` binds is dropped at the end of the branch, not of the function.
    pub fn if_let(log: &Cell<u64>, some: bool) {
        let _outer = make(log, 1);
        if let Some(_inner) = some.then(|| make(log, 2)) {
            log.set(log.get() * 10 + 3);
        } else {
            log.set(log.get() * 10 + 4);
        }
        log.set(log.get() * 10 + 5);
    }
    pub fn strings(n: usize) -> usize {
        let s = "ab".repeat(n);
        let t = s.clone();
        let v: Vec<String> = vec![s, t];
        v.len() + v[0].len()
    }
}

pub mod heap {
    pub fn boxed(n: u64) -> Box<u64> {
        Box::new(n + 1)
    }
    pub fn vec(n: u32) -> Vec<u32> {
        let mut v = Vec::new();
        for i in 0..n {
            v.push(i * i);
        }
        v
    }
    pub fn string(n: u32) -> String {
        let mut s = String::from("n=");
        for _ in 0..n {
            s.push('x');
        }
        s
    }
    pub fn consume(v: Vec<u32>) -> usize {
        v.len()
    }
    pub fn clears(n: usize) -> usize {
        let mut empty: Vec<Vec<Option<std::sync::Arc<str>>>> = Vec::new();
        empty.clear();
        let mut full = vec![vec![Some(std::sync::Arc::<str>::from("a")), None]; n];
        full.clear();
        full.push(vec![]);
        empty.len() + full.len()
    }
    /// A `?Sized` last field stays last, even where sorting by size would move it first.
    pub fn unsizable_tail(
        cell: &std::cell::RefCell<[u64; 14]>,
        lock: &std::sync::Mutex<(u128, u8)>,
    ) -> u64 {
        *lock.lock().unwrap() = (7, 1);
        cell.borrow_mut()[13] += 1;
        cell.borrow()[0] + cell.borrow()[13]
    }
    pub trait Named {
        fn name(&self) -> usize;
    }
    impl Named for String {
        fn name(&self) -> usize {
            self.len()
        }
    }
    pub fn shared(text: &str) -> (std::sync::Arc<dyn Named>, Box<dyn Fn() -> usize>) {
        let shared: std::sync::Arc<dyn Named> = std::sync::Arc::new(text.to_owned());
        let call = {
            let shared = std::sync::Arc::clone(&shared);
            let call: Box<dyn Fn() -> usize> = Box::new(move || shared.name());
            call
        };
        (shared, call)
    }
    pub fn hashmap() -> std::collections::HashMap<std::sync::Arc<str>, u32> {
        std::collections::HashMap::new()
    }
    pub fn hashmaps(n: usize) -> Vec<std::collections::HashMap<std::sync::Arc<str>, u32>> {
        let mut maps = vec![];
        for _ in 0..n {
            maps.push(std::collections::HashMap::new());
        }
        maps
    }
    pub fn format(a: u32, b: &str) -> String {
        format!("{a}-{b}")
    }
}

pub mod simd {
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    pub fn ascii(s: &str) -> bool {
        s.is_ascii()
    }
    pub fn ascii_bytes(b: &[u8]) -> bool {
        b.is_ascii()
    }
    #[cfg(target_arch = "x86_64")]
    pub fn find(hay: &[u8; 16], needle: u8) -> i32 {
        unsafe {
            let hay = _mm_loadu_si128(hay.as_ptr().cast());
            _mm_movemask_epi8(_mm_cmpeq_epi8(hay, _mm_set1_epi8(needle as i8)))
        }
    }
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "ssse3")]
    pub unsafe fn shuffle(a: &[u8; 16], b: &[u8; 16]) -> ([u8; 16], i32) {
        unsafe {
            let x = _mm_lddqu_si128(a.as_ptr().cast());
            let y = _mm_lddqu_si128(b.as_ptr().cast());
            let z = _mm_shuffle_epi8(x, _mm_max_epu8(y, _mm_srli_epi16::<3>(x)));
            let z = _mm_andnot_si128(_mm_slli_epi16::<1>(y), _mm_min_epi8(z, x));
            let mut out = [0; 16];
            _mm_storeu_si128(out.as_mut_ptr().cast(), z);
            (out, _mm_extract_epi16::<3>(z))
        }
    }
    #[cfg(target_arch = "x86_64")]
    pub fn mix(a: &[u8; 16], b: &[u8; 16]) -> [u8; 16] {
        unsafe {
            let x = _mm_loadu_si128(a.as_ptr().cast());
            let y = _mm_loadu_si128(b.as_ptr().cast());
            let z = _mm_xor_si128(_mm_and_si128(x, y), _mm_or_si128(x, _mm_add_epi8(x, y)));
            let mut out = [0; 16];
            _mm_storeu_si128(out.as_mut_ptr().cast(), _mm_sub_epi8(z, _mm_cmpgt_epi8(x, y)));
            out
        }
    }
    #[cfg(target_arch = "aarch64")]
    use std::arch::aarch64::*;

    /// A bit per byte that is the needle, as `_mm_movemask_epi8` has them.
    #[cfg(target_arch = "aarch64")]
    pub fn find(hay: &[u8; 16], needle: u8) -> i32 {
        unsafe {
            let hay = vld1q_u8(hay.as_ptr());
            let equal = vceqq_u8(hay, vdupq_n_u8(needle));
            let bits =
                vld1q_u8([1u8, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128].as_ptr());
            let masked = vandq_u8(equal, bits);
            let low = vaddv_u8(vget_low_u8(masked)) as i32;
            let high = vaddv_u8(vget_high_u8(masked)) as i32;
            low | high << 8
        }
    }
    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "neon")]
    pub unsafe fn shuffle(a: &[u8; 16], b: &[u8; 16]) -> ([u8; 16], i32) {
        unsafe {
            let x = vld1q_u8(a.as_ptr());
            let y = vld1q_u8(b.as_ptr());
            let shifted = vreinterpretq_u8_u16(vshrq_n_u16::<3>(vreinterpretq_u16_u8(x)));
            let z = vqtbl1q_u8(x, vmaxq_u8(y, shifted));
            let doubled = vreinterpretq_u8_u16(vshlq_n_u16::<1>(vreinterpretq_u16_u8(y)));
            let least =
                vreinterpretq_u8_s8(vminq_s8(vreinterpretq_s8_u8(z), vreinterpretq_s8_u8(x)));
            let z = vbicq_u8(least, doubled);
            let mut out = [0; 16];
            vst1q_u8(out.as_mut_ptr(), z);
            (out, vgetq_lane_u16::<3>(vreinterpretq_u16_u8(z)) as i32)
        }
    }
    #[cfg(target_arch = "aarch64")]
    pub fn mix(a: &[u8; 16], b: &[u8; 16]) -> [u8; 16] {
        unsafe {
            let x = vld1q_u8(a.as_ptr());
            let y = vld1q_u8(b.as_ptr());
            let z = veorq_u8(vandq_u8(x, y), vorrq_u8(x, vaddq_u8(x, y)));
            let greater = vcgtq_s8(vreinterpretq_s8_u8(x), vreinterpretq_s8_u8(y));
            let mut out = [0; 16];
            vst1q_u8(out.as_mut_ptr(), vsubq_u8(z, greater));
            out
        }
    }
}

pub mod statics {
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(5);
    static FLAG: AtomicBool = AtomicBool::new(false);
    static LOW: AtomicI32 = AtomicI32::new(0);
    static mut PLAIN: [u32; 3] = [1, 2, 3];

    pub fn bump(n: u64) -> u64 {
        COUNTER.fetch_add(n, Ordering::Relaxed);
        COUNTER.fetch_sub(1, Ordering::SeqCst);
        COUNTER.fetch_or(1 << 40, Ordering::AcqRel);
        COUNTER.load(Ordering::Acquire)
    }
    pub fn once() -> (bool, bool, bool) {
        let first = FLAG.swap(true, Ordering::SeqCst);
        let second = FLAG.compare_exchange(true, false, Ordering::SeqCst, Ordering::Relaxed);
        let third = FLAG.compare_exchange(true, false, Ordering::SeqCst, Ordering::Relaxed);
        (first, second.is_ok(), third.is_ok())
    }
    pub fn lowest(n: i32) -> i32 {
        LOW.fetch_min(n, Ordering::Relaxed);
        LOW.fetch_xor(0, Ordering::Relaxed);
        LOW.load(Ordering::Relaxed)
    }
    pub fn local(n: usize) -> usize {
        let a = AtomicUsize::new(n);
        a.store(a.load(Ordering::Relaxed) * 3, Ordering::Release);
        a.fetch_max(10, Ordering::Relaxed);
        a.fetch_and(!1, Ordering::Relaxed);
        a.into_inner()
    }
    pub fn plain(i: usize, n: u32) -> u32 {
        unsafe {
            PLAIN[i] += n;
            PLAIN[0] + PLAIN[1] + PLAIN[2]
        }
    }
}

pub mod floats {
    pub fn arith(a: f64, b: f64) -> (f64, f64, f64, f64, f64) {
        (a + b, a - b, a * b, a / b, a % b)
    }
    pub fn single(a: f32, b: f32) -> (f32, f32, bool, bool) {
        (a * b + 0.5, -a % b, a < b, a == b)
    }
    pub fn compare(a: f64, b: f64) -> u8 {
        (a < b) as u8
            | ((a <= b) as u8) << 1
            | ((a > b) as u8) << 2
            | ((a >= b) as u8) << 3
            | ((a != b) as u8) << 4
    }
    pub fn to_int(x: f64) -> (i64, u64, i32, u8, i8, u16) {
        (x as i64, x as u64, x as i32, x as u8, x as i8, x as u16)
    }
    pub fn from_int(a: i64, b: u64, c: u8, d: i16) -> (f64, f64, f32, f64, f32) {
        (a as f64, b as f64, c as f32, d as f64, b as f32)
    }
    pub fn widen(x: f32, y: f64) -> (f64, f32) {
        (x as f64, y as f32)
    }
    pub fn bits(x: f64) -> (u64, f64, bool) {
        (x.to_bits(), f64::from_bits(x.to_bits() ^ (1 << 63)), x.is_nan())
    }
    #[derive(Clone, Copy)]
    pub struct Point {
        pub x: f64,
        pub y: f64,
    }
    pub fn point(p: Point, scale: f32) -> Point {
        Point { x: p.x * scale as f64, y: -p.y }
    }
    pub fn mixed(n: u32, x: f32) -> (u32, f32) {
        (n + 1, x + n as f32)
    }
    pub fn sum(values: &[f64]) -> f64 {
        let mut total = 0.0;
        for v in values {
            total += *v;
        }
        total
    }
}

pub mod locations {
    use std::panic::Location;

    #[track_caller]
    fn here() -> &'static Location<'static> {
        Location::caller()
    }
    #[track_caller]
    fn through() -> &'static Location<'static> {
        here()
    }
    /// The same location reaches both, however deep the tracked chain is.
    pub fn tracked() -> bool {
        let (a, b) = (here(), through());
        a.file() == b.file() && !a.file().is_empty()
    }
    pub fn unwraps(x: Option<u8>, y: Result<u8, u8>) -> u8 {
        x.unwrap() + y.unwrap() + x.expect("some")
    }
    pub fn borrows(cell: &std::cell::RefCell<u8>) -> u8 {
        *cell.borrow_mut() += 1;
        *cell.borrow()
    }
    pub struct Thrower;
    impl Thrower {
        /// doc
        #[track_caller]
        pub fn method<T>(_: T) -> &'static Location<'static> {
            Location::caller()
        }
    }
    /// A pointer to a tracked function reports where the function is declared, not the call.
    pub fn through_pointers(x: Option<u8>) -> (u32, u32, u32, u32, bool) {
        let direct: fn() -> &'static Location<'static> = here;
        let method: fn(u8) -> &'static Location<'static> = Thrower::method::<u8>;
        let table: [(fn(u8) -> &'static Location<'static>, u8); 2] =
            [(Thrower::method, 1), (Thrower::method, 2)];
        let (a, b) = (direct(), method(3));
        let c = table[x.unwrap_or(0) as usize & 1].0(x.unwrap_or(0));
        let lines = (a.line(), a.column(), b.line(), b.column());
        (lines.0, lines.1, lines.2, lines.3, c.file() == a.file() && c.line() == b.line())
    }
}

pub mod misc {
    pub fn big_repeat(seed: u8) -> u32 {
        let table = [seed; 4096];
        let wide = [(seed as u32, 7u16); 100];
        table.iter().map(|&b| b as u32).sum::<u32>() + wide[99].0 + wide[50].1 as u32
    }
    pub enum Shape {
        Circle(u32),
        Rect(u32, u32),
    }
    pub struct Wrapper(pub u32, pub bool);
    pub fn constructors(values: &[u32]) -> (u32, u32, u32) {
        let options: Vec<Option<u32>> = values.iter().copied().map(Some).collect();
        let shapes: Vec<Shape> = values.iter().copied().map(Shape::Circle).collect();
        let wrapped = values.first().copied().map(|v| (v, true)).map(|(a, b)| Wrapper(a, b));
        let area: u32 = shapes
            .iter()
            .map(|s| match s {
                Shape::Circle(r) => 3 * r * r,
                Shape::Rect(w, h) => w * h,
            })
            .sum();
        (options.iter().flatten().sum(), area, wrapped.map_or(0, |w| w.0 + w.1 as u32))
    }
    pub fn lines(text: &str) -> (usize, usize) {
        (text.lines().count(), text.lines().map(str::len).sum())
    }
    pub fn keyword(name: &str) -> u32 {
        match name {
            "alpha" => 1,
            "beta" | "gamma" => 2,
            "" => 3,
            "alphabet" => 4,
            _ => 0,
        }
    }
    pub fn barrier(mut x: u64) -> u64 {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            core::arch::asm!("/*{0}*/", inout(reg) x);
        }
        x = x.wrapping_mul(3);
        x
    }
    fn target(x: u32) -> u32 {
        x + 1
    }
    pub fn fn_item_cast(x: u32) -> (bool, u32) {
        let raw = target as *const ();
        let addr = target as usize;
        let back: fn(u32) -> u32 = unsafe { core::mem::transmute(raw) };
        (raw as usize == addr, back(x))
    }
    pub fn float_math(x: f64, y: f32) -> (f64, f64, f64, f64, f64, f32, f64, f64) {
        (
            x.abs(),
            x.sqrt(),
            x.floor(),
            x.ceil(),
            x.round(),
            y.abs().trunc(),
            x.copysign(-1.0),
            x.max(2.5).min(100.0),
        )
    }
    fn double(x: u32) -> u32 {
        x * 2
    }
    fn triple(x: u32) -> u32 {
        x * 3
    }
    static TABLE: [fn(u32) -> u32; 2] = [double, triple];
    static LAZY: core::sync::atomic::AtomicPtr<()> =
        core::sync::atomic::AtomicPtr::new(double as *mut ());
    pub fn fn_statics(x: u32) -> (u32, u32, u32) {
        use core::sync::atomic::Ordering;
        let first: fn(u32) -> u32 = unsafe { core::mem::transmute(LAZY.load(Ordering::Relaxed)) };
        LAZY.store(triple as *mut (), Ordering::Relaxed);
        let second: fn(u32) -> u32 = unsafe { core::mem::transmute(LAZY.load(Ordering::Relaxed)) };
        (TABLE[0](x) + TABLE[1](x), first(x), second(x))
    }
}

/// Functions with the C calling convention, called from the rustc-built runner.
pub mod foreign {
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Big {
        pub a: u64,
        pub b: u64,
        pub c: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Mixed {
        pub a: f64,
        pub b: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Small {
        pub a: u8,
        pub b: u8,
        pub c: u8,
    }

    pub extern "C" fn big(x: Big, n: u64) -> u64 {
        x.a + 2 * x.b + 3 * x.c + n
    }

    pub extern "C" fn make_big(n: u64) -> Big {
        Big { a: n, b: n + 1, c: n + 2 }
    }

    pub extern "C" fn mixed(x: Mixed, y: Mixed) -> Mixed {
        Mixed { a: x.a + y.a, b: x.b * y.b }
    }

    pub extern "C" fn small(x: Small, flag: bool, y: i8) -> Small {
        Small { a: x.c, b: x.b + flag as u8, c: x.a.wrapping_add(y as u8) }
    }

    pub extern "C" fn wide(x: u128, y: i16) -> i128 {
        x as i128 - y as i128
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Floats {
        pub a: f32,
        pub b: [f32; 2],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Nested {
        pub tag: u8,
        pub inner: (u16, f64),
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Two {
        pub a: u64,
        pub b: u64,
    }

    pub extern "C" fn floats(x: Floats, scale: f32) -> Floats {
        Floats { a: x.b[1] * scale, b: [x.a, x.b[0] + scale] }
    }

    pub extern "C" fn nested(x: Nested) -> Nested {
        Nested { tag: x.tag + 1, inner: (x.inner.0 * 2, x.inner.1 / 2.0) }
    }

    /// `two` no longer fits the integer registers and goes to the stack whole; `last` still
    /// gets the one that is left.
    #[allow(clippy::too_many_arguments)]
    pub extern "C" fn spill(a: u64, b: u64, c: u64, d: u64, e: u64, two: Two, last: u64) -> Two {
        Two { a: a + b + c + d + e + two.a, b: two.b * last }
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Quad {
        pub at: [f64; 4],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Five {
        pub at: [f32; 5],
    }

    /// Two of four floats fill the float registers a target has eight of, so the third goes
    /// to the stack whole, and the float after it follows it there.
    pub extern "C" fn quads(a: Quad, b: Quad, c: Quad, last: f64) -> Quad {
        Quad { at: [a.at[0] + b.at[1], b.at[2] - c.at[3], c.at[0] * last, a.at[3] + c.at[1]] }
    }

    /// Five floats are one too many to come back in registers.
    pub extern "C" fn five(x: Five, by: f32) -> Five {
        Five { at: [x.at[4] * by, x.at[3], x.at[2] + by, x.at[1], x.at[0]] }
    }

    /// A 128-bit integer starts at an even register, and at an aligned slot once on the stack.
    #[allow(clippy::too_many_arguments)]
    pub extern "C" fn wides(
        a: u64,
        b: u128,
        c: u64,
        d: i128,
        e: u64,
        f: u128,
        g: u8,
        h: i128,
    ) -> u128 {
        (a as u128 + b + c as u128).wrapping_add(d as u128) + e as u128 + f + g as u128 + h as u128
    }

    #[repr(C, align(16))]
    #[derive(Clone, Copy)]
    pub struct Aligned {
        pub a: u64,
        pub b: u32,
    }

    /// An aggregate aligned to sixteen bytes that the registers left do not hold is on the
    /// stack at a multiple of sixteen, and what comes after it follows it there.
    #[allow(clippy::too_many_arguments)]
    pub extern "C" fn aligned(
        a: u64,
        x: Aligned,
        b: u64,
        c: u64,
        d: u64,
        y: Aligned,
        e: u8,
        z: Aligned,
        f: u128,
        g: u16,
    ) -> u128 {
        let near = a + x.a * 3 + x.b as u64 + b * 5 + c * 7 + d * 11;
        let far = y.a * 13 + y.b as u64 * 17 + e as u64 * 19 + z.a * 23 + z.b as u64 * 29;
        (near + far) as u128 + f * 31 + g as u128 * 37
    }

    /// A 128-bit integer that the registers left do not hold takes them out of use.
    #[allow(clippy::too_many_arguments)]
    pub extern "C" fn wide_last(a: [u64; 7], b: i128, c: u64, d: u32, e: i128, f: u8) -> i128 {
        let near = a.iter().enumerate().map(|(at, it)| it * (at as u64 + 1)).sum::<u64>();
        near as i128 + b * 3 + c as i128 * 5 + d as i128 * 7 + e * 11 + f as i128 * 13
    }

    pub fn through_pointer(
        f: extern "C" fn(Floats, f32) -> Floats,
        g: extern "C" fn(u64) -> Big,
    ) -> f32 {
        let x = f(Floats { a: 1.0, b: [2.0, 3.0] }, 0.5);
        x.a + x.b[0] + x.b[1] + g(3).c as f32
    }

    pub fn call_c(text: &[u8]) -> usize {
        unsafe extern "C" {
            fn strlen(s: *const u8) -> usize;
            fn div(a: i32, b: i32) -> Div;
        }
        #[repr(C)]
        struct Div {
            quot: i32,
            rem: i32,
        }
        let d = unsafe { div(47, 5) };
        let len = unsafe { strlen(text.as_ptr()) };
        len + (d.quot * 10 + d.rem) as usize
    }
}

pub mod shapes {
    use std::sync::Arc;

    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum Kind {
        Plain,
        Word,
        Line,
    }

    #[derive(Clone)]
    pub struct ByteMap {
        pub map: [Kind; 256],
    }

    pub fn byte_map() -> ByteMap {
        let mut map = [Kind::Plain; 256];
        map[b'a' as usize] = Kind::Word;
        map[b'\n' as usize] = Kind::Line;
        ByteMap { map }.clone()
    }

    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct State(pub Arc<[u8]>);

    #[derive(Clone, Debug)]
    pub struct Names(pub Arc<str>, pub Vec<Option<Arc<str>>>);

    pub fn state(bytes: &[u8]) -> State {
        State(Arc::from(bytes))
    }

    pub fn names(text: &str) -> Names {
        let name: Arc<str> = Arc::from(text);
        Names(Arc::clone(&name), vec![None, Some(name)])
    }

    /// Passes its own lifetime on to the generic iterators it calls.
    pub struct SetBits<'a> {
        pub it: core::iter::Enumerate<core::slice::Iter<'a, bool>>,
    }

    impl<'a> Iterator for SetBits<'a> {
        type Item = usize;

        fn next(&mut self) -> Option<usize> {
            self.it.by_ref().find_map(|(i, &set)| set.then_some(i))
        }
    }

    pub fn wide_pointers(a: &[u8], b: &[u8]) -> (bool, bool, bool, bool) {
        let (short, long): (*const [u8], *const [u8]) = (&a[..1], a);
        let other: *const [u8] = b;
        (short == long, short < long, long == a as *const [u8], (long < other) == (other > long))
    }

    pub fn same(a: &State, b: &State) -> (bool, bool, bool) {
        (a == b, a < b, Arc::ptr_eq(&a.0, &b.0))
    }
}

/// A unit struct is a value like any other: naming it makes one, which has to be dropped.
pub mod unit_drops {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static DROPS: AtomicUsize = AtomicUsize::new(0);

    pub struct Unit;
    impl Drop for Unit {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl Unit {
        fn this() -> Self {
            Self
        }
    }

    pub fn count() -> [usize; 4] {
        let mut seen = [0; 4];
        {
            Unit;
        }
        seen[0] = DROPS.load(Ordering::Relaxed);
        {
            let _ = Unit;
        }
        seen[1] = DROPS.load(Ordering::Relaxed);
        {
            let _unit = Unit;
        }
        seen[2] = DROPS.load(Ordering::Relaxed);
        Unit::this();
        seen[3] = DROPS.load(Ordering::Relaxed);
        seen
    }
}

/// Operands are read when they are evaluated, not when the operation runs.
pub fn operand_order() -> (i32, i32) {
    let x = &mut 1;
    let sum = *x + {
        *x = 2;
        1
    };
    let mut boxed: Box<i32> = Box::new(3);
    #[allow(clippy::self_assignment)]
    {
        boxed = boxed;
    }
    (sum, *boxed)
}

/// A temporary in a format argument lives until the end of the statement that prints it.
pub fn format_temporaries(x: i32) -> String {
    fn boxed(x: i32) -> Option<Box<i32>> {
        Some(Box::new(x))
    }
    format!("{} {:?}", boxed(x).unwrap(), vec![x + 1])
}

pub fn exclusive_ranges(x: u32) -> [bool; 3] {
    [matches!(x, 6..7), matches!(x, 6..=7), matches!(x, ..7)]
}

pub mod const_pointers {
    static mut COUNTER: u32 = 42;
    struct Ptr(*mut u32);
    unsafe impl Sync for Ptr {}
    static TO_STATIC: Ptr = Ptr(&raw mut COUNTER);
    static mut SLICE: &mut [isize] = &mut [1];
    type Big = [u64; 4];
    const BIG: &Big = &[13, 14, 10, 13];
    const PAIR: &(isize, &Big) = &(15, BIG);
    const NUMS: [u32; 2] = [7, 9];
    #[repr(align(4))]
    struct Aligned;
    const ZST: *const () = (&Aligned as *const Aligned as *const ()).wrapping_byte_add(2);

    fn element(i: usize) -> &'static u32 {
        &NUMS[i]
    }

    fn field() -> &'static u32 {
        &(42, 1).0
    }

    /// A static that points to another static, to memory it may write, and constants that
    /// share what they point to.
    #[allow(static_mut_refs)]
    pub fn run() -> (u32, isize, bool, u32, u32, usize, usize) {
        unsafe {
            *TO_STATIC.0 += 1;
            SLICE[0] += 1;
            let unit: &'static () = &();
            (
                COUNTER,
                SLICE[0],
                std::ptr::eq(BIG, PAIR.1),
                *element(1),
                *field(),
                ZST.addr(),
                unit as *const () as usize,
            )
        }
    }
}

pub mod const_unsized_struct {
    #[repr(transparent)]
    pub struct Bytes {
        all: [u8],
    }
    #[repr(C)]
    pub struct Headed {
        head: u16,
        rest: [u16],
    }
    pub enum Held<'a> {
        Owned(Vec<u8>),
        Borrowed(&'a Bytes),
    }
    const fn bytes(all: &[u8]) -> &Bytes {
        unsafe { &*(all as *const [u8] as *const Bytes) }
    }
    const fn headed(all: &[u16]) -> &Headed {
        unsafe { &*(std::ptr::slice_from_raw_parts(all.as_ptr(), all.len() - 1) as *const Headed) }
    }
    const HELD: Held<'static> = Held::Borrowed(bytes(b"abcd"));
    const HEADED: &Headed = headed(&[7, 8, 9]);
    static IN_STATIC: (u8, &Bytes) = (3, bytes(b"xyz"));

    /// A constant that points to a struct whose last field is a slice.
    pub fn run() -> (Vec<u8>, u16, Vec<u16>, u8) {
        let held = match &HELD {
            Held::Owned(it) => it.clone(),
            Held::Borrowed(it) => it.all.to_vec(),
        };
        (held, HEADED.head, HEADED.rest.to_vec(), IN_STATIC.1.all[IN_STATIC.0 as usize - 1])
    }
}

pub mod promoted {
    use std::hash::{Hash, Hasher};

    trait Any {}
    impl<T> Any for T {}

    fn same(a: *const dyn Any, b: *const dyn Any) -> bool {
        a == b
    }

    fn sum() -> &'static (u8, u16) {
        &(1, 2 + 3)
    }

    #[derive(Hash)]
    enum Single {
        Only(u8),
    }

    struct Bytes(Vec<u8>);
    impl Hasher for Bytes {
        fn finish(&self) -> u64 {
            0
        }
        fn write(&mut self, bytes: &[u8]) {
            self.0.extend(bytes);
        }
    }

    /// Equal promoted constants are one object, arithmetic on literals is promoted, two impls
    /// of one trait for `String` are different functions, and an enum with one variant
    /// hashes no discriminant.
    pub fn run() -> (bool, (u8, u16), bool, bool, Vec<u8>) {
        let text = "foo".to_string();
        let mut bytes = Bytes(Vec::new());
        Single::Only(17).hash(&mut bytes);
        (same(&0u8, &0u8), *sum(), text == "foo".to_string(), text == "foo", bytes.0)
    }
}

pub mod evaluation_order {
    fn log(order: &mut Vec<u8>, tag: u8) -> u32 {
        order.push(tag);
        tag as u32
    }

    struct Pair {
        a: u32,
        b: u32,
    }

    /// The right-hand side of `+=` on integers runs first, and the base of a struct literal
    /// runs last.
    pub fn run() -> (Vec<u8>, u32, u32) {
        let mut order = Vec::new();
        let mut xs = [0u32; 4];
        xs[log(&mut order, 1) as usize] += log(&mut order, 2);
        let pair = Pair { a: log(&mut order, 3), ..Pair { a: 0, b: log(&mut order, 4) } };
        (order, xs[1], pair.a + pair.b)
    }
}

pub mod pattern_alternatives {
    /// A guard that fails is run again for the next alternative of an or-pattern.
    pub fn guard(pair: (u32, u32)) -> (u32, u32) {
        let mut guards = 0;
        let found = match pair {
            (a, _) | (_, a)
                if {
                    guards += 1;
                    a > 10
                } =>
            {
                a
            }
            _ => 0,
        };
        (found, guards)
    }

    pub fn slices(xs: [u8; 5]) -> (u8, [u8; 2], u8, u8) {
        let [a, middle @ .., d, e] = xs;
        (a, middle, d, e)
    }
}

pub mod unsized_tail {
    use std::cell::Cell;
    use std::mem::{align_of_val, size_of_val};

    pub trait Get {
        fn get(&self) -> u64;
    }
    impl Get for u64 {
        fn get(&self) -> u64 {
            *self
        }
    }
    impl Get for u8 {
        fn get(&self) -> u64 {
            *self as u64
        }
    }
    struct Noisy<'a>(u8, &'a Cell<u32>);
    impl Get for Noisy<'_> {
        fn get(&self) -> u64 {
            self.0 as u64
        }
    }
    impl Drop for Noisy<'_> {
        fn drop(&mut self) {
            self.1.set(self.1.get() * 10 + self.0 as u32);
        }
    }

    struct Tail<T: ?Sized> {
        head: u16,
        tail: T,
    }

    #[repr(C, packed(2))]
    struct Packed<T: ?Sized>(u8, std::mem::ManuallyDrop<T>);

    /// The offset of a `dyn` field depends on the alignment in its vtable.
    pub fn field(wide: u64, narrow: u8) -> (u64, u64, u64, usize, usize) {
        let a: &Tail<dyn Get> = &Tail { head: 1, tail: wide };
        let b: &Tail<dyn Get> = &Tail { head: 2, tail: narrow };
        let nested: &Tail<Tail<dyn Get>> = &Tail { head: 3, tail: Tail { head: 4, tail: wide } };
        (
            a.tail.get() + a.head as u64,
            b.tail.get() + b.head as u64,
            nested.tail.tail.get() + nested.tail.head as u64,
            size_of_val(a) * 100 + align_of_val(a),
            size_of_val(b) * 100 + align_of_val(b),
        )
    }

    pub fn packed(value: u64) -> (usize, usize, isize) {
        let p: &Packed<dyn Get> = &Packed(0, std::mem::ManuallyDrop::new(value));
        let offset = unsafe { (&raw const p.1).cast::<u8>().offset_from(&raw const p.0) };
        (size_of_val(p), align_of_val(p), offset)
    }

    pub fn drop_tail(value: u8) -> u32 {
        let log = Cell::new(0);
        let boxed: Box<Tail<dyn Get + '_>> = Box::new(Tail { head: 0, tail: Noisy(value, &log) });
        let seen = boxed.tail.get() as u32;
        drop(boxed);
        log.get() * 100 + seen
    }

    pub fn run() -> ((u64, u64, u64, usize, usize), (usize, usize, isize), u32) {
        (field(11, 5), packed(7), drop_tail(3))
    }
}

pub mod upcast_generic {
    pub trait Named<T: Default + ToString> {
        fn name(&self) -> String {
            T::default().to_string()
        }
    }
    pub trait Both<T: Default + ToString>: Named<i32> + Named<T> {}

    struct S;
    impl Named<i32> for S {}
    impl Named<bool> for S {}
    impl Both<bool> for S {}

    /// Two supertraits that differ only in their arguments have separate vtables.
    pub fn run() -> (String, String) {
        let both: &dyn Both<bool> = &S;
        let int = both as &dyn Named<i32>;
        let boolean = both as &dyn Named<bool>;
        (int.name(), boolean.name())
    }
}

pub mod packed_drop {
    use std::cell::Cell;

    thread_local!(static ODD: Cell<u32> = const { Cell::new(0) });

    struct Aligned(u16);
    impl Drop for Aligned {
        fn drop(&mut self) {
            let odd = std::hint::black_box(self as *mut Aligned as usize) % 2;
            ODD.with(|it| it.set(it.get() * 10 + odd as u32 + self.0 as u32));
        }
    }
    struct Other;
    impl Drop for Other {
        fn drop(&mut self) {}
    }
    struct Pair {
        a: Aligned,
        b: Other,
    }
    #[repr(packed)]
    struct Misalign(u8, Pair);
    #[repr(packed)]
    struct Whole(u8, Aligned);

    /// A destructor never sees a misaligned `self`, with or without a partial move.
    pub fn run() -> u32 {
        let m: ([u16; 0], Misalign) = ([], Misalign(0, Pair { a: Aligned(2), b: Other }));
        let _b = m.1.1.b;
        let w: ([u16; 0], Whole) = ([], Whole(0, Aligned(4)));
        drop(w);
        drop(m.1.1.a);
        ODD.with(|it| it.get())
    }
}

pub mod array_move {
    use std::cell::RefCell;

    struct Loud<'a>(u32, &'a RefCell<Vec<u32>>);
    impl Drop for Loud<'_> {
        fn drop(&mut self) {
            self.1.borrow_mut().push(self.0);
        }
    }

    fn take<'a>([a, ref mut b, ref rest @ .., ref c, d]: [Loud<'a>; 6]) -> u32 {
        a.0 + b.0 + rest.len() as u32 + c.0 + d.0
    }

    /// Moving the first and last element out of an array parameter leaves the middle to drop.
    pub fn run() -> (u32, Vec<u32>) {
        let log = RefCell::new(Vec::new());
        let mk = |v| Loud(v, &log);
        let sum = take([mk(1), mk(2), mk(3), mk(4), mk(5), mk(6)]);
        (sum, log.into_inner())
    }
}

/// Overflow checks on signed integers when a constant is evaluated.
pub mod const_overflow {
    const SUB: Option<i8> = (-127i8).checked_sub(2);
    const ADD: Option<i8> = 127i8.checked_add(2);
    const MUL: Option<i8> = (-127i8).checked_mul(-99);
    const FITS: Option<i8> = (-5i8).checked_mul(3);
    const WIDE: (i128, bool) = i128::MIN.overflowing_sub(1);
    const UNSIGNED: (u8, bool) = 3u8.overflowing_sub(5);
    const DIV: Option<i8> = (i8::MIN + 1).checked_div(-1);
    const REM: Option<i8> = (-7i8).checked_rem(2);
    const SHR: i8 = (-64i8) >> 3;
    const SATURATED: (i8, i8, u128, u8) = (
        100i8.saturating_add(100),
        (-100i8).saturating_sub(100),
        u128::MAX.saturating_add(1),
        3u8.saturating_sub(5),
    );

    pub fn run() -> (Option<i8>, Option<i8>, Option<i8>, Option<i8>, bool, (u8, bool)) {
        (SUB, ADD, MUL, FITS, WIDE.1, UNSIGNED)
    }

    pub fn more() -> (Option<i8>, Option<i8>, i8, (i8, i8, u128, u8)) {
        (DIV, REM, SHR, SATURATED)
    }
}

/// A panic unwinds through frames that own values, and each is dropped once.
pub mod unwind_cleanup {
    use std::cell::RefCell;
    use std::panic::{self, AssertUnwindSafe};

    struct Noisy<'a>(&'a RefCell<Vec<u32>>, u32);
    impl Drop for Noisy<'_> {
        fn drop(&mut self) {
            self.0.borrow_mut().push(self.1);
        }
    }

    #[inline(never)]
    fn inner(log: &RefCell<Vec<u32>>, fail: bool) -> u32 {
        let _a = Noisy(log, 1);
        let moved = Noisy(log, 2);
        let _b = Noisy(log, 3);
        drop(moved);
        let pair = (Noisy(log, 4), Noisy(log, 5));
        let _kept = pair.0;
        if fail {
            panic!("unwinding with {} values live", log.borrow().len());
        }
        6
    }

    #[inline(never)]
    fn outer(log: &RefCell<Vec<u32>>, fail: bool) -> u32 {
        let _o = Noisy(log, 10);
        let boxed = Box::new(Noisy(log, 11));
        let n = inner(log, fail);
        drop(boxed);
        n
    }

    struct Failing<'a>(Noisy<'a>, Noisy<'a>);
    impl Drop for Failing<'_> {
        fn drop(&mut self) {
            panic!("a destructor that fails after {} drops", (self.0).0.borrow().len());
        }
    }

    #[inline(never)]
    fn scopes(log: &RefCell<Vec<u32>>) {
        let _first = Noisy(log, 20);
        let _second = Noisy(log, 21);
        {
            let _inner = Noisy(log, 22);
            let _failing = Failing(Noisy(log, 23), Noisy(log, 24));
            let _last = Noisy(log, 25);
        }
        let _unreached = Noisy(log, 26);
    }

    /// A destructor that panics still has its fields dropped, and the scopes around it unwind
    /// innermost first.
    pub fn failing_drop() -> (Vec<u32>, bool) {
        let log = RefCell::new(Vec::new());
        let caught = panic::catch_unwind(AssertUnwindSafe(|| scopes(&log))).is_err();
        (log.into_inner(), caught)
    }

    pub fn run() -> (Vec<u32>, Vec<u32>, bool) {
        let (ok, failed) = (RefCell::new(Vec::new()), RefCell::new(Vec::new()));
        outer(&ok, false);
        let caught = panic::catch_unwind(AssertUnwindSafe(|| outer(&failed, true))).is_err();
        (ok.into_inner(), failed.into_inner(), caught)
    }
}

/// Closures that each capture one field of an enum with a single variant.
pub mod capture_enum_field {
    #[derive(Debug, PartialEq)]
    pub enum Color {
        Rgb(u8, u8, u8),
    }

    pub fn run() -> Color {
        let mut color = Color::Rgb(9, 9, 9);
        let mut red = |v| {
            let Color::Rgb(ref mut r, _, _) = color;
            *r = v;
        };
        let mut blue = |v| {
            let Color::Rgb(_, _, ref mut b) = color;
            *b = v;
        };
        red(1);
        blue(3);
        color
    }
}

/// A method call that borrows a constant uses the constant where it is, not a copy on the stack.
pub mod autoref_promoted {
    const BIG: [u8; 32 * 1024 * 1024] = [42; 32 * 1024 * 1024];
    const SMALL: [u16; 3] = [1, 2, 3];

    pub fn run() -> (usize, Option<&'static u16>, bool) {
        (BIG.len(), SMALL.last(), std::ptr::eq(SMALL.as_ptr(), SMALL.as_ptr()))
    }
}

/// A borrowed array of `Self::Variant`s outlives the function, as `derive(ValueEnum)` relies on.
pub mod promoted_self_variant {
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub enum Kind {
        Webview,
        Native,
    }

    impl Kind {
        fn all<'a>() -> &'a [Self] {
            &[Self::Webview, Self::Native]
        }
    }

    #[inline(never)]
    fn clobber() -> [u64; 8] {
        std::hint::black_box([u64::MAX; 8])
    }

    pub fn run() -> (Vec<Kind>, u64) {
        let all = Kind::all();
        let noise = clobber();
        (all.to_vec(), noise[0])
    }
}

/// Two statics that point at each other, and a static in a function body whose value is made
/// by a closure on first use.
pub mod static_cycle {
    use std::sync::LazyLock;

    pub struct Site {
        meta: &'static Meta,
        id: u32,
    }
    pub struct Meta {
        name: &'static str,
        site: &'static Site,
    }

    fn pair() -> (&'static Site, &'static Meta) {
        static SITE: Site = Site { meta: &META, id: 7 };
        static META: Meta = Meta { name: "event", site: &SITE };
        (&SITE, &META)
    }

    fn settings() -> &'static Vec<String> {
        static SETTINGS: LazyLock<Vec<String>> =
            LazyLock::new(|| vec!["a".to_string(), "b".to_string()]);
        &SETTINGS
    }

    pub fn run() -> (u32, &'static str, bool, bool, Vec<String>, bool) {
        let (site, meta) = pair();
        (
            meta.site.id,
            site.meta.name,
            std::ptr::eq(site.meta, meta),
            std::ptr::eq(meta.site, site),
            settings().clone(),
            std::ptr::eq(settings(), settings()),
        )
    }
}

/// An `async` block keeps what lives across an `.await`, borrowed locals included, and not
/// the large temporaries in between.
pub mod async_frames {
    use std::future::Future;
    use std::pin::{Pin, pin};
    use std::task::{Context, Poll, Waker};

    struct Twice(bool);
    impl Future for Twice {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            if std::mem::replace(&mut self.0, true) { Poll::Ready(()) } else { Poll::Pending }
        }
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            // Whatever a suspended future left on `poll`'s stack is gone by the next call.
            std::hint::black_box([u64::MAX; 64]);
            if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
                return value;
            }
        }
    }

    fn checksum(bytes: &[u8]) -> u64 {
        bytes.iter().map(|&it| it as u64).sum()
    }

    async fn leaf(seed: u8) -> u64 {
        let scratch = [seed; 4096];
        let before = checksum(&scratch);
        Twice(false).await;
        let after = [seed.wrapping_add(1); 4096];
        before + checksum(&after)
    }

    async fn borrows(seed: u8) -> u64 {
        let kept = [seed; 64];
        let view = &kept[1..];
        let mut total = String::from("t");
        let grows = &mut total;
        Twice(false).await;
        grows.push('u');
        let moved = total;
        Twice(false).await;
        checksum(view) + moved.len() as u64
    }

    async fn nested(depth: u8) -> u64 {
        let mut total = 0;
        for round in 0..3 {
            let step = if round == depth { borrows(round).await } else { leaf(round).await };
            total += step;
        }
        let pinned = Box::pin(async move { leaf(depth).await + total });
        pinned.await
    }

    /// The futures of the arms never exist at once and share the memory of the future
    /// awaiting them; the ones awaited in turn while `kept` is borrowed do too.
    async fn arms(which: u8) -> u64 {
        let kept = [which; 32];
        let first = match which {
            0 => leaf(1).await,
            1 => leaf(2).await + leaf(3).await,
            2 => borrows(4).await,
            _ => leaf(5).await,
        };
        let view = &kept[..4];
        let second = leaf(first as u8).await;
        let third = leaf(second as u8).await;
        first + second + third + checksum(view)
    }

    pub fn shared() -> (Vec<u64>, bool) {
        let size = std::mem::size_of_val(&arms(0));
        ((0..4).map(|which| block_on(arms(which))).collect(), size < 2 * 4096)
    }

    pub fn run() -> (u64, u64, u64) {
        (block_on(leaf(3)), block_on(borrows(5)), block_on(nested(1)))
    }
}

/// `Box<dyn FnMut>` forwards its arguments to the closure as the tuple it was called with.
pub mod boxed_fn_tuple {
    fn join(key: String, val: String, n: u8) -> String {
        format!("{key}={val}/{n}")
    }

    pub fn run() -> (String, u32, String) {
        let mut seen = 0;
        let mut two: Box<dyn FnMut(String, String, u8) -> String> = Box::new(|a, b, n| {
            seen += 1;
            join(a, b, n)
        });
        let joined = two("k".to_string(), "v".to_string(), 7);
        drop(two);
        let none: Box<dyn Fn() -> String> = Box::new(|| "none".to_string());
        (joined, seen, none())
    }
}

/// `line!()`, `column!()` and `file!()` are where the outermost macro is called.
pub mod builtin_location {
    macro_rules! here {
        () => {
            (line!(), column!(), file!())
        };
    }

    macro_rules! column_of {
        ($e:expr) => {
            ($e, column!())
        };
    }

    /// A macro call passed to another macro is where it was written.
    pub fn passed() -> (u32, u32) {
        column_of!(column_of!(7).1)
    }

    pub fn run() -> ((u32, u32, &'static str), (u32, u32, &'static str), u32) {
        let direct = (line!(), column!(), file!());
        let nested = here!();
        assert_eq!(nested.0, line!() - 1, "in {}", file!());
        (direct, nested, line!() + 1)
    }
}

/// `stringify!` spaces tokens the way rustc does.
pub mod stringify_spacing {
    macro_rules! ty {
        (<$t:ty>) => {
            stringify!($t)
        };
    }
    macro_rules! expr {
        ($e:expr) => {
            stringify!($e)
        };
    }
    macro_rules! list {
        ($($e:ident),*) => {
            stringify!($($e),*)
        };
    }

    pub fn run() -> Vec<&'static str> {
        vec![
            stringify!(std::option),
            stringify!(a, b, c),
            stringify!(
                f(x).y.0
                    + #[attr]
                    g(1)
            ),
            stringify!({
                foo += 1;
                eprintln!("before");
                7331
            }),
            stringify!(let (a, b) = &'a mut x;),
            stringify!(
                fn f<T: Tr>(x: Vec<T>) -> [u8; 3] {}
            ),
            ty!(<Vec<i32>>),
            list!(a, b, c),
            stringify!(("Yeah",)),
            stringify!(1u8),
            stringify!(r#struct.r#fn),
            expr!(1 + 2),
            expr!(Point { x: 42 }),
            expr!(&a),
        ]
    }
}

/// Raw identifiers mean what the plain ones do, except to a macro matching on them.
pub mod raw_identifiers {
    #[r#repr(r#C, r#packed)]
    pub struct Packed {
        pub a: bool,
        pub b: u64,
    }

    #[derive(Debug, Clone)]
    pub struct Keyword {
        pub r#struct: u32,
    }

    macro_rules! which {
        (a) => {
            6
        };
        (r#a) => {
            7
        };
    }

    pub fn run() -> (usize, String, u32, u32) {
        let keyword = Keyword { r#struct: 3 };
        (std::mem::size_of::<Packed>(), format!("{:?}", keyword.clone()), which!(a), which!(r#a))
    }
}

/// A derive calls the trait's own methods, whatever else is in scope under those names.
pub mod derive_ufcs {
    use std::fmt;

    #[derive(Debug, Clone)]
    pub struct Named {
        pub bar: u8,
        pub baz: (),
    }

    #[derive(Debug, Clone)]
    pub struct Tuple(pub u8);

    #[derive(Debug, Clone)]
    pub struct Unit;

    pub trait Shadow {
        fn debug_struct(&self, _: &str) -> u8 {
            panic!("got into debug_struct")
        }
        fn debug_tuple(&self, _: &str) -> u8 {
            panic!("got into debug_tuple")
        }
        fn write_str(&self, _: &str) -> u8 {
            panic!("got into write_str")
        }
        fn clone(&self) -> u8 {
            panic!("got into clone")
        }
    }
    impl Shadow for &mut fmt::Formatter<'_> {}
    impl Shadow for &u8 {}
    impl Shadow for &() {}

    pub fn run() -> String {
        let named = Named { bar: 1, baz: () };
        format!("{:?} {:?} {:?}", named.clone(), Tuple(2).clone(), Unit.clone())
    }
}

/// `Clone` of the types that have no impl written anywhere.
pub mod builtin_clone {
    fn twice(n: u8) -> u8 {
        n * 2
    }

    pub fn run() -> ((String, u8), [String; 2], u8, ((), (Vec<u8>,))) {
        let pair = ("a".to_string(), 1u8);
        let array = ["b".to_string(), "c".to_string()];
        let pointer: fn(u8) -> u8 = twice;
        let nested = ((), (vec![1u8, 2],));
        (Clone::clone(&pair), array.clone(), Clone::clone(&pointer)(4), nested.clone())
    }
}

/// A guard that fails gives back what the pattern took, and `&&` drops as it goes.
pub mod guard_moves {
    use std::cell::RefCell;

    struct Noisy<'a>(&'a RefCell<Vec<u32>>, u32);

    impl Drop for Noisy<'_> {
        fn drop(&mut self) {
            self.0.borrow_mut().push(self.1);
        }
    }

    impl Noisy<'_> {
        fn yes(&self) -> bool {
            true
        }
    }

    fn failed(log: &RefCell<Vec<u32>>) -> u32 {
        match (Noisy(log, 2), Noisy(log, 3)) {
            (_x, _y)
                if let _z = Noisy(log, 1)
                    && false =>
            {
                0
            }
            (x, _) if x.1 == 7 => 1,
            (_, y) => y.1,
        }
    }

    fn chain(log: &RefCell<Vec<u32>>) -> u32 {
        if Noisy(log, 1).yes()
            && let Some(_v) = Some(Noisy(log, 5))
            && Noisy(log, 2).yes()
            && let Some(_) = Some(Noisy(log, 4)).as_ref()
        {
            log.borrow_mut().push(3);
        }
        (Noisy(log, 6).yes() || Noisy(log, 99).yes()) as u32
    }

    pub fn run() -> (u32, u32, Vec<u32>) {
        let log = RefCell::new(Vec::new());
        let (a, b) = (failed(&log), chain(&log));
        (a, b, log.into_inner())
    }
}

/// Dividing by zero and `MIN / -1` panic and unwind.
pub mod division_panics {
    use std::hint::black_box;
    use std::panic;

    pub fn run() -> (Vec<bool>, i128, u8, i64) {
        let caught = vec![
            panic::catch_unwind(|| black_box(1i32) / black_box(0)).is_err(),
            panic::catch_unwind(|| black_box(1u8) % black_box(0)).is_err(),
            panic::catch_unwind(|| black_box(i8::MIN) / black_box(-1)).is_err(),
            panic::catch_unwind(|| black_box(i64::MIN) % black_box(-1)).is_err(),
            panic::catch_unwind(|| black_box(i128::MIN) / black_box(-1)).is_err(),
            panic::catch_unwind(|| black_box(i16::MIN) / black_box(1)).is_err(),
            panic::catch_unwind(|| -black_box(i32::MIN)).is_err(),
            panic::catch_unwind(|| -black_box(i128::MIN)).is_err(),
            panic::catch_unwind(|| -black_box(-128i8 + 1)).is_err(),
            panic::catch_unwind(|| black_box(-128i8) == i8::MIN).is_err(),
        ];
        (
            caught,
            black_box(i128::MIN + 1) / black_box(-1),
            black_box(7u8) % black_box(4),
            black_box(-7i64) / 2,
        )
    }
}

/// What a panic unwinds through is dropped: the captures of a closure called by value, and the
/// part of a tuple or an array that was already cloned.
pub mod unwind_owned {
    use std::panic::{self, AssertUnwindSafe};
    use std::rc::Rc;

    struct Flag<'a>(&'a mut bool);

    impl Drop for Flag<'_> {
        fn drop(&mut self) {
            *self.0 = true;
        }
    }

    fn call<T: FnOnce()>(t: T) {
        t()
    }

    struct Seventh(Rc<()>);

    impl Clone for Seventh {
        fn clone(&self) -> Self {
            if Rc::strong_count(&self.0) == 7 {
                panic!("oops");
            }
            Seventh(self.0.clone())
        }
    }

    pub fn run() -> (bool, bool, usize, usize) {
        let mut dropped = false;
        let caught = {
            let flag = Flag(&mut dropped);
            panic::catch_unwind(AssertUnwindSafe(move || {
                let inner = move || {
                    let _flag = &flag;
                    panic!()
                };
                call(inner);
            }))
            .is_err()
        };

        let counter = Rc::new(());
        let c = counter.clone();
        let _ = panic::catch_unwind(AssertUnwindSafe(move || {
            let _ =
                (Seventh(c.clone()), Seventh(c.clone()), Seventh(c.clone()), Seventh(c)).clone();
        }));
        let tuple = Rc::strong_count(&counter);
        let c = counter.clone();
        let _ = panic::catch_unwind(AssertUnwindSafe(move || {
            let _ =
                [Seventh(c.clone()), Seventh(c.clone()), Seventh(c.clone()), Seventh(c)].clone();
        }));
        (caught, dropped, tuple, Rc::strong_count(&counter))
    }
}

/// Derives on a `Copy` type copy it, and on a packed type they copy the fields out before
/// calling anything on them.
pub mod derive_copy_packed {
    use std::sync::atomic::{AtomicU32, Ordering};

    pub static CLONED: AtomicU32 = AtomicU32::new(0);
    pub static MISALIGNED: AtomicU32 = AtomicU32::new(0);

    #[derive(Copy)]
    pub struct Liar;

    impl Clone for Liar {
        fn clone(&self) -> Self {
            CLONED.fetch_add(1, Ordering::SeqCst);
            *self
        }
    }

    #[derive(Copy, Clone)]
    pub struct Innocent(pub Liar);

    #[derive(Clone)]
    pub struct Honest(pub Liar);

    #[derive(Copy, Clone, Debug, Hash, PartialOrd)]
    pub struct Aligned(pub usize);

    impl PartialEq for Aligned {
        fn eq(&self, other: &Self) -> bool {
            for it in [self, other] {
                if it as *const Aligned as usize % std::mem::align_of::<Aligned>() != 0 {
                    MISALIGNED.fetch_add(1, Ordering::SeqCst);
                }
            }
            self.0 == other.0
        }
    }

    #[repr(packed)]
    #[derive(Copy, Clone, PartialEq, PartialOrd, Debug, Hash)]
    pub struct Packed(pub Aligned, pub Aligned);

    #[derive(PartialEq)]
    #[repr(C)]
    pub struct Dealigned<T>(pub u8, pub T);

    pub fn run() -> (u32, u32, bool, bool, String, u32) {
        let _ = Innocent(Liar).clone();
        let copied = CLONED.load(Ordering::SeqCst);
        let _ = Honest(Liar).clone();
        let a = Dealigned(0, Packed(Aligned(1), Aligned(2)));
        let b = Dealigned(0, Packed(Aligned(1), Aligned(3)));
        (
            copied,
            CLONED.load(Ordering::SeqCst),
            a == a,
            a.1 < b.1,
            format!("{:?}", b.1),
            MISALIGNED.load(Ordering::SeqCst),
        )
    }
}

/// A constant whose pointer sits in a union, where its type does not show it.
pub mod union_pointer {
    use std::mem::MaybeUninit;

    type Byte = MaybeUninit<u8>;

    const unsafe fn memcpy(dst: *mut Byte, src: *const Byte, n: usize) {
        let mut i = 0;
        while i < n {
            unsafe { *dst.add(i) = *src.add(i) };
            i += 1;
        }
    }

    const COPIED: MaybeUninit<*const i32> = unsafe {
        let ptr = &42;
        let mut ptr2 = MaybeUninit::new(std::ptr::null::<i32>());
        memcpy(&mut ptr2 as *mut _ as *mut _, &ptr as *const _ as *const _, size_of::<&i32>());
        ptr2
    };
    const DIRECT: MaybeUninit<(u64, &'static u8)> = MaybeUninit::new((7, &9));

    pub fn run() -> (i32, u64, u8) {
        let direct = unsafe { DIRECT.assume_init() };
        (unsafe { COPIED.assume_init().read() }, direct.0, *direct.1)
    }
}

/// Items in the body of an `async fn`, which rustc names through the closure the body is.
pub mod async_items {
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    async fn outer(n: u32) -> u32 {
        static TABLE: [u32; 3] = [3, 5, 7];
        fn double(n: u32) -> u32 {
            n * 2
        }
        async fn inner(n: u32) -> u32 {
            fn triple(n: u32) -> u32 {
                n * 3
            }
            triple(n) + 1
        }
        let scale = |n: u32| {
            fn in_closure(n: u32) -> u32 {
                n + 100
            }
            in_closure(n)
        };
        double(n) + inner(n).await + TABLE[n as usize % 3] + scale(n)
    }

    pub fn run() -> u32 {
        let mut future = pin!(outer(4));
        let mut cx = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => value,
            Poll::Pending => 0,
        }
    }
}

pub mod async_basic {
    use std::cell::RefCell;
    use std::future::Future;
    use std::pin::{Pin, pin};
    use std::rc::Rc;
    use std::task::{Context, Poll, Waker};

    type Log = Rc<RefCell<Vec<&'static str>>>;

    struct Noisy(&'static str, Log);

    impl Drop for Noisy {
        fn drop(&mut self) {
            self.1.borrow_mut().push(self.0);
        }
    }

    /// Pending the given number of times, then ready.
    struct Countdown(u32);

    impl Future for Countdown {
        type Output = u32;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u32> {
            if self.0 == 0 {
                return Poll::Ready(7);
            }
            self.0 -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    fn block_on<F: Future>(future: F) -> (F::Output, u32) {
        let mut future = pin!(future);
        let mut cx = Context::from_waker(Waker::noop());
        let mut polls = 1;
        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
                return (value, polls);
            }
            polls += 1;
        }
    }

    async fn add(a: u32, b: u32) -> u32 {
        a + Countdown(b).await
    }

    async fn sum(values: &[u32], log: Log) -> u32 {
        let _whole = Noisy("sum", log.clone());
        let mut total = 0;
        for &value in values {
            let _each = Noisy("step", log.clone());
            total = add(total, value).await;
        }
        total
    }

    pub fn run() -> (u32, u32, (u32, u32), (u32, u32), Vec<&'static str>, Vec<&'static str>) {
        let (plain, _) = block_on(async { 5 });
        let captured = String::from("abc");
        let (moved, _) = block_on(async move { captured.len() as u32 + Countdown(2).await });
        let log = Log::default();
        let summed = block_on(sum(&[1, 2, 0], log.clone()));
        let boxed: Pin<Box<dyn Future<Output = u32>>> = Box::pin(add(1, 3));
        let dynamic = block_on(boxed);

        // Dropped before it ran, and dropped while it waits.
        let dropped = Log::default();
        let never = sum(&[1], dropped.clone());
        let guard = Noisy("captured", dropped.clone());
        let unresumed = async move {
            let _guard = guard;
        };
        drop(unresumed);
        {
            let mut waiting = pin!(never);
            let mut cx = Context::from_waker(Waker::noop());
            assert!(waiting.as_mut().poll(&mut cx).is_pending());
            dropped.borrow_mut().push("polled");
        }
        dropped.borrow_mut().push("end");
        let first = log.borrow().clone();
        let second = dropped.borrow().clone();
        (plain, moved, summed, dynamic, first, second)
    }
}

pub mod async_closures {
    use std::cell::RefCell;
    use std::future::Future;
    use std::pin::{Pin, pin};
    use std::rc::Rc;
    use std::task::{Context, Poll, Waker};

    type Log = Rc<RefCell<Vec<&'static str>>>;

    struct Noisy(&'static str, Log);

    impl Drop for Noisy {
        fn drop(&mut self) {
            self.1.borrow_mut().push(self.0);
        }
    }

    /// Pending the given number of times, then ready.
    struct Countdown(u32);

    impl Future for Countdown {
        type Output = u32;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u32> {
            if self.0 == 0 {
                return Poll::Ready(7);
            }
            self.0 -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
                return value;
            }
        }
    }

    async fn twice(f: impl AsyncFn(u32) -> u32) -> u32 {
        f(1).await + f(2).await
    }

    async fn counted(mut f: impl AsyncFnMut(u32) -> u32) -> u32 {
        f(1).await;
        f(2).await
    }

    async fn once(f: impl AsyncFnOnce(u32) -> usize) -> usize {
        f(3).await
    }

    async fn plainly<F: Fn(u32) -> R, R: Future<Output = u32>>(f: F) -> u32 {
        f(4).await + f(5).await
    }

    pub fn run() -> (u32, u32, u32, usize, usize, u32, usize, Vec<&'static str>) {
        // Captures nothing, and is called where it is written.
        let plain = async |a: u32, b: u32| a + b + Countdown(1).await;
        let direct = block_on(plain(1, 2));

        // Borrows what it captures, on every call.
        let base = 10;
        let shared = block_on(twice(async |x| x + base + Countdown(2).await));

        // Changes what it captures.
        let mut total = 0;
        let changed = block_on(counted(async |x| {
            total += x + Countdown(1).await;
            total
        }));

        // Gives away what it captures.
        let text = String::from("abcd");
        let moved = block_on(once(async move |x| {
            let text = text;
            text.len() + x as usize
        }));

        // Borrows, and is called as one that is called once.
        let kept = String::from("ab");
        let lent = block_on(once(async |x| kept.len() + x as usize + Countdown(1).await as usize));

        // Through the plain closure traits.
        let plain_traits = block_on(plainly(async |y: u32| y * 3 + Countdown(1).await));

        // What is dropped when: one never called, one called and not polled, one polled and
        // left waiting.
        let log = Log::default();
        let guard = Noisy("closure", log.clone());
        let holds = async move |name: Noisy| {
            let _guard = &guard;
            Countdown(1).await;
            drop(name);
        };
        {
            let unpolled = holds(Noisy("unpolled", log.clone()));
            log.borrow_mut().push("made");
            drop(unpolled);
            let mut waiting = pin!(holds(Noisy("waiting", log.clone())));
            let mut cx = Context::from_waker(Waker::noop());
            assert!(waiting.as_mut().poll(&mut cx).is_pending());
            log.borrow_mut().push("polled");
        }
        block_on(holds(Noisy("finished", log.clone())));
        log.borrow_mut().push("called");
        drop(holds);
        log.borrow_mut().push("end");
        let sizes = size_of_val(&plain) + size_of_val(&plain(1, 2));
        let dropped = log.borrow().clone();
        (direct, shared, changed, moved, lent, plain_traits, sizes, dropped)
    }

    async fn once_noisy<R>(f: impl AsyncFnOnce(Noisy) -> R) -> R {
        let log = Log::default();
        f(Noisy("arg", log)).await
    }
    async fn once_u<R>(f: impl AsyncFnOnce(u32) -> R) -> R {
        f(3).await
    }
    async fn nested(f: impl AsyncFn(u32) -> u32) -> u32 {
        twice(&f).await + once_u(&f).await + counted(&f).await + once_u(f).await
    }
    async fn named(x: u32) -> u32 {
        x + Countdown(1).await
    }
    fn boxed(
        f: impl AsyncFn(u32) -> u32 + 'static,
    ) -> Box<dyn Fn(u32) -> Pin<Box<dyn Future<Output = u32>>>> {
        let f = Rc::new(f);
        Box::new(move |x| {
            let f = f.clone();
            Box::pin(async move { f(x).await })
        })
    }
    pub fn more() -> Vec<String> {
        let mut out = Vec::new();
        // FnOnce by use, with a borrowed capture beside the consumed one.
        let text = String::from("abcd");
        let other = String::from("xy");
        let log = Log::default();
        let noisy = Noisy("moved", log.clone());
        let f = async move |x: u32| {
            let n = other.len();
            Countdown(1).await;
            let text = text;
            drop(noisy);
            text.len() + n + x as usize
        };
        out.push(block_on(once_u(f)).to_string());
        // Mutable capture by-move.
        let mut total = 5;
        let g = async |x: u32| {
            total += x;
            Countdown(1).await;
            total
        };
        out.push(block_on(once_u(g)).to_string());
        out.push(total.to_string());
        let base = String::from("base");
        out.push(
            block_on(nested(async |x| x + base.len() as u32 + Countdown(2).await)).to_string(),
        );
        out.push(block_on(nested(async move |x| x + base.len() as u32)).to_string());
        out.push(block_on(nested(named)).to_string());
        out.push(block_on(twice(|x| async move { x * 2 })).to_string());
        out.push(block_on(twice(Box::new(async |x| x * 5))).to_string());
        let b = boxed(async |x| x + 100);
        out.push(block_on(b(1)).to_string());
        // Dropped unpolled after a by-move call.
        let keep = Noisy("kept", log.clone());
        let h = async move |n: Noisy| {
            let _k = &keep;
            Countdown(1).await;
            drop(n);
        };
        let fut = once_noisy(h);
        drop(fut);
        out.push(format!("{:?}", log.borrow()));
        // Sizes and copies.
        let small = async |a: u8, b: u64| a as u64 + b;
        let copy = small;
        out.push(format!("{} {}", size_of_val(&small), size_of_val(&small(1, 2))));
        out.push(block_on(copy(1, 2)).to_string());
        let s = String::from("q");
        let cl = async move |a: u8| s.len() + a as usize;
        let cl2 = cl.clone();
        out.push(format!("{} {} {}", size_of_val(&cl), size_of_val(&cl(1)), block_on(cl2(4))));
        out
    }
}

pub mod async_more {
    use std::cell::RefCell;
    use std::future::Future;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::pin::{Pin, pin};
    use std::rc::Rc;
    use std::task::{Context, Poll, Waker};

    type Log = Rc<RefCell<Vec<String>>>;

    struct Noisy(&'static str, Log);

    impl Drop for Noisy {
        fn drop(&mut self) {
            self.1.borrow_mut().push(self.0.to_string());
        }
    }

    struct Yield(bool);

    impl Future for Yield {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                return Poll::Ready(());
            }
            self.0 = true;
            Poll::Pending
        }
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
                return value;
            }
        }
    }

    trait Named {
        fn name(&self) -> String;
    }

    impl Named for u8 {
        fn name(&self) -> String {
            format!("u8:{self}")
        }
    }

    impl Named for &str {
        fn name(&self) -> String {
            format!("str:{self}")
        }
    }

    async fn generic<T: Named>(value: T, log: Log) -> usize {
        let name = value.name();
        Yield(false).await;
        log.borrow_mut().push(name.clone());
        name.len()
    }

    async fn borrows(text: &mut String) -> &str {
        text.push('!');
        Yield(false).await;
        text.push('?');
        &text[1..]
    }

    async fn branches(n: u32, log: Log) -> Result<u32, String> {
        let _guard = Noisy("branches", log.clone());
        let mut seen = 0;
        loop {
            if seen == n {
                break;
            }
            let _step = Noisy("iter", log.clone());
            Yield(false).await;
            seen += 1;
            if seen == 3 {
                return Err(format!("stopped at {seen}"));
            }
        }
        match seen {
            0 => Ok(0),
            other => {
                let doubled = async { other * 2 }.await;
                Ok(doubled)
            }
        }
    }

    async fn panics(log: Log) -> u32 {
        let _outer = Noisy("panic-outer", log.clone());
        Yield(false).await;
        let _inner = Noisy("panic-inner", log.clone());
        if log.borrow().len() < 100 {
            panic!("in poll");
        }
        1
    }

    fn boxed(depth: u32) -> Pin<Box<dyn Future<Output = u32>>> {
        Box::pin(async move {
            if depth == 0 {
                return 1;
            }
            Yield(false).await;
            boxed(depth - 1).await + depth
        })
    }

    struct Counting(std::sync::atomic::AtomicU32);

    impl std::task::Wake for Counting {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    struct WakeTwice(u32);

    impl Future for WakeTwice {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 == 2 {
                return Poll::Ready(());
            }
            self.0 += 1;
            cx.waker().clone().wake();
            Poll::Pending
        }
    }

    /// A waker is a pointer to a promoted table of functions.
    fn wakes() -> u32 {
        let counting = std::sync::Arc::new(Counting(Default::default()));
        let waker = Waker::from(counting.clone());
        let mut cx = Context::from_waker(&waker);
        let mut future = pin!(async { WakeTwice(0).await });
        while future.as_mut().poll(&mut cx).is_pending() {}
        counting.0.load(std::sync::atomic::Ordering::Relaxed)
    }

    trait Greets {
        async fn name() -> String {
            "default".to_owned()
        }

        async fn greet() -> String {
            format!("hello {}", Self::name().await)
        }
    }

    struct Plain;
    struct Custom;

    impl Greets for Plain {}

    impl Greets for Custom {
        async fn name() -> String {
            Yield(false).await;
            "custom".to_owned()
        }
    }

    async fn join(log: Log) -> (usize, usize) {
        let a = generic(7u8, log.clone());
        let b = generic("xyz", log.clone());
        let second = b.await;
        (a.await, second)
    }

    pub fn run() -> Vec<String> {
        let log = Log::default();
        let mut out = Vec::new();
        out.push(format!("{:?}", block_on(join(log.clone()))));
        let mut text = String::from("ab");
        out.push(block_on(borrows(&mut text)).to_string());
        out.push(format!("{:?}", block_on(branches(0, log.clone()))));
        out.push(format!("{:?}", block_on(branches(2, log.clone()))));
        out.push(format!("{:?}", block_on(branches(5, log.clone()))));
        out.push(format!("{}", block_on(boxed(4))));
        let caught = catch_unwind(AssertUnwindSafe(|| block_on(panics(log.clone()))));
        out.push(format!("{}", caught.is_err()));
        let closure = |x: u32| async move { x + 1 };
        out.push(format!("{}", block_on(closure(4))));
        out.push(format!("{}", wakes()));
        out.push(block_on(Plain::greet()));
        out.push(block_on(Custom::greet()));
        out.extend(log.borrow().iter().cloned());
        out
    }
}

pub mod const_patterns {
    #[derive(PartialEq, Eq, Debug, Clone, Copy)]
    pub enum Shape {
        Dot,
        Line(u8, u8),
        Named(&'static str),
        Many(&'static [u16]),
    }
    #[derive(PartialEq, Eq)]
    pub struct Pair(pub i32, pub Option<Shape>);
    const A: Shape = Shape::Line(1, 2);
    const B: Shape = Shape::Named("bee");
    const C: Pair = Pair(7, Some(Shape::Many(&[1, 2, 3])));
    const D: &[u8] = b"hello";
    const E: [(u8, bool); 2] = [(1, true), (2, false)];
    const F: &Pair = &Pair(1, None);
    const G: (f32, char) = (1.5, 'x');
    fn shape(s: Shape) -> u8 {
        match s {
            A => 1,
            B => 2,
            Shape::Dot => 3,
            _ => 4,
        }
    }
    fn pair(p: &Pair) -> u8 {
        match *p {
            C => 1,
            _ => match p {
                F => 2,
                _ => 3,
            },
        }
    }
    fn bytes(b: &[u8]) -> u8 {
        match b {
            D => 1,
            b"" => 2,
            _ => 3,
        }
    }
    fn arr(e: [(u8, bool); 2]) -> u8 {
        match e {
            E => 1,
            _ => 0,
        }
    }
    fn fl(e: (f32, char)) -> u8 {
        match e {
            G => 1,
            _ => 0,
        }
    }
    pub fn run() -> Vec<String> {
        let mut out = Vec::new();
        out.push(format!(
            "{:?}",
            [
                shape(Shape::Line(1, 2)),
                shape(Shape::Line(1, 3)),
                shape(Shape::Named("bee")),
                shape(Shape::Named("be")),
                shape(Shape::Dot),
                shape(Shape::Many(&[]))
            ]
        ));
        out.push(format!(
            "{:?}",
            [
                pair(&Pair(7, Some(Shape::Many(&[1, 2, 3])))),
                pair(&Pair(7, Some(Shape::Many(&[1, 2])))),
                pair(&Pair(7, None)),
                pair(&Pair(1, None)),
                pair(&Pair(7, Some(Shape::Many(&[1, 2, 4]))))
            ]
        ));
        out.push(format!("{:?}", [bytes(b"hello"), bytes(b""), bytes(b"hellp"), bytes(b"hell")]));
        out.push(format!("{:?}", [arr(E), arr([(1, true), (2, true)])]));
        out.push(format!("{:?}", [fl((1.5, 'x')), fl((1.5, 'y')), fl((f32::NAN, 'x'))]));
        out
    }
}

pub mod array_map {
    pub fn run() -> Vec<String> {
        let doubled = [1u8, 2, 3].map(|x| x as u32 * 2);
        let owned = [String::from("a"), String::from("b")].map(|s| s + "!");
        vec![format!("{doubled:?}"), format!("{owned:?}")]
    }
}

#[cfg(feature = "nightly")]
pub mod box_patterns {
    pub struct Noisy(pub &'static str, pub std::rc::Rc<std::cell::RefCell<Vec<String>>>);
    impl Drop for Noisy {
        fn drop(&mut self) {
            self.1.borrow_mut().push(format!("drop {}", self.0));
        }
    }
    pub enum List {
        Nil,
        Cons(u32, Box<List>),
    }
    fn sum(list: &List) -> u32 {
        match list {
            List::Nil => 0,
            List::Cons(value, box rest) => *value + sum(rest),
        }
    }
    fn take(pair: Box<(Noisy, Noisy)>) -> Noisy {
        let box (first, _second) = pair;
        first.1.borrow_mut().push("taken".to_owned());
        first
    }
    fn by_ref(option: &Box<Option<String>>) -> usize {
        match option {
            box Some(text) => text.len(),
            box None => 0,
        }
    }
    fn mutate(pair: &mut Box<(u8, String)>) {
        let box (_, ref mut text) = *pair;
        text.push('!');
    }
    fn nested(inner: Box<Box<Noisy>>) -> &'static str {
        let box box noisy = inner;
        noisy.0
    }

    pub fn run() -> Vec<String> {
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let list = List::Cons(1, Box::new(List::Cons(2, Box::new(List::Nil))));
        let mut out = vec![sum(&list).to_string()];
        let kept = take(Box::new((Noisy("a", log.clone()), Noisy("b", log.clone()))));
        out.push(kept.0.to_owned());
        out.push(by_ref(&Box::new(Some("hey".to_owned()))).to_string());
        out.push(by_ref(&Box::new(None)).to_string());
        let mut pair = Box::new((1, "x".to_owned()));
        mutate(&mut pair);
        out.push(pair.1.clone());
        out.push(nested(Box::new(Box::new(Noisy("n", log.clone())))).to_owned());
        drop(kept);
        out.extend(log.borrow().iter().cloned());
        out
    }
}

pub mod promoted_call {
    #[repr(packed)]
    pub struct Packed(pub u8, pub u32);

    // `size_of` is promotable: `&size_of::<T>()` is a constant, not a call that writes one.
    pub fn run() -> Vec<String> {
        let size: &'static usize = &std::mem::size_of::<Packed>();
        assert_eq!(std::mem::size_of::<Packed>(), 5);
        vec![size.to_string()]
    }
}

pub mod type_ids {
    use std::any::{Any, TypeId};

    #[derive(Debug)]
    pub struct Local(pub u8);
    pub struct Generic<T>(pub T);

    // A `TypeId` has to be the one rustc computed, or a value boxed by one compiler does not
    // downcast in code from the other.
    pub fn run() -> Vec<String> {
        let boxed: Box<dyn Any> = Box::new(Local(3));
        vec![
            format!("{:?}", boxed.downcast_ref::<Local>()),
            boxed.downcast_ref::<String>().is_some().to_string(),
            boxed.is::<Local>().to_string(),
            (TypeId::of::<Generic<Vec<u8>>>() == TypeId::of::<Generic<Vec<u8>>>()).to_string(),
            (TypeId::of::<(u8, &'static str)>() == TypeId::of::<[u8; 4]>()).to_string(),
            (TypeId::of::<dyn Any>() == TypeId::of::<fn(u8) -> u8>()).to_string(),
            (boxed.type_id() == TypeId::of::<Box<dyn Any>>()).to_string(),
        ]
    }
}

/// A destructor that panics does not stop the values next to it from being dropped, each once.
pub mod unwind_drops {
    use std::cell::RefCell;
    use std::panic::{self, AssertUnwindSafe};

    type Log = RefCell<Vec<u8>>;

    /// Logs its number when dropped, and panics after that if the number is the log's first
    /// entry, which is the one to fail.
    struct Noisy<'a>(u8, &'a Log);

    impl Drop for Noisy<'_> {
        fn drop(&mut self) {
            let failing = self.1.borrow()[0];
            self.1.borrow_mut().push(self.0);
            if self.0 == failing {
                panic::panic_any(self.0);
            }
        }
    }

    struct Two<'a>(Noisy<'a>, Noisy<'a>);

    fn array(log: &Log) {
        let _all = [Noisy(1, log), Noisy(2, log), Noisy(3, log)];
    }

    fn tuple(log: &Log) {
        let _all = (Noisy(1, log), Two(Noisy(2, log), Noisy(3, log)), Noisy(4, log));
    }

    fn boxed(log: &Log) {
        let _all = Box::new((Noisy(1, log), Noisy(2, log)));
        let _slice: Box<[Noisy<'_>]> = vec![Noisy(3, log), Noisy(4, log)].into_boxed_slice();
    }

    fn assigned(log: &Log) {
        let mut pair = (Noisy(1, log), Noisy(2, log));
        pair.1 = Noisy(3, log);
        let moved = pair.0;
        pair.0 = Noisy(4, log);
        drop(moved);
    }

    fn subslice(log: &Log) {
        let all = [Noisy(1, log), Noisy(2, log), Noisy(3, log)];
        let [_, _rest @ ..] = all;
    }

    fn partly_moved(log: &Log) {
        let all = (Noisy(1, log), Noisy(2, log), Noisy(3, log));
        drop(all.1);
    }

    /// The operands of an expression that unwinds go before the temporaries it borrowed.
    fn operands(log: &Log) {
        (Noisy(1, log), &Noisy(2, log), Noisy(3, log), &Noisy(4, log), panic::panic_any(0u8));
    }

    pub fn run() -> Vec<Vec<u8>> {
        let cases: [fn(&Log); 6] = [array, tuple, boxed, assigned, subslice, partly_moved];
        let mut logs = Vec::new();
        let mut run = |case: fn(&Log), failing| {
            let log = RefCell::new(vec![failing]);
            let _ = panic::catch_unwind(AssertUnwindSafe(|| case(&log)));
            logs.push(log.into_inner());
        };
        for case in cases {
            (0..5).for_each(|failing| run(case, failing));
        }
        // This one unwinds by itself, and a second panic on the way out would abort.
        run(operands, 0);
        logs
    }
}

pub mod async_drops {
    use std::cell::RefCell;
    use std::future::Future;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::pin::Pin;
    use std::rc::Rc;
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    type Log = Rc<RefCell<Vec<u32>>>;

    struct Noisy(u32, Log, u32);

    impl Drop for Noisy {
        fn drop(&mut self) {
            self.1.borrow_mut().push(self.0);
            if self.0 == self.2 {
                panic!("drop of {}", self.0);
            }
        }
    }

    struct Pair(Noisy, Noisy);

    struct Yield(bool);

    impl Future for Yield {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            if std::mem::replace(&mut self.0, true) { Poll::Ready(()) } else { Poll::Pending }
        }
    }

    // The safe `drop` is coerced to an unsafe function pointer in a promoted constant.
    fn raw_waker(data: *const ()) -> RawWaker {
        RawWaker::new(data, &RawWakerVTable::new(raw_waker, drop, drop, drop))
    }

    async fn moves(log: Log, first: bool, second: bool, fails: u32) {
        for i in 0..2u32 {
            let x;
            let y;
            if (first && i == 0) || (second && i == 1) {
                let at = i * 10;
                x = (
                    Noisy(at + 1, log.clone(), fails),
                    Noisy(at + 2, log.clone(), fails),
                    Noisy(at + 3, log.clone(), fails),
                );
                Yield(false).await;
                y = Pair(Noisy(at + 4, log.clone(), fails), Noisy(at + 5, log.clone(), fails));
                drop(x.1);
                Yield(false).await;
                drop(y.0);
                Yield(false).await;
            }
        }
    }

    pub fn run() -> Vec<String> {
        let waker = unsafe { Waker::from_raw(raw_waker(std::ptr::null())) };
        let cloned = waker.clone();
        let cx = &mut Context::from_waker(&cloned);
        let mut out = Vec::new();
        for (first, second) in [(true, false), (false, true), (true, true)] {
            for fails in [0, 3, 5, 12, 14] {
                for polls in 0..8 {
                    let log = Log::default();
                    let panicked = catch_unwind(AssertUnwindSafe(|| {
                        let mut future = Box::pin(moves(log.clone(), first, second, fails));
                        for _ in 0..polls {
                            if future.as_mut().poll(cx).is_ready() {
                                break;
                            }
                        }
                    }))
                    .is_err();
                    let log = log.borrow();
                    out.push(format!("{first} {second} {fails} {polls} {panicked} {log:?}"));
                }
            }
        }
        out
    }
}

pub mod promoted_assoc_const {
    use std::marker::PhantomData;

    struct Vtable {
        align: fn() -> usize,
        size: fn() -> usize,
    }
    struct Owned<T>(PhantomData<T>);
    fn size_of_it<T>() -> usize {
        std::mem::size_of::<T>()
    }
    fn align_of_it<T>() -> usize {
        std::mem::align_of::<T>()
    }
    impl<T> Owned<T> {
        const VTABLE: Vtable = Vtable { align: align_of_it::<T>, size: size_of_it::<T> };
    }
    struct Handle {
        vtable: &'static Vtable,
    }
    // The borrow outlives the call, so the constant has to be a static.
    fn make<T>(_: T) -> Handle {
        Handle { vtable: &Owned::<T>::VTABLE }
    }

    pub fn run() -> Vec<usize> {
        let (a, b) = (make(1u8), make([0u32; 5]));
        vec![(a.vtable.size)(), (b.vtable.size)(), (a.vtable.align)(), (b.vtable.align)()]
    }
}

pub mod offsets {
    use std::mem::offset_of;

    #[repr(C)]
    struct Outer {
        a: u8,
        b: u32,
        c: (u16, u64),
        d: Inner,
    }
    #[repr(C)]
    struct Inner {
        x: u8,
        y: [u16; 3],
    }
    struct Generic<T> {
        _a: u8,
        t: T,
        z: u16,
    }
    fn generic<T>() -> (usize, usize) {
        (offset_of!(Generic<T>, t), offset_of!(Generic<T>, z))
    }
    const NESTED: usize = offset_of!(Outer, d.y);

    pub fn run() -> Vec<usize> {
        let (wide, narrow) = (generic::<u64>(), generic::<[u8; 3]>());
        vec![
            offset_of!(Outer, a),
            offset_of!(Outer, b),
            offset_of!(Outer, c.1),
            offset_of!(Outer, d.x),
            NESTED,
            wide.0,
            wide.1,
            narrow.0,
            narrow.1,
            offset_of!((u8, u64, u16), 2),
        ]
    }
}

pub mod backtraces {
    use std::backtrace::{Backtrace, BacktraceStatus};

    struct Report {
        code: u32,
        // Its layout hangs on a type alias for an `impl Trait` inside std.
        backtrace: Option<Backtrace>,
    }

    fn report(code: u32) -> Report {
        Report { code, backtrace: (code > 1).then(Backtrace::disabled) }
    }

    pub fn run() -> Vec<usize> {
        let (a, b) = (report(1), report(2));
        let disabled =
            b.backtrace.as_ref().is_some_and(|it| it.status() == BacktraceStatus::Disabled);
        vec![
            std::mem::size_of::<Report>(),
            a.code as usize,
            a.backtrace.is_none() as usize,
            b.code as usize,
            disabled as usize,
        ]
    }
}

// An inline `const { .. }` block declares bindings of its own.
pub mod inline_const_bindings {
    trait Swizzle<const N: usize> {
        const INDEX: [usize; N];

        fn indices() -> [u32; N] {
            const {
                let mut output = [0; N];
                let mut i = 0;
                while i < N {
                    let index = Self::INDEX[i];
                    assert!(index < 100);
                    output[i] = index as u32;
                    i += 1;
                }
                struct Indices<const LEN: usize>([u32; LEN]);
                Indices(output)
            }
            .0
        }
    }

    struct Reverse;
    impl Swizzle<3> for Reverse {
        const INDEX: [usize; 3] = [2, 1, 0];
    }

    pub fn run() -> Vec<u32> {
        let nested = std::mem::offset_of!(((u8, (u8, u16)), (u8, u32, u16)), 0.1.1);
        let mut out = Reverse::indices().to_vec();
        out.push(nested as u32);
        out
    }
}

// A closure is `Clone` when what it captured is, and clones capture by capture.
pub mod closure_clone {
    fn twice<F: Fn(i32) -> String + Clone>(f: F) -> (String, String) {
        let g = f.clone();
        (f(1), g(2))
    }

    pub fn run() -> Vec<(String, String)> {
        let s = String::from("hi");
        let v = vec![1, 2, 3];
        let r = &v;
        let captures = move |x: i32| format!("{s} {x} {}", r.len());
        let plain = |x: i32| x.to_string();
        vec![twice(captures.clone()), twice(captures), twice(plain)]
    }
}

/// `TypeId`s are rustc's hash of the type, which its own code compares ours against.
pub mod type_id_hashes {
    use std::any::{Any, TypeId};
    use std::collections::HashMap;
    use std::fmt::Debug;

    pub struct Unit;
    pub struct Tuple(pub u8, u16);
    struct Record {
        _a: u32,
        pub _b: String,
        pub(crate) _c: bool,
    }
    #[repr(C)]
    pub struct ReprC {
        pub a: u8,
    }
    #[repr(transparent)]
    pub struct Transparent(u64);
    #[repr(C, packed(2))]
    pub struct Packed(u64);
    #[repr(align(32))]
    pub struct Aligned(u8);
    pub enum Plain {
        A,
        B,
    }
    #[repr(u8)]
    pub enum Assigned {
        A = 3,
        B,
        C = 9,
        D,
    }
    #[non_exhaustive]
    pub enum Payload {
        Unit,
        Tuple(u8, Box<Payload>),
        Record { _a: u8 },
    }
    pub union Either {
        pub a: u8,
        _b: u32,
    }
    pub struct Generic<T, const N: usize>(pub [T; N]);
    pub struct Borrowed<'a>(pub &'a str);
    pub mod inner {
        pub struct Nested(pub(super) u8, pub(in crate::type_id_hashes) u8, pub(self) u8);
    }
    pub trait Shape {}
    pub trait Convert<T> {}

    fn id<T: ?Sized + 'static>() -> TypeId {
        TypeId::of::<T>()
    }

    pub fn primitives() -> Vec<TypeId> {
        vec![
            id::<bool>(),
            id::<char>(),
            id::<u8>(),
            id::<i8>(),
            id::<u16>(),
            id::<i32>(),
            id::<u64>(),
            id::<i128>(),
            id::<usize>(),
            id::<isize>(),
            id::<f32>(),
            id::<f64>(),
            id::<str>(),
            id::<()>(),
        ]
    }

    pub fn compound() -> Vec<TypeId> {
        vec![
            id::<&'static str>(),
            id::<&'static mut u8>(),
            id::<*const u8>(),
            id::<*mut [u16]>(),
            id::<[u8]>(),
            id::<[u8; 4]>(),
            id::<[[u32; 2]; 300]>(),
            id::<(u8,)>(),
            id::<(u8, &'static str, [i64; 0])>(),
        ]
    }

    pub fn local_structs() -> Vec<TypeId> {
        vec![id::<Unit>(), id::<Tuple>(), id::<Record>(), id::<inner::Nested>()]
    }

    pub fn local_reprs() -> Vec<TypeId> {
        vec![id::<ReprC>(), id::<Transparent>(), id::<Packed>(), id::<Aligned>()]
    }

    pub fn local_enums() -> Vec<TypeId> {
        vec![id::<Plain>(), id::<Assigned>(), id::<Payload>(), id::<Either>()]
    }

    pub fn local_generics() -> Vec<TypeId> {
        vec![
            id::<Generic<u8, 3>>(),
            id::<Generic<Unit, 0>>(),
            id::<Borrowed<'static>>(),
            id::<Generic<Borrowed<'static>, 1>>(),
        ]
    }

    pub fn std_types() -> Vec<TypeId> {
        vec![
            id::<String>(),
            id::<Option<u8>>(),
            id::<Result<(), String>>(),
            id::<Vec<u8>>(),
            id::<Box<str>>(),
            id::<std::cell::Cell<u8>>(),
            id::<std::marker::PhantomData<u8>>(),
            id::<std::mem::ManuallyDrop<u8>>(),
            id::<std::pin::Pin<Box<u8>>>(),
            id::<std::rc::Rc<Unit>>(),
            id::<std::sync::Arc<Plain>>(),
            id::<HashMap<String, Vec<u32>>>(),
            id::<std::io::Error>(),
            id::<std::fmt::Error>(),
            id::<std::num::NonZeroU8>(),
            id::<std::time::Duration>(),
            id::<std::cmp::Ordering>(),
        ]
    }

    pub fn trait_objects() -> Vec<TypeId> {
        vec![
            id::<dyn Any>(),
            id::<dyn Any + Send>(),
            id::<dyn Any + Send + Sync>(),
            id::<dyn Debug>(),
            id::<dyn Shape>(),
            id::<dyn Convert<u8> + Sync>(),
            id::<Box<dyn std::error::Error + Send + Sync>>(),
            id::<&'static dyn Shape>(),
        ]
    }

    type Pick = for<'a, 'b> fn(&'a u8, &'b u8) -> &'b u8;
    type PickSwapped = for<'a, 'b> fn(&'b u8, &'a u8) -> &'a u8;
    type Nested = for<'a> fn(&'a u8, for<'b> fn(&'b u8, &'a u8) -> &'b u8) -> Borrowed<'a>;
    type Callback = dyn for<'a> Fn(&'a str) -> &'a str + Send;
    type Visitor = Box<dyn for<'a, 'b> FnMut(&'a mut Vec<&'b u8>, &'b u8)>;

    pub fn functions() -> Vec<TypeId> {
        vec![
            id::<fn()>(),
            id::<fn(u8) -> u8>(),
            id::<unsafe fn(u8, String) -> Vec<u8>>(),
            id::<extern "C" fn(i32) -> i32>(),
            id::<unsafe extern "C" fn(*const u8, ...) -> i32>(),
            id::<fn(fn(u8) -> String) -> !>(),
            id::<Pick>(),
            id::<PickSwapped>(),
            id::<Nested>(),
            id::<Callback>(),
            id::<Visitor>(),
        ]
    }

    /// The lifetimes these leave out are bound by the signature, which only rustc knows.
    pub fn elided() -> Vec<TypeId> {
        vec![
            id::<fn(&u8) -> &u8>(),
            id::<fn(&mut Borrowed<'_>) -> Borrowed<'static>>(),
            id::<dyn FnMut(&str) -> &str>(),
            id::<Box<dyn Fn(&dyn Fn(&u8) -> &u8) -> Box<dyn Iterator<Item = &'static str>>>>(),
            id::<std::iter::Map<std::slice::Iter<'static, u8>, fn(&u8) -> u8>>(),
            id::<fn(&u8, &u16)>(),
            id::<fn(&u16, &u8)>(),
            id::<fn(Borrowed) -> Borrowed>(),
            id::<fn(Borrowed<'_>, &u8)>(),
            id::<fn(&'static u8) -> &u8>(),
            id::<for<'a> fn(&'a u8, &u8) -> &'a u8>(),
            id::<fn(&dyn Any) -> &dyn Any>(),
            id::<fn(&mut dyn FnMut(&str) -> &str, Box<dyn Any>) -> &str>(),
            id::<dyn Fn(&dyn Any, fn(&u8) -> &u8)>(),
            id::<fn(fn(&u8)) -> fn(&u8) -> &u8>(),
            id::<&'static dyn Fn(&u8) -> std::slice::Iter<u8>>(),
        ]
    }

    fn of_value<T: 'static>(_: &T) -> TypeId {
        TypeId::of::<T>()
    }

    fn generic_item<T, const N: usize>(_: T) {}

    /// Function items, which are named by their path and what they were instantiated with.
    pub fn function_items() -> Vec<TypeId> {
        vec![
            of_value(&id::<u8>),
            of_value(&id::<dyn Shape>),
            of_value(&primitives),
            of_value(&generic_item::<Unit, 3>),
            of_value(&std::io::stdout),
            of_value(&std::mem::drop::<String>),
            of_value(&<u8 as Clone>::clone),
            of_value(&<Vec<u8> as Default>::default),
            id::<Option<Generic<u8, 1>>>(),
        ]
    }

    struct Holder<F>(F, u8);

    fn two_closures<T: 'static>(seed: T) -> (impl Fn() -> u8 + 'static, impl Fn(&str) -> usize) {
        let _ = seed;
        (|| 1, |text: &str| text.len())
    }

    fn is<T: 'static>(_: &T, any: &dyn Any) -> bool {
        any.is::<T>()
    }

    /// A closure's `TypeId` is only ever compared with what was made from the same closure.
    pub fn closures() -> Vec<bool> {
        let captured = String::from("captured");
        let borrowing = move |field: &str| field.len() + captured.len();
        let plain = |a: u8| a + 1;
        let (first, second) = two_closures(0u8);
        let (other, _) = two_closures(0u16);
        let held: Box<dyn Any> = Box::new(Holder(plain, 3));
        vec![
            of_value(&borrowing) == of_value(&borrowing),
            of_value(&borrowing) == of_value(&plain),
            of_value(&first) == of_value(&second),
            of_value(&first) == of_value(&other),
            of_value(&first) == of_value(&two_closures(1u8).0),
            is(&Holder(plain, 0), &*held),
            is(&plain, &*held),
            held.downcast_ref::<Holder<fn(u8) -> u8>>().is_some(),
            borrowing("ab") == 10 && first() == 1 && second("abc") == 3 && other() == 1,
        ]
    }

    /// And so is that of the future of an `async` block or function.
    pub fn futures() -> Vec<bool> {
        async fn waits<T>(value: T) -> T {
            value
        }
        let block = async { 1u8 };
        let other = async { 1u8 };
        let held: Box<dyn Any> = Box::new(Some(waits(2u8)));
        vec![
            of_value(&block) == of_value(&block),
            of_value(&block) == of_value(&other),
            of_value(&waits(1u8)) == of_value(&waits(2u8)),
            of_value(&waits(1u8)) == of_value(&waits(1u16)),
            is(&Some(waits(3u8)), &*held),
            is(&Some(waits(3u16)), &*held),
        ]
    }

    fn name_of<T>(_: &T) -> &'static str {
        std::any::type_name::<T>()
    }

    pub fn type_names() -> Vec<&'static str> {
        let outer = |a: u8| {
            let inner = move |b: u8| a + b;
            (name_of(&inner), inner(1))
        };
        vec![
            name_of(&outer),
            outer(1).0,
            name_of(&primitives),
            name_of(&Holder(|| 3u8, 0)),
            std::any::type_name::<Generic<Unit, 2>>(),
            std::any::type_name::<(&'static str, Vec<Option<Box<dyn Shape>>>)>(),
        ]
    }

    pub fn projections() -> Vec<TypeId> {
        vec![
            id::<dyn Iterator<Item = u8>>(),
            id::<dyn DoubleEndedIterator<Item = (u8, String)> + Send>(),
            id::<dyn Fn(u8) -> u8>(),
            id::<dyn std::ops::Deref<Target = [u8]>>(),
            id::<dyn std::future::Future<Output = Result<(), std::io::Error>> + Send>(),
            id::<dyn Convert<&'static str>>(),
        ]
    }

    pub fn more_std_types() -> Vec<TypeId> {
        vec![
            id::<std::borrow::Cow<'static, str>>(),
            id::<std::collections::BTreeMap<u8, &'static [u8]>>(),
            id::<std::collections::HashSet<char>>(),
            id::<std::collections::VecDeque<f64>>(),
            id::<std::sync::Mutex<Option<Box<dyn Any + Send>>>>(),
            id::<std::sync::atomic::AtomicUsize>(),
            id::<std::path::PathBuf>(),
            id::<std::path::Path>(),
            id::<std::ffi::OsString>(),
            id::<std::ffi::CStr>(),
            id::<std::net::IpAddr>(),
            id::<std::ops::Range<usize>>(),
            id::<std::ops::RangeInclusive<i128>>(),
            id::<std::fmt::Arguments<'static>>(),
            id::<std::thread::Thread>(),
            id::<std::fs::File>(),
            id::<std::cell::RefCell<std::rc::Weak<Record>>>(),
            id::<std::mem::MaybeUninit<[u16; 7]>>(),
            id::<std::num::Wrapping<i8>>(),
            id::<std::task::Poll<Option<u8>>>(),
            id::<std::alloc::Layout>(),
            id::<std::char::ParseCharError>(),
            id::<std::string::FromUtf8Error>(),
            id::<std::num::ParseIntError>(),
            id::<std::convert::Infallible>(),
            id::<std::vec::IntoIter<u8>>(),
            id::<std::panic::Location<'static>>(),
            id::<std::time::Instant>(),
            id::<std::process::ExitCode>(),
        ]
    }

    pub struct BodyHolder;
    pub trait BodyMaker {
        fn make() -> TypeId;
    }
    impl BodyHolder {
        fn inherent() -> TypeId {
            struct Local(#[allow(dead_code)] u8);
            TypeId::of::<Local>()
        }
    }
    impl BodyMaker for BodyHolder {
        fn make() -> TypeId {
            struct Local;
            TypeId::of::<Local>()
        }
    }
    impl BodyMaker for u8 {
        fn make() -> TypeId {
            enum Local {
                #[allow(dead_code)]
                A,
            }
            TypeId::of::<Local>()
        }
    }

    /// Types declared in bodies, and one of the standard library's that only a body names.
    pub fn in_bodies() -> (Vec<TypeId>, bool, String) {
        struct InFn(#[allow(dead_code)] u8);
        let ids = vec![
            TypeId::of::<InFn>(),
            BodyHolder::inherent(),
            <BodyHolder as BodyMaker>::make(),
            <u8 as BodyMaker>::make(),
        ];
        let error: Box<dyn std::error::Error + Send + Sync> = String::from("boxed").into();
        (ids, error.is::<std::fmt::Error>(), error.to_string())
    }

    pub fn downcasts() -> (Option<u8>, Option<String>, bool, bool) {
        let boxed: Box<dyn Any> = Box::new(7u8);
        let error: Box<dyn std::error::Error + Send + Sync> = Box::new(std::fmt::Error);
        let record: &dyn Any = &Tuple(1, 2);
        (
            boxed.downcast_ref::<u8>().copied(),
            boxed.downcast_ref::<String>().cloned(),
            error.downcast_ref::<std::fmt::Error>().is_some(),
            record.is::<Tuple>(),
        )
    }
}

pub mod type_names {
    pub struct Plain;
    pub struct Generic<A, B = u8>(pub A, pub B);
    pub mod inner {
        pub enum Choice {
            A,
        }
    }

    fn of<T: ?Sized>(_: &T) -> &'static str {
        std::any::type_name::<T>()
    }

    pub fn run() -> Vec<&'static str> {
        vec![
            of(&Generic(1i8, [Plain, Plain])),
            std::any::type_name::<Generic<&'static str>>(),
            std::any::type_name::<inner::Choice>(),
            std::any::type_name::<Option<Vec<(u8, String)>>>(),
            std::any::type_name::<*const [u16]>(),
            std::any::type_name::<&mut (i32,)>(),
            std::any::type_name::<dyn std::fmt::Debug + Send>(),
        ]
    }

    pub struct Holder<T>(pub T);

    pub trait Named {
        fn named<F: Fn()>(&self, f: F) -> &'static str;
    }

    fn generic<T, const N: usize>(_: T) {}

    impl<T> Holder<T> {
        fn inherent<F: Fn() -> u8>(&self, f: F) -> [&'static str; 3] {
            let inner = || f();
            [of(&inner), of(&Self::inherent::<F>), of(&generic::<F, 3>)]
        }

        async fn waits(&self, f: impl Fn() -> u8) -> &'static str {
            let inner = || f();
            of(&inner)
        }
    }

    impl<T> Named for Holder<T> {
        fn named<F: Fn()>(&self, f: F) -> &'static str {
            let inner = || f();
            of(&inner)
        }
    }

    impl Named for u8 {
        fn named<F: Fn()>(&self, f: F) -> &'static str {
            let inner = || f();
            of(&inner)
        }
    }

    mod elsewhere {
        impl super::Named for super::inner::Choice {
            fn named<F: Fn()>(&self, f: F) -> &'static str {
                let inner = || f();
                super::of(&inner)
            }
        }
    }

    /// Names that go through an `impl`, a generic function or a closure of one.
    pub fn nested() -> Vec<&'static str> {
        use std::future::Future;
        let holder = Holder(Some(1u16));
        let mut names = holder.inherent(|| 1).to_vec();
        let future = std::pin::pin!(holder.waits(|| 2));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        if let std::task::Poll::Ready(name) = future.poll(&mut cx) {
            names.push(name);
        }
        names.push(holder.named(|| ()));
        names.push(7u8.named(|| ()));
        names.push(inner::Choice::A.named(|| ()));
        names.push(std::any::type_name::<std::slice::Iter<'static, u8>>());
        names
    }
}

pub mod promoted_slices {
    /// `&[..][..]` borrows the array and then indexes it: the temporary that is promoted is
    /// the array, not the slice the index returns.
    pub fn run() -> (usize, Vec<i32>, usize, Vec<u64>, [u64; 2]) {
        let a = &[0i32, 0, 0, 0][..];
        let b = &[5, 6, 7, 8][..];
        let c = &[0u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0][..];
        let d = &[5u64, 6, 7, 8][1..3];
        let e = &[[5u64, 6], [7, 8]][1];
        (a.len(), b.to_vec(), c.len(), d.to_vec(), *e)
    }
}

pub mod reified {
    #[derive(Debug)]
    pub struct W(u8, String);
    #[derive(Debug)]
    pub enum E {
        A(u64, u64, u64),
        B,
    }
    pub trait Tr {
        fn m(&self, x: u32) -> u32;
    }
    impl Tr for u32 {
        fn m(&self, x: u32) -> u32 {
            *self + x
        }
    }
    fn plain(a: u8, b: &str) -> String {
        format!("{a}{b}")
    }
    extern "C" fn cfn(a: u32) -> u32 {
        a * 2
    }

    pub fn run() -> Vec<String> {
        let mut out = Vec::new();
        let f: fn(u8, String) -> W = W;
        out.push(format!("{:?}", f(1, "a".into())));
        let g: fn(u64, u64, u64) -> E = E::A;
        out.push(format!("{:?} {:?}", g(1, 2, 3), E::B));
        let s: fn(u8) -> Option<u8> = Some;
        out.push(format!("{:?}", s(4)));
        let d: fn(&(dyn Tr + 'static), u32) -> u32 = <dyn Tr>::m;
        out.push(format!("{}", d(&5u32, 6)));
        let b: Box<dyn Fn(u8, String) -> W> = Box::new(W);
        out.push(format!("{:?}", b(2, "b".into())));
        let b: Box<dyn Fn(u8, &str) -> String> = Box::new(plain);
        out.push(b(3, "c"));
        let p: fn(u8, &str) -> String = plain;
        let mut b: Box<dyn FnMut(u8, &str) -> String> = Box::new(p);
        out.push(b(4, "d"));
        let b: Box<dyn FnOnce(u64, u64, u64) -> E> = Box::new(E::A);
        out.push(format!("{:?}", b(7, 8, 9)));
        let b: Box<dyn FnOnce(u8, &str) -> String> = Box::new(p);
        out.push(b(5, "e"));
        let r: extern "C" fn(u32) -> u32 = cfn;
        out.push(format!("{:?}", r(5)));
        let v: Vec<Box<dyn Fn(u8) -> Option<u8>>> =
            vec![Box::new(Some), Box::new(|x| x.checked_add(250))];
        out.push(format!("{:?}", v.iter().map(|f| f(9)).collect::<Vec<_>>()));
        out
    }
}

pub mod boxed_fn_once {
    use std::sync::Mutex;

    static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

    struct D(&'static str);
    impl Drop for D {
        fn drop(&mut self) {
            LOG.lock().unwrap().push(format!("drop {}", self.0));
        }
    }
    fn call(f: Box<dyn FnOnce(u8) -> u8 + Send>) -> u8 {
        f(1)
    }

    pub fn run() -> Vec<String> {
        let log = |it: String| LOG.lock().unwrap().push(it);
        let d = D("a");
        let b: Box<dyn FnOnce(u8) -> u8 + Send> = Box::new(move |x| x + d.0.len() as u8);
        log(format!("{}", call(b)));
        let d = D("b");
        let b: Box<dyn FnOnce()> = Box::new(move || {
            let _d = &d;
        });
        drop(b);
        let d = D("c");
        let b: Box<dyn FnOnce()> = Box::new(move || {
            let _d = d;
            panic!("boom")
        });
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(b));
        log(format!("{}", r.is_err()));
        let d = D("d");
        let b: Box<dyn Fn(u8) -> u8> = Box::new(move |x| x + d.0.len() as u8);
        let once: Box<dyn FnOnce(u8) -> u8> = b;
        log(format!("{}", once(3)));
        let v: Vec<Box<dyn FnOnce() -> String>> =
            vec![Box::new(|| "x".into()), Box::new(String::new)];
        for f in v {
            log(f());
        }
        let h = std::thread::spawn(move || 5);
        log(format!("{}", h.join().unwrap()));
        std::mem::take(&mut *LOG.lock().unwrap())
    }
}

pub mod overloaded_unary {
    use std::ops::{Neg, Not};

    #[derive(Clone, Copy, Debug)]
    pub struct Bits(u8);
    impl Not for Bits {
        type Output = Self;
        fn not(self) -> Self {
            Bits(!self.0 & 7)
        }
    }
    #[derive(Clone, Copy, Debug)]
    pub struct Sat(i8);
    impl Neg for Sat {
        type Output = Self;
        fn neg(self) -> Self {
            Sat(self.0.saturating_neg())
        }
    }
    impl Neg for &Sat {
        type Output = i32;
        fn neg(self) -> i32 {
            -(self.0 as i32) * 100
        }
    }
    #[derive(Debug)]
    pub struct Wide(f64, String);
    impl Neg for Wide {
        type Output = Wide;
        fn neg(self) -> Wide {
            Wide(-self.0, self.1 + "!")
        }
    }
    fn generic<T: Not<Output = T>>(value: T) -> T {
        !value
    }

    pub fn run() -> String {
        let (bits, sat) = (Bits(1), Sat(i8::MIN));
        format!(
            "{:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?}",
            !bits,
            generic(bits),
            generic(5u8),
            generic(true),
            -sat,
            -&sat,
            -Wide(1.5, "w".into()),
            -(-1.5f32),
        )
    }
}

/// A closure whose last capture is the widest one: its fields are free to be reordered, where
/// the tuple of the same captures keeps the last one in place. Made and called in different
/// functions, so that a mixed build has rustc's code and ours agree on where the captures are.
pub mod closure_layout {
    #[derive(Clone, Copy)]
    #[repr(packed)]
    pub struct Edge {
        byte: u8,
        next: u32,
        link: u32,
    }

    pub struct Graph {
        heads: Vec<u32>,
        edges: Vec<Edge>,
    }

    impl Graph {
        pub fn edges(&self, node: usize) -> impl Iterator<Item = Edge> + '_ {
            let mut link = self.heads[node];
            core::iter::from_fn(move || {
                if link == 0 {
                    return None;
                }
                let edge = self.edges[link as usize];
                link = edge.link;
                Some(edge)
            })
        }

        pub fn follow(&self, node: usize, byte: u8) -> u32 {
            for edge in self.edges(node) {
                if byte <= edge.byte {
                    if byte == edge.byte {
                        return edge.next;
                    }
                    break;
                }
            }
            99
        }
    }

    pub fn small_first(a: u8, b: &str, c: u16) -> impl Fn(u64) -> String + '_ {
        move |d| format!("{a} {b} {c} {d}")
    }

    pub fn call(f: &dyn Fn(u64) -> String) -> String {
        f(7)
    }

    /// Collected by `Vec`'s general `from_iter`: the in-place one is for adapters of a
    /// `vec::IntoIter`, which this iterator is not.
    pub fn collected(graph: &Graph) -> Vec<u8> {
        graph.edges(0).map(|edge| edge.byte).collect()
    }

    pub fn run() -> String {
        let edge = |byte, next, link| Edge { byte, next, link };
        let graph =
            Graph { heads: vec![1], edges: vec![edge(0, 0, 0), edge(3, 7, 2), edge(9, 8, 0)] };
        format!(
            "{} {} {} {} {} {:?}",
            graph.follow(0, 9),
            graph.follow(0, 3),
            graph.follow(0, 4),
            graph.edges(0).count(),
            call(&small_first(1, "two", 3)),
            collected(&graph),
        )
    }
}

/// `Vec`'s in-place `collect`, which is chosen by constants of the adapters (`MERGE_BY`) whose
/// impls are spread over `core` and `alloc`.
pub mod in_place_collect {
    pub fn mapped(v: Vec<u32>) -> Vec<(u16, u16)> {
        v.into_iter().map(|m| (m as u16, m as u16 + 1)).collect()
    }

    pub fn filtered(v: Vec<u64>) -> Vec<u32> {
        v.into_iter().map(|m| m as u32).filter(|x| *x != 3).collect()
    }

    pub fn run() -> String {
        format!("{:?} {:?}", mapped(vec![1, 2, 3]), filtered(vec![1, 2, 3, 4]))
    }
}

/// A static that holds a `&dyn Trait` to another static: both the data pointer and the vtable
/// are relocations.
pub mod dyn_statics {
    pub trait Callsite: Sync {
        fn name(&self) -> &'static str;
    }

    pub struct First(u8);
    pub struct Second(u16);

    impl Callsite for First {
        fn name(&self) -> &'static str {
            "first"
        }
    }

    impl Callsite for Second {
        fn name(&self) -> &'static str {
            "second"
        }
    }

    static FIRST: First = First(1);
    static SECOND: Second = Second(2);

    pub struct Meta {
        callsite: &'static dyn Callsite,
        n: u32,
    }

    static ONE: Meta = Meta { callsite: &FIRST, n: 1 };
    static TWO: Meta = Meta { callsite: &SECOND, n: 2 };
    static INLINE: Meta = Meta { callsite: &Second(7), n: 3 };
    const BOTH: [&dyn Callsite; 2] = [&FIRST, &SECOND];

    fn same(a: &Meta, b: &Meta) -> bool {
        core::ptr::addr_eq(a.callsite, b.callsite)
    }

    pub fn run() -> String {
        format!(
            "{} {} {} {} {} {} {}",
            same(&ONE, &TWO),
            same(&ONE, &ONE),
            ONE.n + TWO.n + INLINE.n,
            ONE.callsite.name(),
            TWO.callsite.name(),
            INLINE.callsite.name(),
            BOTH.iter().map(|it| it.name()).collect::<String>(),
        )
    }
}

/// `as` between floats and 128-bit integers, which saturates and turns NaN into zero.
pub mod wide_float_casts {
    pub fn to_int(f: f32, d: f64) -> (i128, u128, i128, u128) {
        (f as i128, f as u128, d as i128, d as u128)
    }

    pub fn to_float(a: i128, b: u128) -> (f32, f64, f32, f64) {
        (a as f32, a as f64, b as f32, b as f64)
    }

    pub fn run() -> String {
        let floats = [0.0f32, -1.0, 1.5, 3.0e38, f32::MAX, f32::MIN, f32::NAN, f32::INFINITY, 1e20];
        let ints = [0i128, -1, i128::MAX, i128::MIN, 1 << 100, -(1 << 70), 12345678901234567890123];
        let mut out = String::new();
        for f in floats {
            out += &format!("{:?} {:?}\n", to_int(f, f as f64 * 4.0), to_int(-f, -f as f64));
        }
        for a in ints {
            out += &format!("{:?}\n", to_float(a, a as u128));
        }
        out
    }
}

/// Floats computed at compile time: a NaN is rustc's positive quiet one whatever the host's
/// arithmetic gives, and casts to and from 128-bit integers saturate.
pub mod const_floats {
    use core::cmp::Ordering;

    pub const CASTS: (f32, f64, u128, u128, i128, u128) = (
        u128::MAX as f32,
        i128::MIN as f64,
        1e30f64 as u128,
        3e38f32 as u128,
        -1e30f32 as i128,
        -5.0f64 as u128,
    );
    pub const NANS: (f32, f64, f64) = (0.0 / 0.0, f64::INFINITY - f64::INFINITY, f64::NAN);
    pub const SMALL: (u8, i8, u32, u64, i64) =
        (-0.5f32 as u8, f32::NAN as i8, f64::INFINITY as u32, -1.0f64 as u64, f64::MAX as i64);

    pub fn total(a: f64, b: f64) -> Ordering {
        a.total_cmp(&b)
    }

    pub fn total_single(a: &f32, b: &f32) -> Ordering {
        f32::total_cmp(a, b)
    }

    pub fn sorted<T: Copy>(mut v: Vec<T>, cmp: impl Fn(&T, &T) -> Ordering) -> Vec<T> {
        v.sort_by(cmp);
        v
    }

    pub fn run() -> String {
        let doubles = [f64::NAN, -f64::NAN, f64::INFINITY, -0.0, 0.0, -1.5, f64::NEG_INFINITY, 2.0];
        let singles = [f32::NAN, -f32::NAN, f32::INFINITY, -0.0, 0.0, -1.5, f32::MIN, 2.0];
        format!(
            "{:?} {:?} {:x} {:x} {:x} {:?} {:?} {:?} {:?} {:?} {:?}",
            CASTS,
            SMALL,
            NANS.0.to_bits(),
            NANS.1.to_bits(),
            NANS.2.to_bits(),
            total(f64::NAN, f64::INFINITY),
            total(-f64::NAN, f64::NEG_INFINITY),
            total(-0.0, 0.0),
            total_single(&f32::NAN, &f32::MAX),
            sorted(doubles.to_vec(), f64::total_cmp),
            sorted(singles.to_vec(), |a, b| total_single(a, b)),
        )
    }
}

/// Statics with equal contents are still different objects.
pub mod static_identity {
    pub trait Callsite: Sync {
        fn id(&self) -> u32;
    }
    pub struct First {
        _unused: u8,
    }
    pub struct Second {
        _unused: u8,
    }
    impl Callsite for First {
        fn id(&self) -> u32 {
            1
        }
    }
    impl Callsite for Second {
        fn id(&self) -> u32 {
            2
        }
    }
    pub struct Meta {
        pub name: &'static str,
        pub site: &'static dyn Callsite,
        pub plain: &'static First,
    }
    static FIRST: First = First { _unused: 0 };
    static TWIN: First = First { _unused: 0 };
    static SECOND: Second = Second { _unused: 0 };
    static META: Meta = Meta { name: "a", site: &FIRST, plain: &FIRST };
    static OTHER: Meta = Meta { name: "b", site: &SECOND, plain: &TWIN };

    pub fn thin(meta: &Meta) -> *const () {
        meta.site as *const _ as *const ()
    }

    pub fn run() -> String {
        format!(
            "{} {} {} {} {} {}",
            thin(&META) == thin(&OTHER),
            core::ptr::eq(META.plain, OTHER.plain),
            core::ptr::eq(META.plain, &FIRST),
            thin(&OTHER) == &SECOND as *const Second as *const (),
            META.site.id(),
            OTHER.site.id(),
        )
    }
}

/// `core::arch` vector functions, whose immediates are const generics written as arguments
/// (`#[rustc_legacy_const_generics]`).
#[cfg(target_arch = "x86_64")]
#[rustfmt::skip]
pub mod simd_x86 {
    use core::arch::x86_64::*;
    pub fn ext(v: __m128i) -> (i32, i32) { unsafe { (_mm_extract_epi16(v, 3), _mm_extract_epi16::<5>(v)) } }
    pub fn shift(v: __m128i) -> [u8; 16] { unsafe { core::mem::transmute(_mm_srli_si128(v, 3)) } }
    pub fn shl(v: __m128i) -> [u8; 16] { unsafe { core::mem::transmute(_mm_slli_si128(v, 5)) } }
    #[target_feature(enable = "ssse3")]
    pub unsafe fn alignr(v: __m128i, w: __m128i) -> [u8; 16] { unsafe { core::mem::transmute(_mm_alignr_epi8(v, w, 13)) } }
    #[target_feature(enable = "ssse3")]
    pub unsafe fn pshufb(v: __m128i, w: __m128i) -> [u8; 16] { unsafe { core::mem::transmute(_mm_shuffle_epi8(v, w)) } }
    #[target_feature(enable = "ssse3")]
    pub unsafe fn unload(v: __m128i) -> [i32; 3] { [_mm_extract_epi8(v, 0), _mm_extract_epi8(v, 7), _mm_extract_epi8(v, 15)] }
    pub fn lanes(v: __m128i) -> (i64, i64, i32) { unsafe { (_mm_cvtsi128_si64(v), _mm_cvtsi128_si64(_mm_srli_si128(v, 8)), _mm_movemask_epi8(_mm_cmpeq_epi8(v, _mm_set1_epi8(7)))) } }
    pub fn shr16(v: __m128i) -> [u8; 16] { unsafe { core::mem::transmute(_mm_and_si128(_mm_srli_epi16(v, 4), _mm_set1_epi8(0xF))) } }
    #[target_feature(enable = "avx2")]
    pub unsafe fn wide(v: __m256i, w: __m256i) -> ([u8; 32], [u8; 32], [u8; 32], i32, i64, [u8; 32]) { unsafe {
        let p = _mm256_permute2x128_si256(w, v, 0x21);
        (core::mem::transmute(p), core::mem::transmute(_mm256_alignr_epi8(v, p, 15)), core::mem::transmute(_mm256_shuffle_epi8(v, w)),
         _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, w)), _mm256_extract_epi64(v, 2), core::mem::transmute(_mm256_and_si256(_mm256_srli_epi16(v, 4), _mm256_set1_epi8(0xF))))
    } }
    #[target_feature(enable = "sse4.1")]
    pub unsafe fn minpos(v: __m128i, w: __m128i) -> ([u16; 8], [u16; 8]) { unsafe { (core::mem::transmute(_mm_minpos_epu16(v)), core::mem::transmute(_mm_minpos_epu16(w))) } }
    #[target_feature(enable = "fma")]
    pub unsafe fn fused(a: [f32; 4], b: [f64; 2]) -> ([f32; 4], [f64; 2]) { unsafe {
        let (x, y) = (_mm_loadu_ps(a.as_ptr()), _mm_loadu_pd(b.as_ptr()));
        (core::mem::transmute(_mm_fmadd_ps(x, x, _mm_set1_ps(0.1))), core::mem::transmute(_mm_fmsub_pd(y, _mm_set1_pd(3.0), y)))
    } }
    #[target_feature(enable = "avx")]
    pub unsafe fn upper_cleared(v: __m256i) -> [u8; 32] { unsafe { _mm256_zeroupper(); core::mem::transmute(v) } }
    #[target_feature(enable = "avx512f")]
    pub unsafe fn blend(a: __m512i, b: __m512i) -> ([i32; 16], [i64; 8]) { unsafe {
        (core::mem::transmute(_mm512_mask_blend_epi32(0b1010_0110_0001_1101, a, b)), core::mem::transmute(_mm512_mask_blend_epi64(0b1100_0101, a, b)))
    } }
    #[target_feature(enable = "avx512bw")]
    pub unsafe fn permute16(a: __m512i, b: __m512i) -> [u16; 32] { unsafe {
        let indices = _mm512_set_epi16(63, 0, 31, 32, 1, 33, 62, 2, 5, 40, 7, 9, 44, 46, 48, 50, 3, 35, 63, 30, 29, 60, 61, 4, 8, 16, 24, 32, 40, 48, 56, 127);
        core::mem::transmute(_mm512_permutex2var_epi16(a, indices, b))
    } }
    #[target_feature(enable = "avx512vbmi")]
    pub unsafe fn permute8(a: __m512i, b: __m512i) -> [u8; 64] { unsafe {
        let mut indices = [0u8; 64];
        for (i, index) in indices.iter_mut().enumerate() { *index = (i as u8).wrapping_mul(37).wrapping_add(11); }
        core::mem::transmute(_mm512_permutex2var_epi8(a, core::mem::transmute(indices), b))
    } }
    pub fn run() -> String {
        let mut out = String::new();
        let v = unsafe { _mm_set_epi8(1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16) };
        let w = unsafe { _mm_set_epi8(-1,0,3,14,5,6,7,-8,9,1,11,2,3,4,15,6) };
        out += &format!("{:?} {:?} {:?} {:?} {:?}", ext(v), shift(v), shl(v), lanes(v), shr16(w));
        if is_x86_feature_detected!("ssse3") { out += &format!(" {:?} {:?} {:?}", unsafe { alignr(v, w) }, unsafe { pshufb(v, w) }, unsafe { unload(w) }); }
        if is_x86_feature_detected!("avx2") { unsafe {
            let a = _mm256_set_m128i(v, w); let b = _mm256_set_m128i(w, _mm_set1_epi8(9));
            out += &format!(" {:?}", wide(a, b)); } }
        if is_x86_feature_detected!("sse4.1") { out += &format!(" {:?}", unsafe { minpos(v, _mm_set1_epi16(-3)) }); }
        if is_x86_feature_detected!("fma") { out += &format!(" {:?}", unsafe { fused([1.5, -2.25, 1e-8, 3.0e30], [0.1, -7.5]) }); }
        if is_x86_feature_detected!("avx") { out += &format!(" {:?}", unsafe { upper_cleared(_mm256_set_m128i(v, w)) }); }
        let mut bytes = [0u8; 64];
        for (i, byte) in bytes.iter_mut().enumerate() { *byte = (i as u8).wrapping_mul(7) ^ 0x5a; }
        let a: __m512i = unsafe { core::mem::transmute(bytes) };
        bytes.reverse();
        let b: __m512i = unsafe { core::mem::transmute(bytes) };
        if is_x86_feature_detected!("avx512f") { out += &format!(" {:?}", unsafe { blend(a, b) }); }
        if is_x86_feature_detected!("avx512bw") { out += &format!(" {:?}", unsafe { permute16(a, b) }); }
        if is_x86_feature_detected!("avx512vbmi") { out += &format!(" {:?}", unsafe { permute8(a, b) }); }
        out
    }
}

/// `core::arch` vector functions of AArch64: lanes, immediates that are const generics, table
/// lookups, reductions across a vector, and the narrowing shift a byte search ends with.
#[cfg(target_arch = "aarch64")]
pub mod simd_aarch64 {
    use core::arch::aarch64::*;

    type Bytes = [u8; 16];

    fn bytes(v: uint8x16_t) -> Bytes {
        unsafe { core::mem::transmute(v) }
    }
    pub fn lanes(v: uint8x16_t) -> (u8, u16, u32, u64, i8) {
        unsafe {
            (
                vgetq_lane_u8::<5>(v),
                vgetq_lane_u16(vreinterpretq_u16_u8(v), 3),
                vgetq_lane_u32::<2>(vreinterpretq_u32_u8(v)),
                vgetq_lane_u64::<1>(vreinterpretq_u64_u8(v)),
                vgetq_lane_s8::<15>(vreinterpretq_s8_u8(v)),
            )
        }
    }
    pub fn rotate(v: uint8x16_t, w: uint8x16_t) -> (Bytes, Bytes) {
        unsafe { (bytes(vextq_u8::<3>(v, w)), bytes(vextq_u8(w, v, 13))) }
    }
    pub fn table(v: uint8x16_t, w: uint8x16_t) -> (Bytes, Bytes) {
        unsafe { (bytes(vqtbl1q_u8(v, w)), bytes(vqtbx1q_u8(v, w, v))) }
    }
    pub fn shifts(v: uint8x16_t) -> (Bytes, Bytes, Bytes, Bytes) {
        unsafe {
            let wide = vreinterpretq_u16_u8(v);
            (
                bytes(vshrq_n_u8::<4>(v)),
                bytes(vshlq_n_u8::<3>(v)),
                bytes(vreinterpretq_u8_u16(vshrq_n_u16::<4>(wide))),
                bytes(vreinterpretq_u8_s8(vshrq_n_s8::<2>(vreinterpretq_s8_u8(v)))),
            )
        }
    }
    /// Pairs of lanes, the larger, the smaller or the sum, of the first vector then of the
    /// second, and a table lookup of eight lanes.
    pub fn pairs(
        v: uint8x16_t,
        w: uint8x16_t,
    ) -> (Bytes, Bytes, [i16; 8], [u32; 4], [u8; 8], [u8; 8]) {
        unsafe {
            let (s, t) = (vreinterpretq_s16_u8(v), vreinterpretq_s16_u8(w));
            let (x, y) = (vreinterpretq_u32_u8(w), vreinterpretq_u32_u8(v));
            (
                bytes(vpmaxq_u8(v, w)),
                bytes(vreinterpretq_u8_s8(vpminq_s8(
                    vreinterpretq_s8_u8(w),
                    vreinterpretq_s8_u8(v),
                ))),
                core::mem::transmute(vpmaxq_s16(s, t)),
                core::mem::transmute(vpminq_u32(x, y)),
                core::mem::transmute(vpadd_u8(vget_low_u8(w), vget_high_u8(v))),
                core::mem::transmute(vqtbl1_u8(v, vget_low_u8(w))),
            )
        }
    }
    /// The sums of pairs of lanes of 8, 16 and 32 bits, wrapping, and a movemask made of them.
    pub fn sums(v: uint8x16_t, w: uint8x16_t) -> (Bytes, [u16; 8], [u32; 4], u16) {
        unsafe {
            let (s, t) = (vreinterpretq_u16_u8(v), vreinterpretq_u16_u8(w));
            let (x, y) = (vreinterpretq_u32_u8(w), vreinterpretq_u32_u8(v));
            let bits: Bytes = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
            let high = vandq_u8(vcgtq_u8(w, vdupq_n_u8(9)), vld1q_u8(bits.as_ptr()));
            let folded = vpaddq_u8(high, high);
            let folded = vpaddq_u8(folded, folded);
            let folded = vpaddq_u8(folded, folded);
            (
                bytes(vpaddq_u8(v, w)),
                core::mem::transmute(vpaddq_u16(t, s)),
                core::mem::transmute(vpaddq_u32(x, y)),
                vgetq_lane_u16::<0>(vreinterpretq_u16_u8(folded)),
            )
        }
    }
    pub fn across(v: uint8x16_t) -> (u8, u8, u8, u16, u32, u8) {
        unsafe {
            (
                vmaxvq_u8(v),
                vminvq_u8(v),
                vaddvq_u8(v),
                vaddvq_u16(vpaddlq_u8(v)),
                vaddvq_u32(vpaddlq_u16(vpaddlq_u8(v))),
                vaddvq_u8(vcntq_u8(v)),
            )
        }
    }
    /// Four bits per byte that is seven: what a search over sixteen bytes reads its match from.
    pub fn matches(v: uint8x16_t) -> (u64, u32) {
        unsafe {
            let equal = vceqq_u8(v, vdupq_n_u8(7));
            let narrowed = vshrn_n_u16::<4>(vreinterpretq_u16_u8(equal));
            let mask = vget_lane_u64::<0>(vreinterpret_u64_u8(narrowed));
            (mask, mask.trailing_zeros() >> 2)
        }
    }
    /// The eight-byte group of a hash table: which bytes match, and which are empty.
    pub fn group(bytes: &[u8; 8]) -> (u64, u64) {
        unsafe {
            let group = vld1_u8(bytes.as_ptr());
            let matched = vceq_u8(group, vdup_n_u8(0x41));
            let empty = vcltz_s8(vreinterpret_s8_u8(group));
            (
                vget_lane_u64::<0>(vreinterpret_u64_u8(matched)) & 0x8080_8080_8080_8080,
                vget_lane_u64(vreinterpret_u64_u8(empty), 0),
            )
        }
    }
    pub fn select(v: uint8x16_t, w: uint8x16_t) -> (Bytes, Bytes, Bytes, Bytes) {
        unsafe {
            let greater = vcgtq_u8(v, w);
            (
                bytes(vbslq_u8(greater, v, w)),
                bytes(vmaxq_u8(v, w)),
                bytes(vqaddq_u8(v, w)),
                bytes(vqsubq_u8(v, w)),
            )
        }
    }
    pub fn wide(
        v: uint8x16_t,
        w: uint8x16_t,
    ) -> ([u16; 8], [u32; 4], [u64; 2], [f32; 4], [f64; 2]) {
        unsafe {
            let product = vmull_u8(vget_low_u8(v), vget_high_u8(w));
            let words = vmulq_u32(vreinterpretq_u32_u8(v), vreinterpretq_u32_u8(w));
            let longs = vaddq_u64(vreinterpretq_u64_u8(v), vreinterpretq_u64_u8(w));
            let singles = vmulq_f32(vcvtq_f32_u32(vpaddlq_u16(product)), vdupq_n_f32(0.5));
            let doubles = vsqrtq_f64(vcvtq_f64_u64(vpaddlq_u32(vpaddlq_u16(product))));
            (
                core::mem::transmute(product),
                core::mem::transmute(words),
                core::mem::transmute(longs),
                core::mem::transmute(singles),
                core::mem::transmute(doubles),
            )
        }
    }
    /// CRC-32 of each width by both polynomials, and twice the products' high halves, the
    /// least by itself among them.
    pub fn checksums(v: uint8x16_t, w: uint8x16_t) -> ([u32; 8], [i16; 8], [i32; 4]) {
        unsafe {
            let x = vgetq_lane_u64::<0>(vreinterpretq_u64_u8(v));
            let crc = vgetq_lane_u32::<1>(vreinterpretq_u32_u8(w));
            let s = vsetq_lane_s16::<0>(i16::MIN, vreinterpretq_s16_u8(v));
            let t = vsetq_lane_s16::<0>(i16::MIN, vreinterpretq_s16_u8(w));
            let (a, b) = (vreinterpretq_s32_u8(v), vreinterpretq_s32_u8(w));
            (
                [
                    __crc32b(crc, x as u8),
                    __crc32h(crc, x as u16),
                    __crc32w(crc, x as u32),
                    __crc32d(crc, x),
                    __crc32cb(crc, x as u8),
                    __crc32ch(crc, x as u16),
                    __crc32cw(crc, x as u32),
                    __crc32cd(crc, x),
                ],
                core::mem::transmute(vqdmulhq_s16(s, t)),
                core::mem::transmute(vqdmulhq_s32(a, b)),
            )
        }
    }
    pub fn run() -> String {
        let first: Bytes = [16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1];
        let second: Bytes = [6, 15, 4, 3, 2, 11, 1, 9, 248, 7, 6, 5, 14, 3, 0, 255];
        let (v, w) = unsafe { (vld1q_u8(first.as_ptr()), vld1q_u8(second.as_ptr())) };
        let mut out =
            format!("{:?} {:?} {:?} {:?}", lanes(v), rotate(v, w), table(v, w), shifts(w));
        out += &format!(
            " {:?} {:?} {:?} {:?}",
            across(w),
            matches(v),
            matches(w),
            group(b"AbA\xffcdA\x80")
        );
        out += &format!(" {:?} {:?}", select(v, w), wide(v, w));
        out += &format!(" {:?} {:?} {:?}", pairs(v, w), sums(v, w), checksums(v, w));
        if std::arch::is_aarch64_feature_detected!("neon") {
            out += " neon";
        }
        out
    }
}

/// An associated constant of an impl that is generic over a lifetime, named from a method of
/// that impl.
pub mod lifetime_impl_consts {
    pub struct State<'a> {
        pub trans: &'a [u32],
    }

    impl<'a> State<'a> {
        const DENSE: u32 = 0xFF;
        const ONE: u32 = Self::DENSE - 1;

        pub fn kind(len: usize, force: bool) -> u32 {
            if force || len > State::ONE as usize {
                State::DENSE
            } else if len == 1 {
                State::ONE
            } else {
                len as u32
            }
        }

        pub fn read(&self) -> u32 {
            self.trans.iter().sum::<u32>() + Self::kind(self.trans.len(), false)
        }
    }

    pub fn run() -> String {
        let state = State { trans: &[1, 2, 3] };
        format!(
            "{} {} {} {}",
            State::kind(1, false),
            State::kind(4, true),
            State::kind(999, false),
            state.read()
        )
    }
}

/// `module_path!()` names the module it is written in, also through a macro and in a static.
pub mod module_paths {
    macro_rules! here {
        () => {
            module_path!()
        };
    }
    static IN_STATIC: &str = module_path!();
    const IN_CONST: (&str, usize) = (module_path!(), module_path!().len());

    pub mod nested {
        pub fn path() -> &'static str {
            fn inner() -> &'static str {
                module_path!()
            }
            inner()
        }
        pub fn through_macro() -> &'static str {
            here!()
        }
    }

    pub fn run() -> String {
        format!(
            "{} {} {} {} {:?}",
            module_path!(),
            nested::path(),
            nested::through_macro(),
            IN_STATIC,
            IN_CONST
        )
    }
}

/// A lazy thread local with a destructor inside a function, as `block_on` caches its parker.
pub mod fn_thread_local {
    use std::cell::RefCell;

    fn push<T: Into<u32>>(value: T) -> usize {
        thread_local! {
            static CACHE: RefCell<Vec<u32>> = RefCell::new(Vec::new());
        }
        CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            cache.push(value.into());
            cache.len()
        })
    }

    pub fn run() -> (usize, usize, usize) {
        let other = std::thread::spawn(|| push(7u8)).join().unwrap();
        (push(1u8), push(2u16), other)
    }
}

/// Items of one name at the module's level, in a function and in a closure are different items.
pub mod shadowed_items {
    struct D(u32);
    impl Drop for D {
        fn drop(&mut self) {
            println!("outer D {}", self.0);
        }
    }
    fn helper() -> u32 {
        1
    }
    trait Name {
        fn name(&self) -> &'static str;
    }
    impl Name for D {
        fn name(&self) -> &'static str {
            "outer"
        }
    }

    fn in_function() -> u32 {
        struct D(u32, bool);
        impl Drop for D {
            fn drop(&mut self) {
                println!("fn D {} {}", self.0, self.1);
            }
        }
        impl Name for D {
            fn name(&self) -> &'static str {
                "in function"
            }
        }
        fn helper() -> u32 {
            2
        }
        let d = D(5, true);
        println!("{}", d.name());
        helper() + d.0
    }

    fn in_closure() -> u32 {
        let f = || {
            struct D(u8);
            impl Drop for D {
                fn drop(&mut self) {
                    println!("closure D {}", self.0);
                }
            }
            fn helper() -> u32 {
                3
            }
            let d = D(9);
            helper() + d.0 as u32
        };
        f()
    }

    impl D {
        fn in_method(&self) -> u32 {
            struct D(u64);
            impl Drop for D {
                fn drop(&mut self) {
                    println!("method D {}", self.0);
                }
            }
            fn helper() -> u32 {
                4
            }
            helper() + D(11).0 as u32 + self.0
        }
    }

    pub fn run() -> u32 {
        let d = D(100);
        println!("{}", d.name());
        helper() + in_function() + in_closure() + d.in_method()
    }
}

/// A `for<'a>` written in a body binds its lifetime, so `Any` can't turn `'a` into `'static`.
pub mod hrtb_type_ids {
    use std::any::Any;

    trait AsStr<'a, 'b> {
        fn get(&'a self) -> &'b str;
    }
    impl<'a> AsStr<'a, 'a> for String {
        fn get(&'a self) -> &'a str {
            self
        }
    }

    fn escape(s: String) -> Option<&'static str> {
        let a: Box<dyn Any> = Box::new(Box::new(s) as Box<dyn for<'a> AsStr<'a, 'a>>);
        a.downcast_ref::<Box<dyn for<'a> AsStr<'a, 'static>>>().map(|x| x.get())
    }

    fn same(s: String) -> Option<usize> {
        let a: Box<dyn Any> = Box::new(Box::new(s) as Box<dyn for<'a> AsStr<'a, 'a>>);
        a.downcast_ref::<Box<dyn for<'b> AsStr<'b, 'b>>>().map(|x| x.get().len())
    }

    pub fn run() -> (Option<&'static str>, Option<usize>) {
        (escape(String::from("bar")), same(String::from("bar")))
    }
}

/// A parameter typed `Self::Residual` in an impl is matched as the type the impl gives it.
pub mod projected_patterns {
    use std::convert::Infallible;
    use std::task::Poll;

    pub trait Try {
        type Output;
        type Residual;
        fn from_residual(residual: Self::Residual) -> Self;
        fn branch(self) -> Result<Self::Output, Self::Residual>;
    }

    impl<T> Try for Option<T> {
        type Output = T;
        type Residual = Option<Infallible>;
        fn from_residual(residual: Self::Residual) -> Self {
            match residual {
                None => None,
                Some(never) => match never {},
            }
        }
        fn branch(self) -> Result<T, Self::Residual> {
            match self {
                Some(value) => Ok(value),
                None => Err(None),
            }
        }
    }

    impl<T, E> Try for Poll<Result<T, E>> {
        type Output = Poll<T>;
        type Residual = Result<Infallible, E>;
        fn from_residual(residual: Self::Residual) -> Self {
            match residual {
                Err(e) => Poll::Ready(Err(e)),
                Ok(never) => match never {},
            }
        }
        fn branch(self) -> Result<Poll<T>, Self::Residual> {
            match self {
                Poll::Pending => Ok(Poll::Pending),
                Poll::Ready(Ok(value)) => Ok(Poll::Ready(value)),
                Poll::Ready(Err(e)) => Err(Err(e)),
            }
        }
    }

    fn round_trip<R: Try>(value: R) -> Option<R> {
        value.branch().err().map(R::from_residual)
    }

    pub fn run() -> (Option<Option<u8>>, Option<Poll<Result<u8, String>>>, bool) {
        (
            round_trip(None::<u8>),
            round_trip(Poll::Ready(Err::<u8, _>(String::from("bad")))),
            round_trip(Poll::Ready(Ok::<u8, String>(3))).is_none(),
        )
    }
}

/// A field typed `<Flags as Public>::Internal` is compared, ordered and hashed as `Inner`.
pub mod projected_fields {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    pub trait Public {
        type Internal;
    }

    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
    pub struct Inner(u8);

    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
    pub struct Flags(<Flags as Public>::Internal);

    impl Public for Flags {
        type Internal = Inner;
    }

    pub fn run() -> (bool, std::cmp::Ordering, Option<std::cmp::Ordering>, u64, String) {
        let (a, b) = (Flags(Inner(1)), Flags(Inner(2)));
        let mut hasher = DefaultHasher::new();
        a.clone().hash(&mut hasher);
        (a == b, a.cmp(&b), b.partial_cmp(&a), hasher.finish(), format!("{a:?}"))
    }
}

/// Two `const _` blocks of one module each have an `Inner`, and rustc's symbols for the two
/// read the same.
pub mod anonymous_consts {
    pub trait Named {
        fn name(&self) -> String;
    }

    pub struct First;
    pub struct Second;

    const _: () = {
        #[derive(Debug, PartialEq)]
        struct Inner(u8);
        impl Inner {
            fn describe(&self) -> String {
                format!("first {:?} {}", self, *self == Inner(1))
            }
        }
        impl Named for First {
            fn name(&self) -> String {
                Inner(1).describe()
            }
        }
    };

    const _: () = {
        #[derive(Debug, PartialEq)]
        struct Inner(&'static str);
        impl Inner {
            fn describe(&self) -> String {
                format!("second {:?} {}", self, *self == Inner("b"))
            }
        }
        impl Named for Second {
            fn name(&self) -> String {
                Inner("a").describe()
            }
        }
    };

    pub fn run() -> (String, String) {
        (First.name(), Second.name())
    }
}

/// `Zip` of two slices takes `fold` and its `Item` from the impls its own impl specializes.
pub mod specialized_items {
    pub fn copy(src: &[u8], dst: &mut [u8]) -> u32 {
        let mut copied = 0;
        src.iter().zip(dst.iter_mut()).for_each(|(from, to)| {
            *to = from.to_ascii_uppercase();
            copied += 1;
        });
        copied
    }

    pub fn run() -> (u32, [u8; 4], usize, Vec<(char, u8)>) {
        let mut dst = [b'-'; 4];
        let copied = copy(b"abc", &mut dst);
        let sum = [1usize, 2, 3].iter().zip([10usize, 20].iter()).fold(0, |n, (a, b)| n + a * b);
        let mixed = "xyz".chars().zip([1u8, 2].iter().copied()).rev_pairs();
        (copied, dst, sum, mixed)
    }

    trait RevPairs: Iterator + Sized {
        fn rev_pairs(self) -> Vec<Self::Item> {
            let mut all = self.fold(Vec::new(), |mut all, it| {
                all.push(it);
                all
            });
            all.reverse();
            all
        }
    }
    impl<I: Iterator> RevPairs for I {}
}

/// A derive on a struct outside a `const _` calls the derives of a type declared inside it.
pub mod hidden_derives {
    pub trait Public {
        type Internal;
    }

    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
    pub struct Small(<Small as Public>::Internal);
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
    pub struct Wide(<Wide as Public>::Internal);

    const _: () = {
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        pub struct Internal(u8);
        impl Public for Small {
            type Internal = Internal;
        }
        impl Small {
            pub fn new(bits: u8) -> Small {
                Small(Internal(bits))
            }
        }
    };

    const _: () = {
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        pub struct Internal(u64, u64);
        impl Public for Wide {
            type Internal = Internal;
        }
        impl Wide {
            pub fn new(bits: u64) -> Wide {
                Wide(Internal(bits, !bits))
            }
        }
    };

    pub fn run() -> (bool, bool, std::cmp::Ordering, std::cmp::Ordering, String, String) {
        let (a, b) = (Small::new(1), Small::new(2));
        let (c, d) = (Wide::new(7), Wide::new(7));
        (a == b, c == d, a.cmp(&b), c.cmp(&d), format!("{a:?}"), format!("{c:?}"))
    }
}

/// A parameter moved out in the operand of a `break` is not dropped again when the function
/// returns.
pub mod moved_in_break {
    use std::mem;
    use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering::SeqCst};

    static LIVE: AtomicUsize = AtomicUsize::new(0);

    pub struct Counted(u8);

    impl Counted {
        fn new(value: u8) -> Counted {
            LIVE.fetch_add(1, SeqCst);
            Counted(value)
        }
    }

    impl Drop for Counted {
        fn drop(&mut self) {
            LIVE.fetch_sub(1, SeqCst);
        }
    }

    const fn same_size<A, B>() -> bool {
        mem::size_of::<A>() == mem::size_of::<B>()
    }

    unsafe fn swap<T>(dst: *mut T, val: T) -> T {
        loop {
            if same_size::<T, ()>() {
                break {
                    let res = unsafe { mem::transmute_copy(&()) };
                    mem::forget(val);
                    res
                };
            }
            if same_size::<T, AtomicU8>() {
                let a: &AtomicU8;
                break {
                    a = unsafe { &*(dst as *const AtomicU8) };
                    let res =
                        unsafe { mem::transmute_copy(&a.swap(mem::transmute_copy(&val), SeqCst)) };
                    mem::forget(val);
                    res
                };
            }
            break unsafe { std::ptr::replace(dst, val) };
        }
    }

    fn local() -> (usize, usize) {
        static CNT: AtomicUsize = AtomicUsize::new(0);
        CNT.store(0, SeqCst);

        #[derive(Debug, PartialEq, Eq)]
        struct Foo();

        impl Foo {
            fn new() -> Foo {
                CNT.fetch_add(1, SeqCst);
                Foo()
            }
        }

        impl Drop for Foo {
            fn drop(&mut self) {
                CNT.fetch_sub(1, SeqCst);
            }
        }

        let mut a = Foo::new();
        assert_eq!(unsafe { swap(&mut a, Foo::new()) }, Foo::new());
        let one = CNT.load(SeqCst);
        drop(a);
        (one, CNT.load(SeqCst))
    }

    pub fn run() -> (u8, usize, usize, usize, (usize, usize)) {
        let mut cell = Counted::new(5);
        let old = unsafe { swap(&mut cell, Counted::new(6)) };
        let two = LIVE.load(SeqCst);
        let value = old.0;
        drop(old);
        let one = LIVE.load(SeqCst);
        let mut wide = (Counted::new(1), 7u32);
        drop(unsafe { swap(&mut wide, (Counted::new(2), 8)) });
        (value, two, one, LIVE.load(SeqCst), local())
    }
}

/// `stringify!` keeps the spacing of the source, also for tokens a macro took apart as `tt`s
/// and put back together.
pub mod stringified {
    macro_rules! all {
        ($($t:tt)*) => {
            stringify!($($t)*)
        };
    }

    // Moves one token at a time into the buffer and writes the `>>` itself, as `anyhow::ensure!`
    // does.
    macro_rules! munch {
        (($($buf:tt)*) $dup:tt >> $($rest:tt)*) => {
            stringify!($($buf)* > > $($rest)*)
        };
        (($($buf:tt)*) ($first:tt $($dup:tt)*) $skip:tt $($rest:tt)*) => {
            munch!(($($buf)* $first) ($($rest)*) $($rest)*)
        };
    }

    macro_rules! body {
        ($a:tt $b:tt) => {
            stringify!($a+$b; $a+ $b; $a +$b, f($a)+1, ($a)$b, [$a]+{$b}+{ $b}, $a::$b, $a+=..=$b<-1)
        };
    }

    macro_rules! tts {
        ($a:tt,$b:tt) => {
            stringify!($a $b|$a+1|$a 1|$a)
        };
    }

    macro_rules! fragments {
        ($a:expr, $b:ty) => {
            stringify!($a+$b;$a,$b)
        };
    }

    pub fn run() -> [&'static str; 14] {
        [
            body!(x y),
            tts!(x, y),
            tts!(+,-),
            fragments!(x + 1, Vec<u8>),
            stringify!(0 as int! {...} != 0),
            stringify!(
                1 + 'a: loop {
                    break 'a 1;
                } == 1
            ),
            stringify!(E::U > E::U::<u8>),
            stringify!(a +-b; x<<=y; &&z; {} { } {a} { a } {a } { a}; f::<'a>(); |&'a x| 1),
            all!(0 as int! {...} != 0),
            all!(
                1 + 'a: loop {
                    break 'a 1;
                } == 1
            ),
            all!(E::U > E::U::<u8>),
            all!(a +-b; x<<=y; &&z; {} { } {a} { a } {a } { a}; f::<'a>(); |&'a x| 1),
            munch!(() (E::U::<>>E::U::<u8>) E::U::<>>E::U::<u8>),
            munch!(() (a; &b + 'c: loop {}>>d) a; &b + 'c: loop {}>>d),
        ]
    }
}

/// What std does with a function pointer goes through `FnPtr::addr`, which only the compiler
/// implements.
pub mod fn_pointers {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    fn one() -> u8 {
        1
    }

    fn two() -> u8 {
        2
    }

    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy)]
    pub struct Callback(fn() -> u8, unsafe extern "C" fn(u8) -> u8);

    unsafe extern "C" fn id(x: u8) -> u8 {
        x
    }

    fn hash(value: impl Hash) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    pub fn run() -> (bool, bool, bool, bool, bool, bool, usize, u8) {
        let (a, b): (fn() -> u8, fn() -> u8) = (one, two);
        let (x, y) = (Callback(a, id), Callback(b, id));
        let shown = format!("{a:?} {a:p} {x:?}");
        (
            a == a,
            a == b,
            (a < b) == ((a as usize) < (b as usize)),
            x == x.clone() && x != y,
            hash(a) == hash(a) && hash(x) == hash(x),
            x.cmp(&y) == a.cmp(&b),
            shown.matches("0x").count(),
            (x.0)() + unsafe { (y.1)(3) },
        )
    }
}

/// A closure whose parameter types are only known from the bound on the field it is put in.
pub mod closure_in_bounded_field {
    struct Guard<T, Data, F>
    where
        F: FnMut(&Data, &mut T),
    {
        value: T,
        data: Data,
        f: F,
    }

    impl<T, Data, F> Drop for Guard<T, Data, F>
    where
        F: FnMut(&Data, &mut T),
    {
        fn drop(&mut self) {
            (self.f)(&self.data, &mut self.value)
        }
    }

    fn fill<I: IntoIterator<Item = u8>, const CHECK: bool>(len: &mut u32, items: I) -> u8 {
        let start = *len as usize;
        let mut guard = Guard {
            value: len,
            data: start,
            f: move |&count, len| {
                **len = count as u32;
            },
        };
        let mut sum = 0;
        for item in items {
            if guard.data == 4 && CHECK {
                break;
            }
            sum += item;
            guard.data += 1;
        }
        sum
    }

    pub fn run() -> (u32, u8, u32, u8) {
        let (mut a, mut b) = (1, 0);
        let x = fill::<_, true>(&mut a, [1, 2, 3, 4, 5].iter().cloned());
        let y = fill::<_, false>(&mut b, 1..=5);
        (a, x, b, y)
    }
}

/// A trait method named like an unstable inherent method of std, which rustc passes over.
pub mod unstable_name_collision {
    use std::alloc::Layout;

    pub trait LayoutExt {
        fn padding_needed_for(&self, align: usize) -> usize;
        fn repeated(&self, n: usize) -> Option<(Layout, usize)>;
    }

    impl LayoutExt for Layout {
        fn padding_needed_for(&self, align: usize) -> usize {
            let len = self.size();
            (len.wrapping_add(align).wrapping_sub(1) & !align.wrapping_sub(1)).wrapping_sub(len)
        }

        fn repeated(&self, n: usize) -> Option<(Layout, usize)> {
            let padded = self.size().checked_add(self.padding_needed_for(self.align()))?;
            let layout = Layout::from_size_align(padded.checked_mul(n)?, self.align()).ok()?;
            Some((layout, padded))
        }
    }

    pub fn run() -> (usize, Option<(Layout, usize)>) {
        let layout = Layout::new::<(u32, u8)>();
        (Layout::new::<[u8; 5]>().padding_needed_for(4), layout.repeated(3))
    }
}

/// A closure that moves out of a local of the block it is the tail of, inside another closure.
pub mod closure_in_block_tail {
    fn call<R>(f: impl FnOnce(u8) -> R) -> R {
        f(1)
    }

    pub fn run() -> (usize, String) {
        let shared = String::from("shared");
        let sync = vec![1u8, 2];
        call(|outer| {
            let first = call({
                let sync = sync.clone();
                |inner| {
                    let mut sync = sync;
                    sync.push(inner + outer);
                    sync.len() + shared.len()
                }
            });
            let second = call(|_| {
                let mut sync = sync;
                sync.push(0);
                format!("{sync:?} {shared}")
            });
            (first, second)
        })
    }
}

/// What cargo's dev profile sets and what it does not.
pub mod build_cfgs {
    pub struct Sized {
        #[cfg(debug_assertions)]
        pub id: usize,
        pub fd: u32,
    }

    pub fn flags() -> (bool, bool, usize) {
        (cfg!(debug_assertions), cfg!(rust_analyzer), size_of::<Sized>())
    }

    pub fn debug_assert_runs() -> u32 {
        let mut ran = 0;
        debug_assert!({
            ran += 1;
            true
        });
        ran
    }
}

/// A bound on a concrete type holds through the type's impl, not through the bound.
pub mod trivial_bounds {
    type Storage = Vec<u8>;

    pub fn default_len() -> usize
    where
        Storage: 'static + Send + Default,
    {
        let mut storage = Storage::default();
        storage.push(7);
        storage.len()
    }
}

/// One variadic function called with different numbers of arguments.
pub mod variadic {
    // The C runtime of MSVC has `snprintf` in its headers, and in this library for the rest.
    #[cfg_attr(target_env = "msvc", link(name = "legacy_stdio_definitions"))]
    unsafe extern "C" {
        fn snprintf(buf: *mut u8, len: usize, format: *const u8, ...) -> i32;
    }

    pub fn none() -> String {
        let mut buf = [0u8; 32];
        let len = unsafe { snprintf(buf.as_mut_ptr(), buf.len(), c"plain".as_ptr().cast()) };
        String::from_utf8_lossy(&buf[..len as usize]).into_owned()
    }

    pub fn two(a: i32, b: u64) -> String {
        let mut buf = [0u8; 32];
        let len =
            unsafe { snprintf(buf.as_mut_ptr(), buf.len(), c"%d-%llu".as_ptr().cast(), a, b) };
        String::from_utf8_lossy(&buf[..len as usize]).into_owned()
    }

    pub fn string(text: &std::ffi::CStr) -> String {
        let mut buf = [0u8; 32];
        let len = unsafe {
            snprintf(buf.as_mut_ptr(), buf.len(), c"<%s>".as_ptr().cast(), text.as_ptr())
        };
        String::from_utf8_lossy(&buf[..len as usize]).into_owned()
    }
}

/// A constant holding a pointer into another, over-aligned constant.
/// Constants that point into constants: a keyword lookup by length then bytes, as bun's
/// lexer tables, a slice that starts past the start of another, and slices of slices.
pub mod const_tables {
    const KEYWORDS: &[(&[u8], u32)] =
        &[(b"do", 1), (b"if", 2), (b"in", 3), (b"for", 4), (b"let", 5), (b"while", 6)];
    const DIGITS: &[u8] = b"0123456789abcdef";
    const TAIL: &[u8] = DIGITS.split_at(10).1;
    const NESTED: &[&[&str]] = &[&["alpha", "beta"], &[], &["gamma"]];

    fn index(key: &[u8]) -> u32 {
        match key.len() {
            2 if key == b"do" => 1,
            2 if key == b"if" => 2,
            2 if key == b"in" => 3,
            3 if key == b"for" => 4,
            3 if key == b"let" => 5,
            5 if key == b"while" => 6,
            _ => u32::MAX,
        }
    }

    fn lookup(key: &[u8]) -> Option<u32> {
        KEYWORDS.iter().find(|(it, _)| *it == key).map(|(_, value)| *value)
    }

    pub fn run(words: &[&[u8]]) -> String {
        let found: Vec<(u32, Option<u32>)> =
            words.iter().map(|it| (index(it), lookup(it))).collect();
        let nested: Vec<String> = NESTED.iter().map(|it| it.join("+")).collect();
        let tail = std::str::from_utf8(TAIL).unwrap();
        format!("{found:?} {tail} {nested:?} {}", DIGITS[TAIL.len() + words.len()] as char)
    }
}

pub mod const_pointer_into_const {
    use std::marker::PhantomData;

    #[repr(align(16))]
    #[derive(Clone, Copy)]
    struct Wide([u8; 16]);

    const fn tags() -> &'static [u8; 16] {
        #[repr(C)]
        struct Aligned {
            _align: [Wide; 0],
            tags: [u8; 16],
        }
        const ALIGNED: Aligned = Aligned { _align: [], tags: [0x80; 16] };
        &ALIGNED.tags
    }

    struct Inner {
        ctrl: std::ptr::NonNull<u8>,
        mask: usize,
    }

    impl Inner {
        const NEW: Self = Inner::new();

        const fn new() -> Self {
            Self {
                ctrl: unsafe { std::ptr::NonNull::new_unchecked(tags().as_ptr().cast_mut()) },
                mask: 0,
            }
        }
    }

    pub struct Table<T, S> {
        inner: Inner,
        hasher: S,
        marker: PhantomData<T>,
    }

    impl<T, S> Table<T, S> {
        pub const fn with_hasher(hasher: S) -> Self {
            Self { inner: Inner::NEW, hasher, marker: PhantomData }
        }

        pub fn first(&self) -> (u8, usize, usize) {
            let ptr = self.inner.ctrl.as_ptr();
            (unsafe { *ptr.add(self.inner.mask) }, ptr as usize % 16, size_of_val(&self.hasher))
        }
    }

    pub fn run() -> ((u8, usize, usize), (u8, usize, usize)) {
        #[derive(Clone)]
        struct Hasher;
        const EMPTY: Table<u32, Hasher> = Table::with_hasher(Hasher);
        let local = EMPTY;
        (local.first(), Table::<u8, u64>::with_hasher(3).first())
    }
}

pub mod included_files {
    static BLOB: &[u8] = include_bytes!("blob.bin");
    const NOTE: &str = include_str!("note.txt");

    pub fn run() -> (usize, u8, &'static [u8], &'static str, usize) {
        let local: &[u8; 11] = include_bytes!("blob.bin");
        (BLOB.len(), BLOB[2], &local[4..9], NOTE, include_str!("note.txt").lines().count())
    }
}

pub mod nested_macro_self {
    #[derive(Default, PartialEq, Clone)]
    pub struct Side {
        pub width: Option<u32>,
        pub style: Option<u8>,
    }

    #[derive(Default)]
    pub struct Handler {
        top: Side,
        bottom: Side,
        start: Side,
        end: Side,
        out: Vec<u32>,
    }

    impl Handler {
        pub fn flush(&mut self) {
            macro_rules! flush_category {
                ($a: expr, $b: expr, $is_logical: expr) => {
                    macro_rules! shorthand {
                        ($key: ident) => {{
                            let has_prop = $a.$key.is_some() && $b.$key.is_some();
                            if has_prop {
                                self.out.push(std::mem::take(&mut $a.$key).unwrap() as u32);
                            }
                        }};
                    }
                    macro_rules! logical_shorthand {
                        ($key: ident, $start: expr, $end: expr) => {{
                            let has_prop = $start.$key.is_some() && $end.$key.is_some();
                            if has_prop {
                                self.out.push(std::mem::take(&mut $end.$key).unwrap() as u32 + 100);
                            }
                        }};
                    }
                    if $is_logical && $a == $b {
                        logical_shorthand!(style, $a, $b);
                        logical_shorthand!(width, $a, $b);
                    } else {
                        shorthand!(style);
                        shorthand!(width);
                    }
                };
            }
            flush_category!(self.top, self.bottom, false);
            flush_category!(self.start, self.end, true);
        }
    }

    pub fn run() -> Vec<u32> {
        let side = Side { width: Some(3), style: Some(4) };
        let mut handler = Handler {
            top: side.clone(),
            bottom: side.clone(),
            start: side.clone(),
            end: side,
            out: Vec::new(),
        };
        handler.flush();
        handler.out
    }
}

pub mod shadowed_item_macro {
    macro_rules! for_each_op {
        ($mac:ident) => {
            $mac! { nop unreachable }
        };
    }

    macro_rules! define_ops {
        ($($op:ident)*) => {
            $( fn $op(&mut self) -> Self::Output; )*
        };
    }

    pub trait Visit {
        type Output;
        for_each_op!(define_ops);
    }

    pub struct Names;

    macro_rules! define_ops {
        ($($op:ident)*) => {
            $( fn $op(&mut self) -> &'static str { stringify!($op) } )*
        };
    }

    impl Visit for Names {
        type Output = &'static str;
        for_each_op!(define_ops);
    }

    fn visit<V: Visit>(visitor: &mut V, code: u8) -> Option<V::Output> {
        Some(match code {
            0 => visitor.nop(),
            1 => {
                let visitor = visitor;
                visitor.unreachable()
            }
            _ => return None,
        })
    }

    pub fn run() -> [Option<&'static str>; 3] {
        [visit(&mut Names, 0), visit(&mut Names, 1), visit(&mut Names, 2)]
    }
}

pub mod repeated_statics {
    use std::future::Future;
    use std::sync::atomic::{AtomicU32, Ordering};

    macro_rules! bump {
        ($by:expr) => {{
            static COUNT: AtomicU32 = AtomicU32::new(0);
            static mut LAST: u32 = 0;
            unsafe { LAST = $by };
            COUNT.fetch_add($by, Ordering::Relaxed) + unsafe { LAST }
        }};
    }

    static LAZY: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| "lazy".repeat(2));

    pub fn lazy() -> usize {
        fn named() -> u32 {
            7
        }
        static NAMED: std::sync::LazyLock<u32> = std::sync::LazyLock::new(named);
        LAZY.len() + *NAMED as usize
    }

    pub fn twice() -> (u32, u32, u32) {
        (bump!(1), bump!(10), bump!(100) + bump!(1000))
    }

    pub fn in_closure() -> (u32, u32) {
        let first = || bump!(2) + bump!(20);
        let second = || bump!(200);
        (first() + first(), second() + bump!(2000))
    }

    async fn waits() -> (u32, u32) {
        let first = bump!(3);
        let second = async { bump!(30) + bump!(300) }.await;
        (first, second + bump!(3000))
    }

    pub fn in_async() -> (u32, u32) {
        let mut future = std::pin::pin!(waits());
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        loop {
            if let std::task::Poll::Ready(value) = future.as_mut().poll(&mut cx) {
                return value;
            }
        }
    }
}

/// `writeln!` adds its newline to a raw string as to any other.
pub mod raw_newline {
    use std::fmt::Write;

    pub fn run() -> String {
        let mut out = String::new();
        let name = "raw";
        writeln!(
            out,
            r#"first {name}
second "quoted"
"#
        )
        .unwrap();
        writeln!(
            out,
            r"plain
 {}",
            1
        )
        .unwrap();
        writeln!(out, "escaped\n {name}\n").unwrap();
        out
    }
}

/// A map filled in here and read by the caller: both sides must agree on where a tree node
/// keeps its keys and values.
pub mod shared_map {
    use std::collections::BTreeMap;

    #[derive(Debug, Clone, PartialEq)]
    pub enum Value {
        Null,
        Bool(bool),
        Number(f64),
        String(String),
        Array(Vec<Value>),
        Object(BTreeMap<String, Value>),
    }

    pub fn build() -> Value {
        let mut inner = BTreeMap::new();
        inner.insert("message".to_string(), Value::String("unexpected token".to_string()));
        inner.insert("line".to_string(), Value::Number(3.0));
        let mut outer = BTreeMap::new();
        outer.insert("error".to_string(), Value::Object(inner));
        outer.insert("ok".to_string(), Value::Bool(false));
        outer.insert("rest".to_string(), Value::Array(vec![Value::Null, Value::Bool(true)]));
        Value::Object(outer)
    }
}

/// A temporary in the tail of a block that an older-edition macro wrote lives to the end of
/// the statement around the block, not to the end of the block.
pub mod tail_temporaries {
    use std::cell::RefCell;

    thread_local! {
        static LOG: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    pub struct Noisy(String);

    impl Drop for Noisy {
        fn drop(&mut self) {
            LOG.with(|log| log.borrow_mut().push(format!("drop {}", self.0)));
        }
    }

    pub struct Shown<'a>(&'a Noisy);

    pub fn show(value: &Noisy) -> Shown<'_> {
        Shown(value)
    }

    fn read(values: &[(&str, Option<&Shown<'_>>)]) -> String {
        let read: Vec<String> =
            values.iter().map(|it| format!("{}={}", it.0, it.1.unwrap().0.0)).collect();
        LOG.with(|log| log.borrow_mut().push(format!("read {read:?}")));
        read.join(",")
    }

    pub fn run(pick: bool) -> Vec<String> {
        let first = read(old_macros::values!(
            show;
            "a" = Noisy("one".to_string()),
            "b" = Noisy("two".to_string())
        ));
        let second = if pick {
            read(old_macros::values!(show; "c" = Noisy(format!("three {first}"))))
        } else {
            read(&[])
        };
        LOG.with(|log| log.borrow_mut().push(second));
        LOG.with(|log| std::mem::take(&mut *log.borrow_mut()))
    }
}

/// Field names a `static` borrows from `const`s in blocks, as tracing 0.1.44's `trace!`
/// writes them (issue #109): the `static`'s value promotes them.
pub mod const_field_names {
    pub fn run() -> Vec<&'static str> {
        let fields = old_macros::fieldset!(start, end);
        fields.0.to_vec()
    }
}

/// A trait object given for a parameter bounded `T: 'a` lives for `'a`, rustc's object
/// lifetime default (issue #84): `Ref<'a, dyn Show>` is `Ref<'a, dyn Show + 'a>`.
pub mod object_lifetime_default {
    pub trait Show {
        fn show(&self) -> String;
    }

    impl Show for String {
        fn show(&self) -> String {
            self.clone()
        }
    }

    pub struct Ref<'a, T: 'a + ?Sized> {
        r: &'a T,
    }

    pub struct Ref2<'a, T: ?Sized>
    where
        T: 'a,
    {
        r: &'a T,
    }

    struct Container<'a> {
        t: Ref<'a, dyn Show>,
        u: Ref2<'a, dyn Show>,
    }

    fn put<'a>(t: Ref<'a, dyn Show + 'a>, u: Ref2<'a, dyn Show + 'a>, c: &mut Container<'a>) {
        c.t = t;
        c.u = u;
    }

    pub fn run() -> String {
        let (a, b, first) = ("a".to_string(), "b".to_string(), "first".to_string());
        let mut c = Container { t: Ref { r: &first }, u: Ref2 { r: &first } };
        put(Ref { r: &a }, Ref2 { r: &b }, &mut c);
        format!("{}{}", c.t.r.show(), c.u.r.show())
    }
}

/// What cargo 0.99 borrows as rustc accepts: rusqlite's `impl Params for &[&dyn ToSql]`,
/// whose elided lifetimes leave the trait object any lifetime; `&|_| true`, promoted as it
/// captures nothing; callbacks that borrow a value without drop glue declared after them and
/// are moved away, which the unwind path still drops; and an `async fn` returning
/// `impl Iterator + 'b` of a struct holding `&'a self` with `'a: 'b`.
pub mod cargo_borrows {
    use std::future::Future;
    use std::pin::pin;
    use std::rc::Rc;
    use std::task::{Context, Poll, Waker};

    pub trait ToSql {
        fn value(&self) -> u64;
    }

    impl ToSql for u64 {
        fn value(&self) -> u64 {
            *self
        }
    }

    impl ToSql for String {
        fn value(&self) -> u64 {
            self.len() as u64
        }
    }

    impl<T: ToSql + ?Sized> ToSql for &T {
        fn value(&self) -> u64 {
            (**self).value()
        }
    }

    pub trait Params {
        fn sum(&self) -> u64;
    }

    impl Params for &[&dyn ToSql] {
        fn sum(&self) -> u64 {
            self.iter().map(|it| it.value()).sum()
        }
    }

    struct Statement<'conn>(&'conn str);

    impl Statement<'_> {
        fn exists<P: Params>(&mut self, params: P) -> bool {
            params.sum() > self.0.len() as u64
        }
    }

    fn params(ids: &[u64], conn: &str) -> usize {
        let mut statement = Statement(conn);
        let mut found = 0;
        for id in ids {
            let names = vec![format!("name{id}")];
            for name in names.iter() {
                let params = &[&id as &dyn ToSql, &name as &dyn ToSql] as &[&dyn ToSql];
                found += usize::from(statement.exists(params));
            }
        }
        found
    }

    fn keep(keep_previous: Option<&dyn Fn(u64) -> bool>, ids: &[u64]) -> usize {
        let keep_previous = keep_previous.unwrap_or(&|_| true);
        ids.iter().filter(|&&id| keep_previous(id)).count()
    }

    struct Counter<const N: usize> {
        slots: [u32; N],
        index: usize,
    }

    impl<const N: usize> Counter<N> {
        fn add(&mut self, value: u32) {
            self.slots[self.index] = value;
            self.index = (self.index + 1) % N;
        }
    }

    struct Callbacks<'a> {
        progress: Option<Box<dyn FnMut(u32) -> bool + 'a>>,
    }

    struct FetchOptions<'cb> {
        callbacks: Option<Callbacks<'cb>>,
    }

    fn fetch(cb: &mut dyn FnMut(FetchOptions<'_>) -> u32) -> u32 {
        let mut callbacks = Callbacks { progress: None };
        let mut counter = Counter::<4> { slots: [0; 4], index: 0 };
        callbacks.progress = Some(Box::new(|value| {
            counter.add(value);
            counter.slots.iter().sum::<u32>() > 10
        }));
        std::hint::black_box(&mut callbacks);
        let options = FetchOptions { callbacks: Some(callbacks) };
        cb(options)
    }

    struct Index<'gctx> {
        name: &'gctx str,
        cache: Vec<u8>,
    }

    impl<'gctx> Index<'gctx> {
        async fn load(&self) -> Rc<Vec<u8>> {
            Rc::new(self.cache.clone())
        }

        async fn summaries<'a, 'b>(
            &'a self,
            req: &'b u8,
        ) -> Result<impl Iterator<Item = u8> + 'b, ()>
        where
            'a: 'b,
        {
            let summaries = self.load().await;
            struct I<'a> {
                index: &'a Index<'a>,
                req: &'a u8,
                summaries: Rc<Vec<u8>>,
                i: usize,
            }
            impl Iterator for I<'_> {
                type Item = u8;
                fn next(&mut self) -> Option<u8> {
                    let it = *self.summaries.get(self.i)?;
                    self.i += 1;
                    Some(it + *self.req + self.index.name.len() as u8)
                }
            }
            Ok(I { index: self, req, summaries, i: 0 })
        }
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
                return value;
            }
        }
    }

    pub fn run() -> (usize, usize, u32, Vec<u8>) {
        let fetched = fetch(&mut |mut options| {
            let mut progress = options.callbacks.take().and_then(|it| it.progress).unwrap();
            (1..=5).filter(|&it| progress(it)).count() as u32
        });
        let index = Index { name: "crates", cache: vec![1, 2, 3] };
        let req = 10;
        let summaries = block_on(index.summaries(&req)).map(|it| it.collect()).unwrap_or_default();
        (params(&[1, 2, 30], "conn"), keep(None, &[1, 2, 3]), fetched, summaries)
    }
}

/// A derived ordering over an enum whose variants carry fields of several types.
pub mod enum_ordering {
    use std::path::PathBuf;

    #[non_exhaustive]
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    pub enum Stage {
        Initializing,
        Starting { crate_count: usize, patch: bool },
        Compiling { current: usize, total: usize, krate: String, fresh: bool },
        Copying { current: usize, total: usize, path: PathBuf },
        Native { detail: String },
        Success,
    }

    pub fn run(seed: usize) -> Vec<(Option<std::cmp::Ordering>, std::cmp::Ordering, bool)> {
        let stages = [
            Stage::Initializing,
            Stage::Starting { crate_count: seed, patch: seed % 2 == 0 },
            Stage::Starting { crate_count: seed, patch: true },
            Stage::Compiling { current: 1, total: seed, krate: "a".into(), fresh: false },
            Stage::Compiling { current: 1, total: seed, krate: "b".into(), fresh: false },
            Stage::Copying { current: 0, total: 1, path: PathBuf::from("x/y") },
            Stage::Copying { current: 0, total: 1, path: PathBuf::from("x/z") },
            Stage::Native { detail: format!("{seed}") },
            Stage::Success,
        ];
        let mut out = Vec::new();
        for a in &stages {
            for b in &stages {
                out.push((a.partial_cmp(b), a.cmp(b), a < b));
            }
        }
        out
    }
}

/// `String + &String`, where the right side only fits the one `Add` impl after a deref, in
/// a closure whose return type is inferred from it.
pub mod added_strings {
    pub enum Segment {
        Literal(String),
        Other(u8),
    }

    pub fn joined(raw: Vec<u32>) -> String {
        raw.into_iter().map(|ident| ident.to_string()).collect::<Vec<_>>().join("-")
    }

    pub struct Word(String);
    impl core::borrow::Borrow<str> for Word {
        fn borrow(&self) -> &str {
            &self.0
        }
    }

    /// An instance of `join` rustc has no copy of.
    pub fn joined_words(raw: Vec<u32>) -> String {
        raw.into_iter().map(|ident| Word(ident.to_string())).collect::<Vec<_>>().join("-")
    }

    pub fn run(segments: &[Segment]) -> Option<String> {
        segments.iter().try_fold(String::new(), |acc, segment| {
            if let Segment::Literal(seg) = segment { Some(acc + seg) } else { None }
        })
    }
}

/// A macro call as a generic argument of a path expression.
pub mod macro_type_arguments {
    pub struct Minus;
    pub struct Plus;
    macro_rules! Tok {
        [-] => { Minus };
        [+] => { Plus };
    }
    pub trait Named {
        const NAME: &'static str;
    }
    impl Named for Minus {
        const NAME: &'static str = "minus";
    }
    impl Named for Plus {
        const NAME: &'static str = "plus";
    }
    pub struct Pair<A, B>(core::marker::PhantomData<(A, B)>);
    impl<A: Named, B: Named> Pair<A, B> {
        pub fn names(extra: &str) -> Result<String, String> {
            Ok(format!("{}{}{extra}", A::NAME, B::NAME))
        }
    }

    pub fn run(extra: &str) -> Result<usize, String> {
        let raw = Pair::<Tok![+], Tok![-]>::names(extra)?;
        if raw.len() == 1 { Ok(0) } else { Ok(raw.len()) }
    }
}

pub mod cfg_macro_arguments {
    pub fn run(extra: &str) -> Vec<String> {
        vec![
            #[cfg(unix)]
            "unix".parse().unwrap(),
            #[cfg(windows)]
            "windows".parse().unwrap(),
            extra.parse().unwrap(),
        ]
    }
}

/// A `move` closure that takes a struct apart: fields read, borrowed, emptied and moved out
/// one by one, a `Copy` field only compared, and a callback of the function called last.
pub mod move_closure_fields {
    use std::collections::BTreeMap;

    #[derive(Clone, Copy, PartialEq)]
    pub enum Separator {
        Undecided,
        Comma,
    }

    pub struct Evaluated {
        pub positional: Vec<u32>,
        pub named: BTreeMap<u32, u32>,
        pub separator: Separator,
        pub span: (u32, u32),
    }

    struct Visitor {
        seen: Vec<u32>,
    }

    type Outcome<T> = Result<T, String>;

    impl Visitor {
        fn with_scope<T, F: FnOnce(&mut Self) -> T>(&mut self, when: bool, callback: F) -> T {
            if !when {
                return callback(self);
            }
            self.seen.push(0);
            callback(self)
        }

        fn bump(&mut self, by: &u32) -> Result<u32, String> {
            self.seen.push(*by);
            if *by > 1000 { Err("too far".to_owned()) } else { Ok(*by * 2) }
        }

        fn with_environment<T>(&mut self, callback: impl FnOnce(&mut Self) -> T) -> T {
            self.seen.push(1);
            callback(self)
        }

        fn run_callable<F: Copy + Into<u32>, V, R: FnOnce(F, &mut Self) -> Result<V, String>>(
            &mut self,
            func: F,
            mut evaluated: Evaluated,
            run: R,
        ) -> Result<V, String> {
            self.with_environment(|visitor| {
                visitor.with_scope(true, move |visitor| {
                    verify(evaluated.positional.len(), &evaluated.named, evaluated.span)?;
                    let positional_len = evaluated.positional.len();
                    for i in (0..positional_len.min(2)).rev() {
                        visitor.seen.push(evaluated.positional.remove(i));
                    }
                    let all = [11u32, 12, 13];
                    let more = if positional_len > 1 { &all[1..all.len()] } else { &[] };
                    for one in more {
                        let value = evaluated.named.remove(one).map_or_else(
                            || {
                                let bumped = visitor.bump(one)?;
                                Ok(bumped + 1)
                            },
                            Outcome::Ok,
                        )?;
                        visitor.seen.push(value);
                    }
                    let value = evaluated
                        .named
                        .remove(&func.into())
                        .map_or_else(|| Ok(7), Result::<u32, String>::Ok)?;
                    visitor.seen.push(value);
                    let num_named = evaluated.named.len();
                    let has_rest = if positional_len > 2 {
                        let rest = if !evaluated.positional.is_empty() {
                            evaluated.positional
                        } else {
                            Vec::new()
                        };
                        visitor.seen.extend(rest);
                        visitor.seen.extend(evaluated.named.clone().into_values());
                        visitor.seen.push(if evaluated.separator == Separator::Undecided {
                            100
                        } else {
                            200
                        });
                        true
                    } else {
                        false
                    };
                    let val = run(func, visitor)?;
                    if !has_rest || num_named == 0 {
                        return Ok(val);
                    }
                    visitor.seen.push(300);
                    Ok(val)
                })
            })
        }
    }

    fn verify(len: usize, named: &BTreeMap<u32, u32>, span: (u32, u32)) -> Result<(), String> {
        if len + named.len() > span.1 as usize {
            Err(format!("too many at {}", span.0))
        } else {
            Ok(())
        }
    }

    pub fn run(positional: Vec<u32>, separator: Separator) -> Result<Vec<u32>, String> {
        let mut visitor = Visitor { seen: Vec::new() };
        let named = BTreeMap::from([(3, 30), (4, 40)]);
        let evaluated = Evaluated { positional, named, separator, span: (5, 6) };
        let extra = visitor.run_callable(3u8, evaluated, |func, visitor| {
            visitor.seen.push(func as u32);
            Ok(visitor.seen.len() as u32)
        })?;
        visitor.seen.push(extra);
        Ok(visitor.seen)
    }
}

#[cfg(target_arch = "x86_64")]
pub mod inline_asm {
    use std::arch::asm;

    fn add(a: u64, b: u64) -> u64 {
        let out: u64;
        unsafe {
            asm!("lea {out}, [{a} + {b}]", a = in(reg) a, b = in(reg) b, out = lateout(reg) out)
        };
        out
    }

    fn in_place(mut x: u32) -> u32 {
        unsafe { asm!("shl {0:e}, 3", "add {0:e}, {1}", inout(reg) x, const 5) };
        x
    }

    fn divide(n: u64, d: u64) -> (u64, u64) {
        let (quotient, remainder);
        unsafe {
            asm!(
                "div {d}",
                d = in(reg) d,
                inout("rax") n => quotient,
                inout("rdx") 0u64 => remainder,
                options(pure, nomem, nostack),
            )
        };
        (quotient, remainder)
    }

    fn vendor() -> [u8; 12] {
        let (b, c, d): (u32, u32, u32);
        unsafe {
            asm!(
                "mov {saved}, rbx",
                "cpuid",
                "xchg {saved}, rbx",
                saved = out(reg) b,
                inout("eax") 0 => _,
                out("ecx") c,
                out("edx") d,
            )
        };
        let mut name = [0; 12];
        name[..4].copy_from_slice(&b.to_le_bytes());
        name[4..8].copy_from_slice(&d.to_le_bytes());
        name[8..].copy_from_slice(&c.to_le_bytes());
        name
    }

    fn bytes(low: u8, high: u8) -> u16 {
        let out: u16;
        unsafe {
            asm!(
                "mov {out:l}, {low}",
                "mov {out:h}, {high}",
                low = in(reg_byte) low,
                high = in(reg_byte) high,
                out = out(reg_abcd) out,
            )
        };
        out
    }

    fn floats(a: f64, b: f64) -> (f64, f32) {
        let sum: f64;
        let mut single = a as f32;
        unsafe {
            asm!(
                "addsd {a}, {b}",
                "sqrtss {s}, {s}",
                a = inout(xmm_reg) a => sum,
                b = in(xmm_reg) b,
                s = inout(xmm_reg) single,
            )
        };
        (sum, single)
    }

    fn att(a: u64) -> u64 {
        let out: u64;
        unsafe {
            asm!("movq {0}, {1}", "addq $7, {1}", in(reg) a, out(reg) out, options(att_syntax))
        };
        out
    }

    /// Registers a function has to restore, and many operands at once.
    fn kept(values: [u64; 6]) -> u64 {
        let [a, b, c, d, e, f] = values;
        let out: u64;
        unsafe {
            asm!(
                "add {a}, {b}", "add {a}, {c}", "add {a}, {d}", "add {a}, {e}", "add {a}, {f}",
                "mov r12, {a}", "mov r13, r12", "mov r14, r13", "mov r15, r14",
                a = inout(reg) a => _, b = in(reg) b, c = in(reg) c, d = in(reg) d,
                e = in(reg) e, f = in(reg) f,
                out("r12") _, out("r13") _, out("r14") _, out("r15") out,
            )
        };
        out
    }

    fn memory(values: &mut [u32; 4]) -> u32 {
        let total: u32;
        unsafe {
            asm!(
                "mov {total:e}, [{p}]",
                "add {total:e}, [{p} + 4]",
                "mov dword ptr [{p} + 8], {total:e}",
                "/* a comment */",
                p = in(reg) values.as_mut_ptr(),
                total = out(reg) total,
            )
        };
        total + values[2]
    }

    fn stops(go: bool) -> u8 {
        if go {
            unsafe { asm!("ud2", options(noreturn)) }
        }
        unsafe { asm!("nop") };
        3
    }

    pub fn run() -> impl std::fmt::Debug {
        (
            add(40, 2),
            in_place(4),
            divide(47, 5),
            vendor().iter().all(u8::is_ascii_graphic),
            bytes(0x34, 0x12),
            floats(1.5, 2.25),
            att(35),
            kept([1, 2, 3, 4, 5, 6]),
            memory(&mut [10, 20, 0, 0]),
            stops(false),
        )
    }
}

#[cfg(target_arch = "aarch64")]
pub mod inline_asm {
    use std::arch::asm;

    fn add(a: u64, b: u64) -> u64 {
        let out: u64;
        unsafe {
            asm!("add {out}, {a}, {b}", a = in(reg) a, b = in(reg) b, out = lateout(reg) out)
        };
        out
    }

    fn in_place(mut x: u32) -> u32 {
        unsafe { asm!("lsl {0:w}, {0:w}, #3", "add {0:w}, {0:w}, #{1}", inout(reg) x, const 5) };
        x
    }

    fn divide(n: u64, d: u64) -> (u64, u64) {
        let (quotient, remainder);
        unsafe {
            asm!(
                "udiv x2, x0, {d}",
                "msub x1, x2, {d}, x0",
                d = in(reg) d,
                in("x0") n,
                out("x1") remainder,
                out("x2") quotient,
                options(pure, nomem, nostack),
            )
        };
        (quotient, remainder)
    }

    fn halves(low: u8, high: u8) -> u16 {
        let out: u16;
        unsafe {
            asm!(
                "and {out:w}, {low:w}, #0xff",
                "orr {out:w}, {out:w}, {high:w}, lsl #8",
                low = in(reg) low,
                high = in(reg) high,
                out = out(reg) out,
            )
        };
        out
    }

    fn floats(a: f64, b: f64) -> (f64, f32) {
        let sum: f64;
        let mut single = a as f32;
        unsafe {
            asm!(
                "fadd {a:d}, {a:d}, {b:d}",
                "fsqrt {s:s}, {s:s}",
                a = inout(vreg) a => sum,
                b = in(vreg) b,
                s = inout(vreg) single,
            )
        };
        (sum, single)
    }

    fn named_vector(a: f64) -> f64 {
        let out: f64;
        unsafe { asm!("fneg d1, d0", "fadd d1, d1, d1", in("v0") a, out("v1") out) };
        out
    }

    /// Registers a function has to restore, and many operands at once.
    fn kept(values: [u64; 6]) -> u64 {
        let [a, b, c, d, e, f] = values;
        let out: u64;
        unsafe {
            asm!(
                "add {a}, {a}, {b}", "add {a}, {a}, {c}", "add {a}, {a}, {d}",
                "add {a}, {a}, {e}", "add {a}, {a}, {f}",
                "mov x20, {a}", "mov x21, x20", "mov x22, x21", "mov x23, x22",
                "fmov d8, x23", "fmov x28, d8",
                a = inout(reg) a => _, b = in(reg) b, c = in(reg) c, d = in(reg) d,
                e = in(reg) e, f = in(reg) f,
                out("x20") _, out("x21") _, out("x22") _, out("x23") _, out("v8") _,
                out("x28") out,
            )
        };
        out
    }

    fn memory(values: &mut [u32; 4]) -> u32 {
        let total: u32;
        unsafe {
            asm!(
                "ldr {total:w}, [{p}]",
                "ldr {scratch:w}, [{p}, #4]",
                "add {total:w}, {total:w}, {scratch:w}",
                "str {total:w}, [{p}, #8]",
                "/* a comment */",
                p = in(reg) values.as_mut_ptr(),
                total = out(reg) total,
                scratch = out(reg) _,
            )
        };
        total + values[2]
    }

    /// The thread pointer, which nothing but an instruction reads.
    #[cfg(not(target_vendor = "apple"))]
    fn thread_pointer() -> bool {
        let (first, second): (u64, u64);
        unsafe {
            asm!("mrs {0}, tpidr_el0", out(reg) first, options(nomem, nostack, preserves_flags));
            asm!("mrs {0}, tpidr_el0", out(reg) second);
        }
        first != 0 && first == second
    }

    /// Apple keeps the thread pointer in the read-only register; the other one holds the
    /// number of the core the thread is on, which is zero on one core and changes.
    #[cfg(target_vendor = "apple")]
    fn thread_pointer() -> bool {
        let (first, second): (u64, u64);
        unsafe {
            asm!("mrs {0}, tpidrro_el0", out(reg) first, options(nomem, nostack, preserves_flags));
            asm!("mrs {0}, tpidrro_el0", out(reg) second);
        }
        first != 0 && first == second
    }

    fn stops(go: bool) -> u8 {
        if go {
            unsafe { asm!("brk #1", options(noreturn)) }
        }
        unsafe { asm!("nop") };
        3
    }

    /// An instruction of an extension the target's baseline lacks, which the function enables.
    #[target_feature(enable = "crc")]
    unsafe fn crc(state: u32, word: u32) -> u32 {
        let out: u32;
        unsafe {
            asm!("crc32w {0:w}, {1:w}, {2:w}", lateout(reg) out, in(reg) state, in(reg) word)
        };
        out
    }

    /// A register of a feature the assembler has no extension of, only an architecture
    /// version (constant_time_eq's).
    #[target_feature(enable = "dit")]
    unsafe fn dit() -> u64 {
        let value: u64;
        unsafe { asm!("mrs {}, dit", lateout(reg) value, options(nomem, preserves_flags, nostack)) };
        value
    }

    fn timing_independent() -> Option<u64> {
        std::arch::is_aarch64_feature_detected!("dit").then(|| unsafe { dit() })
    }

    pub fn run() -> impl std::fmt::Debug {
        (
            add(40, 2),
            in_place(4),
            divide(47, 5),
            halves(0x34, 0x12),
            floats(1.5, 2.25),
            named_vector(1.25),
            kept([1, 2, 3, 4, 5, 6]),
            memory(&mut [10, 20, 0, 0]),
            thread_pointer(),
            stops(false),
            unsafe { crc(!0, 0x6c6c_6568) },
            timing_independent(),
        )
    }
}

/// Constants that hold another constant's value: what that value points to is copied, and
/// the pointers in it have to follow, through a type that is written as an associated type
/// and through a union.
pub mod const_in_const {
    pub trait Marker {
        type Data: 'static;
    }
    pub struct Table {
        names: &'static [&'static str],
        bytes: *const [u8],
        count: u32,
    }
    unsafe impl Sync for Table {}
    pub struct Baked;
    impl Marker for Baked {
        type Data = Table;
    }
    impl Baked {
        pub const TABLE: &'static <Baked as Marker>::Data = &Table {
            names: &["one", "two", "three"],
            bytes: b"\x01\x02\x03\x04" as &[u8],
            count: 3,
        };
    }
    #[derive(Clone, Copy)]
    pub union Either {
        text: &'static str,
        number: u128,
    }
    pub struct Held {
        before: &'static [u16],
        table: &'static Table,
        either: Either,
        after: &'static str,
    }
    const fn padding() -> &'static [u16] {
        &[7, 8, 9, 10, 11, 12, 13]
    }
    const WORD: Either = Either { text: "a word behind a union" };
    const fn held() -> Held {
        let before = padding();
        Held { before, table: Baked::TABLE, either: WORD, after: "after" }
    }
    const HELD: Held = held();
    const AGAIN: (&[u16], Held) = (&[1, 2], held());

    #[inline(never)]
    fn read(held: &Held) -> (usize, String, Vec<u8>, u32, &'static str, &'static str) {
        let table = std::hint::black_box(held).table;
        (
            held.before.len(),
            table.names.concat(),
            unsafe { &*table.bytes }.to_vec(),
            table.count,
            unsafe { held.either.text },
            held.after,
        )
    }

    pub fn run() -> impl std::fmt::Debug {
        (read(&HELD), read(&AGAIN.1), AGAIN.0.len())
    }
}

/// A binding declared without a value takes a field out of what a match arm binds, as
/// `syn::braced!` does: the field is dropped once, where the binding goes out of scope.
pub mod moved_out_field {
    use std::cell::RefCell;

    struct Buffer<'a>(&'a RefCell<Vec<&'static str>>, &'static str);
    impl Drop for Buffer<'_> {
        fn drop(&mut self) {
            self.0.borrow_mut().push(self.1);
        }
    }
    struct Braces<'a> {
        token: u32,
        content: Buffer<'a>,
    }
    #[inline(never)]
    fn braces<'a>(log: &'a RefCell<Vec<&'static str>>, fails: bool) -> Result<Braces<'a>, u8> {
        if fails { Err(3) } else { Ok(Braces { token: 7, content: Buffer(log, "content") }) }
    }
    fn parse(log: &RefCell<Vec<&'static str>>, fails: bool) -> Result<u32, u8> {
        let content: Buffer;
        let token = match braces(log, fails) {
            Ok(braces) => {
                content = braces.content;
                _ = content;
                braces.token
            }
            Err(error) => return Err(error),
        };
        log.borrow_mut().push("parsed");
        Ok(token + content.1.len() as u32)
    }
    pub fn run() -> impl std::fmt::Debug {
        let log = RefCell::new(Vec::new());
        let (taken, failed) = (parse(&log, false), parse(&log, true));
        (taken, failed, log.into_inner())
    }
}

/// `type_name` of a function written in another's body, which is how a test names a component.
pub mod local_type_names {
    fn name_of<T>(_: T) -> &'static str {
        std::any::type_name::<T>()
    }
    pub fn run() -> impl std::fmt::Debug {
        fn inner() {}
        struct Local;
        let nested = {
            fn deeper() -> &'static str {
                fn deepest() {}
                name_of(deepest)
            }
            (name_of(deeper), deeper())
        };
        let in_closure = (|| {
            fn enclosed() {}
            let twice = || {
                struct Twice;
                std::any::type_name::<Twice>()
            };
            (name_of(enclosed), twice())
        })();
        (name_of(inner), std::any::type_name::<Local>(), nested, in_closure, name_of(run))
    }
}

/// Unsizing a value that holds its pointer somewhere other than first, as a hook's
/// `RefCell<Box<dyn FnMut>>` is made.
pub mod unsized_in_struct {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    pub fn run() -> impl std::fmt::Debug {
        let mut count = 3;
        let boxed: RefCell<Box<dyn FnMut() -> i32>> = RefCell::new(Box::new(move || {
            count += 1;
            count
        }));
        let first = (boxed.borrow_mut())();
        let cell: Cell<&[u8]> = Cell::new(&[1u8, 2, 3]);
        let shared: Rc<RefCell<Box<dyn Fn(i32) -> i32>>> =
            Rc::new(RefCell::new(Box::new(move |it| it * first)));
        let slices: RefCell<Box<[u16]>> = RefCell::new(Box::new([7u16, 8]));
        (
            first,
            (boxed.borrow_mut())(),
            cell.get().len(),
            (shared.borrow())(5),
            slices.borrow().len(),
        )
    }
}

/// `&(function as fn())` lives for `'static`, which a `Deref` to a function pointer returns.
pub mod promoted_fn_cast {
    use std::marker::PhantomData;
    use std::ops::Deref;

    struct Callable<T>(PhantomData<T>);
    impl<T: std::fmt::Debug + Default> Callable<T> {
        fn run(by: u32) -> String {
            format!("{:?} by {by}", T::default())
        }
    }
    impl<T: std::fmt::Debug + Default> Deref for Callable<T> {
        type Target = fn(u32) -> String;
        #[inline(never)]
        fn deref(&self) -> &Self::Target {
            &(Self::run as fn(u32) -> String)
        }
    }
    #[inline(never)]
    fn clobber() -> [u64; 16] {
        std::hint::black_box([0x5555_5555_5555_5555; 16])
    }
    pub fn run() -> impl std::fmt::Debug {
        let callable = Callable::<u8>(PhantomData);
        let function = callable.deref();
        let clobbered = clobber();
        (function(3), Callable::<bool>(PhantomData)(4), clobbered[3] as u8)
    }
}

/// What the `warnings` crate's attribute writes: a struct, an enum variant and a function of
/// one name, which a path names by where it stands.
pub mod same_name_callable {
    pub trait Warning: 'static {
        const ID: Id;
    }
    pub struct Id {
        name: fn() -> &'static str,
    }
    impl Id {
        pub const fn of<W: Warning + ?Sized>() -> Self {
            Self { name: std::any::type_name::<W> }
        }
        fn if_enabled(&self, f: impl FnOnce()) {
            if (self.name)().ends_with("hoisted") {
                f()
            }
        }
    }

    #[allow(non_camel_case_types)]
    pub struct hoisted {}
    mod __hoisted {
        use super::*;
        pub(crate) enum __Callable<T: 'static> {
            #[allow(non_camel_case_types)]
            hoisted,
            #[allow(dead_code)]
            PhantomData(std::marker::PhantomData<T>),
        }
        impl<T: 'static> __Callable<T> {
            fn __run_if_enabled(arg0: &std::cell::Cell<T>, arg1: T) {
                <hoisted as Warning>::ID.if_enabled(|| {
                    fn hoisted<T: 'static>(value: &std::cell::Cell<T>, with: T) {
                        value.set(with);
                    }
                    hoisted(arg0, arg1);
                });
            }
        }
        impl<T: 'static> std::ops::Deref for __Callable<T> {
            type Target = fn(&std::cell::Cell<T>, T);
            fn deref(&self) -> &Self::Target {
                &(Self::__run_if_enabled as fn(&std::cell::Cell<T>, T))
            }
        }
    }
    pub use __hoisted::__Callable::*;
    impl Warning for hoisted {
        const ID: Id = Id::of::<hoisted>();
    }

    pub fn run() -> impl std::fmt::Debug {
        let (number, text) = (std::cell::Cell::new(1u8), std::cell::Cell::new("before"));
        hoisted(&number, 9);
        hoisted(&text, "after");
        (number.get(), text.get())
    }
}

/// Two statics of one name in one function, the second inside a closure, as `#[instrument]`
/// around a `trace_span!` leaves them.
pub mod same_name_statics {
    struct Site {
        name: &'static str,
        id: &'static std::sync::atomic::AtomicUsize,
    }

    fn enter<R>(site: &'static Site, f: impl FnOnce() -> R) -> (R, &'static str, usize) {
        let was = site.id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        (f(), site.name, was)
    }

    pub fn run() -> impl std::fmt::Debug {
        let outer = {
            static COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(10);
            static SITE: Site = Site { name: "outer", id: &COUNT };
            &SITE
        };
        enter(outer, || {
            let inner = {
                static COUNT: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(20);
                static SITE: Site = Site { name: "inner", id: &COUNT };
                &SITE
            };
            (enter(inner, || 1), enter(inner, || 2), enter(outer, || 3))
        })
    }
}

/// A static in a method of a generic `impl` of a trait whose parameter is left at its default.
pub mod static_in_defaulted_impl {
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub trait Parse<P = ()>: Sized {
        fn parse(text: &str) -> (Option<Self>, usize);
    }

    pub struct Marker;

    impl<T: FromStr> Parse for T {
        fn parse(text: &str) -> (Option<Self>, usize) {
            static CALLS: AtomicUsize = AtomicUsize::new(0);
            (text.parse().ok(), CALLS.fetch_add(1, Ordering::Relaxed))
        }
    }

    impl<T: FromStr> Parse<Marker> for Option<T> {
        fn parse(text: &str) -> (Option<Self>, usize) {
            static CALLS: AtomicUsize = AtomicUsize::new(100);
            (Some(text.parse().ok()), CALLS.fetch_add(1, Ordering::Relaxed))
        }
    }

    pub fn run() -> impl std::fmt::Debug {
        (
            <u8 as Parse>::parse("7"),
            <i16 as Parse>::parse("x"),
            <Option<u8> as Parse<Marker>>::parse("3"),
            <Option<bool> as Parse<Marker>>::parse("true"),
        )
    }
}

/// A `#[track_caller]` method called through a vtable reports where it is declared.
pub mod track_caller_in_vtable {
    pub trait Here {
        fn here(&self, by: u32) -> (u32, u32, u32);
        #[track_caller]
        fn provided(&self) -> u32 {
            std::panic::Location::caller().line()
        }
    }

    pub struct Tracked(u32);
    pub struct Plain(u32);

    impl Here for Tracked {
        #[track_caller]
        fn here(&self, by: u32) -> (u32, u32, u32) {
            let caller = std::panic::Location::caller();
            (self.0 * by, caller.line(), caller.column())
        }
    }

    impl Here for Plain {
        fn here(&self, by: u32) -> (u32, u32, u32) {
            let caller = std::panic::Location::caller();
            (self.0 + by, caller.line(), caller.column())
        }
    }

    pub fn run() -> impl std::fmt::Debug {
        let all: [&dyn Here; 2] = [&Tracked(3), &Plain(4)];
        let through: Vec<_> = all.iter().map(|it| (it.here(5), it.provided())).collect();
        (through, Tracked(6).here(7), Tracked(1).provided())
    }
}

/// Values whose variant is told by a niche, made on one side of the crate boundary and read
/// on the other: the two sides must agree on which value of the niche means which variant.
pub mod niche_values {
    use std::ptr::NonNull;

    #[derive(Copy, Clone)]
    pub enum Position {
        Major,
        Minor,
        Patch,
        Pre,
        Build,
    }
    #[derive(Copy, Clone)]
    pub enum Op {
        Exact,
        Greater,
        GreaterEq,
        Less,
        LessEq,
        Tilde,
        Caret,
        Wildcard,
    }
    pub struct Prerelease {
        pub head: NonNull<u8>,
    }
    pub struct Comparator {
        pub op: Op,
        pub major: u64,
        pub minor: Option<u64>,
        pub patch: Option<u64>,
        pub pre: Prerelease,
    }
    pub enum ErrorKind {
        Empty,
        UnexpectedEnd(Position),
        UnexpectedChar(Position, char),
        UnexpectedCharAfter(Position, char),
        ExpectedCommaFound(Position, char),
        LeadingZero(Position),
        Overflow(Position),
        EmptySegment(Position),
        IllegalCharacter(Position),
        WildcardNotTheOnlyComparator(char),
        UnexpectedAfterWildcard,
        ExcessiveComparators,
    }
    pub struct Error {
        pub kind: ErrorKind,
    }
    /// What semver's parser of a comparator returns.
    pub type Parsed = Result<(Comparator, Position, &'static str), Error>;

    pub enum Signed {
        A = -3,
        B = -1,
        C = 2,
    }
    #[repr(i8)]
    pub enum Wrap {
        A = -128,
        B = 127,
        C = 5,
    }
    #[repr(u8)]
    pub enum High {
        A = 200,
        B = 255,
        C = 0,
    }
    pub enum Gap {
        A = 0,
        B = 1,
        C = 250,
        D = 255,
    }
    pub struct Two {
        pub a: Option<u64>,
        pub b: Option<u64>,
        pub c: bool,
        pub d: Position,
    }
    pub struct Ends {
        pub a: bool,
        pub b: u64,
        pub c: bool,
    }
    pub enum Data {
        A(u32, Position),
        B(u8),
        C,
    }
    pub enum Nested {
        X(Data),
        Y(Position),
        Z,
    }

    /// For each type: a value, that value under `depth` of two `Option`s, and the depth read
    /// back.
    macro_rules! probes {
        ($($value:ident $make:ident $depth:ident: $ty:ty = $it:expr;)*) => {$(
            pub fn $value() -> $ty {
                $it
            }
            pub fn $make(depth: u8) -> Option<Option<$ty>> {
                match depth {
                    0 => None,
                    1 => Some(None),
                    _ => Some(Some($value())),
                }
            }
            pub fn $depth(value: &Option<Option<$ty>>) -> u8 {
                match value {
                    None => 0,
                    Some(None) => 1,
                    Some(Some(_)) => 2,
                }
            }
        )*};
    }

    probes! {
        parsed_err make_parsed_err depth_parsed_err: Parsed =
            Err(Error { kind: ErrorKind::UnexpectedChar(Position::Pre, 'x') });
        parsed_ok make_parsed_ok depth_parsed_ok: Parsed = Ok((
            Comparator {
                op: Op::Caret,
                major: 1,
                minor: Some(2),
                patch: None,
                pre: Prerelease { head: NonNull::dangling() },
            },
            Position::Patch,
            "",
        ));
        error make_error depth_error: Error = Error { kind: ErrorKind::ExcessiveComparators };
        kind make_kind depth_kind: ErrorKind = ErrorKind::Overflow(Position::Build);
        signed make_signed depth_signed: Signed = Signed::B;
        wrap make_wrap depth_wrap: Wrap = Wrap::C;
        high make_high depth_high: High = High::B;
        gap make_gap depth_gap: Gap = Gap::C;
        two make_two depth_two: Two = Two { a: None, b: Some(7), c: true, d: Position::Minor };
        ends make_ends depth_ends: Ends = Ends { a: true, b: 9, c: false };
        data make_data depth_data: Data = Data::C;
        nested make_nested depth_nested: Nested = Nested::Y(Position::Pre);
        nested_data make_nested_data depth_nested_data: Nested = Nested::X(Data::B(3));
        pair make_pair depth_pair: (bool, Op) = (true, Op::Tilde);
        triple make_triple depth_triple: (Position, Op, char) = (Position::Pre, Op::Less, 'q');
        letter make_letter depth_letter: char = 'z';
        ordering make_ordering depth_ordering: std::cmp::Ordering = std::cmp::Ordering::Less;
        nonzero make_nonzero depth_nonzero: std::num::NonZeroI8 = std::num::NonZeroI8::MIN;
        flag make_flag depth_flag: bool = true;
        text make_text depth_text: String = String::from("text");
        boxed make_boxed depth_boxed: Box<u8> = Box::new(4);
    }

    pub fn is_ok(parsed: &Parsed) -> bool {
        parsed.is_ok()
    }
}

/// Small generic functions of the standard library, whose bodies are put where they are
/// called instead of compiled each as a function of its own.
pub mod inlined {
    use std::hint::black_box;
    use std::panic::{self, AssertUnwindSafe};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static DROPPED: AtomicUsize = AtomicUsize::new(0);

    struct Noisy(u32);

    impl Drop for Noisy {
        fn drop(&mut self) {
            DROPPED.fetch_add(self.0 as usize, Ordering::Relaxed);
        }
    }

    /// Callees that each refer to data of their own, several in one caller.
    pub fn constants<'a>(
        a: Option<u32>,
        b: Option<&'a str>,
        c: Option<char>,
    ) -> (u32, &'a str, char, bool, usize, &'static [u16]) {
        let empty: &[u16] = Default::default();
        (
            a.unwrap_or_default(),
            b.unwrap_or_default(),
            c.unwrap_or(char::REPLACEMENT_CHARACTER),
            b.is_some_and(str::is_empty),
            String::new().capacity() + Vec::<u64>::new().len(),
            empty,
        )
    }

    /// Pointer helpers, which are a cast or an addition each.
    pub fn pointers(values: &[u64], at: usize) -> (u64, bool, usize, u8) {
        let first = values.as_ptr();
        let bytes = first.cast::<u8>();
        let at = at % values.len();
        // SAFETY: `at` is within `values`.
        let (value, byte) = unsafe { (*first.add(at), *bytes.add(at)) };
        (value, first.is_null(), values[at..].len(), byte)
    }

    /// A value that is dropped when a callee that was inlined calls what unwinds.
    pub fn unwinds(n: u32) -> (Option<u32>, usize) {
        let before = DROPPED.load(Ordering::Relaxed);
        let got = panic::catch_unwind(AssertUnwindSafe(|| {
            let held = Noisy(n);
            let none: Vec<u32> = Vec::new();
            let got = none.first().copied().or((n % 2 == 0).then_some(n));
            let got = black_box(got).map(|it| it + held.0);
            got.unwrap_or_else(|| panic::resume_unwind(Box::new(n)))
        }));
        (got.ok(), DROPPED.load(Ordering::Relaxed) - before)
    }

    /// Results that come back in memory and results of no size.
    pub fn shapes(n: u64) -> ([u64; 5], (), Option<[u8; 40]>, std::cmp::Ordering) {
        let wide = std::array::from_fn(|at| n.wrapping_mul(at as u64 + 1));
        let unit = Some(()).unwrap_or_default();
        let bytes = (n % 3 != 0).then(|| [n as u8; 40]);
        (wide, unit, bytes, n.cmp(&7).reverse().then(n.cmp(&3)))
    }

    /// The same small callee with different types in one caller.
    pub fn instances(a: u8, b: i64, c: &str) -> (u8, i64, String, usize, usize) {
        let (mut x, mut y, mut z) = (a, b, c.to_owned());
        let (was_x, was_y, was_z) =
            (std::mem::replace(&mut x, 1), std::mem::take(&mut y), std::mem::take(&mut z));
        std::mem::swap(&mut x, &mut black_box(9));
        (was_x.max(x), was_y.min(y), was_z, size_of_val(&x) + size_of_val(&y), z.len())
    }
}

/// An `impl Trait` whose code is a trait method's `impl Trait`, as through
/// `impl<T: RenderOnce> View for T`: the type is the impl's once the types are known.
pub mod opaque_of_trait_method {
    pub trait Value {
        fn get(self) -> u32;
    }

    impl Value for u32 {
        fn get(self) -> u32 {
            self
        }
    }

    impl Value for (u32, u8) {
        fn get(self) -> u32 {
            self.0 * 1000 + u32::from(self.1)
        }
    }

    pub trait RenderOnce {
        fn render(self) -> impl Value;
    }

    pub trait View {
        fn render(self) -> impl Value;
    }

    impl<T: RenderOnce> View for T {
        fn render(self) -> impl Value {
            RenderOnce::render(self)
        }
    }

    pub struct Well(pub u32);

    impl RenderOnce for Well {
        fn render(self) -> impl Value {
            self.0 + 1
        }
    }

    pub struct Pair(pub u32);

    impl RenderOnce for Pair {
        fn render(self) -> impl Value {
            (self.0, 7u8)
        }
    }

    fn layout<V: View>(view: V) -> u32 {
        view.render().get()
    }

    fn rendered<T: RenderOnce>(it: T) -> impl Value {
        it.render()
    }

    fn wrapped<T: RenderOnce>(it: T) -> Option<impl Value> {
        Some(it.render())
    }

    pub fn run(n: u32) -> (u32, u32, u32, u32, Option<u32>) {
        (
            layout(Well(n)),
            layout(Pair(n)),
            rendered(Well(n)).get(),
            rendered(Pair(n)).get(),
            wrapped(Well(n)).map(Value::get),
        )
    }
}

/// A `const` item whose value passes a borrowed array of `const fn` calls to a `const fn`:
/// rustc promotes the array, as `embed_migrations!` relies on.
pub mod const_fn_promoted {
    pub struct Migration {
        pub name: &'static str,
        pub down: Option<&'static str>,
    }

    impl Migration {
        pub const fn new(name: &'static str, down: Option<&'static str>) -> Self {
            Self { name, down }
        }
    }

    pub struct Migrations {
        pub all: &'static [Migration],
    }

    impl Migrations {
        pub const fn new(all: &'static [Migration]) -> Self {
            Migrations { all }
        }
    }

    pub const MIGRATIONS: Migrations =
        Migrations::new(&[Migration::new("a", Some("b")), Migration::new("c", None)]);

    pub fn run() -> usize {
        MIGRATIONS.all.iter().filter(|it| it.down.is_some()).count() + MIGRATIONS.all[1].name.len()
    }
}

/// A `match` used as a statement whose arms borrow a temporary: the temporary is dropped where
/// its arm ends, and nothing reads the borrow after.
pub mod statement_borrows_temporary {
    pub fn run(flag: bool, value: &Option<String>) -> usize {
        match value {
            None => &String::new(),
            Some(it) => it,
        };
        match (value, flag) {
            (None, _) => &value.clone().unwrap_or_default(),
            (&Some(ref it), _) => it,
        };
        value.as_ref().map_or(0, String::len)
    }
}

/// Temporaries borrowed by a tuple struct's or variant's constructor arguments in a `let`
/// live as long as the variable (Rust 1.89), not through a constructor held in a variable.
pub mod constructor_extends_temporaries {
    use std::cell::RefCell;

    pub struct Loud<'a>(pub u8, pub &'a RefCell<Vec<u8>>);

    impl Drop for Loud<'_> {
        fn drop(&mut self) {
            self.1.borrow_mut().push(self.0);
        }
    }

    pub struct Pair<'a, 'b>(pub &'a Loud<'b>, pub u8);

    pub fn run() -> Vec<u8> {
        let log = RefCell::new(Vec::new());
        {
            let a = Some(&Loud(1, &log));
            let b = Pair(&Loud(2, &log), 3);
            let some = Some;
            let c = some(Loud(4, &log).0);
            log.borrow_mut().push(10 + a.map_or(0, |it| it.0) + b.0.0 + b.1 + c.unwrap_or(0));
        }
        log.into_inner()
    }
}

/// What a signature's associated type is, related to the type it normalizes to: an impl's
/// elided lifetimes in `Self::Target`, and an alias that writes one elided lifetime twice.
pub mod associated_type_returns {
    use std::marker::PhantomData;
    use std::ops::{Deref, DerefMut};

    pub struct Path<'a> {
        pub parts: Vec<&'a str>,
    }

    pub struct Scope<'whole, 'part> {
        inner: &'whole mut Path<'part>,
    }

    impl<'a> Deref for Scope<'_, 'a> {
        type Target = Path<'a>;
        fn deref(&self) -> &Self::Target {
            self.inner
        }
    }

    impl DerefMut for Scope<'_, '_> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            self.inner
        }
    }

    #[derive(Clone, Copy)]
    pub struct Pair<'a, 'b: 'a> {
        marker: PhantomData<&'a &'b ()>,
        pub value: Option<&'b u32>,
    }

    pub type Same<'a> = Pair<'a, 'a>;

    impl<'a, 'b> Deref for Pair<'a, 'b> {
        type Target = Option<&'b u32>;
        fn deref(&self) -> &Option<&'b u32> {
            &self.value
        }
    }

    pub fn value(pair: Same) -> Option<&u32> {
        *pair
    }

    pub fn run(words: &[String]) -> usize {
        let mut path = Path { parts: Vec::new() };
        let mut scope = Scope { inner: &mut path };
        for word in words {
            scope.parts.push(word);
        }
        let mut total = 0;
        for part in &mut scope.parts {
            total += part.len();
        }
        let four = 4;
        let pair: Same = Pair { marker: PhantomData, value: Some(&four) };
        total + value(pair).copied().unwrap_or(0) as usize
    }
}

/// Raw pointers related as rustc relates them: coerced from references, compared through a
/// common supertype, an array's pointer cast to its element's, and a trait object's lifetime
/// shortened.
pub mod pointer_relations {
    use std::fmt::Debug;

    pub fn same<'a, 'b>(x: *const &'a i32, y: *const &'b i32) -> bool {
        x == y
    }

    pub fn first<'a>(items: &[&'a i32; 3]) -> *const &'a i32 {
        items as *const &i32
    }

    pub fn shorten<'a, 'b>(x: *const (dyn Debug + 'a)) -> *const (dyn Debug + 'b)
    where
        'a: 'b,
    {
        x as _
    }

    pub fn run(value: i32) -> String {
        let x = &value;
        let items = [x, x, x];
        let pointer = first(&items);
        let shown: &dyn Debug = &value;
        let shortened = shorten(shown);
        // SAFETY: both point into `items` and `value`, alive for the whole function.
        let (first, shown) = unsafe { (**pointer, format!("{:?}", &*shortened)) };
        format!("{} {} {}", same(pointer, pointer), first, shown)
    }
}

/// Statics read and written with what lives as long as they do, and a `'static` function
/// object called through a static.
pub mod static_places {
    static BASE: u8 = 40;
    static mut LATEST: &u8 = &BASE;
    static ADD: &(dyn Fn(&'static u8) -> u8 + Sync) = &|it| *it + 2;

    pub fn run() -> u8 {
        static TWO: u8 = 2;
        // SAFETY: only this function touches `LATEST`, on one thread.
        let latest = unsafe {
            LATEST = &TWO;
            *LATEST
        };
        ADD(&BASE) + latest
    }
}

/// What bevy's `#[derive(QueryData)]` writes for a read-only query: a `release_state` under
/// `where` clauses on the `ReadOnly` of each field's type, whose locals are of
/// `<<F as QueryData>::ReadOnly as QueryData>::Item<'w, 'static>`. The solver can't decide
/// that alias of a tuple's under those clauses, and the drop passes asked whether a variable
/// has drop glue (#96). `Loud` is behind such an alias, and its drops are counted.
pub mod undecided_alias_drop {
    use std::marker::PhantomData;
    use std::sync::atomic::{AtomicU32, Ordering};

    static DROPS: AtomicU32 = AtomicU32::new(0);

    pub trait WorldQuery {
        type State;
    }

    pub unsafe trait QueryData: WorldQuery {
        type ReadOnly: ReadOnlyQueryData<State = <Self as WorldQuery>::State>;
        type Item<'w, 's>;
    }

    pub unsafe trait IterQueryData: QueryData {}

    pub unsafe trait ReadOnlyQueryData: IterQueryData<ReadOnly = Self> {}

    pub trait ReleaseStateQueryData: QueryData {
        fn release_state<'w>(item: Self::Item<'w, '_>) -> Self::Item<'w, 'static>;
    }

    pub trait Component: 'static {}

    impl<T: Component> WorldQuery for &T {
        type State = u8;
    }

    unsafe impl<T: Component> QueryData for &T {
        type ReadOnly = Self;
        type Item<'w, 's> = &'w T;
    }

    unsafe impl<T: Component> IterQueryData for &T {}

    unsafe impl<T: Component> ReadOnlyQueryData for &T {}

    impl<T: Component> ReleaseStateQueryData for &T {
        fn release_state<'w>(item: Self::Item<'w, '_>) -> Self::Item<'w, 'static> {
            item
        }
    }

    impl<T: WorldQuery> WorldQuery for Option<T> {
        type State = T::State;
    }

    unsafe impl<T: QueryData> QueryData for Option<T> {
        type ReadOnly = Option<T::ReadOnly>;
        type Item<'w, 's> = Option<T::Item<'w, 's>>;
    }

    unsafe impl<T: IterQueryData> IterQueryData for Option<T> {}

    unsafe impl<T: ReadOnlyQueryData> ReadOnlyQueryData for Option<T> {}

    impl<T: ReleaseStateQueryData> ReleaseStateQueryData for Option<T> {
        fn release_state<'w>(item: Self::Item<'w, '_>) -> Self::Item<'w, 'static> {
            item.map(T::release_state)
        }
    }

    impl<A: WorldQuery, B: WorldQuery> WorldQuery for (A, B) {
        type State = (A::State, B::State);
    }

    unsafe impl<A: QueryData, B: QueryData> QueryData for (A, B) {
        type ReadOnly = (A::ReadOnly, B::ReadOnly);
        type Item<'w, 's> = (A::Item<'w, 's>, B::Item<'w, 's>);
    }

    unsafe impl<A: IterQueryData, B: IterQueryData> IterQueryData for (A, B) {}

    unsafe impl<A: ReadOnlyQueryData, B: ReadOnlyQueryData> ReadOnlyQueryData for (A, B) {}

    impl<A: ReleaseStateQueryData, B: ReleaseStateQueryData> ReleaseStateQueryData for (A, B) {
        fn release_state<'w>(item: Self::Item<'w, '_>) -> Self::Item<'w, 'static> {
            (A::release_state(item.0), B::release_state(item.1))
        }
    }

    pub struct Loud(pub u32);

    impl Drop for Loud {
        fn drop(&mut self) {
            DROPS.fetch_add(self.0, Ordering::Relaxed);
        }
    }

    pub struct Owned;

    impl WorldQuery for Owned {
        type State = ();
    }

    unsafe impl QueryData for Owned {
        type ReadOnly = Self;
        type Item<'w, 's> = Loud;
    }

    unsafe impl IterQueryData for Owned {}

    unsafe impl ReadOnlyQueryData for Owned {}

    impl ReleaseStateQueryData for Owned {
        fn release_state<'w>(item: Self::Item<'w, '_>) -> Self::Item<'w, 'static> {
            Loud(item.0 + 1)
        }
    }

    pub struct Clip(pub u32);

    impl Component for Clip {}

    pub struct ReadOnlyQuery;

    pub struct ReadOnlyQueryItem<'w, 's> {
        pub clip: <<Option<&'static Clip> as QueryData>::ReadOnly as QueryData>::Item<'w, 's>,
        pub owned:
            <<Option<(&'static Clip, Owned)> as QueryData>::ReadOnly as QueryData>::Item<'w, 's>,
        pub marker: PhantomData<(&'w (), &'s ())>,
    }

    impl WorldQuery for ReadOnlyQuery {
        type State = ();
    }

    unsafe impl QueryData for ReadOnlyQuery {
        type ReadOnly = Self;
        type Item<'w, 's> = ReadOnlyQueryItem<'w, 's>;
    }

    unsafe impl IterQueryData for ReadOnlyQuery {}

    unsafe impl ReadOnlyQueryData for ReadOnlyQuery {}

    impl ReleaseStateQueryData for ReadOnlyQuery
    where
        for<'a> Option<&'static Clip>: QueryData<ReadOnly: ReleaseStateQueryData>,
        for<'a> Option<(&'static Clip, Owned)>: QueryData<ReadOnly: ReleaseStateQueryData>,
    {
        fn release_state<'w>(item: Self::Item<'w, '_>) -> Self::Item<'w, 'static> {
            ReadOnlyQueryItem {
                clip: <<Option<&'static Clip> as QueryData>::ReadOnly>::release_state(item.clip),
                owned: <<Option<(&'static Clip, Owned)> as QueryData>::ReadOnly>::release_state(
                    item.owned,
                ),
                marker: PhantomData,
            }
        }
    }

    pub fn run(n: u32) -> (Option<u32>, Option<(u32, u32)>, u32) {
        DROPS.store(0, Ordering::Relaxed);
        let clip = Clip(n * 2);
        let owned = Some((&clip, Loud(n)));
        let item = ReadOnlyQueryItem { clip: Some(&clip), owned, marker: PhantomData };
        let released = <ReadOnlyQuery as ReleaseStateQueryData>::release_state(item);
        let seen =
            (released.clip.map(|it| it.0), released.owned.as_ref().map(|it| (it.0.0, it.1.0)));
        drop(released);
        (seen.0, seen.1, DROPS.load(Ordering::Relaxed))
    }
}

/// Bytes compared with needles of a few bytes whose number is known, as LLVM writes them in
/// loads, and the order of short byte arrays, which keeps the sign `memcmp` gives.
pub mod byte_compares {
    use core::cmp::Ordering;

    pub fn run(text: &[u8]) -> Vec<(usize, u8)> {
        let needles: [&[u8]; 6] = [b"/", b"//", b"/**", b"\\\"", b"<!--", b"\"\"\""];
        let mut found = Vec::new();
        for at in 0..text.len() {
            let window = &text[at..];
            let mut which = 0;
            for (bit, needle) in needles.iter().enumerate() {
                if window.starts_with(needle) {
                    which |= 1 << bit;
                }
            }
            if which != 0 {
                found.push((at, which));
            }
        }
        found
    }

    pub fn order(a: [u8; 3], b: [u8; 3], c: &[u8], d: &[u8]) -> (Ordering, Ordering, bool, bool) {
        (a.cmp(&b), c[..2].cmp(&d[..2]), a == b, c[..1] == d[..1])
    }
}

/// Statics of `&dyn` of other statics, read by value in another static's, as rustls's cipher
/// suites are: each was evaluated with vtables of its own.
pub mod dyn_static_suites {
    pub trait Named: Sync {
        fn name(&self) -> String;
    }
    pub struct Hash(pub u32);
    impl Named for Hash {
        fn name(&self) -> String {
            format!("sha{}", self.0)
        }
    }
    pub struct Kdf(pub &'static dyn Named, pub &'static str);
    impl Named for Kdf {
        fn name(&self) -> String {
            format!("{}({})", self.1, self.0.name())
        }
    }
    pub struct Suite {
        pub hash: &'static dyn Named,
        pub kdf: &'static dyn Named,
        pub id: u16,
    }
    #[derive(Clone, Copy)]
    pub enum Supported {
        Tls12(&'static Suite),
        Tls13(&'static Suite),
    }
    pub static SHA256: Hash = Hash(256);
    pub static SHA384: Hash = Hash(384);
    static AES_128_INTERNAL: &Suite =
        &Suite { hash: &SHA256, kdf: &Kdf(&SHA256, "hkdf"), id: 0x1301 };
    pub static AES_128: Supported = Supported::Tls13(AES_128_INTERNAL);
    pub static ECDHE: Supported =
        Supported::Tls12(&Suite { hash: &SHA384, kdf: &Kdf(&SHA384, "prf"), id: 0xc030 });
    pub static ALL: &[Supported] = &[AES_128, ECDHE];

    pub fn run() -> Vec<String> {
        ALL.iter()
            .map(|suite| {
                let (version, suite) = match suite {
                    Supported::Tls12(it) => (12, it),
                    Supported::Tls13(it) => (13, it),
                };
                format!("{version} {:#x} {} {}", suite.id, suite.hash.name(), suite.kdf.name())
            })
            .collect()
    }
}

/// A future that a provided method of a trait returns as `impl Future`, from another method of
/// the trait that returns one and that a blanket impl defines with an `async` block, held
/// across an `.await` of an `async fn` and of an `async` block (rmcp's `ServiceExt::serve`).
pub mod provided_futures {
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    pub trait Role {
        type Error: std::fmt::Debug;
    }
    pub struct Server;
    impl Role for Server {
        type Error = String;
    }
    pub trait Service<R: Role> {
        fn id(&self) -> u32;
    }
    pub struct Running<R: Role, S: Service<R>> {
        service: S,
        role: std::marker::PhantomData<R>,
        steps: u32,
    }
    impl<R: Role, S: Service<R>> Running<R, S> {
        pub async fn waiting(self) -> Result<u32, R::Error> {
            Ok(self.service.id() * 10 + self.steps)
        }
    }
    pub trait IntoTransport<R, A> {
        fn steps(self) -> u32;
    }
    impl<R: Role> IntoTransport<R, (u8, u16)> for (u8, u16) {
        fn steps(self) -> u32 {
            u32::from(self.0) + u32::from(self.1)
        }
    }
    pub trait ServiceExt<R: Role>: Service<R> + Sized {
        fn serve<T, A>(
            self,
            transport: T,
        ) -> impl Future<Output = Result<Running<R, Self>, R::Error>>
        where
            T: IntoTransport<R, A>,
        {
            Self::serve_with(self, transport, 1)
        }
        fn serve_with<T, A>(
            self,
            transport: T,
            extra: u32,
        ) -> impl Future<Output = Result<Running<R, Self>, R::Error>>
        where
            T: IntoTransport<R, A>;
    }
    impl<R: Role, S: Service<R>> ServiceExt<R> for S {
        fn serve_with<T, A>(
            self,
            transport: T,
            extra: u32,
        ) -> impl Future<Output = Result<Running<R, Self>, R::Error>>
        where
            T: IntoTransport<R, A>,
        {
            let steps = transport.steps() + extra;
            async move { Ok(Running { service: self, role: std::marker::PhantomData, steps }) }
        }
    }
    pub struct Shell(pub u32);
    impl Service<Server> for Shell {
        fn id(&self) -> u32 {
            self.0
        }
    }

    async fn run_stdio(shell: Shell) -> Result<u32, String> {
        let running = shell.serve((3u8, 4u16)).await?;
        running.waiting().await
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
                return value;
            }
        }
    }

    pub fn run(id: u32) -> Result<u32, String> {
        block_on(async {
            let result = run_stdio(Shell(id)).await;
            result.map(|it| it + 1)
        })
    }
}

/// Tuple structs whose fields are seen in fewer modules than the struct, both `pub(in path)`
/// (ring's `aes::Counter` and `gcm::Xi`), built and taken apart through a re-export: the
/// constructor is seen where both are.
pub mod narrow_constructors {
    pub mod aead {
        pub mod aes {
            mod ffi {
                pub(in super::super) struct Counter(pub(super) [u8; 4]);
            }
            pub(super) use ffi::Counter;

            pub(super) fn first(iv: [u8; 4]) -> Counter {
                let mut counter = Counter(iv);
                counter.0[3] = counter.0[3].wrapping_add(1);
                counter
            }

            pub(super) fn bytes(Counter(bytes): &Counter) -> [u8; 4] {
                *bytes
            }
        }
        pub mod gcm {
            mod ffi {
                pub(in super::super) struct Xi(pub(super) [u8; 4]);
            }
            pub(super) use ffi::Xi;
            pub(super) mod fallback {
                use super::Xi;

                pub(in super::super) fn swapped(Xi(xi): &mut Xi) -> u32 {
                    xi.reverse();
                    u32::from_be_bytes(*xi)
                }
            }

            pub(super) fn new(bytes: [u8; 4]) -> Xi {
                Xi(bytes)
            }
        }

        pub fn run() -> ([u8; 4], u32) {
            let counter = aes::first([1, 2, 3, 255]);
            let mut xi = gcm::new([9, 8, 7, 6]);
            (aes::bytes(&counter), gcm::fallback::swapped(&mut xi))
        }
    }
}

/// AArch64's SHA and AES instructions, sums of every lane made wider and twice products made
/// wider; AVX-512's loads and stores of the lanes a mask has, and sums of every lane.
#[rustfmt::skip]
pub mod crypto_lanes {
    #[cfg(target_arch = "aarch64")]
    use core::arch::aarch64::*;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::*;

    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "sha2")]
    unsafe fn sha2(a: [u32; 4], b: [u32; 4], c: [u32; 4]) -> [[u32; 4]; 6] { unsafe {
        let (a, b, c) = (vld1q_u32(a.as_ptr()), vld1q_u32(b.as_ptr()), vld1q_u32(c.as_ptr()));
        [vsha256hq_u32(a, b, c), vsha256h2q_u32(a, b, c), vsha256su0q_u32(a, b), vsha256su1q_u32(a, b, c),
         vsha1su0q_u32(a, b, c), vsha1su1q_u32(a, b)].map(|v| core::mem::transmute::<uint32x4_t, [u32; 4]>(v))
    } }
    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "sha3")]
    unsafe fn sha3(a: [u64; 2], b: [u64; 2], c: [u64; 2]) -> [[u64; 2]; 4] { unsafe {
        let (a, b, c) = (vld1q_u64(a.as_ptr()), vld1q_u64(b.as_ptr()), vld1q_u64(c.as_ptr()));
        [vsha512hq_u64(a, b, c), vsha512h2q_u64(a, b, c), vsha512su0q_u64(a, b), vsha512su1q_u64(a, b, c)]
            .map(|v| core::mem::transmute::<uint64x2_t, [u64; 2]>(v))
    } }
    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "aes")]
    unsafe fn aes(a: [u8; 16], b: [u8; 16]) -> [[u8; 16]; 4] { unsafe {
        let (a, b) = (vld1q_u8(a.as_ptr()), vld1q_u8(b.as_ptr()));
        [vaeseq_u8(a, b), vaesdq_u8(a, b), vaesmcq_u8(a), vaesimcq_u8(b)]
            .map(|v| core::mem::transmute::<uint8x16_t, [u8; 16]>(v))
    } }
    #[cfg(target_arch = "aarch64")]
    fn lanes(a: [u8; 16], b: [u8; 16]) -> (u16, i16, u16, u8, [i32; 4], [i64; 2]) { unsafe {
        let (v, w) = (vld1q_u8(a.as_ptr()), vld1q_u8(b.as_ptr()));
        let (s, t) = (vreinterpretq_s16_u8(v), vsetq_lane_s16::<1>(i16::MIN, vreinterpretq_s16_u8(w)));
        let s = vsetq_lane_s16::<1>(i16::MIN, s);
        let (x, y) = (vreinterpretq_s32_u8(v), vsetq_lane_s32::<0>(i32::MIN, vreinterpretq_s32_u8(w)));
        let x = vsetq_lane_s32::<0>(i32::MIN, x);
        (vaddlvq_u8(v), vaddlvq_s8(vreinterpretq_s8_u8(w)), vaddlv_u8(vget_low_u8(w)), vaddv_u8(vget_high_u8(v)),
         core::mem::transmute(vqdmull_s16(vget_low_s16(s), vget_low_s16(t))),
         core::mem::transmute(vqdmull_s32(vget_low_s32(x), vget_low_s32(y))))
    } }
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx512bw")]
    unsafe fn masked(bytes: &[u8; 64], mask: u64) -> ([u8; 64], [u8; 64], i32, i64) { unsafe {
        let loaded = _mm512_mask_loadu_epi8(_mm512_set1_epi8(-1), mask, bytes.as_ptr() as *const i8);
        let mut stored = [7u8; 64];
        _mm512_mask_storeu_epi8(stored.as_mut_ptr() as *mut i8, mask, loaded);
        let sum = _mm512_reduce_add_epi32(loaded);
        let product = _mm512_reduce_mul_epi64(_mm512_srli_epi64(loaded, 60));
        (core::mem::transmute(loaded), stored, sum, product)
    } }

    pub fn run() -> String {
        #[cfg_attr(not(any(target_arch = "aarch64", target_arch = "x86_64")), allow(unused_mut))]
        let mut out = String::new();
        #[cfg(target_arch = "aarch64")]
        {
            let a: [u8; 16] = core::array::from_fn(|i| (i as u8).wrapping_mul(29) ^ 0xa5);
            let b: [u8; 16] = core::array::from_fn(|i| (i as u8).wrapping_mul(71).wrapping_add(3));
            let words = |x: [u8; 16]| -> [u32; 4] { core::array::from_fn(|i| u32::from_le_bytes(x[i * 4..i * 4 + 4].try_into().unwrap())) };
            let longs = |x: [u8; 16]| -> [u64; 2] { core::array::from_fn(|i| u64::from_le_bytes(x[i * 8..i * 8 + 8].try_into().unwrap())) };
            out += &format!("{:?}", lanes(a, b));
            if std::arch::is_aarch64_feature_detected!("sha2") { out += &format!(" {:?}", unsafe { sha2(words(a), words(b), words(a).map(|w| w.rotate_left(7))) }); }
            if std::arch::is_aarch64_feature_detected!("sha3") { out += &format!(" {:?}", unsafe { sha3(longs(a), longs(b), longs(b).map(|w| w.rotate_left(13))) }); }
            if std::arch::is_aarch64_feature_detected!("aes") { out += &format!(" {:?}", unsafe { aes(a, b) }); }
        }
        #[cfg(target_arch = "x86_64")]
        if is_x86_feature_detected!("avx512bw") {
            let bytes: [u8; 64] = core::array::from_fn(|i| (i as u8).wrapping_mul(37) ^ 0x3c);
            out += &format!("{:?}", unsafe { masked(&bytes, 0xf0f0_1234_8000_ffa5) });
        }
        out
    }
}

/// A `$(..)?` repeats as often as the variables in it do, also when they were matched outside
/// it: a getter per field and a setter for the fields that name one, as bevy's
/// `impl_atomic_pod!` makes them.
pub mod optional_repeats_per_field {
    macro_rules! accessors {
        ($blob:ident $(, field($index:literal : $ty:ty, $getter:ident $(, $($setter:ident)?)?))*) => {
            pub struct $blob(pub [u32; 3]);

            impl $blob {
                $(
                    $(
                        pub fn $getter(&self) -> $ty {
                            self.0[$index] as $ty
                        }

                        $(
                            pub fn $setter(&mut self, value: $ty) {
                                self.0[$index] = value as u32;
                            }
                        )?
                    )*
                )?
            }
        };
    }

    accessors!(Blob, field(0: u32, tag, set_tag), field(1: u8, kind), field(2: u16, flags, set_flags));

    pub fn run(n: u32) -> (u32, u16, [u32; 3]) {
        let mut blob = Blob([n, 9, 0]);
        blob.set_tag(n + 1);
        blob.set_flags(n as u16 * 3);
        (blob.tag(), blob.flags(), blob.0)
    }
}

/// Whether a type deep in a cycle of `Arc`s, `Weak`s and locks is `Send` takes as many nested
/// goals as rustc's recursion limit allows once a `Vec`'s capacity, a constant, is evaluated on
/// the way. The trait solver must not then prove the goals above it again, or it never meets the
/// cycle and overflows into an ambiguity: bevy's `in_set` over wgpu's handles.
pub mod deep_send_cycle {
    use core::ops::{Deref, DerefMut};
    use std::sync::{Arc, Mutex, RwLock, Weak};

    pub struct Sent<T: Send>(pub T);

    impl<T: Send> Deref for Sent<T> {
        type Target = T;
        fn deref(&self) -> &T {
            &self.0
        }
    }

    impl<T: Send> DerefMut for Sent<T> {
        fn deref_mut(&mut self) -> &mut T {
            &mut self.0
        }
    }

    macro_rules! ring {
        ($($name:ident => $next:ident, $later:ident),*) => {
            $(
                pub struct $name {
                    pub next: Mutex<Option<Arc<$next>>>,
                    pub first: Weak<Ring0>,
                    pub all: RwLock<Vec<Arc<$later>>>,
                }
            )*
        };
    }

    ring!(
        Ring0 => Ring1, Ring2,
        Ring1 => Ring2, Ring3,
        Ring2 => Ring3, Ring4,
        Ring3 => Ring4, Ring5,
        Ring4 => Ring5, Ring6,
        Ring5 => Ring6, Ring7,
        Ring6 => Ring7, Ring8,
        Ring7 => Ring8, Ring9,
        Ring8 => Ring9, Ring10,
        Ring9 => Ring10, Ring11,
        Ring10 => Ring11, Ring12,
        Ring11 => Ring12, Ring13,
        Ring12 => Ring13, Ring14,
        Ring13 => Ring14, Ring15,
        Ring14 => Ring15, Ring0,
        Ring15 => Ring0, Ring1
    );

    pub fn run(n: usize) -> (usize, usize) {
        let ring =
            || Ring0 { next: Mutex::new(None), first: Weak::new(), all: RwLock::new(Vec::new()) };
        let mut sent = Sent((0..n).map(|_| ring()).collect::<Vec<_>>());
        let taken = core::mem::take(&mut *sent);
        (taken.len(), sent.len())
    }
}

/// Values built in the local a function returns, which code generation puts in the caller's
/// destination: what the callee reads while building and what the caller keeps after a panic
/// are not that memory.
pub mod returned_in_place {
    use std::panic::{self, AssertUnwindSafe};

    #[derive(Debug, Clone, PartialEq)]
    pub struct Big {
        pub words: [u64; 6],
        pub name: String,
    }

    #[inline(never)]
    fn build(seed: u64) -> Big {
        let mut big = Big { words: [seed; 6], name: String::new() };
        for (at, word) in big.words.iter_mut().enumerate() {
            *word = word.wrapping_mul(at as u64 + 3);
        }
        big.name.push_str("built");
        big
    }

    /// `chain = next(&chain)` assigns through a temporary, so `from` is not the destination.
    #[inline(never)]
    fn next(from: &Big) -> Big {
        let mut big = Big { words: [0; 6], name: String::from("next") };
        for at in 0..6 {
            big.words[at] = from.words[5 - at] + big.words[at.saturating_sub(1)];
        }
        big.name.push_str(&from.name);
        big
    }

    #[inline(never)]
    fn early(limit: u64) -> Big {
        let mut big = build(limit);
        for step in 0..10 {
            if step == limit {
                big.name.push('!');
                return big;
            }
            big.words[step as usize % 6] += step;
        }
        big
    }

    #[inline(never)]
    fn half(fail: bool) -> Big {
        let mut big = build(9);
        big.words[0] = 0;
        big.name.clear();
        if fail {
            panic!("half built");
        }
        big
    }

    pub fn run() -> (Big, Big, Big, Big, bool, Big) {
        let first = build(5);
        let mut chain = build(2);
        for _ in 0..3 {
            chain = next(&chain);
        }
        let (stopped, ran) = (early(4), early(20));
        let mut kept = build(1);
        let caught = panic::catch_unwind(AssertUnwindSafe(|| kept = half(true))).is_err();
        (first, chain, stopped, ran, caught, kept)
    }
}

/// What an `impl Trait` borrows of the arguments it captures, the lifetimes they leave out too:
/// the borrow lasts while it is used, and `use<>` captures none.
pub mod elided_captures {
    pub struct Numbers(Vec<i32>);

    impl Numbers {
        pub fn evens(&self) -> impl Iterator<Item = &i32> + '_ {
            self.0.iter().filter(|it| **it % 2 == 0)
        }

        pub fn odds(&self) -> impl Iterator<Item = &i32> {
            self.0.iter().filter(|it| **it % 2 != 0)
        }

        pub fn len_of(&self) -> impl Fn() -> usize + use<> {
            let len = self.0.len();
            move || len
        }
    }

    pub fn plus(v: &[i32], by: i32) -> impl Iterator<Item = i32> + '_ {
        v.iter().map(move |it| it + by)
    }

    pub fn run() -> (usize, Option<i32>, usize, usize, Vec<i32>) {
        let mut numbers = Numbers(vec![1, 2, 3, 4]);
        let evens = numbers.evens().count();
        numbers.0.push(6);
        let first = numbers.odds().next().copied();
        numbers.0.push(7);
        let len = numbers.len_of();
        numbers.0.push(8);
        let mut v = vec![1, 2];
        let sum = plus(&v, 10).sum::<i32>() as usize;
        v.push(3);
        (evens, first, len(), sum, v)
    }
}

/// Destructuring assignments read every part of what they destructure before they assign any,
/// and move out of a place only what the pattern takes, as rustc's desugaring to a `let` does.
pub mod destructuring_assignment {
    use std::cell::RefCell;

    pub struct Loud<'a>(pub u32, pub &'a RefCell<Vec<u32>>);

    impl Drop for Loud<'_> {
        fn drop(&mut self) {
            self.1.borrow_mut().push(self.0);
        }
    }

    pub struct Pair<'a> {
        pub kept: Loud<'a>,
        pub taken: Loud<'a>,
        pub n: u32,
    }

    fn index(log: &RefCell<Vec<u32>>, at: usize) -> usize {
        log.borrow_mut().push(100 + at as u32);
        at
    }

    pub fn run() -> ((u32, u32), [u32; 3], u32, Vec<u32>) {
        let mut t = (1, 2);
        (t.1, t.0) = t;
        let mut a = [10, 20, 30];
        [a[2], a[0], a[1]] = a;
        let log = RefCell::new(Vec::new());
        let mut pair = Pair { kept: Loud(1, &log), taken: Loud(2, &log), n: 7 };
        let mut taken = Loud(3, &log);
        let mut n = 0;
        log.borrow_mut().push(taken.0 + n);
        Pair { taken, n, .. } = pair;
        log.borrow_mut().push(taken.0 + 10 * n);
        pair.taken = Loud(4, &log);
        let r = &mut pair;
        (r.n, _) = (r.kept.0 + 40, ());
        let mut slots = [0; 3];
        (slots[index(&log, 2)], slots[index(&log, 0)]) = (5, 6);
        let n = pair.n + slots[0] + slots[2];
        drop(pair);
        drop(taken);
        (t, a, n, log.into_inner())
    }
}

/// What `for<'a>` bounds prove for any lifetime, which the borrow checker proves for
/// placeholders: an owned type deserialized from a borrow, a forwarding impl through
/// `&mut F`, a closure for any lifetime, a collection borrowed for any lifetime and a function
/// taking `&mut dyn Write` as a callback.
pub mod higher_ranked {
    pub trait Deserialize<'de>: Sized {
        fn parse(text: &'de str) -> Self;
    }

    pub trait DeserializeOwned: for<'de> Deserialize<'de> {}
    impl<T> DeserializeOwned for T where T: for<'de> Deserialize<'de> {}

    impl<'de> Deserialize<'de> for String {
        fn parse(text: &'de str) -> Self {
            text.to_owned()
        }
    }

    impl<'de> Deserialize<'de> for &'de str {
        fn parse(text: &'de str) -> Self {
            text
        }
    }

    fn owned<T: DeserializeOwned>(text: &str) -> T {
        T::parse(text)
    }

    fn borrowed<'de, T: Deserialize<'de>>(text: &'de str) -> T {
        T::parse(text)
    }

    pub trait Bar<X> {
        fn bar(&mut self, x: X) -> usize;
    }

    impl<X, F: Bar<X>> Bar<X> for &mut F {
        fn bar(&mut self, x: X) -> usize {
            (**self).bar(x)
        }
    }

    struct Count(usize);

    impl<'b> Bar<&'b str> for Count {
        fn bar(&mut self, x: &'b str) -> usize {
            self.0 += x.len();
            self.0
        }
    }

    fn needs<T: for<'b> Bar<&'b str>>(t: &mut T, word: &str) -> usize {
        t.bar(word)
    }

    fn apply<F: for<'a> Fn(&'a str) -> &'a str>(f: F, text: &str) -> usize {
        f(text).len()
    }

    fn total<C>(items: &C) -> u32
    where
        for<'a> &'a C: IntoIterator<Item = &'a u32>,
    {
        items.into_iter().sum()
    }

    fn report(text: &str, out: &mut dyn std::fmt::Write) {
        let _ = out.write_str(text.trim());
    }

    fn with_handler(mut handle: impl FnMut(&str, &mut dyn std::fmt::Write)) -> String {
        let mut out = String::new();
        handle(" handled ", &mut out);
        out
    }

    pub fn run() -> (String, usize, usize, usize, u32, String) {
        let text = String::from(" abc ");
        let copy: String = owned(&text);
        let view: &str = borrowed(&text);
        let mut count = Count(0);
        let first = needs(&mut &mut count, view);
        let second = needs(&mut count, &copy);
        let trimmed = apply(|it| it.trim(), &text);
        let sum = total(&vec![1, 2, 3]);
        let handled = with_handler(report);
        (copy, first, second, trimmed, sum, handled)
    }
}
