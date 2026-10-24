//! What cargo builds: the units of a build of one package, with the features and the profile
//! of each. `cargo metadata` has neither as a build of a single package has them.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Stdio,
    str::FromStr,
};

use anyhow::Context;
use cargo_platform::{Cfg, Platform};
use cfg::{CfgAtom, CfgDiff};
use intern::{Symbol, sym};
use rustc_hash::{FxHashMap, FxHashSet};
use serde_derive::{Deserialize, Serialize};

/// What is said of a profile with `lto`. ra-jit compiles every function alone, so there is
/// nothing for it to optimize across functions, and the program does what rustc's does: only
/// how fast and how large it is differ. The libraries it links against are built without.
const LTO_ACCEPTED: &str = "lto: accepted, no cross-function optimization";

/// Which build of a package the daemon stands in for, as the flags of `cargo build` say it.
/// `cargo ra-jit` and `ra-jit` itself both say a build with this.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Selection {
    /// The name of the profile: `dev` unless `--release` or `--profile` says otherwise.
    pub(crate) profile: String,
    pub(crate) features: Vec<String>,
    pub(crate) no_default_features: bool,
    pub(crate) all_features: bool,
    /// The packages asked for with `-p`: none is what `cargo build` builds where it is run.
    pub(crate) packages: Vec<String>,
    /// Whether every member of the workspace is built.
    pub(crate) workspace: bool,
    /// The members `--exclude` leaves out of the workspace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) excludes: Vec<String>,
    /// The values of `-Z`, which cargo is run with as it was.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) unstable: Vec<String>,
    /// The triple built for, when it is not the host.
    pub(crate) target: Option<String>,
    /// Whether tests, examples and benchmarks are built, and so what they depend on.
    pub(crate) dev_deps: bool,
    /// The `--config` values given, which can change a profile or what is built as much as
    /// the configuration files can.
    #[serde(default)]
    pub(crate) config: Vec<String>,
    /// Where the build was asked for, when not in the package's directory: the manifest given
    /// with `--manifest-path`, or the directory cargo looked for one from. What `--workspace`,
    /// `-p` and no package at all select is decided there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) asked_in: Option<PathBuf>,
}

impl Default for Selection {
    fn default() -> Selection {
        Selection {
            profile: "dev".to_owned(),
            features: Vec::new(),
            no_default_features: false,
            all_features: false,
            packages: Vec::new(),
            workspace: false,
            excludes: Vec::new(),
            unstable: Vec::new(),
            target: None,
            dev_deps: false,
            config: Vec::new(),
            asked_in: None,
        }
    }
}

impl Selection {
    /// The selection as flags, which `cargo build` and `ra-jit` both read. Whether tests are
    /// built is not among them: cargo and ra-jit name that differently.
    pub(crate) fn flags(&self) -> Vec<String> {
        let mut flags = vec!["--profile".to_owned(), self.profile.clone()];
        if let Some(target) = &self.target {
            flags.extend(["--target".to_owned(), target.clone()]);
        }
        if !self.features.is_empty() {
            flags.extend(["--features".to_owned(), self.features.join(",")]);
        }
        if self.no_default_features {
            flags.push("--no-default-features".to_owned());
        }
        if self.all_features {
            flags.push("--all-features".to_owned());
        }
        for package in &self.packages {
            flags.extend(["--package".to_owned(), package.clone()]);
        }
        if self.workspace {
            flags.push("--workspace".to_owned());
        }
        for package in &self.excludes {
            flags.extend(["--exclude".to_owned(), package.clone()]);
        }
        for unstable in &self.unstable {
            flags.extend(["-Z".to_owned(), unstable.clone()]);
        }
        for config in &self.config {
            flags.extend(["--config".to_owned(), config.clone()]);
        }
        flags
    }
}

/// How a package is built for the target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Built {
    pub(crate) features: BTreeSet<String>,
    pub(crate) profile: Profile,
    /// Whether the package is one cargo was asked to build, not one those depend on.
    pub(crate) primary: bool,
    /// Whether the package is built into the program, not only for what runs while building.
    pub(crate) for_target: bool,
}

/// What a build is for and who asks for it.
pub(crate) struct Asked<'a> {
    /// The triple built for.
    pub(crate) triple: &'a str,
    /// What `rustc --print cfg` says of the triple.
    pub(crate) cfg: &'a [CfgAtom],
    /// The variables of the process that asks, where they are not those of this one.
    pub(crate) env: &'a FxHashMap<String, Option<String>>,
}

