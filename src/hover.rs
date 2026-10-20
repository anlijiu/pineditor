//! Hover: what is the thing under the cursor?
//!
//! Two sources, in order:
//!
//! 1. **The script's own symbols** — `pine_lang::analyze` builds a symbol table
//!    with each declaration's kind, parameters and type annotation.
//! 2. **Pinecone's reference markdown** — `pine_reference` ships v3–v6 specs in
//!    `crates/pine-reference/spec`, so builtin documentation is available
//!    offline. Lookups are cached: the spec is ~900 KB of markdown and parsing it
//!    on every mouse move would be silly.
//!
//! Nothing is shown for whitespace or punctuation.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use gpui_kit::component::input::{HoverProvider, Rope};
use gpui_kit::{App, Task, Window};
use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind};

use pine_lang::core::PineVersion;
use pine_reference::QueryResult;

use crate::word;

/// Longest documentation excerpt shown in the popover.
const MAX_DOC_CHARS: usize = 1400;

pub struct PineHover;

impl HoverProvider for PineHover {
    fn hover(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<anyhow::Result<Option<Hover>>> {
        Task::ready(Ok(hover_at(text, offset)))
    }
}

fn hover_at(text: &Rope, offset: usize) -> Option<Hover> {
    let (word, start, end) = word::word_at(text, offset)?;
    let source = text.to_string();

    // One shared analysis: hovering several names in a row must not re-run the
    // front end each time.
    let analysis = crate::cache::analysis(&source);

    let markdown = declared_symbol(&analysis, start)
        .or_else(|| reference::document(&word).map(|doc| trim(&doc)))
        // `ta.sma` and friends are documented under their qualified name, but
        // `word_at` stops at the dot — so `sma` alone finds nothing. Look the
        // namespace up from the text just before the cursor.
        .or_else(|| {
            let namespace = namespace_before(&source, start)?;
            reference::document(&format!("{namespace}.{word}")).map(|doc| trim(&doc))
        })
        .unwrap_or_else(|| format!("`{word}`"));

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
        }),
        // The editor already knows the word range; leaving it unset lets it use
        // the one it computed.
        range: Some(lsp_types::Range { start, end }),
    })
}

/// A symbol the script declares itself, described from the symbol table.
///
/// Symbols brought in by `import` live in the same table under their own file,
/// so naming one here reports the library's declaration rather than nothing —
/// which is why the caller's loader matters.
fn declared_symbol(
    analysis: &crate::analysis::Analysis,
    start: lsp_types::Position,
) -> Option<String> {
    let symbols = analysis.symbols.as_ref()?;

    // `symbol_at` takes pinecone's 1-based (line, column).
    let id = symbols.symbol_at(0, start.line + 1, start.character + 1)?;
    let symbol = symbols.symbol(id);

    let mut markdown = format!("**{}** `{}`", symbol.kind.noun(), symbol.name);
    if !symbol.params.is_empty() {
        markdown.push_str(&format!("({})", symbol.params.join(", ")));
    }
    if let Some(annotation) = &symbol.type_annotation {
        markdown.push_str(&format!(" → `{annotation}`"));
    }
    if let Some((line, column)) = symbol.decl {
        markdown.push_str(&format!("\n\nDeclared at **{line}:{column}**"));
    }

    Some(markdown)
}

/// The namespace an identifier belongs to, when it is written as `ns.member`.
///
/// Columns here are counted in `char`s, which is what the editor uses throughout:
/// these `Position`s come from its own `offset_to_position`, and
/// `highlight::encode` emits char offsets too. An emoji earlier on the line is
/// therefore one column, not two.
fn namespace_before(source: &str, start: lsp_types::Position) -> Option<String> {
    let line = source.lines().nth(start.line as usize)?;
    let chars: Vec<char> = line.chars().collect();
    let column = start.character as usize;

    // The character immediately before the identifier has to be the dot.
    if column == 0 || column > chars.len() || chars[column - 1] != '.' {
        return None;
    }

    let mut begin = column - 1;
    while begin > 0 && (chars[begin - 1].is_alphanumeric() || chars[begin - 1] == '_') {
        begin -= 1;
    }

    (begin < column - 1).then(|| chars[begin..column - 1].iter().collect())
}

