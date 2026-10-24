//! Errors in the source, written as rustc writes them: what is wrong, where, and the line
//! with what is wrong marked under it. Each is also what rustc's `--error-format=json` says
//! of it, which is what an editor reads of `cargo check --message-format=json`.

use serde::{Deserialize, Serialize};
use syntax::TextRange;

/// The errors of a verdict on the source, in the order they are written.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Said(pub(crate) Vec<Diagnostic>);

/// An error, and where it is when it is of one place.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Diagnostic {
    /// rustc's, as `E0308`, for the ones that have one.
    pub(crate) code: Option<String>,
    pub(crate) message: String,
    pub(crate) span: Option<Span>,
}

/// A range of a file as rustc's JSON has it: bytes from 0, lines and columns from 1, a
/// column counting chars.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Span {
    /// As the error shows it: from the root of the workspace when the file is below it.
    pub(crate) file_name: String,
    pub(crate) byte_start: usize,
    pub(crate) byte_end: usize,
    pub(crate) line_start: usize,
    pub(crate) line_end: usize,
    pub(crate) column_start: usize,
    pub(crate) column_end: usize,
    /// Each line the range is on, and the columns of it on that line.
    pub(crate) text: Vec<Line>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Line {
    pub(crate) text: String,
    pub(crate) highlight_start: usize,
    pub(crate) highlight_end: usize,
}

/// An error at `range` of `text`, the file shown as `path`. `code` is rustc's, as `E0308`,
/// for the ones that have one.
pub(crate) fn at(
    code: Option<&str>,
    message: &str,
    path: &str,
    text: &str,
    range: TextRange,
) -> Diagnostic {
    let start = floor(text, usize::from(range.start()));
    let end = floor(text, usize::from(range.end())).max(start);
    let (line_start, column_start) = position(text, start);
    let (line_end, column_end) = position(text, end);
    let first = text[..start].rfind('\n').map_or(0, |it| it + 1);
    let lines = text[first..].split('\n').take(line_end - line_start + 1);
    let lines = lines.enumerate().map(|(at, line)| {
        let line = line.trim_end_matches('\r');
        Line {
            text: line.to_owned(),
            highlight_start: if at == 0 { column_start } else { 1 },
            highlight_end: match at == line_end - line_start {
                true => column_end,
                false => line.chars().count() + 1,
            },
        }
    });
    let span = Span {
        file_name: path.to_owned(),
        byte_start: start,
        byte_end: end,
        line_start,
        line_end,
        column_start,
        column_end,
        text: lines.collect(),
    };
    Diagnostic { code: code.map(str::to_owned), message: message.to_owned(), span: Some(span) }
}

/// An error that is of no one place.
pub(crate) fn of_none(message: &str) -> Diagnostic {
    Diagnostic { code: None, message: message.to_owned(), span: None }
}

/// The line and the column of byte `at` of `text`.
fn position(text: &str, at: usize) -> (usize, usize) {
    let line_start = text[..at].rfind('\n').map_or(0, |it| it + 1);
    (text[..at].matches('\n').count() + 1, text[line_start..at].chars().count() + 1)
}

impl Diagnostic {
    /// As rustc writes it, with the first line of where it is and that marked.
    pub(crate) fn rendered(&self) -> String {
        let header = self.header();
        let Some(span) = &self.span else { return header };
        let Some(line) = span.text.first() else { return header };
        // rustc shows a tab as four spaces, and marks what is under it to match.
        let width = |it: &str| it.chars().map(|c| if c == '\t' { 4 } else { 1 }).sum::<usize>();
        let chars = |from: usize, to: usize| -> String {
            line.text.chars().skip(from - 1).take(to.saturating_sub(from)).collect()
        };
        let before = width(&chars(1, line.highlight_start));
        let marked = width(&chars(line.highlight_start, line.highlight_end)).max(1);
        let number = span.line_start;
        let pad = " ".repeat(number.to_string().len());
        format!(
            "{header}\n{pad}--> {}:{number}:{}\n{pad} |\n{number} | {}\n{pad} | {}{}",
            span.file_name,
            span.column_start,
            line.text.replace('\t', "    "),
            " ".repeat(before),
            "^".repeat(marked),
        )
    }

