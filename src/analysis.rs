//! One pass over the buffer — the single source for every panel.
//!
//! Before this, each keystroke ran the front end once per view: `lexer_dump`
//! lexed, `ast_dump` lexed again inside the parser, `diagnostics_dump` lexed and
//! analyzed, and `editor_diagnostics` analyzed a second time and lexed once more
//! for token widths. Six lexes and two analyses for one character typed.
//!
//! Now `Analysis::run` does it once and every view is derived from the result:
//!
//! ```text
//!            ┌─ tokens ──► 词法 Token tab, semantic highlight widths, squiggles
//! source ────┤
//!            └─ program ─► 语法树 AST tab, sema + lint ─► 诊断 tab, squiggles
//! ```

use std::collections::HashMap;

use gpui_kit::component::highlighter::{Diagnostic, DiagnosticSeverity};
use gpui_kit::component::input::Position;

use pine_lang::ast::Program;
use pine_lang::core::{Bar, DefaultPineOutput, PineVersion};
use pine_lang::diagnostics::{Diagnostic as PineDiagnostic, Severity};
use pine_lang::interpreter::Value;
use pine_lang::lexer::{Lexer, Token, TokenType};
use pine_lang::parser::Parser;

/// Everything one parse of the buffer produces.
pub struct Analysis {
    pub tokens: Vec<Token>,
    /// `None` when the source failed to lex or parse; `error` says why.
    pub program: Option<Program>,
    /// The symbol table the analyzer built, kept so hover, go-to-definition and
    /// the `import` jump can share one pass instead of re-analyzing per event.
    pub symbols: Option<pine_lang::sema::SymbolTable>,
    pub diagnostics: Vec<PineDiagnostic>,
    pub error: Option<String>,
}

impl Analysis {
    /// Lex, parse, analyze and lint `source`, resolving `import`s against the
    /// directory of the open file.
    pub fn run(source: &str) -> Self {
        let loader = crate::workspace::loader();
        Self::run_with(source, loader.as_ref())
    }

    /// As [`run`](Self::run), but with an explicit library loader.
    ///
    /// Taking the loader instead of reading the global keeps tests hermetic:
    /// they can point at a fixture directory without touching process state.
    pub fn run_with(source: &str, loader: Option<&pine_lang::DirLoader>) -> Self {
        let loader = loader.map(|l| l as &dyn pine_lang::LibraryLoader);
        // `detect` returns `Result<Option<_>>`: no `//@version` is not an error.
        let version = PineVersion::detect(source)
            .ok()
            .flatten()
            .unwrap_or(PineVersion::LATEST);

        let tokens = match Lexer::with_version(source, version).tokenize() {
            Ok(tokens) => tokens,
            Err(err) => {
                return Self {
                    tokens: vec![],
                    program: None,
                    symbols: None,
                    diagnostics: vec![],
                    error: Some(format!("lex error: {err}")),
                };
            }
        };

        let program = match Parser::new(tokens.clone()).parse_program() {
            Ok(program) => program,
            Err(err) => {
                return Self {
                    tokens,
                    program: None,
                    symbols: None,
                    diagnostics: vec![],
                    error: Some(format!("parse error: {err}")),
                };
            }
        };

        let (mut diagnostics, symbols) = analyze(&program, version, loader);
        diagnostics.sort_by_key(|diagnostic| diagnostic.pos.unwrap_or((u32::MAX, u32::MAX)));

        if std::env::var_os("PINEDITOR_TRACE").is_some() {
            eprintln!(
                "[pineditor] analysis: {} tokens, {} diagnostics",
                tokens.len(),
                diagnostics.len()
            );
        }

        Self {
            tokens,
            program: Some(program),
            symbols: Some(symbols),
            diagnostics,
            error: None,
        }
    }

