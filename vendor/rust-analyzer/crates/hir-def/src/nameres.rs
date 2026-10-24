//! This module implements import-resolution/macro expansion algorithm.
//!
//! The result of this module is `DefMap`: a data structure which contains:
//!
//!   * a tree of modules for the crate
//!   * for each module, a set of items visible in the module (directly declared
//!     or imported)
//!
//! Note that `DefMap` contains fully macro expanded code.
//!
//! Computing `DefMap` can be partitioned into several logically
//! independent "phases". The phases are mutually recursive though, there's no
//! strict ordering.
//!
//! ## Collecting RawItems
//!
//! This happens in the `raw` module, which parses a single source file into a
//! set of top-level items. Nested imports are desugared to flat imports in this
//! phase. Macro calls are represented as a triple of `(Path, Option<Name>,
//! TokenTree)`.
//!
//! ## Collecting Modules
//!
//! This happens in the `collector` module. In this phase, we recursively walk
//! tree of modules, collect raw items from submodules, populate module scopes
//! with defined items (so, we assign item ids in this phase) and record the set
//! of unresolved imports and macros.
//!
//! While we walk tree of modules, we also record macro_rules definitions and
//! expand calls to macro_rules defined macros.
//!
//! ## Resolving Imports
//!
//! We maintain a list of currently unresolved imports. On every iteration, we
//! try to resolve some imports from this list. If the import is resolved, we
//! record it, by adding an item to current module scope and, if necessary, by
//! recursively populating glob imports.
//!
//! ## Resolving Macros
//!
//! macro_rules from the same crate use a global mutable namespace. We expand
//! them immediately, when we collect modules.
//!
//! Macros from other crates (including proc-macros) can be used with
//! `foo::bar!` syntax. We handle them similarly to imports. There's a list of
//! unexpanded macros. On every iteration, we try to resolve each macro call
//! path and, upon success, we run macro expansion and "collect module" phase on
//! the result

pub mod assoc;
pub mod attr_resolution;
mod collector;
pub mod diagnostics;
pub mod frozen;
mod mod_resolution;
mod path_resolution;
pub mod proc_macro;

#[cfg(test)]
mod tests;

use std::{
    hash::Hash,
    ops::{Deref, DerefMut, Index, IndexMut},
};

use base_db::{Crate, SourceDatabase};
use either::Either;
use hir_expand::{
    EditionedFileId, ErasedAstId, HirFileId, InFile, MacroCallId, mod_path::ModPath, name::Name,
    proc_macro::ProcMacroKind,
};
use intern::{Symbol, sym};
use itertools::Itertools;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use span::{Edition, FileAstId, FileId, ROOT_ERASED_FILE_AST_ID};
use stdx::format_to;
use syntax::{AstNode, SmolStr, SyntaxNode, TextSize, ToSmolStr, ast};
use triomphe::Arc;
use tt::TextRange;

use crate::{
    AstId, BlockId, BlockIdLt, BuiltinDeriveImplId, ExternCrateId, FunctionId, FxIndexMap, Lookup,
    MacroCallStyles, MacroExpander, MacroId, ModuleId, ModuleIdLt, ProcMacroId, UseId,
    item_scope::{BuiltinShadowMode, ItemScope},
    item_tree::TreeId,
    nameres::{diagnostics::DefDiagnostic, path_resolution::ResolveMode},
    per_ns::PerNs,
    unstable_features::UnstableFeatures,
    visibility::{Visibility, VisibilityExplicitness},
};

pub use self::path_resolution::ResolvePathResultPrefixInfo;

#[cfg(test)]
thread_local! {
    /// HACK: In order to test builtin derive expansion, we gate their fast path with this atomic when cfg(test).
    pub(crate) static ENABLE_BUILTIN_DERIVE_FAST_PATH: std::cell::Cell<bool> =
        const { std::cell::Cell::new(true) };
}

#[inline]
#[cfg(test)]
fn enable_builtin_derive_fast_path() -> bool {
    ENABLE_BUILTIN_DERIVE_FAST_PATH.get()
}

/// Whether builtin derives are recorded instead of expanded. A code generator needs the bodies
/// of the derived methods, so it turns this off before anything is analysed.
#[cfg(not(test))]
pub static EXPAND_BUILTIN_DERIVES: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[inline]
#[cfg(not(test))]
fn enable_builtin_derive_fast_path() -> bool {
    !EXPAND_BUILTIN_DERIVES.load(std::sync::atomic::Ordering::Relaxed)
}

const PREDEFINED_TOOLS: &[SmolStr] = &[
    SmolStr::new_static("clippy"),
    SmolStr::new_static("rustfmt"),
    SmolStr::new_static("diagnostic"),
    SmolStr::new_static("miri"),
    SmolStr::new_static("rust_analyzer"),
];

/// Parts of the def map that are only needed when analyzing code in the same crate.
///
/// There are some data in the def map (e.g. extern prelude) that is only needed when analyzing
/// things in the same crate (and maybe in the IDE layer), e.g. the extern prelude. If we put
/// it in the DefMap dependant DefMaps will be invalidated when they change (e.g. when we add
/// a dependency to the crate). Instead we split them out of the DefMap into a LocalDefMap struct.
/// `crate_local_def_map()` returns both, and `crate_def_map()` returns only the external-relevant
/// DefMap.
#[derive(Debug, PartialEq, Eq, Default)]
pub struct LocalDefMap {
    // FIXME: There are probably some other things that could be here, but this is less severe and you
    // need to be careful with things that block def maps also have.
    /// The extern prelude which contains all root modules of external crates that are in scope.
    extern_prelude: FxIndexMap<Name, (ModuleId, Option<ExternCrateId>)>,
}