impl Asked<'_> {
    fn var(&self, name: &str) -> Option<String> {
        match self.env.get(name) {
            Some(value) => value.clone(),
            None => std::env::var(name).ok(),
        }
    }

    /// cargo, run in `dir` as whoever asks would run it.
    fn cargo(&self, dir: &Path) -> std::process::Command {
        let mut command = toolchain::command("cargo", dir, self.env);
        // The unit graph and `cargo config get` are not stable yet.
        command.env("RUSTC_BOOTSTRAP", "1").stdin(Stdio::null());
        crate::answers::ready_cargo(&mut command);
        command
    }

    /// Whether the key of a `[target]` table of cargo's configuration names what is built for.
    fn is(&self, key: &str) -> bool {
        if key == self.triple {
            return true;
        }
        let cfg: Vec<Cfg> = self
            .cfg
            .iter()
            .filter_map(|atom| match atom {
                CfgAtom::Flag(name) => Cfg::from_str(name.as_str()).ok(),
                CfgAtom::KeyValue { key, value } => {
                    Cfg::from_str(&format!("{key}={:?}", value.as_str())).ok()
                }
            })
            .collect();
        key.starts_with("cfg(")
            && Platform::from_str(key).is_ok_and(|it| it.matches(self.triple, &cfg))
    }
}

/// The settings of a profile that change what a program does.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub(crate) struct Profile {
    opt_level: String,
    lto: String,
    pub(crate) debug_assertions: bool,
    pub(crate) overflow_checks: bool,
    pub(crate) panic: PanicStrategy,
    debuginfo: DebugLevel,
    split_debuginfo: Option<String>,
    strip: Strip,
}

/// A `debug` setting as cargo passes it on: a level by its number or by its name.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
enum DebugLevel {
    Number(u8),
    Named(String),
}

/// A `strip` setting. One cargo deferred is its own choice for a build without debug info,
/// which has nothing the setting would take out.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Strip {
    Resolved(Stripped),
    Deferred(Stripped),
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
enum Stripped {
    None,
    Named(String),
}

/// What a build tells a debugger of its code, and where that is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Described {
    pub(crate) level: Level,
    pub(crate) kept: Kept,
    /// Whether every function keeps its frame pointer (`-C force-frame-pointers`), for a
    /// profiler or the program's own backtraces to walk the frames by.
    pub(crate) frame_pointers: bool,
}

impl Described {
    pub(crate) const NOTHING: Described =
        Described { level: Level::Nothing, kept: Kept::Linked, frame_pointers: false };

    /// The most either tells, kept the furthest from the program either keeps it.
    fn or(self, other: Described) -> Described {
        Described {
            level: self.level.max(other.level),
            kept: self.kept.max(other.kept),
            frame_pointers: self.frame_pointers || other.frame_pointers,
        }
    }
}

/// How much a build tells a debugger of its code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Level {
    Nothing,
    /// Which line each instruction is of, and the functions.
    Lines,
    /// The variables and their types as well.
    Full,
}

/// Where what a debugger reads of the code is kept (`split-debuginfo`). The lines are with
/// the program always.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Kept {
    /// In the program, which the linker copies it into.
    Linked,
    /// In the objects the program was linked from, which the program names.
    Objects,
    /// In one package next to the program.
    Package,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PanicStrategy {
    Unwind,
    Abort,
    ImmediateAbort,
}

impl PanicStrategy {
    fn name(self) -> &'static str {
        match self {
            PanicStrategy::Unwind => "unwind",
            PanicStrategy::Abort | PanicStrategy::ImmediateAbort => "abort",
        }
    }
}