    /// Every token, with its 1-based source position.
    ///
    /// Trivia (`Newline`, `Indent`, `Dedent`, `Eof`) is included on purpose — it
    /// is exactly what you need to see when indentation-sensitive parsing
    /// misbehaves.
    pub fn lexer_dump(&self) -> String {
        if let Some(error) = &self.error {
            if self.tokens.is_empty() {
                return error.clone();
            }
        }

        let mut out = format!("{} tokens\n\n", self.tokens.len());
        out.push_str(&format!(
            "{:>5}  {:>4}:{:<4} {:<22} {}\n",
            "#", "line", "col", "kind", "lexeme"
        ));
        out.push_str(&"-".repeat(72));
        out.push('\n');

        for (i, token) in self.tokens.iter().enumerate() {
            out.push_str(&format!(
                "{:>5}  {:>4}:{:<4} {:<22} {}\n",
                i,
                token.line,
                token.column,
                kind_name(&token.typ),
                if token.lexeme.is_empty() {
                    "·".to_string()
                } else {
                    token.lexeme.replace('\n', "\\n").replace('\t', "\\t")
                }
            ));
        }
        out
    }

    /// The AST as `{:#?}`, or the failure that stopped us getting one.
    pub fn ast_dump(&self) -> String {
        match (&self.program, &self.error) {
            (Some(program), _) => format!("{program:#?}"),
            (None, Some(error)) => error.clone(),
            (None, None) => "no AST".to_string(),
        }
    }