impl std::hash::Hash for LocalDefMap {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        let LocalDefMap { extern_prelude } = self;
        extern_prelude.len().hash(state);
        for (name, (crate_root, extern_crate)) in extern_prelude {
            name.hash(state);
            crate_root.hash(state);
            extern_crate.hash(state);
        }
    }
}

impl LocalDefMap {
    pub(crate) const EMPTY: &Self =
        &Self { extern_prelude: FxIndexMap::with_hasher(rustc_hash::FxBuildHasher) };

    fn shrink_to_fit(&mut self) {
        let Self { extern_prelude } = self;
        extern_prelude.shrink_to_fit();
    }

    pub(crate) fn extern_prelude(
        &self,
    ) -> impl DoubleEndedIterator<Item = (&Name, (ModuleId, Option<ExternCrateId>))> + '_ {
        self.extern_prelude.iter().map(|(name, &def)| (name, def))
    }
}

/// Contains the results of (early) name resolution.
///
/// A `DefMap` stores the module tree and the definitions that are in scope in every module after
/// item-level macros have been expanded.
///
/// Every crate has a primary `DefMap` whose root is the crate's main file (`main.rs`/`lib.rs`),
/// computed by the `crate_def_map` query. Additionally, every block expression introduces the
/// opportunity to write arbitrary item and module hierarchies, and thus gets its own `DefMap` that
/// is computed by the `block_def_map` query.
#[derive(Debug, PartialEq, Eq)]
pub struct DefMap {
    /// The crate this `DefMap` belongs to.
    krate: Crate,
    /// When this is a block def map, this will hold the block id of the block and module that
    /// contains this block.
    block: Option<BlockInfo>,
    pub root: ModuleId,
    /// The modules and their data declared in this crate.
    modules: ModulesSlot,
    /// The prelude module for this crate. This either comes from an import
    /// marked with the `prelude_import` attribute, or (in the normal case) from
    /// a dependency (`std` or `core`).
    /// The prelude is empty for non-block DefMaps (unless `#[prelude_import]` was used,
    /// but that attribute is nightly and when used in a block, it affects resolution globally
    /// so we aren't handling this correctly anyways).
    prelude: Option<(ModuleId, Option<UseId>)>,
    /// `macro_use` prelude that contains macros from `#[macro_use]`'d external crates. Note that
    /// this contains all kinds of macro, not just `macro_rules!` macro.
    /// ExternCrateId being None implies it being imported from the general prelude import.
    macro_use_prelude: FxHashMap<Name, (MacroId, Option<ExternCrateId>)>,

    /// Tracks which custom derives are in scope for an item, to allow resolution of derive helper
    /// attributes.
    // FIXME: Figure out a better way for the IDE layer to resolve these?
    derive_helpers_in_scope:
        FxHashMap<AstId<ast::Item>, Vec<(Name, MacroId, Either<MacroCallId, BuiltinDeriveImplId>)>>,

    /// The diagnostics that need to be emitted for this crate.
    diagnostics: Vec<DefDiagnostic>,

    /// The crate data that is shared between a crate's def map and all its block def maps.
    data: Arc<DefMapCrateData>,
}

/// Data that belongs to a crate which is shared between a crate's def map and all its block def maps.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DefMapCrateData {
    /// Side table for resolving derive helpers.
    exported_derives: FxHashMap<MacroId, Box<[Name]>>,
    fn_proc_macro_mapping: FxHashMap<FunctionId, ProcMacroId>,
    fn_proc_macro_mapping_back: FxHashMap<ProcMacroId, FunctionId>,

    /// Custom tool modules registered with `#![register_tool]`.
    registered_tools: Vec<Symbol>,
    /// Unstable features of Rust enabled with `#![feature(A, B)]`.
    unstable_features: UnstableFeatures,
    /// `#[rustc_coherence_is_core]`
    rustc_coherence_is_core: bool,
    no_core: bool,
    no_std: bool,
    /// A crate of procedural macros, which gives other crates its macros and nothing else.
    proc_macro: bool,

    edition: Edition,
    recursion_limit: Option<u32>,
}

impl DefMapCrateData {
    fn new(edition: Edition) -> Self {
        Self {
            exported_derives: FxHashMap::default(),
            fn_proc_macro_mapping: FxHashMap::default(),
            fn_proc_macro_mapping_back: FxHashMap::default(),
            registered_tools: PREDEFINED_TOOLS.iter().map(|it| Symbol::intern(it)).collect(),
            unstable_features: UnstableFeatures::default(),
            rustc_coherence_is_core: false,
            no_core: false,
            no_std: false,
            proc_macro: false,
            edition,
            recursion_limit: None,
        }
    }

    fn shrink_to_fit(&mut self) {
        let Self {
            exported_derives,
            fn_proc_macro_mapping,
            fn_proc_macro_mapping_back,
            registered_tools,
            unstable_features,
            rustc_coherence_is_core: _,
            no_core: _,
            no_std: _,
            proc_macro: _,
            edition: _,
            recursion_limit: _,
        } = self;
        exported_derives.shrink_to_fit();
        fn_proc_macro_mapping.shrink_to_fit();
        fn_proc_macro_mapping_back.shrink_to_fit();
        registered_tools.shrink_to_fit();
        unstable_features.shrink_to_fit();
    }
}