impl Profile {
    /// The profile as rustc ends up with it once `flags` follow what cargo passes for it.
    fn with(&self, flags: &Flags) -> Profile {
        // cargo names a check only where it is not what rustc has for the level by itself, so
        // a level among the flags changes the ones cargo left unnamed.
        let unoptimized = self.opt_level == "0";
        let named_assertions =
            (self.debug_assertions != unoptimized).then_some(self.debug_assertions);
        let named_overflow =
            (self.overflow_checks != self.debug_assertions).then_some(self.overflow_checks);
        let unoptimized = flags.opt_level.as_ref().map_or(unoptimized, |it| it == "0");
        let debug_assertions = flags.debug_assertions.or(named_assertions).unwrap_or(unoptimized);
        Profile {
            debug_assertions,
            overflow_checks: flags.overflow_checks.or(named_overflow).unwrap_or(debug_assertions),
            panic: flags.panic.unwrap_or(self.panic),
            debuginfo: flags.debuginfo.clone().unwrap_or_else(|| self.debuginfo.clone()),
            split_debuginfo: flags.split_debuginfo.clone().or_else(|| self.split_debuginfo.clone()),
            strip: flags.strip.clone().map_or_else(
                || self.strip.clone(),
                |named| Strip::Resolved(Stripped::Named(named)),
            ),
            ..self.clone()
        }
    }

    /// What the profile has a debugger told. Where the profile asks for it to be kept apart
    /// from the program, which ra-jit does not do, that is the reason not to build.
    pub(crate) fn described(&self) -> anyhow::Result<Described> {
        let level = match &self.debuginfo {
            DebugLevel::Number(0) => Level::Nothing,
            DebugLevel::Number(1) => Level::Lines,
            DebugLevel::Number(2) => Level::Full,
            DebugLevel::Named(name) => match name.as_str() {
                "none" => Level::Nothing,
                "line-directives-only" | "line-tables-only" | "limited" => Level::Lines,
                "full" => Level::Full,
                _ => anyhow::bail!("`debug = \"{name}\"`: ra-jit does not know the level"),
            },
            DebugLevel::Number(level) => {
                anyhow::bail!("`debug = {level}`: ra-jit does not know the level")
            }
        };
        // Stripping the symbols is the linker's to do; of what ra-jit writes, what a debugger
        // reads is all either setting takes out.
        let stripped = match &self.strip {
            Strip::Resolved(Stripped::Named(what)) => match what.as_str() {
                "none" => false,
                "debuginfo" | "symbols" => true,
                _ => anyhow::bail!("`strip = \"{what}\"`: ra-jit does not know the setting"),
            },
            Strip::Resolved(Stripped::None) | Strip::Deferred(_) => false,
        };
        if stripped || level == Level::Nothing {
            return Ok(Described::NOTHING);
        }
        let kept = match self.split_debuginfo.as_deref() {
            None | Some("off") => Kept::Linked,
            Some("unpacked") => Kept::Objects,
            Some("packed") => Kept::Package,
            Some(split) => {
                anyhow::bail!("`split-debuginfo = \"{split}\"`: ra-jit does not know the setting")
            }
        };
        Ok(Described { level, kept, frame_pointers: false })
    }

    /// Whether rustc compiles the whole program again when it links it.
    fn links_from_bitcode(&self) -> bool {
        matches!(self.lto.as_str(), "fat" | "thin" | "true")
    }

    /// What the profile changes of the cfg `rustc --print cfg -O` reports.
    pub(crate) fn cfg(&self) -> CfgDiff {
        let flag = |name: Symbol| CfgAtom::Flag(name);
        let panic = |strategy: PanicStrategy| CfgAtom::KeyValue {
            key: sym::panic,
            value: Symbol::intern(strategy.name()),
        };
        let (mut enable, mut disable) = (Vec::new(), Vec::new());
        let mut set =
            |on: bool, atom: CfgAtom| if on { enable.push(atom) } else { disable.push(atom) };
        set(self.debug_assertions, flag(sym::debug_assertions));
        set(self.overflow_checks, flag(sym::overflow_checks));
        for strategy in [PanicStrategy::Unwind, PanicStrategy::Abort] {
            set(strategy.name() == self.panic.name(), panic(strategy));
        }
        CfgDiff::new(enable, disable)
    }
}

/// What the flags given to every rustc of a build say of what a profile has too. They come
/// after the profile's own on the command line and so win.
#[derive(Debug, Default, PartialEq, Eq)]
struct Flags {
    opt_level: Option<String>,
    debug_assertions: Option<bool>,
    overflow_checks: Option<bool>,
    panic: Option<PanicStrategy>,
    lto: bool,
    debuginfo: Option<DebugLevel>,
    split_debuginfo: Option<String>,
    strip: Option<String>,
    frame_pointers: bool,
    lints_capped: bool,
}

