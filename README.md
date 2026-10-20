# pineditor

A Pine Script desktop editor built on [gpui-kit](https://gpui-kit.com), with
syntax highlighting, completion, hover, go-to-definition, find-references,
document symbols, rename, formatting, diagnostics and live lexer/AST dumps — all
driven by the pinecone crates next to it.

```bash
cd rust/pineditor
cargo run                            # open the editor window
cargo run -- --dump                  # print tokens + AST + diagnostics, then exit
cargo run -- --lib tests/data        # extra library root (repeatable)
cargo test                           # unit tests, no display needed
```

The window has the editor on the left and a dump panel on the right with five
tabs: **词法 Token**, **语法树 AST**, **诊断**, **引用**, **符号**. The first
three re-run on every keystroke; the last two are filled on demand, since they
answer a question about one symbol rather than describing the whole file.

| Shortcut | Action |
| --- | --- |
| `F12` | go to definition of the symbol at the cursor |
| `ctrl-shift-b` | jump back to where the last definition jump started |
| `secondary-o` / `secondary-s` | open a `.pine` file / save it back |
| `secondary-shift-f` | format the buffer (one undo step) |
| `secondary-shift-r` | rename the symbol under the cursor |
| `secondary-shift-g` | find references |
| `secondary-shift-o` | document symbols |

`secondary` is Cmd on macOS and Ctrl elsewhere.

## Why no tree-sitter grammar

gpui-kit highlights with tree-sitter, and no `tree-sitter-pine` exists. But the
editor also accepts **semantic tokens** (`DocumentRangeSemanticTokensProvider`),
which are plain `(range, type-name)` pairs resolved against the same theme
vocabulary as the tree-sitter path (`"keyword"`, `"string"`, …).

So Pine is registered as a grammar-less language:

```rust
LanguageRegistry::singleton().register("pine", &GrammarConfig::plain("pine"));
```

and color comes from `pine_lexer`'s own tokens instead. That is both less work
and more accurate than a generic grammar — the tokenizer knows Pine's keywords,
operators and trivia exactly.

One wrinkle: gpui-base consults semantic-token providers inside
`highlight_lines`, **after** it has found an `InputHighlighter`, and returns
early when there is none. The component factory builds a highlighter only for
languages with a parser, so a grammar-less language would have every semantic
token silently dropped at paint time — as would its diagnostics. `highlight.rs`
therefore installs a no-op `PineHighlighter` (one unstyled run per visible
range, no parse, no folds) to keep that path reachable; the semantic colours
are composed on top of it and keep theirs.

## Layout

| file | role |
|---|---|
| `src/main.rs` | the app: window, two-pane layout, dump panel, commands, keystroke → refresh |
| `src/analysis.rs` | one parse of the buffer → tokens, AST, symbols, diagnostics, squiggles |
| `src/cache.rs` | that analysis, shared by hover, definition and highlighting |
| `src/highlight.rs` | `PineSemanticTokens`: pinecone tokens → LSP semantic tokens |
| `src/completion.rs` | `PineCompletion`: pinecone's builtin registry → completions |
| `src/hover.rs` | `PineHover`: symbol table + offline reference docs |
| `src/definition.rs` | `PineDefinition`: symbol table → jump to declaration, incl. libraries |
| `src/navigation.rs` | find references and document symbols, as text for the dump panel |
| `src/rename.rs` | rename a symbol across the current file |
| `src/formatting.rs` | the buffer through pinecone's formatter |
| `src/document.rs` | the file behind the buffer: path, atomic save |
| `src/workspace.rs` | library roots, and the jump-back history |
| `src/file_url.rs` | `file:` URI encode/decode for cross-file jumps |
| `src/word.rs` | the identifier under the cursor, shared by hover and definition |
| `src/ui_layout_test.rs` | headless layout checks (see below) |
| `fixtures/example.pine` | the buffer the editor opens with |
| `fixtures/mylib.pine` | the library it imports, so the jump has somewhere to go |

`highlight.rs` is the only part that touches gpui-kit's rendering contract; the
providers are plain pinecone calls and can be reused by any frontend.

`src/ui_layout_test.rs` renders a real frame in a headless window and measures
the dump list. That is not ceremony: `uniform_list` lays its rows out itself and
hands taffy no children, so without an explicit `size_full()` it collapses to its
own padding and the whole panel looks empty — a bug that is invisible without a
display and therefore needs a test that draws one.

## One parse per edit

Every panel is derived from a single `Analysis::run`:

```
           ┌─ tokens ──► 词法 Token tab, squiggle widths
source ────┤
           └─ program ─► 语法树 AST tab, sema + lint ─► 诊断 tab, squiggles
```

Before this, each keystroke ran the front end once per view — `lexer_dump` lexed,
`ast_dump` lexed again inside the parser, `diagnostics_dump` lexed and analyzed,
`editor_diagnostics` analyzed a second time and lexed once more to size the
squiggles. Six lexes and two analyses for one character typed.

Parsing is also **debounced** by 150 ms (`REFRESH_DEBOUNCE`), and a re-parse is
only scheduled on `InputEvent::Change` — focus and blur emit events too, and used
to trigger a full analysis for nothing. Replacing the stored `Task` cancels the
pending one, so a burst of typing ends in exactly one analysis.

The first parse runs synchronously when the window opens, so the panels are never
blank. `PINEDITOR_TRACE=1` prints one line per analysis, which is how you check
that the count matches what you typed.

## Completion and diagnostics

**Completion** comes straight out of `pine_builtins::register_namespace_objects`
— the same call the compiler makes — so it lists exactly what the language
accepts: every namespace member (`ta.sma`, `str.tostring`, `array.push`), the
callable builtins (`plot`, `label`), keywords and series variables. Currently
827 items, none hand-maintained. Typing after a `.` narrows to that namespace
because the identifier prefix keeps dots.

Accepting an item replaces the **whole qualified prefix**, not just its last
segment: completing `ta.s` yields `ta.sma`, not `ta.ta.sma`. That needs an
explicit `text_edit` range — the editor's fallback is the word before the cursor,
and `.` is not a word character, so it would only cover the `s`.

**Diagnostics** come from `pine_lang::check`, which runs the semantic analyzer
and the linter in one pass. Positions are converted from pinecone's 1-based
character `(line, column)` to the editor's 0-based `Position`, and each squiggle
is widened to cover the whole token (a one-character underline is invisible).
The same call feeds the **诊断** tab, so the tab and the underlines cannot
disagree.

The bundled fixture deliberately shadows the builtin `str` namespace (search for
`str =` in `fixtures/example.pine`), so a fresh window shows one
`shadows-builtin` warning — proof the wiring works.

## Hover and go-to-definition

Both read `pine_lang::analyze`, which already builds a symbol table as it
analyzes — so neither re-implements name resolution:

- **Hover** on a declared name shows its kind, parameters, type annotation and
  where it is declared. On a builtin it falls back to pinecone's reference
  markdown, which ships in `crates/pine-reference/spec` (v3–v6), so no network
  and no scraping is involved. Lookups are cached — the spec is ~900 KB and
  parsing it per mouse move would be wasteful.

  Namespace members need a second pass: `word_at` stops at the dot, so hovering
  `ta.sma` only yields `sma`, and the reference has no such entry. The namespace
  is read back from the text before the cursor and `ta.sma` is looked up instead.
  Columns there are counted in `char`s — the editor's own `Position` — not in
  LSP's UTF-16 units.
- **Go to definition** is `symbol_at` → `declaration_location`, turned into a
  `LocationLink`. Same-file symbols come back as a `file:` URI the editor jumps
  to in place; a symbol declared in an imported library resolves to the library's
  real path and is opened through `show_document` (see below).

  Three ways to trigger it, and they are not equivalent: **F12** asks the
  provider directly; the **context menu** entry (`Input.Go to Definition`) does
  the same; **Ctrl+click** only works *after* the symbol has been hovered (the
  click consumes the definition that hovering cached, which is why hovering also
  underlines the identifier). On an `import … as lib` line, jumping from `lib`
  opens the library and lands on its `library()` declaration; from `lib.add` it
  lands on `export add`.

Both are handed a text buffer and an offset — never a path — so the directories
`import` resolves against are published separately: opening a file records its
roots in `workspace`, and every analysis builds a `pine_lang::DirLoader` from
them. A library rarely sits next to the script that imports it, so the roots are,
in order:

1. `--lib <dir>` / `PINEDITOR_LIB_DIR` (colon-separated), if given,
2. the open file's own directory,
3. every `libraries/` directory found walking up from it (up to four levels) —
   which is how pinecone's own test data is laid out (`tests/testdata/import/…`
   imports from `tests/testdata/libraries/`).