/// For `DefMap`s computed for a block expression, this stores its location in the parent map.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct BlockInfo {
    /// The `BlockId` this `DefMap` was created from.
    block: BlockId,
    /// The containing module.
    parent: ModuleId,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum ModuleOrigin {
    CrateRoot {
        definition: EditionedFileId,
    },
    /// Note that non-inline modules, by definition, live inside non-macro file.
    File {
        is_mod_rs: bool,
        declaration: FileAstId<ast::Module>,
        declaration_tree_id: TreeId,
        definition: EditionedFileId,
    },
    Inline {
        definition_tree_id: TreeId,
        definition: FileAstId<ast::Module>,
    },
    /// Pseudo-module introduced by a block scope (contains only inner items).
    BlockExpr {
        id: BlockId,
        block: AstId<ast::BlockExpr>,
    },
}

impl ModuleOrigin {
    pub fn declaration(&self) -> Option<AstId<ast::Module>> {
        match self {
            &ModuleOrigin::File { declaration, declaration_tree_id, .. } => {
                Some(AstId::new(declaration_tree_id.file_id(), declaration))
            }
            &ModuleOrigin::Inline { definition, definition_tree_id } => {
                Some(AstId::new(definition_tree_id.file_id(), definition))
            }
            ModuleOrigin::CrateRoot { .. } | ModuleOrigin::BlockExpr { .. } => None,
        }
    }

    pub fn file_id(&self) -> Option<EditionedFileId> {
        match self {
            ModuleOrigin::File { definition, .. } | ModuleOrigin::CrateRoot { definition } => {
                Some(*definition)
            }
            _ => None,
        }
    }

    pub fn is_inline(&self) -> bool {
        match self {
            ModuleOrigin::Inline { .. } | ModuleOrigin::BlockExpr { .. } => true,
            ModuleOrigin::CrateRoot { .. } | ModuleOrigin::File { .. } => false,
        }
    }