    /// As rustc writes it to a terminal, in colour: `--message-format=json-diagnostic-rendered-ansi`.
    pub(crate) fn rendered_ansi(&self) -> String {
        const BOLD: &str = "\x1b[1m";
        const RED: &str = "\x1b[1m\x1b[91m";
        const BLUE: &str = "\x1b[1m\x1b[94m";
        const RESET: &str = "\x1b[0m";
        let level = match &self.code {
            Some(code) => format!("error[{code}]"),
            None => "error".to_owned(),
        };
        let header = format!("{RED}{level}{RESET}{BOLD}: {}{RESET}", self.message);
        let plain = self.rendered();
        let mut lines = plain.lines().skip(1);
        let (Some(arrow), Some(_), Some(source), Some(marker)) =
            (lines.next(), lines.next(), lines.next(), lines.next())
        else {
            return header;
        };
        let Some(span) = &self.span else { return header };
        let number = span.line_start.to_string();
        let pad = " ".repeat(number.len());
        let at = arrow.trim_start().trim_start_matches("--> ");
        let source = &source[number.len() + 2..];
        let under = &marker[pad.len() + 2..];
        let carets = under.trim_start();
        let before = &under[..under.len() - carets.len()];
        format!(
            "{header}\n{pad}{BLUE}--> {RESET}{at}\n{pad} {BLUE}|{RESET}\n{BLUE}{number}{RESET} \
             {BLUE}|{RESET}{source}\n{pad} {BLUE}|{RESET}{before}{RED}{carets}{RESET}"
        )
    }

    /// As rustc's `--error-format=short` writes it.
    pub(crate) fn short(&self) -> String {
        match &self.span {
            Some(span) => format!(
                "{}:{}:{}: {}",
                span.file_name,
                span.line_start,
                span.column_start,
                self.header()
            ),
            None => self.header(),
        }
    }

    fn header(&self) -> String {
        match &self.code {
            Some(code) => format!("error[{code}]: {}", self.message),
            None => format!("error: {}", self.message),
        }
    }

    /// What rustc's `--error-format=json` writes of it, with `rendered` as the text of it
    /// the format asks for.
    pub(crate) fn rustc_json(&self, rendered: String) -> RustcDiagnostic {
        let spans = self.span.iter().map(|span| RustcSpan {
            file_name: span.file_name.clone(),
            byte_start: span.byte_start,
            byte_end: span.byte_end,
            line_start: span.line_start,
            line_end: span.line_end,
            column_start: span.column_start,
            column_end: span.column_end,
            is_primary: true,
            text: span.text.clone(),
            label: None,
            suggested_replacement: None,
            suggestion_applicability: None,
            expansion: None,
        });
        RustcDiagnostic {
            message_type: "diagnostic",
            message: self.message.clone(),
            code: self.code.clone().map(|code| RustcCode { code, explanation: None }),
            level: "error",
            spans: spans.collect(),
            children: Vec::new(),
            rendered: Some(rendered),
        }
    }
}

/// A diagnostic as rustc's JSON has it, its fields in rustc's order.
#[derive(Serialize, Debug, PartialEq)]
pub(crate) struct RustcDiagnostic {
    #[serde(rename = "$message_type")]
    pub(crate) message_type: &'static str,
    pub(crate) message: String,
    pub(crate) code: Option<RustcCode>,
    pub(crate) level: &'static str,
    pub(crate) spans: Vec<RustcSpan>,
    pub(crate) children: Vec<RustcDiagnostic>,
    pub(crate) rendered: Option<String>,
}

/// A code and the text `rustc --explain` has of it, which only rustc has.
#[derive(Serialize, Debug, PartialEq)]
pub(crate) struct RustcCode {
    pub(crate) code: String,
    pub(crate) explanation: Option<String>,
}

#[derive(Serialize, Debug, PartialEq)]
pub(crate) struct RustcSpan {
    pub(crate) file_name: String,
    pub(crate) byte_start: usize,
    pub(crate) byte_end: usize,
    pub(crate) line_start: usize,
    pub(crate) line_end: usize,
    pub(crate) column_start: usize,
    pub(crate) column_end: usize,
    pub(crate) is_primary: bool,
    pub(crate) text: Vec<Line>,
    pub(crate) label: Option<String>,
    pub(crate) suggested_replacement: Option<String>,
    pub(crate) suggestion_applicability: Option<String>,
    pub(crate) expansion: Option<()>,
}

