//! One analysis, shared by everything that needs it.
//!
//! Hover, go-to-definition and semantic highlighting each used to run the front
//! end themselves: lex, parse and analyze, synchronously, on every mouse move and
//! every repaint. That is the same work up to five times for one keystroke's worth
//! of text. They now ask here instead, and the answer is reused as long as neither
//! the text nor the library roots have changed.
//!
//! `Analysis` holds a `SymbolTable` and a `Program`, neither of which is `Send`, so
//! the cache is thread-local rather than global. That matches how it is used: the
//! editor's providers all run on the UI thread.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use crate::analysis::Analysis;

struct Entry {
    /// The exact text this analysis was produced from.
    source: String,
    /// The roots it was produced under — `import` resolution depends on them, so a
    /// cached analysis is only valid while they are unchanged.
    roots: Vec<PathBuf>,
    analysis: Rc<Analysis>,
}

thread_local! {
    static LAST: RefCell<Option<Entry>> = const { RefCell::new(None) };
}

/// The analysis of `source` under the current library roots.
///
/// Reuses the previous result when the text and the roots both match, which is the
/// common case: hovering several names in a row, or repainting while nothing
/// changed.
pub fn analysis(source: &str) -> Rc<Analysis> {
    let roots = crate::workspace::roots();

    LAST.with(|last| {
        let mut last = last.borrow_mut();
        if let Some(entry) = last.as_ref() {
            if entry.source == source && entry.roots == roots {
                return entry.analysis.clone();
            }
        }

        let analysis = Rc::new(Analysis::run(source));
        *last = Some(Entry {
            source: source.to_string(),
            roots,
            analysis: analysis.clone(),
        });
        analysis
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRIPT: &str = "//@version=5\nindicator(\"x\")\nmyLen = 20\nplot(close, length=myLen)\n";

    #[test]
    fn the_same_source_is_only_analyzed_once() {
        let first = analysis(SCRIPT);
        let second = analysis(SCRIPT);

        // Same allocation, not merely equal contents: that is what "reused" means.
        assert!(Rc::ptr_eq(&first, &second));
    }

    #[test]
    fn a_changed_source_is_analyzed_again() {
        let first = analysis(SCRIPT);
        let second = analysis("//@version=5\nindicator(\"y\")\nplot(close)\n");

        assert!(!Rc::ptr_eq(&first, &second));
        assert!(!second.tokens.is_empty());
    }

    #[test]
    fn the_cached_analysis_carries_what_the_providers_need() {
        let analysis = analysis(SCRIPT);

        assert!(analysis.program.is_some(), "definition needs the AST");
        assert!(analysis.symbols.is_some(), "hover needs the symbol table");
        assert!(!analysis.tokens.is_empty(), "highlighting needs the tokens");
    }
}
