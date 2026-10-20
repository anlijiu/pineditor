//! Completion backed by pinecone's own builtin registry.
//!
//! The list is everything the compiler will actually accept: every namespace
//! member (`ta.sma`, `str.tostring`, `array.push`, …) plus the global functions,
//! taken from `register_namespace_objects` — the same call the compiler makes.
//! Nothing is hand-maintained, so a builtin added to pinecone shows up here for
//! free.
//!
//! Documentation is not part of a completion item here: hover shows it (see
//! `hover.rs`, which reads the offline reference markdown), and duplicating it
//! into every item would only make the popover noisy.

use std::sync::OnceLock;

use gpui_kit::component::input::{CompletionProvider, Rope, RopeExt};
use gpui_kit::{App, Task, Window};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse,
    CompletionTextEdit, TextEdit,
};

use pine_lang::core::DefaultPineOutput;
use pine_lang::interpreter::Value;

/// Pine keywords that start a statement or a declaration. Kept as a literal list
/// because the lexer exposes them as enum variants, not as strings.
const KEYWORDS: &[&str] = &[
    "var", "varip", "const", "type", "enum", "method", "export", "import", "if", "else", "for",
    "while", "break", "continue", "to", "in", "switch", "and", "or", "not", "int", "float", "bool",
    "string", "color", "na",
];

/// Series variables a script reads without declaring them.
const SERIES: &[&str] = &[
    "open", "high", "low", "close", "volume", "time", "hl2", "hlc3", "ohlc4", "hlcc4", "bar_index",
    "last_bar_index", "syminfo", "timeframe", "strategy", "barstate",
];

/// Cap so a bare `.` does not push thousands of items into the popover.
const MAX_ITEMS: usize = 300;

pub struct PineCompletion;

impl CompletionProvider for PineCompletion {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: CompletionContext,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<anyhow::Result<CompletionResponse>> {
        Task::ready(Ok(CompletionResponse::Array(completions_for(text, offset))))
    }

    /// Fire while the cursor sits inside an identifier (or right after a `.`),
    /// not on every keystroke anywhere in the document.
    fn is_completion_trigger(&self, offset: usize, text: &str, _cx: &mut App) -> bool {
        !identifier_prefix(text, offset).is_empty()
    }
}

/// Completions for the cursor at `offset`, each carrying a `text_edit` that
/// replaces the whole qualified prefix.
///
/// Without the edit range, the editor falls back to the word before the cursor —
/// and for `ta.s` that word is just `s`, so accepting `ta.sma` would leave
/// `ta.ta.sma` behind. The range therefore spans the prefix we matched on,
/// dots included.
fn completions_for(text: &Rope, offset: usize) -> Vec<CompletionItem> {
    let source = text.to_string();
    let prefix = identifier_prefix(&source, offset);

    // An empty prefix would fire on every keystroke and at every cursor park;
    // let the user ask for the full list with an explicit trigger.
    if prefix.is_empty() {
        return vec![];
    }

    let range = replacement_range(text, offset, &prefix);
    catalog()
        .iter()
        .filter(|item| item.label.starts_with(&prefix))
        .take(MAX_ITEMS)
        .map(|item| with_edit(item, range))
        .collect()
}

/// The byte-less range covering everything the user typed of this identifier.
fn replacement_range(text: &Rope, offset: usize, prefix: &str) -> lsp_types::Range {
    let start = offset.saturating_sub(prefix.len());
    lsp_types::Range {
        start: text.offset_to_position(start),
        end: text.offset_to_position(offset),
    }
}

/// The same item, but accepting it replaces `range` instead of inserting at the
/// cursor.
fn with_edit(item: &CompletionItem, range: lsp_types::Range) -> CompletionItem {
    CompletionItem {
        text_edit: Some(CompletionTextEdit::Edit(TextEdit {
            range,
            new_text: item.label.clone(),
        })),
        ..item.clone()
    }
}

/// The identifier fragment immediately before the cursor: `ta.sm` → `ta.sm`.
///
/// Dots are kept so `ta.` narrows to that namespace instead of matching every
/// label that happens to contain those letters.
fn identifier_prefix(source: &str, offset: usize) -> String {
    let end = offset.min(source.len());
    let head = &source[..end];

    head.chars()
        .rev()
        .take_while(|ch| ch.is_alphanumeric() || *ch == '_' || *ch == '.')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// How many completions the registry yields. For the startup trace only.
pub fn catalog_size() -> usize {
    catalog().len()
}

/// Every completion, built once and shared.
fn catalog() -> &'static Vec<CompletionItem> {
    static CATALOG: OnceLock<Vec<CompletionItem>> = OnceLock::new();
    CATALOG.get_or_init(build_catalog)
}