    /// Returns a node which defines this module.
    /// That is, a file or a `mod foo {}` with items.
    pub fn definition_source(&self, db: &dyn SourceDatabase) -> InFile<ModuleSource> {
        match self {
            &ModuleOrigin::File { definition: editioned_file_id, .. }
            | &ModuleOrigin::CrateRoot { definition: editioned_file_id } => {
                let sf = editioned_file_id.parse(db).tree();
                InFile::new(editioned_file_id.into(), ModuleSource::SourceFile(sf))
            }
            &ModuleOrigin::Inline { definition, definition_tree_id } => InFile::new(
                definition_tree_id.file_id(),
                ModuleSource::Module(
                    AstId::new(definition_tree_id.file_id(), definition).to_node(db),
                ),
            ),
            ModuleOrigin::BlockExpr { block, .. } => {
                InFile::new(block.file_id, ModuleSource::BlockExpr(block.to_node(db)))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleData {
    /// Where does this module come from?
    pub origin: ModuleOrigin,
    /// Declared visibility of this module.
    pub visibility: Visibility,
    /// Parent module in the same `DefMap`.
    ///
    /// [`None`] for block modules because they are always its `DefMap`'s root.
    pub parent: Option<ModuleId>,
    scope: ScopeSlot,
}

/// The modules of a `DefMap`, held by the database on their own: a `DefMap` stays what it was
/// when a module is added to it, and so does everything that was resolved through it.
#[salsa::tracked]
struct Modules<'db> {
    root: ModuleIdLt<'db>,
    #[tracked]
    #[returns(ref)]
    table: ModulesMap,
}

#[salsa::tracked]
impl<'db> Modules<'db> {
    /// One module of a collected `DefMap`. Depends on the modules there are, and is the same
    /// for as long as the module is.
    #[salsa::tracked(returns(ref))]
    fn module(self, db: &'db dyn SourceDatabase, module: ModuleId) -> Option<ModuleData> {
        self.table(db).get(&module).cloned()
    }
}

impl ModulesSlot {
    /// The modules of a `DefMap` that is being collected.
    fn building(&mut self) -> &mut ModulesMap {
        match self {
            ModulesSlot::Building(modules) => modules,
            ModulesSlot::Collected(_) => {
                panic!("the modules of a collected `DefMap` are read-only")
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ModulesSlot {
    /// While the `DefMap` is being collected.
    Building(ModulesMap),
    /// A `Modules`, once it is.
    Collected(salsa::Id),
}

/// The items of one module, held by the database on their own. A query that reads the scope of
/// a module then depends on that scope and not on the whole `DefMap`, so adding an item to one
/// module leaves what was resolved through the others alone.
#[salsa::tracked]
struct ModuleScope<'db> {
    module: ModuleIdLt<'db>,
    #[tracked]
    #[returns(ref)]
    scope: ItemScope,
    #[tracked]
    #[returns(ref)]
    children: FxIndexMap<Name, ModuleId>,
}

#[salsa::tracked]
impl<'db> ModuleScope<'db> {
    /// What one name means in the module. Adding or removing another item leaves this equal,
    /// so a query that only looked names up is not re-run.
    #[salsa::tracked]
    fn name(self, db: &'db dyn SourceDatabase, name: Name) -> PerNs {
        self.scope(db).get(&name)
    }

    #[salsa::tracked(returns(ref))]
    fn legacy_macro(self, db: &'db dyn SourceDatabase, name: Name) -> Option<Box<[MacroId]>> {
        self.scope(db).get_legacy_macro(&name).map(Box::from)
    }

    #[salsa::tracked(returns(ref))]
    fn traits(self, db: &'db dyn SourceDatabase) -> Box<[crate::TraitId]> {
        self.scope(db).traits().collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ScopeSlot {
    /// While the `DefMap` is being collected.
    Building { scope: Box<ItemScope>, children: FxIndexMap<Name, ModuleId> },
    /// A `ModuleScope`, once it is.
    Collected(salsa::Id),
}

#[inline]
pub fn crate_def_map(db: &dyn SourceDatabase, crate_id: Crate) -> &DefMap {
    crate_local_def_map(db, crate_id).def_map(db)
}

#[salsa::tracked]
pub(crate) struct DefMapPair<'db> {
    #[tracked]
    #[returns(ref)]
    pub(crate) def_map: DefMap,
    #[returns(ref)]
    pub(crate) local: LocalDefMap,
}

#[salsa::tracked(returns(ref))]
pub(crate) fn crate_local_def_map(db: &dyn SourceDatabase, crate_id: Crate) -> DefMapPair<'_> {
    let krate = crate_id.data(db);
    let _p = tracing::info_span!(
        "crate_def_map_query",
        name=?crate_id
            .extra_data(db)
            .display_name
            .as_ref()
            .map(|it| it.crate_name().to_smolstr())
            .unwrap_or_default()
    )
    .entered();

    if let Some((def_map, local_def_map)) = frozen::served(db, crate_id) {
        return DefMapPair::new(db, def_map, local_def_map);
    }
    let root_file_id = crate_id.root_file_id(db);
    let module_data = ModuleData::new(
        ModuleOrigin::CrateRoot { definition: root_file_id },
        Visibility::Public,
        None,
    );

    let def_map = DefMap::empty(
        db,
        crate_id,
        Arc::new(DefMapCrateData::new(krate.edition)),
        module_data,
        None,
    );
    let (mut def_map, local_def_map) =
        collector::collect_defs(db, def_map, TreeId::new(root_file_id.into(), None), None);
    def_map.hand_over_scopes(db);
    frozen::check(db, crate_id, &def_map, &local_def_map);

    DefMapPair::new(db, def_map, local_def_map)
}

#[salsa::tracked(returns(ref))]
pub fn block_def_map<'db>(db: &'db dyn SourceDatabase, block_id: BlockIdLt<'db>) -> DefMap {
    let block_id = unsafe { block_id.to_static() };
    let ast_id = block_id.ast_id(db);
    let module = unsafe { block_id.module(db).to_static() };

    let visibility = Visibility::Module(module, VisibilityExplicitness::Implicit);
    let module_data =
        ModuleData::new(ModuleOrigin::BlockExpr { block: ast_id, id: block_id }, visibility, None);

    let krate = module.krate(db);
    let local_def_map = crate_local_def_map(db, krate);
    let def_map = DefMap::empty(
        db,
        krate,
        local_def_map.def_map(db).data.clone(),
        module_data,
        Some(BlockInfo { block: block_id, parent: module }),
    );

    let (mut def_map, _) = collector::collect_defs(
        db,
        def_map,
        TreeId::new(ast_id.file_id, Some(block_id)),
        Some(local_def_map.local(db)),
    );
    def_map.hand_over_scopes(db);
    def_map
}

impl DefMap {
    pub fn edition(&self) -> Edition {
        self.data.edition
    }

    fn empty(
        db: &dyn SourceDatabase,
        krate: Crate,
        crate_data: Arc<DefMapCrateData>,
        module_data: ModuleData,
        block: Option<BlockInfo>,
    ) -> DefMap {
        let mut modules = ModulesMap::new();
        let root = unsafe {
            ModuleIdLt::new(
                db,
                krate,
                block.map(|it| it.block),
                None,
                Name::new_symbol_root(sym::__empty),
            )
            .to_static()
        };
        modules.insert(root, module_data);

        DefMap {
            block,
            root,
            modules: ModulesSlot::Building(modules),
            krate,
            prelude: None,
            macro_use_prelude: FxHashMap::default(),
            derive_helpers_in_scope: FxHashMap::default(),
            diagnostics: Vec::new(),
            data: crate_data,
        }
    }
    fn shrink_to_fit(&mut self) {
        // Exhaustive match to require handling new fields.
        let Self {
            macro_use_prelude,
            diagnostics,
            modules,
            derive_helpers_in_scope,
            root: _,
            block: _,
            krate: _,
            prelude: _,
            data: _,
        } = self;

        macro_use_prelude.shrink_to_fit();
        diagnostics.shrink_to_fit();
        derive_helpers_in_scope.shrink_to_fit();
        if let ModulesSlot::Building(modules) = modules {
            modules.shrink_to_fit();
            for (_, module) in modules.iter_mut() {
                module.children_mut().shrink_to_fit();
                module.scope_mut().shrink_to_fit();
            }
        }
    }

    /// Hands the scope of every module to the database. Called once, when collection is done.
    fn hand_over_scopes(&mut self, db: &dyn SourceDatabase) {
        let ModulesSlot::Building(modules) = &mut self.modules else { return };
        for (id, module) in modules.iter_mut() {
            if let ScopeSlot::Building { scope, children } = &mut module.scope {
                let scope = ModuleScope::new(
                    db,
                    id,
                    std::mem::take(&mut **scope),
                    std::mem::take(children),
                );
                module.scope = ScopeSlot::Collected(salsa::plumbing::AsId::as_id(&scope));
            }
        }
        let modules = Modules::new(db, self.root, std::mem::replace(modules, ModulesMap::new()));
        self.modules = ModulesSlot::Collected(salsa::plumbing::AsId::as_id(&modules));
    }

    /// One module of the `DefMap`; depends on that module only.
    pub fn module<'a>(&'a self, db: &'a dyn SourceDatabase, id: ModuleIdLt<'_>) -> &'a ModuleData {
        let id = unsafe { id.to_static() };
        let found = match &self.modules {
            ModulesSlot::Building(modules) => modules.get(&id),
            ModulesSlot::Collected(modules) => {
                <Modules<'a> as salsa::plumbing::FromId>::from_id(*modules).module(db, id).as_ref()
            }
        };
        found
            .unwrap_or_else(|| panic!("ModuleId not found in ModulesMap {:#?}: {id:#?}", self.root))
    }

    /// Every module of the `DefMap`; depends on which there are.
    fn table<'a>(&'a self, db: &'a dyn SourceDatabase) -> &'a ModulesMap {
        match &self.modules {
            ModulesSlot::Building(modules) => modules,
            ModulesSlot::Collected(id) => {
                <Modules<'a> as salsa::plumbing::FromId>::from_id(*id).table(db)
            }
        }
    }
}

impl DefMap {
    /// Returns all modules in the crate that are associated with the given file.
    pub fn modules_for_file<'a>(
        &'a self,
        db: &'a dyn SourceDatabase,
        file_id: FileId,
    ) -> impl Iterator<Item = ModuleId> + 'a {
        self.table(db)
            .iter()
            .filter(move |(_, data)| {
                data.origin.file_id().map(|file_id| file_id.file_id(db)) == Some(file_id)
            })
            .map(|(id, _)| id)
    }

    pub fn modules<'a>(
        &'a self,
        db: &'a dyn SourceDatabase,
    ) -> impl Iterator<Item = (ModuleId, &'a ModuleData)> + 'a {
        self.table(db).iter()
    }

    /// Returns all inline modules (mod name { ... }) in the crate that are associated with the given macro expansion.
    pub fn inline_modules_for_macro_file<'a>(
        &'a self,
        db: &'a dyn SourceDatabase,
        file_id: MacroCallId,
    ) -> impl Iterator<Item = ModuleId> + 'a {
        self.table(db)
            .iter()
            .filter(move |(_, data)| {
                matches!(
                    data.origin,
                    ModuleOrigin::Inline { definition_tree_id, .. }
                    if definition_tree_id.file_id().macro_file() == Some(file_id)
                )
            })
            .map(|(id, _)| id)
    }

    pub fn derive_helpers_in_scope(
        &self,
        id: AstId<ast::Adt>,
    ) -> Option<&[(Name, MacroId, Either<MacroCallId, BuiltinDeriveImplId>)]> {
        self.derive_helpers_in_scope.get(&id.map(|it| it.upcast())).map(Deref::deref)
    }

    pub fn registered_tools(&self) -> &[Symbol] {
        &self.data.registered_tools
    }

    #[inline]
    pub fn features(&self) -> &UnstableFeatures {
        &self.data.unstable_features
    }

    pub fn is_rustc_coherence_is_core(&self) -> bool {
        self.data.rustc_coherence_is_core
    }

    pub fn is_no_std(&self) -> bool {
        self.data.no_std || self.data.no_core
    }

    /// Whether this is a crate of procedural macros (`proc-macro = true`).
    pub fn is_proc_macro(&self) -> bool {
        self.data.proc_macro
    }

    pub fn is_no_core(&self) -> bool {
        self.data.no_core
    }

    pub fn fn_as_proc_macro(&self, id: FunctionId) -> Option<ProcMacroId> {
        self.data.fn_proc_macro_mapping.get(&id).copied()
    }

    pub fn proc_macro_as_fn(&self, id: ProcMacroId) -> Option<FunctionId> {
        self.data.fn_proc_macro_mapping_back.get(&id).copied()
    }

    pub fn krate(&self) -> Crate {
        self.krate
    }

    #[inline]
    pub fn crate_root(&self, db: &dyn SourceDatabase) -> ModuleId {
        match self.block {
            Some(_) => crate_def_map(db, self.krate()).root,
            None => self.root,
        }
    }

    /// This is the same as [`Self::crate_root`] for crate def maps, but for block def maps, it
    /// returns the root block module.
    pub fn root_module_id(&self) -> ModuleId {
        self.root
    }

    /// If this `DefMap` is for a block expression, returns the module containing the block (which
    /// might again be a block, or a module inside a block).
    pub fn parent(&self) -> Option<ModuleId> {
        Some(self.block?.parent)
    }

    /// Returns the module containing `local_mod`, either the parent `mod`, or the module (or block) containing
    /// the block, if `self` corresponds to a block expression.
    pub fn containing_module(
        &self,
        db: &dyn SourceDatabase,
        local_mod: ModuleIdLt<'_>,
    ) -> Option<ModuleId> {
        match self.module(db, local_mod).parent {
            Some(parent) => Some(parent),
            None => self.block.map(|BlockInfo { parent, .. }| parent),
        }
    }

    /// Get a reference to the def map's diagnostics.
    pub fn diagnostics(&self) -> &[DefDiagnostic] {
        self.diagnostics.as_slice()
    }

    pub fn recursion_limit(&self) -> u32 {
        // 128 is the default in rustc
        self.data.recursion_limit.unwrap_or(128)
    }

    // FIXME: this can use some more human-readable format (ideally, an IR
    // even), as this should be a great debugging aid.
    pub fn dump(&self, db: &dyn SourceDatabase) -> String {
        let mut buf = String::new();
        let mut current_map = self;
        while let Some(block) = current_map.block {
            go(&mut buf, db, current_map, "(block scope)", current_map.root);
            buf.push('\n');
            current_map = block.parent.def_map(db);
        }
        go(&mut buf, db, current_map, "crate", current_map.root);
        return buf;

        fn go(
            buf: &mut String,
            db: &dyn SourceDatabase,
            map: &DefMap,
            path: &str,
            module: ModuleId,
        ) {
            format_to!(buf, "{}\n", path);

            map.module(db, module).scope(db).dump(db, buf);

            for (name, child) in
                map.module(db, module).children(db).iter().sorted_by(|a, b| Ord::cmp(&a.0, &b.0))
            {
                let path = format!("{path}::{}", name.display(db, Edition::LATEST));
                buf.push('\n');
                go(buf, db, map, &path, *child);
            }
        }
    }
}