impl Said {
    pub(crate) fn one(diagnostic: Diagnostic) -> Said {
        Said(vec![diagnostic])
    }

    /// How many errors it has.
    pub(crate) fn count(&self) -> usize {
        self.0.len()
    }

    /// What rustc says of a crate it turned down after its errors, under cargo: where to read
    /// of their codes.
    pub(crate) fn notes(&self) -> Vec<String> {
        let mut codes: Vec<&str> = self.0.iter().filter_map(|it| it.code.as_deref()).collect();
        codes.sort_unstable();
        codes.dedup();
        match codes[..] {
            [] => Vec::new(),
            [code] => {
                vec![format!(
                    "For more information about this error, try `rustc --explain {code}`."
                )]
            }
            [first, ..] => vec![
                format!("Some errors have detailed explanations: {}.", codes.join(", ")),
                format!("For more information about an error, try `rustc --explain {first}`."),
            ],
        }
    }

    /// The line a worker answers with: the errors as rustc writes them, which a client that
    /// reads no more shows, and then them as they are.
    pub(crate) fn reply(&self) -> String {
        let mut line = "error: ".to_owned();
        ra_jit_live::format::escape(&self.to_string(), &mut line);
        line.push_str(AS_THEY_ARE);
        line.push_str(&serde_json::to_string(self).unwrap_or_default());
        line
    }

    /// What [`Said::reply`] wrote after `error: `, or the text of a reply of no more than
    /// that.
    pub(crate) fn of_reply(after: &str) -> Said {
        let (text, as_they_are) = match after.split_once(AS_THEY_ARE) {
            Some((text, json)) => (text, serde_json::from_str(json).ok()),
            None => (after, None),
        };
        as_they_are.unwrap_or_else(|| {
            let text = ra_jit_live::format::unescape(text);
            Said::one(of_none(text.strip_prefix("error: ").unwrap_or(&text)))
        })
    }

    /// The text of a reply, for a client that reads no more.
    pub(crate) fn text_of_reply(line: &str) -> &str {
        line.split_once(AS_THEY_ARE).map_or(line, |(text, _)| text)
    }
}

/// What a worker's `error: ` line has after its text: the errors as they are, in JSON. No
/// escaped text and no JSON has a tab.
const AS_THEY_ARE: &str = "\tsaid=";

impl std::fmt::Display for Said {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (at, diagnostic) in self.0.iter().enumerate() {
            if at > 0 {
                f.write_str("\n\n")?;
            }
            f.write_str(&diagnostic.rendered())?;
        }
        Ok(())
    }
}

