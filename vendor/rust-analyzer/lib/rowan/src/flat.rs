//! A syntax tree as arrays, for trees nobody edits.
//!
//! Every node and token is an element, numbered in preorder. An element is its kind, its
//! start in the text, the number after its subtree and its parent; the text is kept once,
//! whole, and a token's text is a slice of it. Nothing is allocated per element, and walking
//! is stepping through the arrays. The cursor ([`crate::cursor`]) serves these trees through
//! the same API as green ones, and makes a green tree of a part only when asked for one; a
//! walk that reads many elements reads them here, by number, and makes a cursor only of what
//! it keeps ([`crate::cursor::SyntaxNode::flat`]).

use std::{
    fmt,
    hash::{Hash, Hasher},
};

use crate::{GreenNode, GreenNodeBuilder, NodeCache, SyntaxKind, TextRange, TextSize};

const TOKEN: u32 = 1 << 31;
const NO_PARENT: u32 = !TOKEN;

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct FlatTree {
    text: Box<str>,
    /// The kinds' numbers: a slice of `u16` hashes in one write.
    kinds: Box<[u16]>,
    starts: Box<[u32]>,
    ends: Box<[u32]>,
    /// The parent's number, with [`TOKEN`] set for a token.
    parents: Box<[u32]>,
}

impl fmt::Debug for FlatTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlatTree")
            .field("elements", &self.kinds.len())
            .field("text_len", &self.text.len())
            .finish()
    }
}

impl FlatTree {
    /// How many nodes and tokens the tree has; the root is number 0.
    #[inline]
    pub fn len(&self) -> u32 {
        self.kinds.len() as u32
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }

    #[inline]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The bytes the tree holds besides itself.
    pub fn heap_size(&self) -> usize {
        self.text.len()
            + self.kinds.len() * 2
            + (self.starts.len() + self.ends.len() + self.parents.len()) * 4
    }

    #[inline]
    pub fn kind(&self, at: u32) -> SyntaxKind {
        SyntaxKind(self.kinds[at as usize])
    }

    #[inline]
    pub fn is_token(&self, at: u32) -> bool {
        self.parents[at as usize] & TOKEN != 0
    }

    #[inline]
    pub fn parent(&self, at: u32) -> Option<u32> {
        let parent = self.parents[at as usize] & !TOKEN;
        (parent != NO_PARENT).then_some(parent)
    }

    /// The number after the element's subtree.
    #[inline]
    pub fn end(&self, at: u32) -> u32 {
        self.ends[at as usize]
    }