impl DefMap {
    pub(crate) fn block_id(&self) -> Option<BlockId> {
        self.block.map(|block| block.block)
    }

    pub(crate) fn prelude(&self) -> Option<(ModuleId, Option<UseId>)> {
        self.prelude
    }

    pub(crate) fn macro_use_prelude(&self) -> &FxHashMap<Name, (MacroId, Option<ExternCrateId>)> {
        &self.macro_use_prelude
    }

    pub(crate) fn resolve_path(
        &self,
        local_def_map: &LocalDefMap,
        db: &dyn SourceDatabase,
        original_module: ModuleId,
        path: &ModPath,
        shadow: BuiltinShadowMode,
        expected_macro_subns: Option<MacroSubNs>,
    ) -> (PerNs, Option<usize>) {
        let res = self.resolve_path_fp_with_macro(
            local_def_map,
            db,
            ResolveMode::Other,
            original_module,
            path,
            shadow,
            expected_macro_subns,
        );
        (res.resolved_def, res.segment_index)
    }

    /// The first `Option<usize>` points at the `Enum` segment in case of `Enum::Variant`, the second
    /// points at the unresolved segments.
    pub(crate) fn resolve_path_locally(
        &self,
        local_def_map: &LocalDefMap,
        db: &dyn SourceDatabase,
        original_module: ModuleId,
        path: &ModPath,
        shadow: BuiltinShadowMode,
    ) -> (PerNs, Option<usize>, ResolvePathResultPrefixInfo) {
        let res = self.resolve_path_fp_with_macro_single(
            local_def_map,
            db,
            ResolveMode::Other,
            original_module,
            path,
            shadow,
            None, // Currently this function isn't used for macro resolution.
        );
        (res.resolved_def, res.segment_index, res.prefix_info)
    }