`DirLoader` tries each root in turn until a file exists, so the order only
decides which copy wins when a name is duplicated.

The bundled fixture is `include_str!`-ed, so its buffer has no path — but it was
compiled from `fixtures/example.pine`, and the roots are derived from *that*
directory. That is why the demo script's `import mylib as lib` resolves against
`fixtures/mylib.pine`. The buffer still counts as unnamed, so `secondary-s`
offers "save as" rather than overwriting the file in the repository. With no
roots at all, a `cannot load library` diagnostic is the honest answer rather
than a silent skip.

`pine_reference` resolves its spec directory relative to the **current working
directory**, which breaks when the binary starts in `rust/pineditor`. Rather
than fork it, `ensure_spec_root()` points the process at the pinecone checkout
once, on first use. If the spec is missing, hover quietly degrades to showing
just the name.

## Opening libraries, formatting, rename

`show_document` is gpui-base's hook for "a definition lives in another file".
`file_url` encodes the path into the URI and decodes it back, and the handler
loads the file into the buffer and puts the cursor on the declaration. There is
still only one buffer: jumping into a library replaces what you were editing, so
save first.

Each of those jumps records where it started — a snapshot of the buffer, so unsaved
edits and the unnamed fixture come back too — and `ctrl-shift-b` walks back
through them (reopening the earlier file and restoring the caret). Only jumps
into another file are recorded — an in-file jump is handled inside the text
editor itself and never reaches the host, so there is nothing to undo for it.
Recording has to happen in the deferred callback, not in the handler: reading the
caret while the editor is mid-update is as illegal as writing it.

