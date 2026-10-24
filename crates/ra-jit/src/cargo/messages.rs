//! What `--message-format` has `cargo ra-jit check` write of the daemon's verdict: the errors
//! as rustc writes them, a line each, or in cargo's JSON messages, which an editor reads as
//! it reads those of `cargo check`.

use std::path::Path;

use cargo_metadata::{Metadata, Package, Target, TargetKind};
use serde::Serialize;

use crate::errors::{Diagnostic, RustcDiagnostic, Said};

/// What the values of `--message-format` ask for.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) enum Format {
    #[default]
    Human,
    Short,
    Json {
        /// `json-diagnostic-short`: `rendered` is the line `short` writes.
        short: bool,
        /// `json-diagnostic-rendered-ansi`: `rendered` is in colour.
        ansi: bool,
        /// `json-render-diagnostics`: the errors are written as text, and not as messages.
        render: bool,
    },
}

impl Format {
    /// The format `values` ask for, read as cargo reads them, or cargo's error for them.
    pub(crate) fn of(values: &[String]) -> Result<Format, String> {
        fn two() -> String {
            "cannot specify two kinds of `message-format` arguments".to_owned()
        }
        fn json(format: &mut Option<Format>) -> Result<(&mut bool, &mut bool, &mut bool), String> {
            let json = Format::Json { short: false, ansi: false, render: false };
            match format.get_or_insert(json) {
                Format::Json { short, ansi, render } => Ok((short, ansi, render)),
                _ => Err(two()),
            }
        }
        let mut format = None;
        for value in values.iter().flat_map(|it| it.split(',')) {
            let value = value.to_ascii_lowercase();
            match value.as_str() {
                "human" | "short" | "json" if format.is_some() => return Err(two()),
                "human" => format = Some(Format::Human),
                "short" => format = Some(Format::Short),
                "json" => _ = json(&mut format)?,
                "json-diagnostic-short" => *json(&mut format)?.0 = true,
                "json-diagnostic-rendered-ansi" => *json(&mut format)?.1 = true,
                "json-render-diagnostics" => *json(&mut format)?.2 = true,
                other => return Err(format!("invalid message format specifier: `{other}`")),
            }
        }
        Ok(format.unwrap_or_default())
    }

    pub(crate) fn is_json(self) -> bool {
        matches!(self, Format::Json { .. })
    }
}

