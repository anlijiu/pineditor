//! Pine syntax highlighting, driven by pinecone's own lexer.
//!
//! gpui-kit's editor highlights with tree-sitter, and there is no
//! `tree-sitter-pine` — but it also accepts *semantic* tokens layered on top
//! (`DocumentRangeSemanticTokensProvider`). Those are plain
//! `(range, type-name)` pairs whose names resolve against the same theme
//! vocabulary the tree-sitter path uses (`"keyword"`, `"string"`, …).
//!
//! So instead of writing a grammar, we lift `pine_lexer`'s tokens into that
//! vocabulary. That is both less work and more accurate than a generic grammar:
//! the tokenizer knows Pine's keywords and operators exactly.

use std::ops::Range;
use std::rc::Rc;

use gpui_kit::component::input::{
    DocumentRangeSemanticTokensProvider, EditorState, FoldRange, HighlightStyleResolver, InputEdit,
    InputHighlighter, InputHighlighterFactory, Rope,
};
// Imported by name rather than `gpui_kit::*`: the glob also brings in gpui's
// `test` module, which then shadows the `#[test]` attribute below and blows up
// macro expansion.
use gpui_kit::{App, Context, HighlightStyle, SharedString, Task, Window};
use lsp_types::{SemanticToken, SemanticTokenType, SemanticTokens, SemanticTokensLegend};

use pine_lang::lexer::{Token, TokenType};

/// Token type names, in legend order — and they are **not** free-form.
///
/// The editor resolves each name against the active `HighlightTheme`, and the
/// default theme leaves several of the 41 syntax slots unset: `variable`,
/// `operator` and every `punctuation.*` resolve to `None`, which drops the token
/// from the render entirely. Since most of a Pine script is identifiers and
/// operators, mapping to those names makes the buffer look unhighlighted.
///
/// Every name below is one the default theme actually colors (verified by
/// `every_legend_name_resolves_against_the_default_theme`):
///
/// * identifiers → `variable.special` (`variable` is unset)
/// * operators → `keyword` (`operator` is unset; sharing the keyword colour is
///   what most grammars do anyway since both are syntax scaffolding)
/// * brackets and commas → `punctuation`, deliberately left uncolored so they
///   keep the foreground colour, which is the usual look.
const LEGEND: &[&str] = &[
    "keyword",         // 0
    "string",          // 1
    "number",          // 2
    "comment",         // 3
    "keyword",         // 4 — operators
    "punctuation",     // 5 — deliberately uncolored
    "variable.special", // 6
    "function",        // 7
    "constant",        // 8
    "boolean",         // 9
];

const KEYWORD: u32 = 0;
const STRING: u32 = 1;
const NUMBER: u32 = 2;
const COMMENT: u32 = 3;
const OPERATOR: u32 = 4;
const PUNCTUATION: u32 = 5;
const VARIABLE: u32 = 6;
const FUNCTION: u32 = 7;
const CONSTANT: u32 = 8;
const BOOLEAN: u32 = 9;

/// The Pine script series variables a script reads without declaring them.
/// Coloring them apart from user identifiers makes a plot much easier to scan.
const BUILTIN_SERIES: &[&str] = &[
    "open", "high", "low", "close", "volume", "time", "hl2", "hlc3", "ohlc4", "hlcc4", "bar_index",
    "na", "true", "false",
];

/// A no-op tree-sitter stand-in, needed only to make semantic tokens render.
///
/// `highlight_lines` in gpui-base consults semantic-token providers *after*
/// it has found an [`InputHighlighter`], and returns early when there is none.
/// The component layer's highlighter factory returns `None` for a language
/// without a grammar — which is exactly `pine` — so a semantic-only language
/// would have every token silently dropped at paint time.
///
/// Installing this keeps that path alive. It parses nothing: `styles` answers
/// one [`HighlightStyle::default`] run covering the requested range, and since
/// an unset style has no colour, the semantic tokens composed on top keep
/// theirs.
pub struct PineHighlighter;

impl InputHighlighter for PineHighlighter {
    fn language(&self) -> SharedString {
        "pine".into()
    }