impl Flags {
    /// The flags cargo passes in `dir` for what is `asked`: the first of
    /// `CARGO_ENCODED_RUSTFLAGS`, `RUSTFLAGS`, the `[target]` tables that name what is built
    /// for and `[build]` of the configuration that has any.
    fn of(dir: &Path, asked: &Asked<'_>) -> anyhow::Result<Flags> {
        if let Some(encoded) = asked.var("CARGO_ENCODED_RUSTFLAGS") {
            return Ok(Flags::parse(encoded.split('\u{1f}')));
        }
        if let Some(flags) = asked.var("RUSTFLAGS") {
            return Ok(Flags::parse(flags.split_whitespace()));
        }
        let mut command = asked.cargo(dir);
        // `cargo config get` is how cargo says what its configuration files and variables
        // come to.
        command.args(["-Z", "unstable-options", "config", "get", "--format", "json"]);
        let output = command.output().context("running `cargo config`")?;
        anyhow::ensure!(
            output.status.success(),
            "`cargo config get` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let config: Config =
            serde_json::from_slice(&output.stdout).context("reading cargo's configuration")?;
        // The table of the triple, then those of a cfg in the order of their keys.
        let mut tables: Vec<_> = config.target.iter().filter(|(key, _)| asked.is(key)).collect();
        tables.sort_by_key(|(key, _)| (key.starts_with("cfg("), key.as_str()));
        let of_target: Vec<&str> = tables
            .iter()
            .filter_map(|(_, table)| table.rustflags.as_ref())
            .flat_map(Words::words)
            .collect();
        if !of_target.is_empty() {
            return Ok(Flags::parse(of_target.into_iter()));
        }
        Ok(Flags::parse(config.build.rustflags.iter().flat_map(Words::words)))
    }

    fn parse<'a>(mut words: impl Iterator<Item = &'a str>) -> Flags {
        let mut flags = Flags::default();
        while let Some(word) = words.next() {
            if ["--cap-lints", "--force-warn"].iter().any(|it| {
                word.strip_prefix(it).is_some_and(|rest| rest.is_empty() || rest.starts_with('='))
            }) {
                flags.lints_capped = true;
                continue;
            }
            let option = match word {
                "-O" => "opt-level=3",
                "-g" => "debuginfo=2",
                "-C" => words.next().unwrap_or_default(),
                _ => match word.strip_prefix("-C") {
                    Some(option) => option,
                    None => continue,
                },
            };
            let (name, value) = option.split_once('=').unwrap_or((option, ""));
            let on = match value {
                "y" | "yes" | "on" | "true" | "" => Some(true),
                "n" | "no" | "off" | "false" => Some(false),
                _ => None,
            };
            match name.replace('_', "-").as_str() {
                "opt-level" => flags.opt_level = Some(value.to_owned()),
                "debug-assertions" => flags.debug_assertions = on,
                "overflow-checks" => flags.overflow_checks = on,
                "panic" if value == "abort" => flags.panic = Some(PanicStrategy::Abort),
                "panic" if value == "unwind" => flags.panic = Some(PanicStrategy::Unwind),
                "lto" => flags.lto = on != Some(false),
                "linker-plugin-lto" => flags.lto = on != Some(false),
                "debuginfo" => {
                    flags.debuginfo = Some(match value.parse() {
                        Ok(level) => DebugLevel::Number(level),
                        Err(_) => DebugLevel::Named(value.to_owned()),
                    });
                }
                "split-debuginfo" => flags.split_debuginfo = Some(value.to_owned()),
                "strip" => flags.strip = Some(value.to_owned()),
                // `always` and `non-leaf` keep them too, `non-leaf` in more functions than asked.
                "force-frame-pointers" => {
                    flags.frame_pointers = on.unwrap_or(matches!(value, "always" | "non-leaf"));
                }
                _ => {}
            }
        }
        flags
    }
}

/// The part of cargo's configuration that has flags for rustc.
#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    build: ConfigFlags,
    #[serde(default)]
    target: FxHashMap<String, ConfigFlags>,
}

#[derive(Deserialize, Default)]
struct ConfigFlags {
    rustflags: Option<Words>,
}

/// Flags as a configuration has them: in one string, or one by one.
#[derive(Deserialize)]
#[serde(untagged)]
enum Words {
    Joined(String),
    Each(Vec<String>),
}