/// The last char boundary of `text` at or before `at`.
fn floor(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;
    use syntax::TextSize;

    fn range(text: &str, of: &str) -> TextRange {
        let start = text.find(of).unwrap();
        TextRange::at(TextSize::new(start as u32), TextSize::new(of.len() as u32))
    }

    #[test]
    fn as_rustc_writes_it() {
        let text = "fn main() {}\n\nfn broken() -> u32 { \"text\" }\n";
        let said = at(
            Some("E0308"),
            "expected u32, found &str",
            "app/src/main.rs",
            text,
            range(text, "\"text\""),
        );
        assert_eq!(
            said.rendered(),
            "error[E0308]: expected u32, found &str\n\
             \x20--> app/src/main.rs:3:22\n\
             \x20 |\n\
             3 | fn broken() -> u32 { \"text\" }\n\
             \x20 |                      ^^^^^^"
        );
        assert_eq!(said.short(), "app/src/main.rs:3:22: error[E0308]: expected u32, found &str");
        let span = said.span.unwrap();
        assert_eq!((span.byte_start, span.byte_end), (35, 41));
        assert_eq!(
            (span.line_start, span.column_start, span.line_end, span.column_end),
            (3, 22, 3, 28)
        );
        assert_eq!(
            span.text,
            [Line {
                text: "fn broken() -> u32 { \"text\" }".to_owned(),
                highlight_start: 22,
                highlight_end: 28
            }]
        );
    }

    #[test]
    fn tabs_lines_and_ranges_past_the_line() {
        let text = "\tlet x = é;\n\tfoo(\n";
        let said = at(None, "cannot find value", "a.rs", text, range(text, "é")).rendered();
        assert!(said.ends_with("1 |     let x = é;\n  |             ^"), "{said}");
        let text = format!("{}\n\tfoo(\n  bar)\n", "\n".repeat(8));
        let unclosed = at(None, "unclosed", "a.rs", &text, range(&text, "foo(\n  bar"));
        let said = unclosed.rendered();
        assert!(said.contains(" --> a.rs:10:2\n"), "{said}");
        assert!(said.ends_with("10 |     foo(\n   |     ^^^^"), "{said}");
        let span = unclosed.span.as_ref().unwrap();
        assert_eq!(
            (span.line_start, span.column_start, span.line_end, span.column_end),
            (10, 2, 11, 6)
        );
        let highlights: Vec<_> =
            span.text.iter().map(|it| (it.highlight_start, it.highlight_end)).collect();
        assert_eq!(highlights, [(2, 6), (1, 6)]);
        let said = Said(vec![unclosed, of_none("x")]);
        assert_eq!(said.count(), 2);
        assert!(said.to_string().ends_with("^^^^\n\nerror: x"));
    }

    #[test]
    fn a_reply_has_the_text_and_the_errors_as_they_are() {
        let text = "fn f() {\n\tg()\n}\n";
        let said = Said(vec![at(
            Some("E0425"),
            "cannot find function `g`",
            "src/lib.rs",
            text,
            range(text, "g"),
        )]);
        let reply = said.reply();
        let after = reply.strip_prefix("error: ").unwrap();
        assert!(!after.contains('\n'));
        assert_eq!(Said::of_reply(after), said);
        let text = ra_jit_live::format::unescape(Said::text_of_reply(after));
        assert_eq!(text, said.to_string());
        let old = Said::of_reply("error: the source does not compile");
        assert_eq!(old, Said::one(of_none("the source does not compile")));
    }

    #[test]
    fn rustc_ends_a_crate_it_turned_down_so() {
        let notes = |codes: &[Option<&str>]| {
            let said = codes.iter().map(|it| Diagnostic {
                code: it.map(str::to_owned),
                message: String::new(),
                span: None,
            });
            Said(said.collect()).notes()
        };
        assert!(notes(&[None]).is_empty());
        assert_eq!(
            notes(&[Some("E0308"), None, Some("E0308")]),
            ["For more information about this error, try `rustc --explain E0308`."]
        );
        assert_eq!(
            notes(&[Some("E0425"), Some("E0308")]),
            [
                "Some errors have detailed explanations: E0308, E0425.",
                "For more information about an error, try `rustc --explain E0308`."
            ]
        );
    }

    // What rustc 1.99 writes of `let _ = y;` with `y` unknown, but the help it adds.
    #[test]
    fn in_colour_as_rustc_writes_it() {
        let text = "fn main() {\n    let _ = y;\n}\n";
        let said = at(
            Some("E0425"),
            "cannot find value `y` in this scope",
            "src/main.rs",
            text,
            range(text, "y"),
        );
        assert_eq!(
            said.rendered_ansi(),
            "\x1b[1m\x1b[91merror[E0425]\x1b[0m\x1b[1m: cannot find value `y` in this scope\x1b[0m\n \
             \x1b[1m\x1b[94m--> \x1b[0msrc/main.rs:2:13\n  \x1b[1m\x1b[94m|\x1b[0m\n\x1b[1m\x1b[94m2\x1b[0m \
             \x1b[1m\x1b[94m|\x1b[0m     let _ = y;\n  \x1b[1m\x1b[94m|\x1b[0m             \x1b[1m\x1b[91m^\x1b[0m"
        );
        assert_eq!(of_none("x").rendered_ansi(), "\x1b[1m\x1b[91merror\x1b[0m\x1b[1m: x\x1b[0m");
    }
}