    /// Ascends the `DefMap` hierarchy and calls `f` with every `DefMap` and containing module.
    ///
    /// If `f` returns `Some(val)`, iteration is stopped and `Some(val)` is returned. If `f` returns
    /// `None`, iteration continues.
    pub(crate) fn with_ancestor_maps<'db, T>(
        &self,
        db: &'db dyn SourceDatabase,
        local_mod: ModuleIdLt<'db>,
        f: &mut dyn FnMut(&DefMap, ModuleIdLt<'db>) -> Option<T>,
    ) -> Option<T> {
        if let Some(it) = f(self, local_mod) {
            return Some(it);
        }
        let mut block = self.block;
        while let Some(block_info) = block {
            let parent = block_info.parent.def_map(db);
            if let Some(it) = f(parent, block_info.parent) {
                return Some(it);
            }
            block = parent.block;
        }

        None
    }
}

impl DefMap {
    /// The `macro_rules!` that `name` is in the textual scope of `module`: the latest one of
    /// this map, or the latest one an enclosing block defines before the block inside it.
    pub(crate) fn legacy_macro_in_textual_scope<'db>(
        &self,
        db: &'db dyn SourceDatabase,
        module: ModuleIdLt<'db>,
        name: &Name,
    ) -> Option<MacroId> {
        if let Some(&latest) =
            self.module(db, module).legacy_macro_in_scope(db, name).and_then(<[_]>::last)
        {
            return Some(latest);
        }
        let mut inner = self.block;
        while let Some(BlockInfo { block, parent }) = inner {
            let map = parent.def_map(db);
            let defined =
                map.module(db, parent).legacy_macro_in_scope(db, name).unwrap_or_default();
            let visible = SourcePosition::latest_before(db, defined, block.ast_id(db).erase());
            if visible.is_some() {
                return visible;
            }
            inner = map.block;
        }
        None
    }

    /// The `macro_rules!` of `module` that a call at `call` names by `name`, when the module
    /// defines one before the call. Items of a trait or an impl are collected after the
    /// module is, when a later `macro_rules!` of the same name has joined the scope.
    pub(crate) fn legacy_macro_before<'db>(
        &self,
        db: &'db dyn SourceDatabase,
        module: ModuleIdLt<'db>,
        name: &Name,
        call: ErasedAstId,
    ) -> Option<MacroId> {
        let defined = self.module(db, module).legacy_macro_in_scope(db, name)?;
        SourcePosition::latest_before(db, defined, call)
    }
}

/// Where a node starts in its file and, inside a macro expansion, where each call that led
/// to that expansion starts.
struct SourcePosition(SmallVec<[(HirFileId, TextSize); 2]>);

impl SourcePosition {
    fn new(db: &dyn SourceDatabase, mut file: HirFileId, range: TextRange) -> Self {
        let mut calls = SmallVec::from_iter([(file, range.start())]);
        while let Some(call) = file.macro_file() {
            let node = call.call_node(db);
            file = node.file_id;
            calls.push((file, node.value.text_range().start()));
        }
        SourcePosition(calls)
    }

