//! Find references and document symbols, rendered as text for the dump panel.
//!
//! Both are answers the analyzer already has — `SymbolTable::references` and the
//! per-file symbol list — so this module only decides how to lay them out. The
//! panel is plain text (no clickable rows), which is why positions are printed
//! rather than turned into links.

use gpui_kit::component::input::Rope;
use pine_lang::sema::SymbolTable;
use pine_lang::DirLoader;

fn analyze(source: &str, loader: Option<&DirLoader>) -> Option<pine_lang::Analysis> {
    let resolving = loader.map(|loader| loader as &dyn pine_lang::LibraryLoader);
    pine_lang::analyze(source, resolving).ok()
}

/// Every place the identifier under the cursor is declared or used.
///
/// References in imported libraries are reported with their file, since a bare
/// `line:column` would be ambiguous there.
pub fn references_report(text: &Rope, offset: usize, loader: Option<&DirLoader>) -> String {
    let Some((word, start, _)) = crate::word::word_at(text, offset) else {
        return "没有可查找的标识符\n".to_string();
    };
    let source = text.to_string();
    let Some(analysis) = analyze(&source, loader) else {
        return "无法解析当前缓冲区\n".to_string();
    };
    let symbols = &analysis.symbols;

    let Some(id) = symbols.symbol_at(SymbolTable::MAIN, start.line + 1, start.character + 1) else {
        return format!("`{word}` 不是本脚本声明的符号\n");
    };

    let symbol = symbols.symbol(id);
    let mut out = format!("{} `{}`\n", symbol.kind.noun(), symbol.name);
    match symbols.declaration_location(id) {
        Some((file, line, column)) => {
            out.push_str(&format!("声明于 {}\n", location(symbols, file, line, column)));
        }
        None => out.push_str("没有声明位置\n"),
    }

    let references: Vec<String> = symbols
        .references(id)
        .map(|(file, line, column)| location(symbols, file, line, column))
        .collect();
    out.push_str(&format!("\n{} 处引用\n", references.len()));
    for reference in references {
        out.push_str(&format!("  {reference}\n"));
    }
    out
}

/// `line:column`, prefixed with the file when it is not the script itself.
fn location(symbols: &SymbolTable, file: pine_lang::sema::FileId, line: u32, column: u32) -> String {
    if file == SymbolTable::MAIN {
        format!("{line}:{column}")
    } else {
        format!("{}:{line}:{column}", symbols.file_path(file))
    }
}

/// The script's own top-level symbols, in declaration order.
pub fn symbols_report(text: &Rope, loader: Option<&DirLoader>) -> String {
    let source = text.to_string();
    let Some(analysis) = analyze(&source, loader) else {
        return "无法解析当前缓冲区\n".to_string();
    };
    let symbols = &analysis.symbols;

    let root = symbols.file_root(SymbolTable::MAIN);
    let mut out = String::from("本文件的符号\n\n");
    let mut count = 0;
    for symbol in symbols.symbols_in(root) {
        let Some((line, column)) = symbol.decl else {
            continue;
        };
        count += 1;
        out.push_str(&format!(
            "{:<10} {:<20} {line}:{column}\n",
            symbol.kind.noun(),
            signature(symbol)
        ));
    }
    out.push_str(&format!("\n{count} 个符号\n"));
    out
}

/// `name(a, b)` for a function, `name` for anything else.
fn signature(symbol: &pine_lang::sema::Symbol) -> String {
    if symbol.params.is_empty() {
        return symbol.name.clone();
    }
    format!("{}({})", symbol.name, symbol.params.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRIPT: &str = "//@version=5\nindicator(\"x\")\nmyLen = 20\nf(a) => a + myLen\nplot(f(close), length=myLen)\n";

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
    fn references_lists_the_declaration_and_every_use() {
        let text = rope(SCRIPT);
        // `myLen` used as a keyword argument on line 5.
        let report = references_report(&text, offset_of(SCRIPT, "myLen", 2), None);

        assert!(report.contains("variable"), "{report}");
        assert!(report.contains("myLen"), "{report}");
        assert!(report.contains("声明于 3:1"), "{report}");
        // Used once in `f` and once as `length=myLen`; the declaration is separate.
        assert!(report.contains("2 处引用"), "{report}");
        assert!(report.contains("4:13"), "{report}");
        assert!(report.contains("5:23"), "{report}");
    }

    #[test]
    fn a_builtin_is_not_a_script_symbol() {
        let text = rope(SCRIPT);
        let report = references_report(&text, offset_of(SCRIPT, "close", 0), None);
        assert!(report.contains("不是本脚本声明的符号"), "{report}");
    }

    #[test]
    fn document_symbols_lists_declarations_with_positions() {
        let text = rope(SCRIPT);
        let report = symbols_report(&text, None);

        assert!(report.contains("2 个符号"), "{report}");
        assert!(report.contains("myLen"), "{report}");
        assert!(report.contains("f(a)"), "{report}");
        assert!(report.contains("3:1"), "{report}");
    }

    #[test]
    fn a_source_that_does_not_parse_says_so() {
        let text = rope("if\n");
        assert!(references_report(&text, 0, None).contains("无法解析"));
        assert!(symbols_report(&text, None).contains("无法解析"));
    }
}