    /// The children and tokens of `at`, in order.
    #[inline]
    pub fn children_with_tokens(&self, at: u32) -> impl Iterator<Item = u32> + '_ {
        let end = self.end(at);
        let mut next = at + 1;
        std::iter::from_fn(move || {
            let it = next;
            (it < end).then(|| {
                next = self.end(it);
                it
            })
        })
    }

    /// The children of `at` that are nodes, in order.
    #[inline]
    pub fn children(&self, at: u32) -> impl Iterator<Item = u32> + '_ {
        self.children_with_tokens(at).filter(|&it| !self.is_token(it))
    }

    /// `at` and the nodes under it, in preorder.
    #[inline]
    pub fn descendants(&self, at: u32) -> impl Iterator<Item = u32> + '_ {
        (at..self.end(at)).filter(|&it| !self.is_token(it))
    }

    /// Hashes what the subtree of `at` is made of, as the same subtree is hashed anywhere in
    /// any tree: its text, and its elements' kinds and shape, numbered from `at`.
    pub fn hash_subtree<H: Hasher>(&self, at: u32, state: &mut H) {
        const CHUNK: usize = 64;
        let (lo, hi) = (at as usize, self.end(at) as usize);
        let base = self.starts[lo];
        self.text[self.range(at)].hash(state);
        self.kinds[lo..hi].hash(state);
        let mut shape = [0u32; 3 * CHUNK];
        for from in (lo..hi).step_by(CHUNK) {
            let to = (from + CHUNK).min(hi);
            for (slot, it) in shape.chunks_exact_mut(3).zip(from..to) {
                let parent = self.parents[it];
                slot[0] = self.starts[it] - base;
                slot[1] = self.ends[it] - at;
                slot[2] = match it == lo {
                    true => NO_PARENT,
                    false => (parent & TOKEN) | ((parent & !TOKEN) - at),
                };
            }
            u32::hash_slice(&shape[..3 * (to - from)], state);
        }
    }

    #[inline]
    fn offset(&self, at: u32) -> u32 {
        match self.starts.get(at as usize) {
            Some(&start) => start,
            None => self.text.len() as u32,
        }
    }

    #[inline]
    pub fn range(&self, at: u32) -> TextRange {
        let start = self.offset(at);
        let end = self.offset(self.end(at));
        TextRange::new(TextSize::from(start), TextSize::from(end))
    }

    #[inline]
    pub fn token_text(&self, at: u32) -> &str {
        &self.text[self.range(at)]
    }

    #[inline]
    pub(crate) fn first_child_or_token(&self, at: u32) -> Option<u32> {
        (self.end(at) > at + 1).then_some(at + 1)
    }

    #[inline]
    pub(crate) fn next_sibling_or_token(&self, at: u32) -> Option<u32> {
        let parent = self.parent(at)?;
        let next = self.end(at);
        (next < self.end(parent)).then_some(next)
    }

    /// The child of `parent` that `at` is in. Up from `at` is as many steps as `at` is deep
    /// below `parent`, which an `else if` chain makes as many as it has arms, and across
    /// `parent`'s children as many as come before it, which a long block makes many: a step
    /// of each at a time takes the fewer.
    #[inline]
    fn child_containing(&self, parent: u32, at: u32) -> u32 {
        let (mut up, mut across) = (at, parent + 1);
        loop {
            match self.parent(up) {
                Some(it) if it != parent => up = it,
                _ => return up,
            }
            if at < self.end(across) {
                return across;
            }
            across = self.end(across);
        }
    }

    #[inline]
    pub(crate) fn prev_sibling_or_token(&self, at: u32) -> Option<u32> {
        let parent = self.parent(at)?;
        (at > parent + 1).then(|| self.child_containing(parent, at - 1))
    }

    #[inline]
    pub(crate) fn last_child_or_token(&self, at: u32) -> Option<u32> {
        self.first_child_or_token(at).map(|_| self.child_containing(at, self.end(at) - 1))
    }

    /// The element's position among its parent's children and tokens.
    pub(crate) fn index(&self, at: u32) -> usize {
        let Some(parent) = self.parent(at) else { return 0 };
        let mut child = parent + 1;
        let mut index = 0;
        while child != at {
            child = self.end(child);
            index += 1;
        }
        index
    }

    /// The first child node of `at` from `from` on, `from` being `at`'s child or its end.
    #[inline]
    fn node_from(&self, at: u32, mut from: u32) -> Option<u32> {
        let end = self.end(at);
        // A token's subtree is itself.
        while from < end {
            if self.parents[from as usize] & TOKEN == 0 {
                return Some(from);
            }
            from += 1;
        }
        None
    }

    #[inline]
    pub(crate) fn first_child(&self, at: u32) -> Option<u32> {
        self.node_from(at, at + 1)
    }

    #[inline]
    pub(crate) fn next_sibling(&self, at: u32) -> Option<u32> {
        self.node_from(self.parent(at)?, self.end(at))
    }

    /// The child or token of `at` that holds `range`, as rowan's green nodes find it with a
    /// binary search of the children: the one that holds `range` and overlaps it, else the
    /// last that ends at or before its start, else the first.
    pub(crate) fn child_at_range(&self, at: u32, range: TextRange) -> Option<u32> {
        let first = self.first_child_or_token(at)?;
        let (lo, hi) = (at as usize + 1, self.end(at) as usize);
        let start = u32::from(range.start());
        let contains = |it: u32| self.range(it).contains_range(range);
        if !range.is_empty() {
            // Elements are in preorder, so the last to start at or before `start` is in the
            // only child that can hold a range that is not empty.
            let n = self.starts[lo..hi].partition_point(|&it| it <= start);
            if n == 0 {
                return None;
            }
            let child = self.child_containing(at, (lo + n - 1) as u32);
            return contains(child).then_some(child);
        }
        let n = self.starts[lo..hi].partition_point(|&it| it < start);
        let mut last_less = None;
        if n > 0 {
            let child = self.child_containing(at, (lo + n - 1) as u32);
            if u32::from(self.range(child).end()) > start {
                return Some(child);
            }
            last_less = Some(child);
        }
        // Empty children at `start` end at or before it too.
        let mut next = last_less.map_or(Some(first), |it| self.next_sibling_or_token(it));
        while let Some(it) = next.filter(|&it| self.range(it) == range) {
            last_less = Some(it);
            next = self.next_sibling_or_token(it);
        }
        last_less.or(Some(first)).filter(|&it| contains(it))
    }

    /// The first node of `kind` and `range` that `child_at_range` steps down to from `at`, for
    /// a range that is not empty: those steps go through the ancestors of the last element to
    /// start at or before the range, and stop at the first that does not hold it or is a
    /// token. Found from the bottom up, up to the first ancestor larger than the range, above
    /// which every one is larger too.
    pub(crate) fn node_at_range(&self, at: u32, range: TextRange, kind: SyntaxKind) -> Option<u32> {
        debug_assert!(!range.is_empty());
        let (lo, hi) = (at as usize, self.end(at) as usize);
        let start = u32::from(range.start());
        let n = self.starts[lo..hi].partition_point(|&it| it <= start);
        let mut it = (lo + n).checked_sub(1).filter(|&it| it >= lo)? as u32;
        let mut found = None;
        loop {
            let of_it = self.range(it);
            if of_it == range {
                if !self.is_token(it) && self.kind(it) == kind {
                    found = Some(it);
                }
            } else if of_it.contains_range(range) {
                return found;
            }
            if it == at {
                return found;
            }
            it = self.parent(it)?;
        }
    }

    /// The subtree of node `at`, as a tree of its own.
    pub(crate) fn subtree(&self, at: u32) -> FlatTree {
        let (lo, hi) = (at as usize, self.end(at) as usize);
        let base = self.starts[lo];
        FlatTree {
            text: self.text[self.range(at)].into(),
            kinds: self.kinds[lo..hi].into(),
            starts: self.starts[lo..hi].iter().map(|it| it - base).collect(),
            ends: self.ends[lo..hi].iter().map(|it| it - at).collect(),
            parents: std::iter::once(NO_PARENT)
                .chain(
                    self.parents[lo + 1..hi].iter().map(|it| (it & TOKEN) | ((it & !TOKEN) - at)),
                )
                .collect(),
        }
    }

    /// A green tree of the element's subtree, made with `cache`.
    pub(crate) fn green(&self, at: u32, cache: &mut NodeCache) -> GreenNode {
        let mut builder = GreenNodeBuilder::with_cache(cache);
        let mut open = Vec::new();
        for it in at..self.end(at) {
            while open.last().is_some_and(|&end| end <= it) {
                open.pop();
                builder.finish_node();
            }
            if self.is_token(it) {
                builder.token(self.kind(it), self.token_text(it));
            } else {
                builder.start_node(self.kind(it));
                open.push(self.end(it));
            }
        }
        for _ in open {
            builder.finish_node();
        }
        builder.finish()
    }
}