/// Says that the check found nothing wrong, as cargo ends a build: in JSON, that it finished.
pub(crate) fn passed(format: Format) {
    if format.is_json() {
        println!(r#"{{"reason":"build-finished","success":true}}"#);
    }
}

/// Writes `said`, the errors of a check, as `format` asks, and ends as cargo ends a build
/// rustc turned down: with a line for each crate that has an error, on stderr.
/// `metadata` is the workspace's, which says what package and target each error is of.
pub(crate) fn turned_down(format: Format, said: &[Said], metadata: &Metadata) {
    let units = Units::of(metadata);
    let mut failed: Vec<(usize, Vec<&Diagnostic>)> = Vec::new();
    for said in said {
        let mut of_unit: Vec<(usize, Vec<&Diagnostic>)> = Vec::new();
        for diagnostic in &said.0 {
            let unit = units.of_diagnostic(diagnostic, &metadata.workspace_root);
            match of_unit.iter_mut().find(|(it, _)| *it == unit) {
                Some((_, them)) => them.push(diagnostic),
                None => of_unit.push((unit, vec![diagnostic])),
            }
        }
        failed.extend(of_unit);
    }
    for (unit, diagnostics) in &failed {
        let said = Said(diagnostics.iter().map(|it| (*it).clone()).collect());
        let notes = said.notes();
        match format {
            Format::Human | Format::Json { render: true, .. } => {
                for diagnostic in &said.0 {
                    eprintln!("{}\n", diagnostic.rendered());
                }
                notes.iter().for_each(|it| eprintln!("{it}"));
            }
            Format::Short => said.0.iter().for_each(|it| eprintln!("{}", it.short())),
            Format::Json { short, ansi, render: false } => {
                for diagnostic in &said.0 {
                    let rendered = match (short, ansi) {
                        (true, _) => format!("{}\n", diagnostic.short()),
                        (false, true) => format!("{}\n\n", diagnostic.rendered_ansi()),
                        (false, false) => format!("{}\n\n", diagnostic.rendered()),
                    };
                    units.message(*unit, diagnostic.rustc_json(rendered));
                }
                for note in notes {
                    let rendered = match ansi {
                        true => format!("\x1b[1m{note}\x1b[0m\n"),
                        false => format!("{note}\n"),
                    };
                    units.message(*unit, failure_note(note, rendered));
                }
            }
        }
        let errors = match said.count() {
            1 => "1 previous error".to_owned(),
            n => format!("{n} previous errors"),
        };
        eprintln!("error: could not compile {} due to {errors}", units.named(*unit));
    }
    if format.is_json() {
        println!(r#"{{"reason":"build-finished","success":false}}"#);
    }
}

fn failure_note(message: String, rendered: String) -> RustcDiagnostic {
    RustcDiagnostic {
        message_type: "diagnostic",
        message,
        code: None,
        level: "failure-note",
        spans: Vec::new(),
        children: Vec::new(),
        rendered: Some(rendered),
    }
}

/// The targets of the packages of the workspace, which cargo builds a unit each of.
struct Units<'a> {
    targets: Vec<(&'a Package, &'a Target)>,
}

impl<'a> Units<'a> {
    fn of(metadata: &'a Metadata) -> Units<'a> {
        let packages = metadata.workspace_packages();
        let targets = packages.into_iter().flat_map(|package| {
            let targets = package.targets.iter().filter(|it| !it.is_custom_build());
            targets.map(move |target| (package, target))
        });
        Units { targets: targets.collect() }
    }

    /// The target whose crate `diagnostic` is in: the one whose root it is, else the one
    /// whose directory is the nearest above it, a library before a program. An error of no
    /// one place is of the first target.
    fn of_diagnostic(&self, diagnostic: &Diagnostic, root: impl AsRef<Path>) -> usize {
        let Some(span) = &diagnostic.span else { return 0 };
        let file = root.as_ref().join(&span.file_name);
        let rank = |(_, target): &(&Package, &Target)| {
            let src: &Path = target.src_path.as_ref();
            let dir = src.parent().filter(|it| file.starts_with(it))?;
            let first = [TargetKind::Lib, TargetKind::Bin, TargetKind::Example, TargetKind::Test];
            let kind = first.iter().position(|it| target.kind.contains(it)).unwrap_or(first.len());
            Some((src == file, dir.components().count(), std::cmp::Reverse(kind)))
        };
        let ranked = self.targets.iter().enumerate().filter_map(|(at, it)| Some((rank(it)?, at)));
        ranked.max_by_key(|(rank, at)| (*rank, std::cmp::Reverse(*at))).map_or(0, |(_, at)| at)
    }

    /// The unit as cargo names it: `` `app` (bin "app") ``.
    fn named(&self, unit: usize) -> String {
        let Some((package, target)) = self.targets.get(unit) else {
            return "the workspace".to_owned();
        };
        let kind = match target.kind.first() {
            Some(TargetKind::Lib | TargetKind::RLib | TargetKind::DyLib | TargetKind::CDyLib)
            | Some(TargetKind::StaticLib | TargetKind::ProcMacro) => "lib".to_owned(),
            Some(TargetKind::Bin) => format!("bin \"{}\"", target.name),
            Some(TargetKind::Example) => format!("example \"{}\"", target.name),
            Some(TargetKind::Test) => format!("test \"{}\"", target.name),
            Some(TargetKind::Bench) => format!("bench \"{}\"", target.name),
            _ => format!("\"{}\"", target.name),
        };
        format!("`{}` ({kind})", package.name)
    }

    /// Writes `message` as cargo's `compiler-message` of the unit.
    fn message(&self, unit: usize, message: RustcDiagnostic) {
        let Some((package, target)) = self.targets.get(unit) else { return };
        let message = Message {
            reason: "compiler-message",
            package_id: &package.id.repr,
            manifest_path: package.manifest_path.as_str(),
            target: OfTarget {
                kind: &target.kind,
                crate_types: &target.crate_types,
                name: &target.name,
                src_path: target.src_path.as_str(),
                edition: target.edition.as_str(),
                doc: target.doc,
                doctest: target.doctest,
                test: target.test,
            },
            message,
        };
        if let Ok(line) = serde_json::to_string(&message) {
            println!("{line}");
        }
    }
}

/// A line of cargo's JSON with a message of rustc's, its fields in cargo's order.
#[derive(Serialize)]
struct Message<'a> {
    reason: &'static str,
    package_id: &'a str,
    manifest_path: &'a str,
    target: OfTarget<'a>,
    message: RustcDiagnostic,
}

#[derive(Serialize)]
struct OfTarget<'a> {
    kind: &'a [TargetKind],
    crate_types: &'a [cargo_metadata::CrateType],
    name: &'a str,
    src_path: &'a str,
    edition: &'a str,
    doc: bool,
    doctest: bool,
    test: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(values: &[&str]) -> Result<Format, String> {
        Format::of(&values.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn as_cargo_reads_it() {
        assert_eq!(of(&[]), Ok(Format::Human));
        assert_eq!(of(&["short"]), Ok(Format::Short));
        let json = |short, ansi, render| Ok(Format::Json { short, ansi, render });
        assert_eq!(of(&["json"]), json(false, false, false));
        assert_eq!(of(&["json-diagnostic-rendered-ansi"]), json(false, true, false));
        assert_eq!(of(&["json-diagnostic-short,json-render-diagnostics"]), json(true, false, true));
        assert_eq!(of(&["JSON", "json-diagnostic-short"]), json(true, false, false));
        assert!(of(&["json", "human"]).is_err());
        assert!(of(&["short", "json-diagnostic-short"]).is_err());
        assert_eq!(of(&["xml"]), Err("invalid message format specifier: `xml`".to_owned()));
    }
}