    /// Every diagnostic, one per line.
    pub fn diagnostics_dump(&self) -> String {
        if let Some(error) = &self.error {
            return error.clone();
        }
        if self.diagnostics.is_empty() {
            return "no diagnostics".to_string();
        }

        self.diagnostics
            .iter()
            .map(|diagnostic| {
                let at = diagnostic
                    .pos
                    .map(|(line, col)| format!("{line}:{col}"))
                    .unwrap_or_else(|| "-".to_string());
                format!(
                    "{:<8} {:<28} {}  ({at})",
                    match diagnostic.severity {
                        Severity::Error => "error",
                        Severity::Warning => "warning",
                    },
                    diagnostic.rule,
                    diagnostic.message
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The diagnostics as editor squiggles: 0-based positions, each widened to
    /// cover the whole token it points at.
    pub fn squiggles(&self) -> Vec<Diagnostic> {
        let widths = self.token_widths();

        self.diagnostics
            .iter()
            .filter_map(|diagnostic| {
                let (line, column) = diagnostic.pos?;
                let start = Position::new(line.saturating_sub(1), column.saturating_sub(1));
                let width = widths.get(&(line, column)).copied().unwrap_or(1).max(1);
                let end = Position::new(start.line, start.character + width);

                let severity = match diagnostic.severity {
                    Severity::Error => DiagnosticSeverity::Error,
                    Severity::Warning => DiagnosticSeverity::Warning,
                };

                Some(
                    Diagnostic::new(start..end, diagnostic.message.clone())
                        .with_severity(severity)
                        .with_code(diagnostic.rule)
                        .with_source("pinecone"),
                )
            })
            .collect()
    }

    /// How wide the token at each `(line, column)` is, counted in `char`s.
    ///
    /// Diagnostics point at a token's start; its length is what lets a squiggle
    /// cover the identifier instead of one character of it.
    ///
    /// The unit has to match the *start* the lexer reports, which counts `char`s.
    /// Mixing in a UTF-16 width (as this once did) makes a squiggle too wide by one
    /// per astral character, e.g. an emoji inside a string. Nothing in this editor
    /// converts to UTF-16: `highlight::encode` emits char offsets as well.
    fn token_widths(&self) -> HashMap<(u32, u32), u32> {
        self.tokens
            .iter()
            .map(|token| {
                (
                    (token.line as u32, token.column as u32),
                    token.lexeme.chars().count() as u32,
                )
            })
            .collect()
    }
}

/// Semantic analysis + lint, with the builtin environment they resolve against.
///
/// `loader` resolves `import`s; without one, a script that imports anything gets
/// "cannot resolve library" from sema instead of the library's symbols.
fn analyze(
    program: &Program,
    version: PineVersion,
    loader: Option<&dyn pine_lang::LibraryLoader>,
) -> (Vec<PineDiagnostic>, pine_lang::sema::SymbolTable) {
    let mut env: HashMap<String, Value<DefaultPineOutput>> =
        pine_lang::builtins::register_namespace_objects(version, None, None).0;
    for (name, value) in pine_lang::builtins::per_bar_variables(&Bar::default(), None) {
        env.insert(name, value);
    }

    let (mut diagnostics, symbols) = pine_lang::sema::analyze_with_symbols(program, &env, loader);
    diagnostics.extend(pine_lang::lint::lint(program));
    (diagnostics, symbols)
}

/// `Ident("close")` → `Ident`. The payload only repeats what the lexeme column
/// already shows.
fn kind_name(typ: &TokenType) -> String {
    let debug = format!("{typ:?}");
    debug.split('(').next().unwrap_or(&debug).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../fixtures/example.pine");

    #[test]
    fn one_run_populates_every_view() {
        let analysis = Analysis::run(FIXTURE);

        assert!(analysis.error.is_none(), "{:?}", analysis.error);
        assert!(analysis.program.is_some());
        assert!(analysis.tokens.len() > 100);
        assert!(analysis.lexer_dump().contains("Ident"));
        assert!(analysis.ast_dump().contains("Program"));
    }

    #[test]
    fn the_fixture_shadows_a_builtin_and_that_becomes_a_squiggle() {
        let analysis = Analysis::run(FIXTURE);
        assert!(!analysis.diagnostics.is_empty(), "expected shadows-builtin");

        let squiggles = analysis.squiggles();
        assert_eq!(squiggles.len(), analysis.diagnostics.len());

        // Located by content, not by line number: the fixture is a live demo and
        // editing it must not require editing this test.
        let shadowing = FIXTURE
            .lines()
            .position(|line| line.starts_with("str ="))
            .expect("the fixture declares `str`, shadowing the builtin namespace");

        let first = &squiggles[0];
        assert_eq!(first.range.start.line, shadowing as u32);
        assert!(first.range.end > first.range.start, "must cover the token");
    }

    #[test]
    fn a_lex_error_stops_at_the_lexer() {
        let analysis = Analysis::run("x = \"unterminated\n");
        assert!(analysis.tokens.is_empty());
        assert!(analysis.program.is_none());
        assert!(analysis.error.as_deref().unwrap_or("").starts_with("lex error"));
        assert!(analysis.ast_dump().starts_with("lex error"));
    }

    #[test]
    fn a_parse_error_keeps_the_tokens() {
        let analysis = Analysis::run("if\n");
        assert!(!analysis.tokens.is_empty(), "lexing still succeeded");
        assert!(analysis.program.is_none());
        assert!(analysis.ast_dump().starts_with("parse error"));
    }

    #[test]
    fn a_clean_script_has_no_squiggles() {
        let analysis = Analysis::run("//@version=5\nindicator(\"ok\")\nplot(close)\n");
        assert!(analysis.diagnostics.is_empty());
        assert!(analysis.squiggles().is_empty());
        assert_eq!(analysis.diagnostics_dump(), "no diagnostics");
    }

    #[test]
    fn token_widths_count_chars_not_utf16_units() {
        // An astral character is one column to the lexer but two UTF-16 units.
        // The width has to agree with the start column, so it must be chars.
        let analysis = Analysis::run("x = \"\u{1F600}\u{1F600}\"\n");
        let widths = analysis.token_widths();

        let string = widths
            .iter()
            .find(|((_, column), _)| *column == 5)
            .map(|(_, width)| *width);
        assert_eq!(string, Some(4), "the string token is `\"` + 2 chars + `\"`");
    }

    #[test]
    fn token_widths_come_from_the_same_tokens() {
        let analysis = Analysis::run("plot(close)\n");
        let widths = analysis.token_widths();
        assert_eq!(widths.get(&(1, 1)), Some(&4), "`plot` is 4 units");
        assert_eq!(widths.get(&(1, 6)), Some(&5), "`close` is 5 units");
    }
}