    /// The last of `macros`, which are in definition order, that is defined before `node`.
    fn latest_before(
        db: &dyn SourceDatabase,
        macros: &[MacroId],
        node: ErasedAstId,
    ) -> Option<MacroId> {
        match macros {
            [] => None,
            [only] => Some(*only),
            _ => {
                let node = Self::new(db, node.file_id, node.to_range(db));
                macros
                    .iter()
                    .rev()
                    .copied()
                    .find(|&it| Self::of_macro(db, it).is_none_or(|it| it.precedes(&node)))
            }
        }
    }

    fn of_macro(db: &dyn SourceDatabase, id: MacroId) -> Option<Self> {
        let MacroId::MacroRulesId(id) = id else { return None };
        let ast_id = id.lookup(db).id;
        Some(Self::new(db, ast_id.file_id, ast_id.to_range(db)))
    }

    /// Whether this is before `other` in the innermost file both are in.
    fn precedes(&self, other: &Self) -> bool {
        let shared = self.0.iter().find_map(|(file, offset)| {
            other.0.iter().find(|it| it.0 == *file).map(|it| *offset < it.1)
        });
        shared.unwrap_or(true)
    }
}

impl ModuleData {
    pub(crate) fn new(
        origin: ModuleOrigin,
        visibility: Visibility,
        parent: Option<ModuleId>,
    ) -> Self {
        ModuleData {
            origin,
            visibility,
            parent,
            scope: ScopeSlot::Building { scope: Box::default(), children: FxIndexMap::default() },
        }
    }

    pub fn scope<'a>(&'a self, db: &'a dyn SourceDatabase) -> &'a ItemScope {
        match &self.scope {
            ScopeSlot::Building { scope, .. } => scope,
            ScopeSlot::Collected(id) => {
                <ModuleScope<'a> as salsa::plumbing::FromId>::from_id(*id).scope(db)
            }
        }
    }

    /// What `name` means in this module; depends on that name only.
    pub fn name_in_scope(&self, db: &dyn SourceDatabase, name: &Name) -> PerNs {
        match &self.scope {
            ScopeSlot::Building { scope, .. } => scope.get(name),
            ScopeSlot::Collected(id) => {
                *<ModuleScope<'_> as salsa::plumbing::FromId>::from_id(*id).name(db, name.clone())
            }
        }
    }

    pub(crate) fn legacy_macro_in_scope<'a>(
        &'a self,
        db: &'a dyn SourceDatabase,
        name: &Name,
    ) -> Option<&'a [MacroId]> {
        match &self.scope {
            ScopeSlot::Building { scope, .. } => scope.get_legacy_macro(name),
            ScopeSlot::Collected(id) => <ModuleScope<'a> as salsa::plumbing::FromId>::from_id(*id)
                .legacy_macro(db, name.clone())
                .as_deref(),
        }
    }

    /// The traits in scope in this module; depends on those only.
    pub fn traits_in_scope<'a>(
        &'a self,
        db: &'a dyn SourceDatabase,
    ) -> impl Iterator<Item = crate::TraitId> + 'a {
        let (building, collected) = match &self.scope {
            ScopeSlot::Building { scope, .. } => (Some(scope.traits()), None),
            ScopeSlot::Collected(id) => (
                None,
                Some(
                    <ModuleScope<'a> as salsa::plumbing::FromId>::from_id(*id)
                        .traits(db)
                        .iter()
                        .copied(),
                ),
            ),
        };
        building.into_iter().flatten().chain(collected.into_iter().flatten())
    }

    /// The modules declared in this one, by name.
    pub fn children<'a>(&'a self, db: &'a dyn SourceDatabase) -> &'a FxIndexMap<Name, ModuleId> {
        match &self.scope {
            ScopeSlot::Building { children, .. } => children,
            ScopeSlot::Collected(id) => {
                <ModuleScope<'a> as salsa::plumbing::FromId>::from_id(*id).children(db)
            }
        }
    }

    pub(crate) fn children_mut(&mut self) -> &mut FxIndexMap<Name, ModuleId> {
        match &mut self.scope {
            ScopeSlot::Building { children, .. } => children,
            ScopeSlot::Collected(_) => panic!("the children of a collected module are read-only"),
        }
    }

    /// The scope of a module of the `DefMap` being collected.
    pub(crate) fn scope_mut(&mut self) -> &mut ItemScope {
        match &mut self.scope {
            ScopeSlot::Building { scope, .. } => scope,
            ScopeSlot::Collected(_) => panic!("the scope of a collected module is read-only"),
        }
    }

    /// Returns a node which defines this module. That is, a file or a `mod foo {}` with items.
    pub fn definition_source(&self, db: &dyn SourceDatabase) -> InFile<ModuleSource> {
        self.origin.definition_source(db)
    }

    /// Same as [`ModuleData::definition_source`] but only returns the file id to prevent parsing the ASt.
    pub fn definition_source_file_id(&self) -> HirFileId {
        match self.origin {
            ModuleOrigin::File { definition, .. } | ModuleOrigin::CrateRoot { definition } => {
                definition.into()
            }
            ModuleOrigin::Inline { definition_tree_id, .. } => definition_tree_id.file_id(),
            ModuleOrigin::BlockExpr { block, .. } => block.file_id,
        }
    }

    pub fn definition_source_range(&self, db: &dyn SourceDatabase) -> InFile<TextRange> {
        match &self.origin {
            &ModuleOrigin::File { definition, .. } | &ModuleOrigin::CrateRoot { definition } => {
                InFile::new(
                    definition.into(),
                    ErasedAstId::new(definition.into(), ROOT_ERASED_FILE_AST_ID).to_range(db),
                )
            }
            &ModuleOrigin::Inline { definition, definition_tree_id } => InFile::new(
                definition_tree_id.file_id(),
                AstId::new(definition_tree_id.file_id(), definition).to_range(db),
            ),
            ModuleOrigin::BlockExpr { block, .. } => InFile::new(block.file_id, block.to_range(db)),
        }
    }

    /// Returns a node which declares this module, either a `mod foo;` or a `mod foo {}`.
    /// `None` for the crate root or block.
    pub fn declaration_source(&self, db: &dyn SourceDatabase) -> Option<InFile<ast::Module>> {
        let decl = self.origin.declaration()?;
        let value = decl.to_node(db);
        Some(InFile { file_id: decl.file_id, value })
    }

    /// Returns the range which declares this module, either a `mod foo;` or a `mod foo {}`.
    /// `None` for the crate root or block.
    pub fn declaration_source_range(&self, db: &dyn SourceDatabase) -> Option<InFile<TextRange>> {
        let decl = self.origin.declaration()?;
        Some(InFile { file_id: decl.file_id, value: decl.to_range(db) })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleSource {
    SourceFile(ast::SourceFile),
    Module(ast::Module),
    BlockExpr(ast::BlockExpr),
}

impl ModuleSource {
    pub fn node(&self) -> SyntaxNode {
        match self {
            ModuleSource::SourceFile(it) => it.syntax().clone(),
            ModuleSource::Module(it) => it.syntax().clone(),
            ModuleSource::BlockExpr(it) => it.syntax().clone(),
        }
    }
}

/// See `sub_namespace_match()`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MacroSubNs {
    /// Function-like macros, suffixed with `!`.
    Bang,
    /// Macros inside attributes, i.e. attribute macros and derive macros.
    Attr,
}