/// Trim a reference section down to something that fits in a popover.
fn trim(document: &str) -> String {
    let document = document.trim();
    if document.chars().count() <= MAX_DOC_CHARS {
        return document.to_string();
    }
    let cut: String = document.chars().take(MAX_DOC_CHARS).collect();
    format!("{cut}…")
}

/// Builtin documentation from `crates/pine-reference/spec`.
mod reference {
    use super::*;

    /// Cache of name → markdown.
    fn documents() -> &'static Mutex<HashMap<String, String>> {
        static DOCS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
        DOCS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn known_names() -> &'static HashSet<String> {
        static NAMES: OnceLock<HashSet<String>> = OnceLock::new();
        NAMES.get_or_init(|| {
            ensure_spec_root();
            pine_reference::symbols(PineVersion::LATEST)
                .unwrap_or_default()
                .into_iter()
                .collect()
        })
    }

    /// Look up a builtin's documentation, or `None` if the name is not one.
    pub fn document(name: &str) -> Option<String> {
        if !known_names().contains(name) {
            return None;
        }
        if let Ok(cache) = documents().lock() {
            if let Some(hit) = cache.get(name) {
                return Some(hit.clone());
            }
        }

        let found = lookup(name)?;
        if let Ok(mut cache) = documents().lock() {
            cache.insert(name.to_string(), found.clone());
        }
        Some(found)
    }

    /// The reference is a flat markdown file with `## Section` / `### item`
    /// headings. `query` matches `###` titles exactly, and those titles carry
    /// their call syntax (`ta.sma()`), so a bare name needs a prefix pass first
    /// and an exact pass second.
    fn lookup(name: &str) -> Option<String> {
        let version = PineVersion::LATEST;
        for section in ["Functions", "Variables", "Constants"] {
            let Ok(QueryResult::List(items)) =
                pine_reference::query(version, Some(&format!("{section}.{name}")))
            else {
                continue;
            };
            let Some(title) = items.into_iter().find(|title| bare(title) == name) else {
                continue;
            };
            if let Ok(QueryResult::Content(content)) =
                pine_reference::query(version, Some(&format!("{section}.{title}")))
            {
                return Some(content);
            }
        }
        None
    }

    /// `ta.sma()` → `ta.sma`; `array\<T\>.new()` → `array`.
    pub(super) fn bare(title: &str) -> String {
        let name = title.replace("\\_", "_");
        let name = name.split(['(', '<']).next().unwrap_or(&name);
        name.trim_end_matches('\\').trim().to_string()
    }

    /// `pine_reference` resolves its spec directory relative to the *current
    /// working directory* (`crates/pine-reference/spec`). Launching from
    /// `rust/pineditor` would therefore find nothing, so point the process at
    /// the pinecone checkout once.
    fn ensure_spec_root() {
        static DONE: OnceLock<()> = OnceLock::new();
        DONE.get_or_init(|| {
            // This editor sits next to the pinecone checkout, so the specs are
            // one level up and across. (It used to live in pinecone's own
            // `editors/`, where `../..` reached the same place.)
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../pinecone");
            let Ok(root) = root.canonicalize() else {
                return;
            };
            if root.join("crates/pine-reference/spec/v5.md").exists() {
                let _ = std::env::set_current_dir(root);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRIPT: &str = "//@version=5\nindicator(\"x\")\nmyLen = 20\nplot(close, length=myLen)\n";

    #[test]
    fn a_declared_symbol_describes_itself() {
        let analysis = crate::analysis::Analysis::run_with(SCRIPT, None);
        let markdown = declared_symbol(&analysis, lsp_types::Position::new(2, 0))
            .expect("myLen is declared on line 3");
        assert!(markdown.contains("variable"), "{markdown}");
        assert!(markdown.contains("myLen"), "{markdown}");
        assert!(markdown.contains("Declared at"), "{markdown}");
    }

    #[test]
    fn an_undeclared_word_has_no_symbol() {
        // `close` is a builtin series variable, not a script declaration.
        let analysis = crate::analysis::Analysis::run_with(SCRIPT, None);
        assert!(declared_symbol(&analysis, lsp_types::Position::new(3, 5)).is_none());
    }

    /// An imported name is declared in another file, so hovering it needs the
    /// loader — the same reason go-to-definition does.
    #[test]
    fn an_imported_function_is_described_from_its_library() {
        let dir = std::env::temp_dir().join(format!("pineditor-hover-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(
            dir.join("mylib.pine"),
            "//@version=6\nlibrary(\"mylib\")\n\nexport add(x, y) => x + y\n",
        )
        .expect("write library");

        let source = "//@version=6\nindicator(\"x\")\nimport mylib as lib\nplot(lib.add(1, 2))\n";
        // `add` in `plot(lib.add(...))` — line 4 (index 3), column 10 (index 9).
        let at = lsp_types::Position::new(3, 9);

        let loader = pine_lang::DirLoader::new(vec![dir.clone()]);
        let with_library = crate::analysis::Analysis::run_with(source, Some(&loader));
        let markdown = declared_symbol(&with_library, at)
            .expect("`lib.add` resolves through the loader");
        assert!(markdown.contains("add"), "{markdown}");
        assert!(markdown.contains("Declared at"), "{markdown}");

        let without_library = crate::analysis::Analysis::run_with(source, None);
        assert!(
            declared_symbol(&without_library, at).is_none(),
            "without a loader the library is not read at all"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn builtin_documentation_is_available_from_the_editor_directory() {
        // `cargo test` runs with the CWD at `editors/pineditor`; `ensure_spec_root`
        // must redirect pine_reference to the repository root for this to work.
        let doc = reference::document("ta.sma").expect("the v5 spec ships with the repo");
        assert!(!doc.trim().is_empty(), "sma should have documentation");
    }

    #[test]
    fn a_name_that_is_not_a_builtin_has_no_document() {
        assert!(reference::document("definitely_not_a_pine_builtin").is_none());
    }

    #[test]
    fn bare_strips_call_syntax_and_generics() {
        assert_eq!(reference::bare("ta.sma()"), "ta.sma");
        assert_eq!(reference::bare("array.push()"), "array.push");
        assert_eq!(reference::bare("array\\<T\\>.new()"), "array");
    }

    #[test]
    fn a_namespace_member_is_documented_under_its_qualified_name() {
        let source = "//@version=5\nindicator(\"x\")\nfast = ta.sma(close, 14)\n";
        let text = Rope::from(source);
        // `sma` inside `ta.sma` — `word_at` only sees the part after the dot.
        let offset = source.find("sma").expect("sma");

        let hover = hover_at(&text, offset).expect("hover");
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("expected markdown");
        };

        assert!(
            markup.value.contains("moving average"),
            "expected the ta.sma entry, got {:?}",
            markup.value
        );
    }

    #[test]
    fn a_plain_builtin_still_resolves_without_a_namespace() {
        let source = "//@version=5\nindicator(\"x\")\nplot(close)\n";
        let text = Rope::from(source);
        let offset = source.find("plot").expect("plot");

        let hover = hover_at(&text, offset).expect("hover");
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("expected markdown");
        };
        assert_ne!(markup.value, "`plot`", "plot should have documentation");
    }

    #[test]
    fn a_namespace_is_read_from_the_utf16_text_before_the_cursor() {
        let source = "a = 1\n😀 ta.sma(close)\n";
        let text = Rope::from(source);
        let offset = source.find("sma").expect("sma");
        let (_, start, _) = word::word_at(&text, offset).expect("a word");

        assert_eq!(
            namespace_before(source, start).as_deref(),
            Some("ta"),
            "an emoji earlier on the line must not shift the column"
        );
    }

    #[test]
    fn a_bare_name_has_no_namespace() {
        let source = "plot(close)\n";
        let text = Rope::from(source);
        let offset = source.find("close").expect("close");
        let (_, start, _) = word::word_at(&text, offset).expect("a word");

        assert!(namespace_before(source, start).is_none());
    }

    #[test]
    fn long_documents_are_trimmed() {
        let long = "x".repeat(MAX_DOC_CHARS + 100);
        let trimmed = trim(&long);
        assert!(trimmed.chars().count() <= MAX_DOC_CHARS + 1, "should cut");
        assert!(trimmed.ends_with('…'));
    }
}