impl Words {
    fn words(&self) -> Box<dyn Iterator<Item = &str> + '_> {
        match self {
            Words::Joined(joined) => Box::new(joined.split_whitespace()),
            Words::Each(each) => Box::new(each.iter().map(String::as_str)),
        }
    }
}

/// The packages of a build by their id, each as it is built for the target.
pub(crate) struct Plan {
    packages: FxHashMap<String, Built>,
    lto: bool,
    frame_pointers: bool,
    lints_capped: bool,
}

/// What a plan says of a program as a whole.
#[derive(Clone, Copy)]
pub(crate) struct Whole {
    /// What the build has a debugger told of the code.
    pub(crate) debug: Described,
    /// Whether rustc links it from bitcode, with `lto`.
    pub(crate) lto: bool,
    /// Whether `--cap-lints` or `--force-warn` is among the flags, so that a lint the source
    /// denies may not fail rustc's build.
    pub(crate) lints_capped: bool,
}

impl Plan {
    /// Asks cargo for the units of the build `selection` names of the package of `manifest`.
    pub(crate) fn of(
        manifest: &Path,
        selection: &Selection,
        asked: &Asked<'_>,
    ) -> anyhow::Result<Plan> {
        // The unit graph is the only place cargo says which features and which profile each
        // package of one build has.
        let (dir, manifest) = match &selection.asked_in {
            Some(asked_in) if asked_in.is_dir() => (asked_in.as_path(), None),
            Some(asked_in) => {
                (asked_in.parent().unwrap_or(Path::new(".")), Some(asked_in.as_path()))
            }
            None => (manifest.parent().unwrap_or(Path::new(".")), Some(manifest)),
        };
        let mut command = asked.cargo(dir);
        command.args(["build", "--unit-graph", "-Z", "unstable-options"]);
        if let Some(manifest) = manifest {
            command.arg("--manifest-path").arg(manifest);
        }
        command.args(selection.flags());
        if selection.dev_deps {
            command.arg("--all-targets");
        }
        let output = command.output().context("running `cargo build`")?;
        anyhow::ensure!(
            output.status.success(),
            "`cargo build --unit-graph` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let graph: UnitGraph =
            serde_json::from_slice(&output.stdout).context("reading cargo's unit graph")?;
        graph.plan(&Flags::of(dir, asked)?)
    }

    /// What the build has a debugger told of the packages cargo was asked to build: the
    /// most any of them asks for, which is one answer for all the code ra-jit writes.
    pub(crate) fn described(&self) -> anyhow::Result<Described> {
        let mut described = Described::NOTHING;
        for built in self.packages.values().filter(|it| it.primary) {
            described = described.or(built.profile.described()?);
        }
        Ok(described)
    }

    /// What the plan says of the programs cargo was asked to build.
    pub(crate) fn whole(&self) -> anyhow::Result<Whole> {
        let debug = Described { frame_pointers: self.frame_pointers, ..self.described()? };
        Ok(Whole { debug, lto: self.lto, lints_capped: self.lints_capped })
    }

    /// How the package cargo calls `id` is built, when the build has it.
    pub(crate) fn package(&self, id: &str) -> Option<&Built> {
        self.packages.get(&package_named(id))
    }

    /// The profile of the packages cargo was asked to build, for the target.
    pub(crate) fn target_profile(&self) -> Option<&Profile> {
        self.packages.values().find(|it| it.primary && it.for_target).map(|it| &it.profile)
    }
}

/// The one name of a package for the two ways cargo spells its id: `source#name@version`,
/// and `name version (source)` as the unit graph of an older cargo does, with the
/// commit of a git source after it.
fn package_named(id: &str) -> String {
    let old = id.strip_suffix(')').and_then(|it| it.split_once(" ("));
    if let Some((name_version, source)) = old
        && let Some((name, version)) = name_version.split_once(' ')
    {
        let source = source.split_once('#').map_or(source, |it| it.0);
        return format!("{source}#{name}@{version}");
    }
    let Some((source, named)) = id.split_once('#') else { return id.to_owned() };
    if named.contains('@') {
        return id.to_owned();
    }
    // A package whose directory or repository is called what it is has only its version.
    let path = source.split_once('?').map_or(source, |it| it.0);
    let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or(path);
    let name = name.strip_suffix(".git").unwrap_or(name);
    format!("{source}#{name}@{named}")
}

#[derive(Deserialize)]
struct UnitGraph {
    units: Vec<Unit>,
    roots: Vec<usize>,
}

#[derive(Deserialize)]
struct Unit {
    pkg_id: String,
    target: UnitTarget,
    profile: Profile,
    features: Vec<String>,
    dependencies: Vec<UnitDependency>,
}

#[derive(Deserialize)]
struct UnitTarget {
    kind: Vec<String>,
}

#[derive(Deserialize)]
struct UnitDependency {
    index: usize,
}

impl Unit {
    /// Whether the unit runs while building: a build script or a proc macro. What it depends
    /// on is built for the host, with features and a profile of its own.
    fn runs_on_host(&self) -> bool {
        self.target.kind.iter().any(|it| it == "custom-build" || it == "proc-macro")
    }