    fn update(
        &mut self,
        _edit: Option<InputEdit>,
        _text: &Rope,
        _folding: bool,
        _window: &mut Window,
        _cx: &mut Context<EditorState>,
    ) {
        // Semantic tokens come from `PineSemanticTokens`, not from a parse.
    }

    fn styles(
        &self,
        range: &Range<usize>,
        _resolver: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        // The contract asks for runs that fully cover the range; one unstyled
        // run is the whole of a grammar-less language's contribution.
        vec![(range.clone(), HighlightStyle::default())]
    }

    fn fold_ranges(&self, _text: &Rope) -> Vec<FoldRange> {
        // Indentation-based folding is not wired up for Pine; an empty list
        // (rather than a guessed one) keeps the fold gutter quiet.
        Vec::new()
    }
}

/// The factory handed to `EditorState::set_highlighter_factory`.
pub fn highlighter_factory() -> InputHighlighterFactory {
    Rc::new(|_language| Some(Box::new(PineHighlighter) as Box<dyn InputHighlighter>))
}

pub struct PineSemanticTokens;

impl DocumentRangeSemanticTokensProvider for PineSemanticTokens {
    fn legend(&self) -> SemanticTokensLegend {
        SemanticTokensLegend {
            token_types: LEGEND.iter().map(|name| SemanticTokenType::new(name)).collect(),
            token_modifiers: vec![],
        }
    }

    fn semantic_tokens(
        &self,
        text: &Rope,
        _range: Range<usize>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<anyhow::Result<SemanticTokens>> {
        // The editor hands us the whole document (it filters to the viewport at
        // paint time), so there is no range logic to do here.
        // Shares the one analysis: this provider runs on every repaint, and a
        // full lex per frame is wasted work when the text has not changed.
        let source = text.to_string();
        let analysis = crate::cache::analysis(&source);
        let tokens = &analysis.tokens;

        // A lex error is reported by the diagnostics path; highlighting simply
        // goes quiet rather than painting a half-parsed document.
        let data = encode(tokens);
        if std::env::var_os("PINEDITOR_TRACE").is_some() {
            eprintln!(
                "[pineditor] {} lexer tokens -> {} semantic tokens",
                tokens.len(),
                data.len()
            );
        }

        Task::ready(Ok(SemanticTokens {
            result_id: None,
            data,
        }))
    }
}

/// Lexer tokens → delta-encoded semantic tokens.
///
/// `pine_lexer` reports 1-based `(line, column)` counted in `char`s, and that is
/// also the unit the editor uses: its `offset_to_position` counts chars, so the
/// tokens it hands back are placed by char offsets. No conversion happens here.
///
/// It used to mix the two — a char column with a UTF-16 length — which shifted
/// every token after an astral character (an emoji in a string) one column per
/// extra code unit. CJK text is BMP and was unaffected, which is why it went
/// unnoticed.
fn encode(tokens: &[Token]) -> Vec<SemanticToken> {
    // (line, start, length, type index), all in chars, sorted as required.
    let mut items: Vec<(u32, u32, u32, u32)> = Vec::with_capacity(tokens.len());

    for (i, token) in tokens.iter().enumerate() {
        let Some(kind) = classify(token, tokens.get(i + 1)) else {
            continue;
        };
        let length = token.lexeme.chars().count() as u32;
        if length == 0 {
            continue;
        }
        items.push((
            token.line.saturating_sub(1) as u32,
            token.column.saturating_sub(1) as u32,
            length,
            kind,
        ));
    }

    items.sort_by_key(|(line, start, _, _)| (*line, *start));

    let mut data = Vec::with_capacity(items.len());
    let (mut prev_line, mut prev_start) = (0u32, 0u32);
    for (line, start, length, kind) in items {
        let delta_line = line.saturating_sub(prev_line);
        let delta_start = if delta_line == 0 {
            start.saturating_sub(prev_start)
        } else {
            // LSP: on a new line, delta_start is absolute.
            start
        };
        data.push(SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type: kind,
            token_modifiers_bitset: 0,
        });
        prev_line = line;
        prev_start = start;
    }
    data
}