Formatting is `pine_lang::format::format` applied through `replace_all`, which
keeps it on the undo stack — deliberately unlike `set_value`, which clears the
history (that is what opening a file uses).

Rename and find-references both come from the symbol table, so a name shadowed in
an inner scope is its own symbol and is not dragged along. Rename only rewrites
the current file: a symbol declared in a library is refused rather than having
its call sites detached from the library.

## Details worth knowing

- **Positions are counted in `char`s, everywhere.** `pine_lexer` reports 1-based
  `(line, column)` in characters, and the editor's own `offset_to_position` also
  counts characters, so nothing converts to UTF-16 — not `highlight::encode`, not
  `token_widths`. Mixing the two units (which the code once did) shifts every
  token after an astral character by one column per extra code unit; CJK text is
  BMP and hides the bug. Both paths have a test with an emoji.
- **Trivia is dropped from color but kept in the dump**: `Newline`, `Indent`,
  `Dedent`, `Eof`, `BlankLine` get no highlight (they have no visible glyphs),
  but the token panel shows them — they are exactly what you need when
  indentation-sensitive parsing misbehaves.
- **A lex error silences highlighting** rather than painting a half-parsed
  document; the diagnostics tab reports the error instead.
- Set `PINEDITOR_TRACE=1` to print the token → semantic-token counts on every
  refresh.

## Known limits

- The dump panel renders up to `MAX_DUMP_LINES` (4000) lines as plain elements;
  there is no virtualization. A huge script's AST will be truncated.
- `ta.sma` and friends are lexed as `Ident Dot Ident`: there is no parser-level
  symbol resolution yet, so `sma` gets the `function` color only because it is
  followed by `(`, not because it was resolved to a builtin.
- Cross-file go-to-definition replaces the buffer instead of opening a second
  tab, so the library file you jumped into *is* what you are editing. A multi-tab
  workspace would fix this.
- Rename refuses symbols declared in a library, and does not rename across
  files.
- Builtin hover only covers names in the reference spec (`Functions`,
  `Variables`, `Constants`). Namespace members are documented under their
  qualified name (`ta.sma`), not the bare `sma`.