fn build_catalog() -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = Vec::new();

    for keyword in KEYWORDS {
        items.push(item(keyword, CompletionItemKind::KEYWORD, "keyword"));
    }
    for series in SERIES {
        items.push(item(series, CompletionItemKind::VARIABLE, "series"));
    }

    // The same registry the compiler uses, so this cannot drift from it.
    let (namespaces, _advances) = pine_lang::builtins::register_namespace_objects::<
        DefaultPineOutput,
    >(pine_lang::core::PineVersion::LATEST, None, None);

    for (namespace, value) in &namespaces {
        match value {
            // `ta`, `str`, `array`, … — each member becomes `namespace.member`.
            // Some of these are *also* callable themselves (`plot`, `label`), so
            // register the bare name when the object carries a call.
            Value::Object { fields, call, .. } => {
                if call.is_some() {
                    items.push(item(namespace, CompletionItemKind::FUNCTION, "builtin"));
                }
                for member in fields.borrow().keys() {
                    items.push(item(
                        &format!("{namespace}.{member}"),
                        CompletionItemKind::FUNCTION,
                        namespace,
                    ));
                }
            }
            // Global functions such as `plot`, `indicator`, `na`.
            Value::BuiltinFunction(_) => {
                items.push(item(namespace, CompletionItemKind::FUNCTION, "builtin"));
            }
            _ => {}
        }
    }

    items.sort_by(|a, b| a.label.cmp(&b.label));
    items.dedup_by(|a, b| a.label == b.label);
    items
}

fn item(label: &str, kind: CompletionItemKind, detail: &str) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(kind),
        detail: Some(detail.to_string()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::Position;

    #[test]
    fn the_catalog_contains_namespaces_globals_and_keywords() {
        let labels: Vec<&str> = catalog().iter().map(|i| i.label.as_str()).collect();

        assert!(labels.contains(&"ta.sma"), "namespace member missing");
        assert!(labels.contains(&"str.tostring"), "namespace member missing");
        assert!(labels.contains(&"plot"), "global builtin missing");
        assert!(labels.contains(&"if"), "keyword missing");
        assert!(labels.contains(&"close"), "series variable missing");
    }

    #[test]
    fn the_catalog_has_no_duplicate_labels() {
        let mut labels: Vec<&str> = catalog().iter().map(|i| i.label.as_str()).collect();
        let before = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(before, labels.len(), "duplicate completion labels");
    }

    #[test]
    fn prefix_stops_at_a_non_identifier_character() {
        let source = "x = ta.sm";
        assert_eq!(identifier_prefix(source, source.len()), "ta.sm");

        let source = "x = (cl";
        assert_eq!(identifier_prefix(source, source.len()), "cl");

        // A newline or space ends the fragment.
        let source = "close ";
        assert_eq!(identifier_prefix(source, source.len()), "");
    }

    fn edit_of(items: &[CompletionItem], label: &str) -> (lsp_types::Range, String) {
        let item = items
            .iter()
            .find(|item| item.label == label)
            .unwrap_or_else(|| panic!("{label} should be offered, got {} items", items.len()));
        match &item.text_edit {
            Some(CompletionTextEdit::Edit(edit)) => (edit.range, edit.new_text.clone()),
            other => panic!("expected a plain text edit, got {other:?}"),
        }
    }

    #[test]
    fn accepting_a_namespace_member_replaces_the_qualified_prefix() {
        // This is the case that used to produce `ta.ta.sma`: the editor's own
        // word range covers only `s`.
        let source = "x = ta.s";
        let text = Rope::from(source);
        let items = completions_for(&text, source.len());

        let (range, new_text) = edit_of(&items, "ta.sma");
        assert_eq!(range.start, Position::new(0, 4), "`ta.s` starts at column 4");
        assert_eq!(range.end, Position::new(0, 8), "the cursor");
        assert_eq!(new_text, "ta.sma");
    }

    #[test]
    fn accepting_a_plain_builtin_replaces_only_that_word() {
        let source = "plot(clo";
        let text = Rope::from(source);
        let items = completions_for(&text, source.len());

        let (range, new_text) = edit_of(&items, "close");
        assert_eq!(range.start, Position::new(0, 5));
        assert_eq!(range.end, Position::new(0, 8));
        assert_eq!(new_text, "close");
    }

    #[test]
    fn the_replacement_range_stays_on_the_cursor_line() {
        let source = "//@version=5\nx = ta.s";
        let text = Rope::from(source);
        let items = completions_for(&text, source.len());

        let (range, _) = edit_of(&items, "ta.sma");
        assert_eq!(range.start.line, 1);
        assert_eq!(range.end.line, 1);
    }

    #[test]
    fn nothing_is_offered_on_whitespace() {
        let source = "x = ";
        assert!(completions_for(&Rope::from(source), source.len()).is_empty());
    }

    #[test]
    fn prefix_is_utf8_safe() {
        // A comment with multibyte chars before the cursor must not panic.
        let source = "// 中文注释\nvalue = clo";
        assert_eq!(identifier_prefix(source, source.len()), "clo");
    }
}
