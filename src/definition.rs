//! Go to definition, driven by pinecone's symbol table.
//!
//! `SymbolTable::symbol_at` resolves the identifier under the cursor to a symbol
//! id, and `declaration_location` gives back where it is declared — both already
//! maintained by the analyzer, so there is no symbol resolution to write here.
//!
//! The editor only treats `http(s)` targets as external (`go_to_definition` in
//! gpui-base), so any other URI lands as an in-document jump. That is exactly
//! what we want for same-file symbols; imported libraries get their own `FileId`
//! and would need a `show_document` handler, which this editor does not have yet.

use gpui_kit::component::input::{DefinitionProvider, Rope};
use gpui_kit::{App, Task, Window};
use lsp_types::{LocationLink, Position, Range};

use crate::word;

pub struct PineDefinition;

impl DefinitionProvider for PineDefinition {
    fn definitions(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<anyhow::Result<Vec<LocationLink>>> {
        Task::ready(Ok(definition_at(text, offset)))
    }
}

fn definition_at(text: &Rope, offset: usize) -> Vec<LocationLink> {
    // The shared analysis: this runs on every F12 and every Ctrl-click.
    let source = text.to_string();
    let analysis = crate::cache::analysis(&source);
    let loader = crate::workspace::loader();

    definition_with(text, offset, &analysis, loader.as_ref())
}

/// Test entry point: builds its own analysis, so a test can point at a temporary
/// library directory without touching the process-wide cache.
#[cfg(test)]
fn definition_at_with(
    text: &Rope,
    offset: usize,
    loader: Option<&pine_lang::DirLoader>,
) -> Vec<LocationLink> {
    let source = text.to_string();
    let analysis = crate::analysis::Analysis::run_with(&source, loader);

    definition_with(text, offset, &analysis, loader)
}

/// The resolving half, given an analysis to work from.
///
/// `loader` is the concrete [`pine_lang::DirLoader`], not the trait object: besides
/// reading a library during analysis, go-to-definition has to map a library's name
/// back to the file on disk, and only the loader knows that mapping.
fn definition_with(
    text: &Rope,
    offset: usize,
    analysis: &crate::analysis::Analysis,
    loader: Option<&pine_lang::DirLoader>,
) -> Vec<LocationLink> {
    let Some((_word, origin_start, origin_end)) = word::word_at(text, offset) else {
        return vec![];
    };
    let Some(symbols) = analysis.symbols.as_ref() else {
        return vec![];
    };

    // `symbol_at` takes pinecone's 1-based (line, column).
    let Some(id) = symbols.symbol_at(0, origin_start.line + 1, origin_start.character + 1) else {
        return vec![];
    };
    let Some((file, line, column)) = symbols.declaration_location(id) else {
        return vec![];
    };

    let symbol = symbols.symbol(id);

    // An import alias is declared by the `import` line itself, so its own
    // `declaration_location` would jump in place. What the reader wants there is
    // the library: open the file it names.
    if symbol.kind == pine_lang::sema::SymbolKind::Import {
        if let Some((uri, range)) = import_target(analysis, &symbol.name, loader) {
            return vec![LocationLink {
                origin_selection_range: Some(Range {
                    start: origin_start,
                    end: origin_end,
                }),
                target_uri: uri,
                target_range: range,
                target_selection_range: range,
            }];
        }
        // No loader (or an unresolvable path): fall through to the in-place jump.
    }

    let target_start = Position::new(line.saturating_sub(1), column.saturating_sub(1));
    let width = symbol.name.encode_utf16().count().max(1) as u32;
    let target_end = Position::new(target_start.line, target_start.character + width);
    let target = Range {
        start: target_start,
        end: target_end,
    };

    vec![LocationLink {
        // The identifier being jumped from, so the editor can highlight the link.
        origin_selection_range: Some(Range {
            start: origin_start,
            end: origin_end,
        }),
        target_uri: target_uri(loader, symbols, file),
        target_range: target,
        target_selection_range: target,
    }]
}

/// The library an `import … as <alias>` names, plus the line to land on.
///
/// The target is the library's `library()` declaration rather than byte 0: that
/// is the line that says "this file is the library you asked for".
fn import_target(
    analysis: &crate::analysis::Analysis,
    alias: &str,
    loader: Option<&pine_lang::DirLoader>,
) -> Option<(lsp_types::Uri, Range)> {
    let loader = loader?;
    let path = import_path(analysis.program.as_ref()?, alias)?;
    let file = loader.resolve_path(&path)?;

    // The loader already read this file during analysis, so the source is cached;
    // without it, the top of the file is still a fine place to land.
    let line = loader
        .source_of(&path)
        .and_then(|library| {
            library
                .lines()
                .position(|line| line.trim_start().starts_with("library("))
        })
        .unwrap_or(0) as u32;
    let position = Position::new(line, 0);

    Some((crate::file_url::encode(&file), Range::new(position, position)))
}

/// The `path` of the `import <path> as <alias>` statement that declares `alias`.
///
/// The symbol table records the alias and where it was declared, not the library
/// it points at, so the statement has to be read back out of the AST — which the
/// shared analysis already parsed.
fn import_path(program: &pine_lang::ast::Program, alias: &str) -> Option<String> {
    program.statements.iter().find_map(|statement| match statement {
        pine_lang::ast::Stmt::Import { path, alias: name, .. } if name == alias => {
            Some(path.clone())
        }
        _ => None,
    })
}

/// Where to send the editor for a symbol declared in `file`.
///
/// For a library that is a real path — `show_document` needs one to open the
/// file. For the script's own file there is nothing to open (it is already on
/// screen), so the in-document jump is left to the editor.
fn target_uri(
    loader: Option<&pine_lang::DirLoader>,
    symbols: &pine_lang::sema::SymbolTable,
    file: pine_lang::sema::FileId,
) -> lsp_types::Uri {
    if file != pine_lang::sema::SymbolTable::MAIN {
        if let Some(path) = loader.and_then(|loader| loader.resolve_path(symbols.file_path(file))) {
            return crate::file_url::encode(&path);
        }
    }
    file_uri(symbols.file_path(file))
}

/// A `file://` URI for a symbol table file. Only the scheme matters to the
/// editor (anything that is not `http(s)` jumps in place), so the path is
/// informational.
fn file_uri(path: &str) -> lsp_types::Uri {
    let encoded: String = path
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '-' | '_' | '~') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    format!("file:///{encoded}")
        .parse()
        .unwrap_or_else(|_| "file:///pine".parse().expect("a valid fallback URI"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRIPT: &str = "//@version=5\nindicator(\"x\")\nmyLen = 20\nplot(close, length=myLen)\n";

    fn rope(source: &str) -> Rope {
        Rope::from(source)
    }

    /// Byte offset of the `nth` occurrence of `needle`.
    fn offset_of(source: &str, needle: &str, nth: usize) -> usize {
        source
            .match_indices(needle)
            .nth(nth)
            .unwrap_or_else(|| panic!("{needle} #{nth} not found"))
            .0
    }

    #[test]
    fn a_use_jumps_to_its_declaration() {
        let source = SCRIPT;
        let text = rope(source);
        // `myLen` used on line 4 (index 3).
        let offset = offset_of(source, "myLen", 1);
        let links = definition_at(&text, offset);

        assert_eq!(links.len(), 1);
        let link = &links[0];
        // Declared on line 3 (index 2), column 1 (index 0).
        assert_eq!(link.target_selection_range.start, Position::new(2, 0));
        assert_eq!(link.target_selection_range.end, Position::new(2, 5));
        // The origin is the use site on line 4.
        assert_eq!(
            link.origin_selection_range.map(|range| range.start.line),
            Some(3)
        );
    }

    #[test]
    fn a_builtin_has_no_definition() {
        let source = SCRIPT;
        let text = rope(source);
        // `close` is a builtin, not a script declaration.
        let offset = offset_of(source, "close", 0);
        assert!(definition_at(&text, offset).is_empty());
    }

    #[test]
    fn whitespace_has_no_definition() {
        // A line of only spaces, so the offset cannot land on a neighbouring
        // token the way it does inside `a = b`.
        let source = "//@version=5\nmyLen = 1\n   \nplot(myLen)\n";
        let text = rope(source);
        let offset = source.find("   ").expect("the blank line") + 1;

        assert!(word::word_at(&text, offset).is_none(), "no word on blank line");
        assert!(definition_at(&text, offset).is_empty());
    }

    #[test]
    fn file_uris_are_parseable() {
        assert_eq!(file_uri("main").scheme().map(|s| s.as_str()), Some("file"));
        assert_eq!(
            file_uri("libs/my lib/1").scheme().map(|s| s.as_str()),
            Some("file")
        );
    }

    /// The import alias has no declaration to jump to inside the library, so the
    /// target is the library file itself — landing on its `library()` line rather
    /// than byte 0.
    #[test]
    fn an_import_alias_jumps_to_the_library_file() {
        let dir = std::env::temp_dir().join(format!("pineditor-import-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(
            dir.join("mylib.pine"),
            "//@version=6
library(\"mylib\")
\nexport add(x, y) => x + y\n",
        )
        .expect("write library");

        let source = "//@version=6\nindicator(\"x\")\nimport mylib as lib\nplot(lib.add(1, 2))\n";
        let text = rope(source);
        // `lib` in `import mylib as lib` — the second `lib`, not the one in `mylib`.
        let offset = offset_of(source, "lib", 1);

        let loader = pine_lang::DirLoader::new(vec![dir.clone()]);
        let links = definition_at_with(&text, offset, Some(&loader));

        assert_eq!(links.len(), 1, "the alias should resolve to its library");
        assert_eq!(
            links[0].target_uri.as_str(),
            crate::file_url::encode(&dir.join("mylib.pine")).as_str(),
        );
        assert_eq!(
            links[0].target_range.start,
            Position::new(1, 0),
            "should land on the `library()` line"
        );

        // Without a loader there is no file to open, so the jump stays in place:
        // the alias itself, at line 3 (index 2), column 17 (index 16).
        let in_place = definition_at_with(&text, offset, None);
        assert_eq!(in_place.len(), 1);
        assert_eq!(in_place[0].target_range.start, Position::new(2, 16));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The reason for threading a loader through: with one, a name defined in an
    /// imported library resolves, and the link points at the library file. This
    /// is the difference the editor could not show before — the same call yields
    /// nothing without the loader.
    #[test]
    fn an_imported_function_jumps_into_its_library() {
        let dir = std::env::temp_dir().join(format!("pineditor-def-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(
            dir.join("mylib.pine"),
            "//@version=6\nlibrary(\"mylib\")\n\nexport add(x, y) => x + y\n",
        )
        .expect("write library");

        let source = "//@version=6\nindicator(\"x\")\nimport mylib as lib\nplot(lib.add(1, 2))\n";
        let text = rope(source);
        let offset = offset_of(source, "add", 0);

        let loader = pine_lang::DirLoader::new(vec![dir.clone()]);
        let links = definition_at_with(&text, offset, Some(&loader));

        assert_eq!(links.len(), 1, "the library's `add` should resolve");
        // A real path, not the symbol table's `mylib` key: `show_document` has to
        // be able to open it.
        assert_eq!(
            links[0].target_uri.as_str(),
            crate::file_url::encode(&dir.join("mylib.pine")).as_str(),
        );

        // Without a loader the import is left unresolved, so nothing to jump to.
        assert!(definition_at_with(&text, offset, None).is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }
}