pub(crate) fn macro_styles_from_id(db: &dyn SourceDatabase, macro_id: MacroId) -> MacroCallStyles {
    let expander = match macro_id {
        MacroId::Macro2Id(it) => it.lookup(db).expander,
        MacroId::MacroRulesId(it) => it.lookup(db).expander,
        MacroId::ProcMacroId(it) => {
            return match it.lookup(db).kind {
                ProcMacroKind::CustomDerive => MacroCallStyles::DERIVE,
                ProcMacroKind::Bang => MacroCallStyles::FN_LIKE,
                ProcMacroKind::Attr => MacroCallStyles::ATTR,
            };
        }
    };

    match expander {
        MacroExpander::Declarative { styles } => styles,
        // Eager macros aren't *guaranteed* to be bang macros, but they *are* all bang macros currently.
        MacroExpander::BuiltIn(_) | MacroExpander::BuiltInEager(_) => MacroCallStyles::FN_LIKE,
        MacroExpander::BuiltInAttr(_) => MacroCallStyles::ATTR,
        MacroExpander::BuiltInDerive(_) => MacroCallStyles::DERIVE,
        MacroExpander::UnimplementedBuiltIn => MacroCallStyles::all(), // Unknown.
    }
}

/// Quoted from [rustc]:
/// Macro namespace is separated into two sub-namespaces, one for bang macros and
/// one for attribute-like macros (attributes, derives).
/// We ignore resolutions from one sub-namespace when searching names in scope for another.
///
/// [rustc]: https://github.com/rust-lang/rust/blob/1.69.0/compiler/rustc_resolve/src/macros.rs#L75
fn sub_namespace_match(
    db: &dyn SourceDatabase,
    macro_id: MacroId,
    expected: Option<MacroSubNs>,
) -> bool {
    let candidate = macro_styles_from_id(db, macro_id);
    match expected {
        Some(MacroSubNs::Bang) => candidate.contains(MacroCallStyles::FN_LIKE),
        Some(MacroSubNs::Attr) => {
            candidate.contains(MacroCallStyles::ATTR) || candidate.contains(MacroCallStyles::DERIVE)
        }
        // If we aren't expecting a specific sub-namespace
        // (e.g. in `use` declarations), match any macro.
        None => true,
    }
}

/// A newtype wrapper around `FxHashMap<ModuleId, ModuleData>` that implements `IndexMut`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ModulesMap {
    inner: FxIndexMap<ModuleId, ModuleData>,
}

impl ModulesMap {
    fn new() -> Self {
        Self { inner: FxIndexMap::default() }
    }

    fn iter(&self) -> impl Iterator<Item = (ModuleId, &ModuleData)> + '_ {
        self.inner.iter().map(|(&k, v)| (k, v))
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = (ModuleId, &mut ModuleData)> + '_ {
        self.inner.iter_mut().map(|(&k, v)| (k, v))
    }
}

impl Deref for ModulesMap {
    type Target = FxIndexMap<ModuleId, ModuleData>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for ModulesMap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Index<ModuleIdLt<'_>> for ModulesMap {
    type Output = ModuleData;

    fn index(&self, id: ModuleIdLt<'_>) -> &ModuleData {
        self.inner
            .get(&unsafe { id.to_static() })
            .unwrap_or_else(|| panic!("ModuleId not found in ModulesMap: {id:#?}"))
    }
}

impl IndexMut<ModuleId> for ModulesMap {
    fn index_mut(&mut self, id: ModuleId) -> &mut ModuleData {
        self.inner
            .get_mut(&id)
            .unwrap_or_else(|| panic!("ModuleId not found in ModulesMap: {id:#?}"))
    }
}