/// The vector's elements in an allocation of their size: an allocator may keep a block that
/// is shrunk to more than half of it whole (mimalloc does), so a vector grown by doubling
/// and boxed holds up to twice what it needs.
fn exact<T: Copy>(vec: Vec<T>) -> Box<[T]> {
    match vec.capacity() == vec.len() {
        true => vec.into_boxed_slice(),
        false => vec.as_slice().into(),
    }
}

/// Makes a [`FlatTree`] from the same calls as [`GreenNodeBuilder`].
#[derive(Default, Debug)]
pub struct FlatTreeBuilder {
    text: String,
    kinds: Vec<u16>,
    starts: Vec<u32>,
    ends: Vec<u32>,
    parents: Vec<u32>,
    open: Vec<u32>,
}

impl FlatTreeBuilder {
    pub fn new() -> FlatTreeBuilder {
        FlatTreeBuilder::default()
    }

    /// A builder for a text of about `len` bytes.
    pub fn with_capacity(len: usize) -> FlatTreeBuilder {
        // About one element for every two bytes of Rust.
        let elements = len / 2;
        FlatTreeBuilder {
            text: String::with_capacity(len),
            kinds: Vec::with_capacity(elements),
            starts: Vec::with_capacity(elements),
            ends: Vec::with_capacity(elements),
            parents: Vec::with_capacity(elements),
            open: Vec::new(),
        }
    }

