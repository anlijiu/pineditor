//! Formatting the buffer through pinecone's own formatter.
//!
//! Kept out of `main.rs` on purpose: `use super::*` there would drag in
//! `gpui_kit::test`, which shadows the built-in `#[test]` attribute and sends the
//! macro into unbounded recursion.

/// What formatting the buffer would do.
///
/// The three cases are kept apart because the caller has to say something
/// different for each: a rewrite, "nothing to do", and "your script has a syntax
/// error". Collapsing them into `Option` — as this used to — leaves the user
/// pressing the key and seeing nothing happen, twice for two different reasons.
#[derive(Debug, PartialEq, Eq)]
pub enum FormatOutcome {
    Formatted(String),
    AlreadyFormatted,
    /// The formatter parses first, so a broken buffer is left exactly as typed
    /// rather than half-rewritten.
    Unparsable,
}

pub fn format_source(source: &str) -> FormatOutcome {
    match pine_lang::format::format(source) {
        Ok(formatted) if formatted == source => FormatOutcome::AlreadyFormatted,
        Ok(formatted) => FormatOutcome::Formatted(formatted),
        Err(_) => FormatOutcome::Unparsable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_misindented_script_is_reformatted() {
        let messy = "//@version=5\nindicator(\"x\")\nif close > open\n        plot(close)\n";
        let FormatOutcome::Formatted(formatted) = format_source(messy) else {
            panic!("should reformat");
        };

        assert_ne!(formatted, messy);
        // Formatting is idempotent, so a second pass finds nothing to change.
        assert_eq!(format_source(&formatted), FormatOutcome::AlreadyFormatted);
    }

    #[test]
    fn an_already_formatted_script_is_left_alone() {
        assert_eq!(
            format_source("//@version=5\nindicator(\"x\")\nplot(close)\n"),
            FormatOutcome::AlreadyFormatted
        );
    }

    #[test]
    fn a_source_that_does_not_parse_is_reported_as_such() {
        // Distinct from "already formatted": the user needs to know why nothing
        // happened.
        assert_eq!(format_source("if\n"), FormatOutcome::Unparsable);
    }
}