    fn is_library(&self) -> bool {
        self.target.kind.iter().any(|it| it.ends_with("lib"))
    }
}

impl UnitGraph {
    /// The units that end up in the program: those the roots reach without going through one
    /// that runs on the host.
    fn for_target(&self) -> FxHashSet<usize> {
        let mut reached = FxHashSet::default();
        let mut queue: Vec<usize> =
            self.roots.iter().copied().filter(|&it| it < self.units.len()).collect();
        while let Some(index) = queue.pop() {
            let unit = &self.units[index];
            if unit.runs_on_host() || !reached.insert(index) {
                continue;
            }
            queue.extend(
                unit.dependencies.iter().map(|it| it.index).filter(|&it| it < self.units.len()),
            );
        }
        reached
    }

    fn plan(self, flags: &Flags) -> anyhow::Result<Plan> {
        let for_target = self.for_target();
        // The flag is not cargo's to know: the libraries are then built for a link without it
        // and rustc finds no bitcode in them.
        anyhow::ensure!(
            !flags.lto,
            "`-C lto` is among the flags: rustc compiles the whole program again as it links \
             it and leaves no library on the link line for ra-jit's functions to be linked against"
        );
        let lto = for_target.iter().any(|&it| self.units[it].profile.links_from_bitcode());
        if lto {
            eprintln!("{LTO_ACCEPTED}");
        }
        // A package that is only built for the host, a proc macro, is as that build has it.
        let rank = |index: usize, unit: &Unit| (for_target.contains(&index), unit.is_library());
        let mut best: FxHashMap<&str, (usize, &Unit)> = FxHashMap::default();
        for (index, unit) in self.units.iter().enumerate() {
            let chosen = best.entry(unit.pkg_id.as_str()).or_insert((index, unit));
            if rank(index, unit) > rank(chosen.0, chosen.1) {
                *chosen = (index, unit);
            }
        }
        let roots: FxHashSet<&str> =
            self.roots.iter().filter_map(|&it| Some(self.units.get(it)?.pkg_id.as_str())).collect();
        let packages = best
            .into_iter()
            .map(|(id, (index, unit))| {
                let features = unit.features.iter().cloned().collect();
                let (profile, primary) = (unit.profile.with(flags), roots.contains(id));
                let for_target = for_target.contains(&index);
                (package_named(id), Built { features, profile, primary, for_target })
            })
            .collect();
        Ok(Plan {
            packages,
            lto,
            frame_pointers: flags.frame_pointers,
            lints_capped: flags.lints_capped,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Flags;

    #[test]
    fn force_frame_pointers_is_read_in_each_spelling() {
        let kept = |flags: &str| Flags::parse(flags.split_whitespace()).frame_pointers;
        assert!(kept("-Cforce-frame-pointers=yes"));
        assert!(kept("--cap-lints allow -C force-frame-pointers"));
        assert!(kept("-C force_frame_pointers=always"));
        assert!(kept("-Cforce-frame-pointers=non-leaf"));
        assert!(!kept("-Cforce-frame-pointers=yes -Cforce-frame-pointers=no"));
        assert!(!kept("-C link-dead-code"));
    }

    #[test]
    fn the_flags_that_cap_lints_are_read() {
        let capped = |flags: &str| Flags::parse(flags.split_whitespace()).lints_capped;
        assert!(capped("--cap-lints warn"));
        assert!(capped("-C opt-level=1 --cap-lints=allow"));
        assert!(capped("--force-warn non_snake_case"));
        assert!(!capped("-D warnings -A dead_code"));
        assert!(!capped("--cap-lintsx"));
    }
}
