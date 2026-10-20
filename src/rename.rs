//! Renaming a symbol across the current file.
//!
//! The editor has no rename hook (`Lsp` only exposes completion, code actions,
//! hover, definition, colors and semantic tokens), so this is a command of our
//! own: compute the renamed text, then put it in the buffer with `replace_all` so
//! the whole rename is one undo step.
//!
//! Sites are produced from the analyzer's symbol table — the declaration and
//! every recorded reference — which means a name that is shadowed elsewhere is
//! not touched by accident: those uses resolve to a different symbol id.
//!
//! Only the script's own file is rewritten. A symbol exported by a library is
//! declared in *that* file, and editing another file behind the user's back is
//! not something a keystroke should do.

use gpui_kit::component::input::Rope;
use pine_lang::sema::SymbolTable;
use pine_lang::DirLoader;

/// Words the lexer turns into a token rather than an identifier.
///
/// Renaming to one of these produces a buffer that parses as syntax, not as the
/// symbol the user meant — worth refusing before the edit rather than after.
const KEYWORDS: &[&str] = &[
    "and", "break", "const", "continue", "else", "enum", "export", "false", "float", "for", "if",
    "import", "in", "int", "method", "na", "not", "or", "switch", "to", "true", "type", "var",
    "varip", "while",
];

/// Whether `name` can be used as a declaration name in Pine.
pub fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let well_formed = chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_');

    well_formed && !KEYWORDS.contains(&name)
}

/// `source` with every occurrence of the symbol under `offset` renamed.
///
/// `None` when the cursor is not on a symbol this file declares (a builtin, or
/// whitespace), or when `new_name` is not a usable identifier.
pub fn rename_source(
    text: &Rope,
    offset: usize,
    new_name: &str,
    loader: Option<&DirLoader>,
) -> Option<String> {
    if !is_identifier(new_name) {
        return None;
    }

    let (_word, start, _) = crate::word::word_at(text, offset)?;
    let source = text.to_string();

    let resolving = loader.map(|loader| loader as &dyn pine_lang::LibraryLoader);
    let analysis = pine_lang::analyze(&source, resolving).ok()?;
    let symbols = &analysis.symbols;

    let id = symbols.symbol_at(SymbolTable::MAIN, start.line + 1, start.character + 1)?;
    let old_name = symbols.symbol(id).name.clone();
    if old_name == new_name {
        return Some(source);
    }

    // A symbol declared in an imported library has no declaration here: renaming
    // the call sites would only detach them from the library.
    let (declaring_file, decl_line, decl_column) = symbols.declaration_location(id)?;
    if declaring_file != SymbolTable::MAIN {
        return None;
    }

    let mut sites: Vec<(u32, u32)> = vec![(decl_line, decl_column)];
    sites.extend(
        symbols
            .references(id)
            .filter(|(file, _, _)| *file == SymbolTable::MAIN)
            .map(|(_, line, column)| (line, column)),
    );

    Some(rename_at(&source, &sites, &old_name, new_name))
}

/// Replace `old` with `new` at every 1-based `(line, column)` in `sites`.
///
/// Columns are walked right to left within a line so earlier replacements cannot
/// shift later ones, and lines are split inclusively to keep the file's trailing
/// newline exactly as it was.
fn rename_at(source: &str, sites: &[(u32, u32)], old: &str, new: &str) -> String {
    let width = old.chars().count();

    source
        .split_inclusive('\n')
        .enumerate()
        .map(|(index, line)| {
            let line_number = index as u32 + 1;
            let mut chars: Vec<char> = line.chars().collect();

            let mut columns: Vec<u32> = sites
                .iter()
                .filter(|(line, _)| *line == line_number)
                .map(|(_, column)| *column)
                .collect();
            columns.sort_unstable_by(|a, b| b.cmp(a));

            for column in columns {
                let start = column.saturating_sub(1) as usize;
                let end = start + width;
                if end <= chars.len() {
                    chars.splice(start..end, new.chars());
                }
            }

            chars.into_iter().collect::<String>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRIPT: &str =
        "//@version=5\nindicator(\"x\")\nmyLen = 20\nf(a) => a + myLen\nplot(f(close), length=myLen)\n";

    fn rope(source: &str) -> Rope {
        Rope::from(source)
    }

    fn offset_of(source: &str, needle: &str, nth: usize) -> usize {
        source
            .match_indices(needle)
            .nth(nth)
            .unwrap_or_else(|| panic!("{needle} #{nth} not found"))
            .0
    }

    #[test]
    fn every_use_and_the_declaration_are_renamed() {
        let text = rope(SCRIPT);
        let renamed =
            rename_source(&text, offset_of(SCRIPT, "myLen", 0), "len", None).expect("renames");

        assert!(!renamed.contains("myLen"), "{renamed}");
        assert!(renamed.contains("len = 20"), "{renamed}");
        assert!(renamed.contains("f(a) => a + len"), "{renamed}");
        // Everything else, including the call `f`, is untouched.
        assert!(renamed.contains("plot(f(close), length=len)"), "{renamed}");
    }

    #[test]
    fn renaming_to_the_same_name_leaves_the_text_alone() {
        let text = rope(SCRIPT);
        let renamed = rename_source(&text, offset_of(SCRIPT, "myLen", 0), "myLen", None)
            .expect("still a symbol");
        assert_eq!(renamed, SCRIPT);
    }

    #[test]
    fn an_invalid_name_is_refused() {
        let text = rope(SCRIPT);
        for bad in ["", "1abc", "a b", "a-b", "if", "na", "true"] {
            assert!(
                rename_source(&text, offset_of(SCRIPT, "myLen", 0), bad, None).is_none(),
                "`{bad}` must not be accepted"
            );
        }
    }

    #[test]
    fn a_builtin_cannot_be_renamed() {
        let text = rope(SCRIPT);
        assert!(rename_source(&text, offset_of(SCRIPT, "close", 0), "cls", None).is_none());
    }

    #[test]
    fn a_shadowing_local_is_its_own_symbol() {
        // `x` inside the function must not take the outer `x` with it.
        let source = "//@version=5\nx = 1\nf() =>\n    x = 2\n    x\nplot(x)\n";
        let text = rope(source);
        // The inner `x` on line 4.
        let renamed = rename_source(&text, offset_of(source, "x = 2", 0), "y", None)
            .expect("renames the local");

        assert!(renamed.contains("y = 2"), "{renamed}");
        assert!(renamed.contains("\n    y\n"), "{renamed}");
        assert!(renamed.contains("x = 1"), "outer x must survive: {renamed}");
        assert!(renamed.contains("plot(x)"), "outer x must survive: {renamed}");
    }

    #[test]
    fn only_the_current_file_is_rewritten() {
        let dir = std::env::temp_dir().join(format!("pineditor-rename-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(
            dir.join("mylib.pine"),
            "//@version=6\nlibrary(\"mylib\")\n\nexport add(x, y) => x + y\n",
        )
        .expect("write library");

        let source = "//@version=6\nindicator(\"x\")\nimport mylib as lib\nplot(lib.add(1, 2))\n";
        let text = rope(source);
        let loader = pine_lang::DirLoader::new(vec![dir.clone()]);

        // `add` is declared in the library, so there is nothing in this file to
        // rename — the call is not a declaration site here.
        assert!(rename_source(&text, offset_of(source, "add", 0), "plus", Some(&loader)).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }
}