    fn push(&mut self, kind: SyntaxKind, flag: u32) -> u32 {
        let at = self.kinds.len() as u32;
        assert!(at < NO_PARENT, "a tree of more than 2^31 elements");
        self.kinds.push(kind.0);
        self.starts.push(self.text.len() as u32);
        self.ends.push(at + 1);
        self.parents.push(self.open.last().copied().unwrap_or(NO_PARENT) | flag);
        at
    }

    pub fn start_node(&mut self, kind: SyntaxKind) {
        assert!(!self.open.is_empty() || self.kinds.is_empty(), "a tree has one root");
        let at = self.push(kind, 0);
        self.open.push(at);
    }

    pub fn token(&mut self, kind: SyntaxKind, text: &str) {
        assert!(!self.open.is_empty(), "a token outside every node");
        self.push(kind, TOKEN);
        self.text.push_str(text);
    }

    pub fn finish_node(&mut self) {
        let at = self.open.pop().expect("a node finished that was not started");
        self.ends[at as usize] = self.kinds.len() as u32;
    }

    pub fn finish(self) -> FlatTree {
        assert!(self.open.is_empty() && !self.kinds.is_empty(), "the tree is not finished");
        assert!(self.text.len() < u32::MAX as usize, "a text of 4 GB or more");
        FlatTree {
            text: match self.text.capacity() == self.text.len() {
                true => self.text.into_boxed_str(),
                false => self.text.as_str().into(),
            },
            kinds: exact(self.kinds),
            starts: exact(self.starts),
            ends: exact(self.ends),
            parents: exact(self.parents),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{hash::Hasher, sync::Arc};

    use crate::{
        cursor::{SyntaxElement, SyntaxNode},
        FlatTreeBuilder, GreenNodeBuilder, NodeOrToken, SyntaxKind, TextRange, TextSize,
        TokenAtOffset, WalkEvent,
    };

    /// The same tree twice, green and flat, from a seed: nodes up to five deep, some empty,
    /// tokens of zero to three bytes.
    fn trees(seed: u64) -> (SyntaxNode, SyntaxNode) {
        let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut next = move |below: u64| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) % below
        };
        let mut green = GreenNodeBuilder::new();
        let mut flat = FlatTreeBuilder::new();
        green.start_node(SyntaxKind(100));
        flat.start_node(SyntaxKind(100));
        let mut depth = 1;
        for _ in 0..next(60) {
            match next(4) {
                0 if depth < 5 => {
                    let kind = SyntaxKind(100 + next(5) as u16);
                    green.start_node(kind);
                    flat.start_node(kind);
                    depth += 1;
                }
                1 if depth > 1 => {
                    green.finish_node();
                    flat.finish_node();
                    depth -= 1;
                }
                _ => {
                    let kind = SyntaxKind(next(5) as u16);
                    let text = &"abc"[..next(4) as usize];
                    green.token(kind, text);
                    flat.token(kind, text);
                }
            }
        }
        for _ in 0..depth {
            green.finish_node();
            flat.finish_node();
        }
        (SyntaxNode::new_root(green.finish()), SyntaxNode::new_flat_root(Arc::new(flat.finish())))
    }