/// The theme name for a token, or `None` for tokens that should not be
/// painted (trivia and structural markers).
fn classify(token: &Token, next: Option<&Token>) -> Option<u32> {
    Some(match &token.typ {
        // Trivia: the editor renders the text itself; these carry no color.
        TokenType::Comment(_) => COMMENT,
        TokenType::BlankLine
        | TokenType::Newline
        | TokenType::Indent
        | TokenType::Dedent
        | TokenType::Eof => return None,

        TokenType::String(_) | TokenType::HexColor(_) => STRING,
        TokenType::IntLiteral(_) | TokenType::Number(_) => NUMBER,
        TokenType::Bool(_) => BOOLEAN,

        TokenType::Ident(name) => {
            if matches!(name.as_str(), "true" | "false") {
                BOOLEAN
            } else if matches!(name.as_str(), "na") {
                CONSTANT
            } else if BUILTIN_SERIES.contains(&name.as_str()) {
                VARIABLE
            } else if next.is_some_and(|n| matches!(n.typ, TokenType::LParen)) {
                // `foo(` — a user function, a builtin, or a `ta.sma` style
                // call whose last segment we cannot tell apart lexically.
                FUNCTION
            } else {
                VARIABLE
            }
        }

        // Declarations and control flow.
        TokenType::Var
        | TokenType::Varip
        | TokenType::Const
        | TokenType::Type
        | TokenType::Enum
        | TokenType::Method
        | TokenType::Export
        | TokenType::Import
        | TokenType::If
        | TokenType::Else
        | TokenType::For
        | TokenType::While
        | TokenType::Break
        | TokenType::Continue
        | TokenType::To
        | TokenType::In
        | TokenType::Switch => KEYWORD,

        // Type keywords get the type color rather than the statement color.
        TokenType::Int | TokenType::Float => CONSTANT,
        TokenType::Na => CONSTANT,
        TokenType::And | TokenType::Or | TokenType::Not => OPERATOR,

        TokenType::Plus
        | TokenType::Minus
        | TokenType::Star
        | TokenType::Slash
        | TokenType::Percent
        | TokenType::Equal
        | TokenType::NotEqual
        | TokenType::Less
        | TokenType::Greater
        | TokenType::LessEqual
        | TokenType::GreaterEqual
        | TokenType::Assign
        | TokenType::ColonAssign
        | TokenType::Arrow
        | TokenType::PlusAssign
        | TokenType::MinusAssign
        | TokenType::StarAssign
        | TokenType::SlashAssign
        | TokenType::Question => OPERATOR,

        TokenType::LParen
        | TokenType::RParen
        | TokenType::LBracket
        | TokenType::RBracket
        | TokenType::Comma
        | TokenType::Dot
        | TokenType::Colon => PUNCTUATION,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pine_lang::lexer::Lexer;

    fn tokens(source: &str) -> Vec<Token> {
        Lexer::new(source).tokenize().expect("lexes")
    }

    fn classified(source: &str) -> Vec<(String, u32)> {
        let tokens = tokens(source);
        tokens
            .iter()
            .enumerate()
            .filter_map(|(i, token)| {
                let kind = classify(token, tokens.get(i + 1))?;
                Some((token.lexeme.clone(), kind))
            })
            .collect()
    }

    #[test]
    fn keywords_builtins_numbers_and_strings_get_their_own_kinds() {
        let kinds = classified("if close > 1\nlabel.new(bar_index, \"hi\")\n");
        let find = |lexeme: &str| {
            kinds
                .iter()
                .find(|(text, _)| text == lexeme)
                .unwrap_or_else(|| panic!("{lexeme} missing from {kinds:?}"))
                .1
        };

        assert_eq!(find("if"), KEYWORD);
        assert_eq!(find("close"), VARIABLE);
        assert_eq!(find(">"), OPERATOR);
        assert_eq!(find("1"), NUMBER);
        assert_eq!(find("bar_index"), VARIABLE);
        // `label` is followed by `.`, not `(` — it stays a variable.
        assert_eq!(find("label"), VARIABLE);
        // `new` is directly followed by `(`.
        assert_eq!(find("new"), FUNCTION);
        assert_eq!(find("\"hi\""), STRING);
    }

    #[test]
    fn comments_are_colored_and_trivia_is_dropped() {
        let kinds = classified("// note\nx = 1\n");
        assert!(kinds.iter().any(|(text, kind)| text.contains("note") && *kind == COMMENT));
        // Newline / Indent / Dedent / Eof carry no color.
        assert!(kinds.iter().all(|(text, _)| text != "\n"));
    }

    #[test]
    fn positions_convert_from_1_based_chars_to_0_based_utf16() {
        // Two lines: `a = 1` then `b = 2`.
        let data = encode(&tokens("a = 1\nb = 2\n"));
        let first = &data[0];
        assert_eq!((first.delta_line, first.delta_start, first.length), (0, 0, 1));

        // `=` starts at column 3 (1-based) → character 2.
        assert_eq!((data[1].delta_line, data[1].delta_start), (0, 2));

        // First token of line 2: delta_line 1, and delta_start is absolute.
        let second_line = data
            .iter()
            .find(|token| token.delta_line == 1)
            .expect("a token on line 2");
        assert_eq!(second_line.delta_start, 0);
    }

    #[test]
    fn a_multibyte_comment_is_measured_in_chars() {
        // The editor places these tokens by char offset, so the length must be
        // chars too. Counting UTF-16 units (as this once did) makes the token one
        // column too wide per astral character — `👋` is 1 char but 2 units.
        let data = encode(&tokens("// 👋 ok\n"));
        let comment = data.first().expect("a comment token");

        assert_eq!(comment.length, "// 👋 ok".chars().count() as u32);
        assert_eq!(comment.length, 7);
    }

    #[test]
    fn every_legend_name_resolves_against_the_default_theme() {
        // Guards the bug where tokens were colored `variable`/`operator`, names the
        // default theme leaves unset — the render then dropped those tokens and the
        // buffer looked like plain text.
        use gpui_kit::component::highlighter::HighlightTheme;
        use gpui_kit::component::input::HighlightStyleResolver;

        let theme = HighlightTheme::default_dark();
        for name in LEGEND {
            if *name == "punctuation" {
                continue; // intentionally the foreground colour
            }
            assert!(
                theme.style(name).and_then(|style| style.color).is_some(),
                "`{name}` has no color in the default theme, so those tokens render as plain text"
            );
        }
    }

    #[test]
    fn a_lex_error_yields_no_tokens_instead_of_panicking() {
        // An unterminated string is a lexer error.
        let source = "x = \"unterminated\n";
        assert!(Lexer::new(source).tokenize().is_err());
    }

    /// The component highlighter factory returns `None` for grammar-less
    /// languages, and gpui-base then skips semantic tokens entirely. Our
    /// factory must answer for `pine` unconditionally.
    #[test]
    fn the_factory_always_yields_a_highlighter() {
        let highlighter = highlighter_factory()("pine").expect("a highlighter for `pine`");
        assert_eq!(highlighter.language(), "pine");
    }

    #[test]
    fn the_stand_in_highlighter_does_not_swallow_semantic_colours() {
        struct NoStyles;
        impl HighlightStyleResolver for NoStyles {
            fn style(&self, _: &str) -> Option<HighlightStyle> {
                None
            }
        }

        let range = 0..10;
        let plain = PineHighlighter.styles(&range, &NoStyles);
        // One uncoloured run that fully covers the range: what the editor's
        // run-based painting expects from a highlighter.
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].0, range);
        assert_eq!(plain[0].1.color, None);

        // Composed the way `highlight_lines` does it: semantic first, then the
        // highlighter. The keyword colour has to survive the extra run.
        let keyword = HighlightStyle {
            color: Some(gpui_kit::hsla(0.0, 1.0, 0.5, 1.0)),
            ..Default::default()
        };
        let combined: Vec<_> =
            gpui_kit::combine_highlights(vec![(0..3, keyword)], plain).collect();
        assert!(
            combined.iter().any(|(_, style)| style.color.is_some()),
            "the semantic colour was dropped: {combined:?}"
        );
    }
}