    fn describe(element: Option<SyntaxElement>) -> Option<(SyntaxKind, TextRange, bool)> {
        element.map(|it| (it.kind(), it.text_range(), it.as_token().is_some()))
    }

    fn elements(root: &SyntaxNode) -> Vec<SyntaxElement> {
        root.preorder_with_tokens()
            .filter_map(|it| match it {
                WalkEvent::Enter(it) => Some(it),
                WalkEvent::Leave(_) => None,
            })
            .collect()
    }

    #[test]
    fn a_flat_tree_answers_as_a_green_one() {
        for seed in 0..2000 {
            let (green, flat) = trees(seed);
            assert_eq!(green.to_string(), flat.to_string());
            let (greens, flats) = (elements(&green), elements(&flat));
            assert_eq!(greens.len(), flats.len());
            for (g, f) in greens.iter().zip(&flats) {
                assert_eq!(describe(Some(g.clone())), describe(Some(f.clone())), "seed {seed}");
                assert_eq!(g.index(), f.index());
                assert_eq!(
                    describe(g.parent().map(Into::into)),
                    describe(f.parent().map(Into::into))
                );
                assert_eq!(
                    describe(g.next_sibling_or_token()),
                    describe(f.next_sibling_or_token())
                );
                assert_eq!(
                    describe(g.prev_sibling_or_token()),
                    describe(f.prev_sibling_or_token())
                );
                assert_eq!(
                    describe(g.first_token().map(Into::into)),
                    describe(f.first_token().map(Into::into))
                );
                assert_eq!(
                    describe(g.last_token().map(Into::into)),
                    describe(f.last_token().map(Into::into))
                );
                match (g, f) {
                    (NodeOrToken::Node(g), NodeOrToken::Node(f)) => {
                        let node = |it: Option<SyntaxNode>| describe(it.map(Into::into));
                        assert_eq!(node(g.first_child()), node(f.first_child()));
                        assert_eq!(node(g.last_child()), node(f.last_child()));
                        assert_eq!(node(g.next_sibling()), node(f.next_sibling()));
                        assert_eq!(node(g.prev_sibling()), node(f.prev_sibling()));
                        assert_eq!(
                            describe(g.first_child_or_token()),
                            describe(f.first_child_or_token())
                        );
                        assert_eq!(
                            describe(g.last_child_or_token()),
                            describe(f.last_child_or_token())
                        );
                        assert_eq!(g.green(), f.green());
                        assert_eq!(g.text().to_string(), f.text().to_string());
                        let range = g.text_range();
                        for start in u32::from(range.start())..=u32::from(range.end()) {
                            for end in start..=u32::from(range.end()) {
                                let at = TextRange::new(TextSize::from(start), TextSize::from(end));
                                assert_eq!(
                                    describe(g.child_or_token_at_range(at)),
                                    describe(f.child_or_token_at_range(at)),
                                    "seed {seed}, {at:?} in {range:?}"
                                );
                                assert_eq!(
                                    describe(Some(g.covering_element(at))),
                                    describe(Some(f.covering_element(at)))
                                );
                                for kind in (0..5).chain(100..105).map(SyntaxKind) {
                                    let stepped = |from: &SyntaxNode| {
                                        std::iter::successors(Some(from.clone()), |it| {
                                            it.child_or_token_at_range(at)?.into_node()
                                        })
                                        .find(|it| it.text_range() == at && it.kind() == kind)
                                    };
                                    let found = f
                                        .flat_node_at_range(at, kind)
                                        .unwrap_or_else(|| stepped(f));
                                    assert_eq!(
                                        stepped(g).map(|it| describe(Some(it.into()))),
                                        found.map(|it| describe(Some(it.into()))),
                                        "seed {seed}, {kind:?} at {at:?}"
                                    );
                                }
                            }
                            let offset = TextSize::from(start);
                            let tokens = |it: TokenAtOffset<_>| {
                                it.map(|it| describe(Some(NodeOrToken::Token(it))))
                                    .collect::<Vec<_>>()
                            };
                            assert_eq!(
                                tokens(g.token_at_offset(offset)),
                                tokens(f.token_at_offset(offset))
                            );
                        }
                    }
                    (NodeOrToken::Token(g), NodeOrToken::Token(f)) => {
                        assert_eq!(g.text(), f.text());
                        assert_eq!(g.green(), f.green());
                        let token = |it: Option<_>| describe(it.map(NodeOrToken::Token));
                        assert_eq!(token(g.next_token()), token(f.next_token()));
                        assert_eq!(token(g.prev_token()), token(f.prev_token()));
                    }
                    _ => unreachable!(),
                }
            }
            for (g, f) in greens.iter().zip(&flats).filter(|(g, _)| g.as_node().is_some()) {
                let (g, f) = (g.as_node().unwrap(), f.as_node().unwrap());
                let replacement = g.green().to_owned();
                assert_eq!(g.replace_with(replacement.clone()), f.replace_with(replacement));
                assert_eq!(g.clone_subtree().to_string(), f.clone_subtree().to_string());
                let alone = f.clone_subtree();
                assert_eq!(alone.green(), g.green());
                let mut again = FlatTreeBuilder::new();
                for event in g.preorder_with_tokens() {
                    match event {
                        WalkEvent::Enter(NodeOrToken::Node(it)) => again.start_node(it.kind()),
                        WalkEvent::Enter(NodeOrToken::Token(it)) => {
                            again.token(it.kind(), it.text())
                        }
                        WalkEvent::Leave(NodeOrToken::Node(_)) => again.finish_node(),
                        WalkEvent::Leave(NodeOrToken::Token(_)) => {}
                    }
                }
                assert_eq!(alone.flat_tree().map(|it| &**it), Some(&again.finish()));
                let hash = |it: &SyntaxNode| {
                    let (tree, at) = it.flat().unwrap();
                    let mut state = std::collections::hash_map::DefaultHasher::new();
                    tree.hash_subtree(at, &mut state);
                    state.finish()
                };
                assert_eq!(hash(f), hash(&alone), "seed {seed}");
                let twin = g.to_flat();
                assert_eq!(describe(Some(twin.clone().into())), describe(Some(f.clone().into())));
                assert_eq!(twin.parent().is_none(), g.parent().is_none());
                assert_eq!(hash(&twin), hash(f));
                for node in [g, f] {
                    for kind in (0..5).chain(100..105).map(SyntaxKind) {
                        let ancestor = node.ancestors().find(|it| it.kind() == kind);
                        assert_eq!(node.ancestor_where(|it| it == kind), ancestor);
                        let child = node.children().find(|it| it.kind() == kind);
                        assert_eq!(node.child_where(|it| it == kind), child);
                        let token = node
                            .children_with_tokens()
                            .filter_map(NodeOrToken::into_token)
                            .find(|it| it.kind() == kind);
                        assert_eq!(node.child_token(kind), token);
                    }
                    let mut odd = node.children_where();
                    let by_kind = std::iter::from_fn(|| odd.next_where(|it| it.0 % 2 == 1));
                    let children = node.children().filter(|it| it.kind().0 % 2 == 1);
                    assert!(by_kind.eq(children));
                }
                let (tree, at) = f.flat().unwrap();
                let children: Vec<_> = f.children().collect();
                let read: Vec<_> = tree.children(at).map(|it| f.flat_element(it)).collect();
                assert_eq!(read, children.into_iter().map(NodeOrToken::Node).collect::<Vec<_>>());
                let descendants = f.descendants().map(|it| (it.kind(), it.text_range()));
                assert!(
                    descendants.eq(tree.descendants(at).map(|it| (tree.kind(it), tree.range(it))))
                );
            }
        }
    }

    #[test]
    fn equal_cursors_are_the_same_element() {
        let (_, flat) = trees(7);
        let all = elements(&flat);
        let again = elements(&SyntaxNode::new_flat_root(flat.flat_tree().unwrap().clone()));
        for (i, a) in all.iter().enumerate() {
            for (j, b) in again.iter().enumerate() {
                assert_eq!(a == b, i == j);
            }
        }
    }
}
