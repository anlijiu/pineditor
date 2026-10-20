//! A Pine Script editor built on gpui-kit, with syntax highlighting from
//! pinecone's own lexer and live lexer/AST dumps.
//!
//! ```text
//! cargo run
//! ```
//!
//! Layout is two panes: the editor on the left, a dump panel on the right that
//! shows the token stream, the AST, or the diagnostics for whatever is currently
//! in the buffer. It re-runs on every keystroke.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::{
    ActiveTheme, Sizable,
    button::Button,
    h_flex,
    highlighter::{GrammarConfig, LanguageRegistry},
    input::{Editor, EditorState, GoToDefinition, Input, InputEvent, InputState, Rope, TabSize},
    resizable::{h_resizable, resizable_panel},
    scroll::Scrollbar,
    status_bar::StatusBar,
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder, *};

mod analysis;
mod cache;
mod completion;
mod definition;
mod document;
mod file_url;
mod formatting;
mod highlight;
mod hover;
mod navigation;
mod rename;
#[cfg(test)]
mod ui_layout_test;
mod word;
mod workspace;

const DEFAULT_SOURCE: &str = include_str!("../fixtures/example.pine");

/// The key context the editor's own actions are bound under.
const CONTEXT: &str = "PineEditor";

actions!(pineditor, [
    OpenFile,
    SaveFile,
    FormatDocument,
    FindReferences,
    DocumentSymbols,
    RenameSymbol,
    JumpBack,
]);

/// How many lines of a dump to hand to the element tree. There is no
/// virtualization here, and a big script's AST can run to thousands of lines.
const MAX_DUMP_LINES: usize = 4000;

/// How long to wait after the last keystroke before re-parsing. Typing is bursty;
/// analyzing every intermediate buffer wastes the work six times over and, past a
/// few thousand lines, is enough to drop frames.
const REFRESH_DEBOUNCE: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Panel {
    Lexer,
    Ast,
    Diagnostics,
    References,
    Symbols,
}

impl Panel {
    fn title(self) -> &'static str {
        match self {
            Panel::Lexer => "词法 Token",
            Panel::Ast => "语法树 AST",
            Panel::Diagnostics => "诊断",
            Panel::References => "引用",
            Panel::Symbols => "符号",
        }
    }
}

pub struct PineEditor {
    editor: Entity<EditorState>,
    /// The file the buffer came from; also the root `import` is resolved against.
    document: document::Document,
    panel: Panel,
    /// Dump panels, one `Rc` per tab. Lines are split once per analysis, not per
    /// frame: `render` runs on every scroll tick, and re-splitting thousands of
    /// lines each time was what made scrolling feel heavy.
    lexer_lines: Rc<Vec<SharedString>>,
    ast_lines: Rc<Vec<SharedString>>,
    diagnostics_lines: Rc<Vec<SharedString>>,
    /// Filled on demand by `find_references`; these two are not re-run per keystroke.
    references_lines: Rc<Vec<SharedString>>,
    symbols_lines: Rc<Vec<SharedString>>,
    /// Shared by the virtual list and its scrollbar, so the thumb tracks content.
    dump_scroll: UniformListScrollHandle,
    /// Real token count, as the lexer produced it — the dump panel adds headers
    /// and a separator, and is capped, so its line count is not the token count.
    token_count: usize,
    /// What the last command had to say. Errors used to go to `eprintln!` only,
    /// which a GUI user never sees.
    status: Option<Status>,
    /// The rename prompt, while it is open.
    rename: Option<RenameBar>,
    _subscriptions: Vec<Subscription>,
    /// Cancelled (by being replaced) whenever another key lands, which is what
    /// makes the debounce work.
    _refresh_task: Task<()>,
}

/// What the status bar is reporting, and how loudly.
///
/// Success and failure used to share one red rendering, which made "已保存" look
/// like an error.
enum Status {
    Info(SharedString),
    Error(SharedString),
}

/// An open rename prompt: the identifier it will replace, and the input holding
/// the replacement. The subscription lives here so closing the prompt cancels it.
struct RenameBar {
    input: Entity<InputState>,
    target: String,
    offset: usize,
    _subscription: Subscription,
}

impl PineEditor {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Captured by the `show_document` hook below, which is handed an `App`
        // and no `Context<Self>` of its own.
        let this_entity = cx.entity();

        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("pine")
                .line_number(true)
                .indent_guides(true)
                .tab_size(TabSize {
                    tab_size: 4,
                    hard_tabs: false,
                })
                .soft_wrap(false)
                .default_value(DEFAULT_SOURCE)
        });

        // No tree-sitter grammar exists for Pine, so the syntax highlighter is
        // inert for this language (`LanguageConfig::plain`). Color comes from
        // `PineSemanticTokens` instead, which reads pinecone's tokens and maps
        // them onto the theme's own highlight vocabulary.
        editor.update(cx, |state, cx| {
            // gpui-base only reads semantic tokens once a highlighter exists,
            // and the component factory declines grammar-less languages. This
            // no-op highlighter keeps the semantic path reachable.
            state.set_highlighter_factory(highlight::highlighter_factory(), cx);
            state.lsp_mut().semantic_tokens_provider = Some(Rc::new(highlight::PineSemanticTokens));
            state.lsp_mut().completion_provider = Some(Rc::new(completion::PineCompletion));
            state.lsp_mut().hover_provider = Some(Rc::new(hover::PineHover));
            state.lsp_mut().definition_provider = Some(Rc::new(definition::PineDefinition));

            // A definition declared in an imported library points at a `file:`
            // URI. The editor would otherwise jump *within* this document, so the
            // file is opened here instead. Returning `false` leaves the editor's
            // own handling in place for anything unreadable.
            let target = this_entity.clone();
            state.lsp_mut().show_document = Some(Rc::new(move |params, window, cx| {
                let Some(path) = file_url::decode(&params.uri) else {
                    return false;
                };
                let Ok(source) = document::open(&path) else {
                    return false;
                };
                let cursor = params.selection.map(|range| range.start);

                // gpui-base calls this from `go_to_definition`, which runs inside
                // the editor's *own* update. Replacing the buffer here would be a
                // re-entrant update of that same entity, and gpui panics on it
                // ("cannot update … while it is already being updated"). Defer,
                // so the load happens once that update has finished.
                let entity = target.clone();
                window.defer(cx, move |window, cx| {
                    entity.update(cx, |this, cx| {
                        // Remember where the jump started, so `secondary-left`
                        // can come back here. Recording it inside the deferred
                        // callback (rather than above) is what makes reading the
                        // caret legal: the editor is no longer mid-update.
                        let position = this.editor.read(cx).cursor_position();
                        let previous = this.editor.read(cx).value().to_string();
                        workspace::push_jump_site(workspace::JumpSite {
                            path: this.document.path().map(Path::to_path_buf),
                            position,
                            source: previous,
                        });
                        this.load_document(Some(path), source, cursor, window, cx)
                    });
                });

                true
            }));

            cx.notify();
        });

        let focus_handle = editor.focus_handle(cx);
        window.defer(cx, move |window, cx| {
            focus_handle.focus(window, cx);
        });

        let mut this = Self {
            editor: editor.clone(),
            document: document::Document::detached(),
            panel: Panel::Lexer,
            lexer_lines: Rc::new(Vec::new()),
            ast_lines: Rc::new(Vec::new()),
            diagnostics_lines: Rc::new(Vec::new()),
            references_lines: Rc::new(Vec::new()),
            symbols_lines: Rc::new(Vec::new()),
            dump_scroll: UniformListScrollHandle::new(),
            token_count: 0,
            status: None,
            rename: None,
            _subscriptions: vec![],
            _refresh_task: Task::ready(()),
        };

        this._subscriptions = vec![cx.subscribe(&editor, |this, _, event: &InputEvent, cx| {
            // Focus/blur also emit events; only a text change needs a re-parse.
            if matches!(event, InputEvent::Change) {
                this.refresh(cx);
            }
        })];

        // The first parse runs now rather than after the debounce, so the window
        // opens with its panels filled.
        this.apply(cache::analysis(DEFAULT_SOURCE), cx);
        this
    }

    /// Report a success or a neutral outcome.
    fn set_status(&mut self, message: impl Into<SharedString>) {
        self.status = Some(Status::Info(message.into()));
    }

    /// Report a failure — the only case rendered in the error colour.
    fn set_error(&mut self, message: impl Into<SharedString>) {
        self.status = Some(Status::Error(message.into()));
    }

    /// Put `source` in the buffer as the file `path`.
    ///
    /// Shared by the open dialog and `show_document`: both replace the whole
    /// buffer with a file, and both have to re-publish the library root first.
    fn load_document(
        &mut self,
        path: Option<PathBuf>,
        source: String,
        cursor: Option<lsp_types::Position>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let roots = match &path {
            Some(path) => workspace::roots_for_file(path),
            // Back to the fixture: it has no file, but its roots still apply.
            None => workspace::fixture_roots(),
        };

        match path {
            Some(path) => self.document.attach(path),
            None => self.document.detach(),
        }
        // Hover and go-to-definition take a text buffer and an offset, never a
        // path, so the new roots have to be published before the re-parse for
        // imported symbols to resolve.
        workspace::set_document_roots(roots);

        // A debounce scheduled a moment ago still holds the *previous* file's
        // text and would paint its analysis over this one — `set_value` below
        // suppresses the `Change` event, so nothing re-schedules it. Dropping the
        // task is what cancels it.
        self._refresh_task = Task::ready(());

        // `set_value` clears the undo history *and* suppresses the
        // `InputEvent::Change` the subscription listens for, so the re-parse has
        // to be asked for here rather than arriving later.
        self.editor
            .update(cx, |state, cx| state.set_value(source.clone(), window, cx));
        self.apply(cache::analysis(&source), cx);

        if let Some(cursor) = cursor {
            self.editor
                .update(cx, |state, cx| state.set_cursor_position(cursor, window, cx));
        }
        cx.notify();
    }

    /// `secondary-o`: ask the platform for a file, then load it into the buffer.
    fn open_file(&mut self, _: &OpenFile, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("打开 Pine 脚本".into()),
        });

        cx.spawn_in(window, async move |this, cx| {
            // A cancelled dialog and a picker error both mean "nothing to open".
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let source = match document::open(&path) {
                Ok(source) => source,
                Err(err) => {
                    let message = format!("打开失败 {}：{err}", path.display());
                    this.update_in(cx, |this, _, cx| {
                        this.set_error(message);
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };

            this.update_in(cx, |this, window, cx| {
                this.load_document(Some(path), source, None, window, cx)
            })
            .ok();
        })
        .detach();
    }

    /// `secondary-left`: go back to where the last jump started.
    ///
    /// Only jumps this editor performed are recorded — that is, jumps into an
    /// imported library. An in-file jump is handled inside the text editor and
    /// never reaches us, so there is nothing to undo for it.
    fn jump_back(&mut self, _: &JumpBack, window: &mut Window, cx: &mut Context<Self>) {
        let Some(site) = workspace::pop_jump_site() else {
            return;
        };

        // Same buffer: nothing was swapped out, so the caret is the whole jump.
        if site.path.as_deref() == self.document.path() {
            self.editor.update(cx, |state, cx| {
                state.set_cursor_position(site.position, window, cx)
            });
            cx.notify();
            return;
        }

        // Otherwise the buffer has to be swapped back — including back to the
        // unnamed fixture, which is why the payload is a snapshot rather than a
        // path to re-read. Unsaved edits survive the round trip this way too.
        self.load_document(site.path, site.source, Some(site.position), window, cx);
    }

    /// `secondary-shift-r`: open a prompt to rename the symbol under the cursor.
    fn rename_symbol(&mut self, _: &RenameSymbol, window: &mut Window, cx: &mut Context<Self>) {
        let (source, cursor) = {
            let state = self.editor.read(cx);
            (state.value().to_string(), state.cursor())
        };
        let text = Rope::from(source.as_str());
        let Some((target, _, _)) = word::word_at(&text, cursor) else {
            return;
        };

        let input = cx.new(|cx| InputState::new(window, cx).placeholder(&target));
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.apply_rename(window, cx),
                // Clicking away is a cancel, not a silent no-op: the prompt closes
                // so the buffer cannot be changed by a later keystroke.
                InputEvent::Blur => this.cancel_rename(cx),
                _ => {}
            },
        );
        input.update(cx, |state, cx| state.focus(window, cx));

        self.rename = Some(RenameBar {
            input,
            target,
            offset: cursor,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Put the renamed text in the buffer, as one undoable edit.
    fn apply_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bar) = self.rename.take() else {
            return;
        };
        let new_name = bar.input.read(cx).value().to_string();
        let source = self.editor.read(cx).value().to_string();
        let text = Rope::from(source.as_str());
        let loader = workspace::loader();

        let Some(renamed) = rename::rename_source(&text, bar.offset, &new_name, loader.as_ref())
        else {
            // Nothing renameable under the cursor, an unusable name, or a symbol
            // declared outside this file: leave the buffer alone rather than
            // writing a half-done edit, and say so.
            self.set_error(format!(
                "无法重命名为「{new_name}」：光标处不是本文件声明的符号，或该名字不可用"
            ));
            cx.notify();
            return;
        };

        self.editor
            .update(cx, |state, cx| state.replace_all(renamed.clone(), window, cx));
        self.apply(cache::analysis(&renamed), cx);
        self.set_status(format!("已把 {} 重命名为 {new_name}", bar.target));
        cx.notify();
    }

    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        self.rename = None;
        cx.notify();
    }

    /// `secondary-shift-g`: every use of the symbol under the cursor.
    fn find_references(&mut self, _: &FindReferences, _: &mut Window, cx: &mut Context<Self>) {
        let (source, cursor) = {
            let state = self.editor.read(cx);
            (state.value().to_string(), state.cursor())
        };
        let text = Rope::from(source.as_str());
        let loader = workspace::loader();

        self.references_lines = lines_of(navigation::references_report(&text, cursor, loader.as_ref()));
        self.panel = Panel::References;
        cx.notify();
    }

    /// `secondary-shift-o`: the script's own symbols, as a jump-from list.
    fn document_symbols(&mut self, _: &DocumentSymbols, _: &mut Window, cx: &mut Context<Self>) {
        let source = self.editor.read(cx).value().to_string();
        let text = Rope::from(source.as_str());
        let loader = workspace::loader();

        self.symbols_lines = lines_of(navigation::symbols_report(&text, loader.as_ref()));
        self.panel = Panel::Symbols;
        cx.notify();
    }

    /// `secondary-shift-f`: re-indent the buffer through pinecone's formatter.
    ///
    /// `replace_all` rather than `set_value`: the result stays on the undo stack,
    /// so a wrong-looking reformat is one `secondary-z` away.
    fn format_document(&mut self, _: &FormatDocument, window: &mut Window, cx: &mut Context<Self>) {
        let source = self.editor.read(cx).value().to_string();
        let formatted = match formatting::format_source(&source) {
            formatting::FormatOutcome::Formatted(formatted) => formatted,
            formatting::FormatOutcome::AlreadyFormatted => {
                self.set_status("已经是格式化状态");
                cx.notify();
                return;
            }
            formatting::FormatOutcome::Unparsable => {
                // The formatter parses first; a broken buffer keeps its text.
                self.set_error("脚本无法解析，未格式化");
                cx.notify();
                return;
            }
        };

        self.editor
            .update(cx, |state, cx| state.replace_all(formatted.clone(), window, cx));
        self.apply(cache::analysis(&formatted), cx);
        self.set_status("已格式化");
        cx.notify();
    }

    /// `secondary-s`: write the buffer back, asking for a path if it has none.
    fn save_file(&mut self, _: &SaveFile, window: &mut Window, cx: &mut Context<Self>) {
        let source = self.editor.read(cx).value().to_string();

        let Some(path) = self.document.path().map(Path::to_path_buf) else {
            // No file yet: let the user choose where it should go, then save.
            let directory = workspace::startup_dir()
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."));
            let picked = cx.prompt_for_new_path(&directory, Some("untitled.pine"));

            cx.spawn_in(window, async move |this, cx| {
                let Ok(Ok(Some(path))) = picked.await else {
                    return;
                };
                if let Err(err) = document::save(&path, &source) {
                    let message = format!("保存失败 {}：{err}", path.display());
                    this.update_in(cx, |this, _, cx| {
                        this.set_error(message);
                        cx.notify();
                    })
                    .ok();
                    return;
                }
                this.update_in(cx, |this, _, cx| {
                    workspace::set_document_roots(workspace::roots_for_file(&path));
                    this.document.attach(path.clone());
                    this.set_status(format!("已保存 {}", path.display()));
                    cx.notify();
                })
                .ok();
            })
            .detach();
            return;
        };

        match document::save(&path, &source) {
            Ok(()) => self.set_status(format!("已保存 {}", path.display())),
            Err(err) => self.set_error(format!("保存失败 {}：{err}", path.display())),
        }
        cx.notify();
    }

    /// Schedule a re-parse of the buffer.
    ///
    /// Replacing `_refresh_task` drops the previous one, so a burst of keystrokes
    /// ends in exactly one analysis instead of one per character.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let source = self.editor.read(cx).value().to_string();
        self._refresh_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REFRESH_DEBOUNCE).await;
            this.update(cx, |this, cx| this.apply(cache::analysis(&source), cx))
                .ok();
        });
    }

    /// Put one analysis on screen: all three dump tabs and the squiggles.
    fn apply(&mut self, analysis: Rc<analysis::Analysis>, cx: &mut Context<Self>) {
        self.token_count = analysis.tokens.len();
        self.lexer_lines = lines_of(analysis.lexer_dump());
        self.ast_lines = lines_of(analysis.ast_dump());
        self.diagnostics_lines = lines_of(analysis.diagnostics_dump());

        // Squiggles are derived from the same parse as the 诊断 tab, so the tab
        // and the underlines can never disagree.
        let squiggles = analysis.squiggles();
        if std::env::var_os("PINEDITOR_TRACE").is_some() {
            eprintln!(
                "[pineditor] {} completions in catalog, {} diagnostics",
                completion::catalog_size(),
                squiggles.len()
            );
        }
        self.editor.update(cx, |state, cx| {
            if let Some(set) = state.diagnostics_mut() {
                set.clear();
                set.extend(squiggles);
            }
            cx.notify();
        });

        cx.notify();
    }

    fn show(&mut self, panel: Panel, cx: &mut Context<Self>) {
        self.panel = panel;
        cx.notify();
    }

    fn dump_lines(&self) -> Rc<Vec<SharedString>> {
        match self.panel {
            Panel::Lexer => self.lexer_lines.clone(),
            Panel::Ast => self.ast_lines.clone(),
            Panel::Diagnostics => self.diagnostics_lines.clone(),
            Panel::References => self.references_lines.clone(),
            Panel::Symbols => self.symbols_lines.clone(),
        }
    }

    fn render_panel(&self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lines = self.dump_lines();
        let count = lines.len();

        v_flex()
            .id("dump-panel")
            .size_full()
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .gap_2()
                    .p_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("tab-lexer")
                            .small()
                            .label(Panel::Lexer.title())
                            .on_click(cx.listener(|this, _, _, cx| this.show(Panel::Lexer, cx))),
                    )
                    .child(
                        Button::new("tab-ast")
                            .small()
                            .label(Panel::Ast.title())
                            .on_click(cx.listener(|this, _, _, cx| this.show(Panel::Ast, cx))),
                    )
                    .child(
                        Button::new("tab-diagnostics")
                            .small()
                            .label(Panel::Diagnostics.title())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show(Panel::Diagnostics, cx)
                            })),
                    )
                    .child(
                        Button::new("tab-references")
                            .small()
                            .label(Panel::References.title())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show(Panel::References, cx)
                            })),
                    )
                    .child(
                        Button::new("tab-symbols")
                            .small()
                            .label(Panel::Symbols.title())
                            .on_click(cx.listener(|this, _, _, cx| this.show(Panel::Symbols, cx))),
                    )
                    .child(
                        Button::new("refresh")
                            .small()
                            .label("重新解析")
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .child(
                        uniform_list("dump-scroll", count, move |range, _, _| {
                            range
                                .map(|index| {
                                    let line = lines
                                        .get(index)
                                        .cloned()
                                        .unwrap_or_else(|| SharedString::from("—"));
                                    // Every row must stay exactly one line tall:
                                    // `uniform_list` measures the first row and
                                    // lays the rest out at that height, so a row
                                    // that wraps would overlap the next one. Long
                                    // lines (a lexeme column, a wide AST node) are
                                    // ellipsized instead.
                                    div().truncate().child(line)
                                })
                                .collect::<Vec<_>>()
                        })
                        // Load-bearing: `uniform_list` lays its items out itself and
                        // gives taffy no children, so without an explicit height it
                        // collapses to its padding — the panel then looks empty.
                        .size_full()
                        .debug_selector(|| "dump-list".to_string())
                        .track_scroll(&self.dump_scroll)
                        .p_2()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(px(12.)),
                    )
                    .child(Scrollbar::vertical(&self.dump_scroll)),
            )
    }
}

/// Split a dump into display lines, capped at what the panel will show.
///
/// Done once per analysis rather than once per frame: `render` runs on every
/// scroll tick, and any per-frame work here shows up as jerky scrolling.
fn lines_of(text: String) -> Rc<Vec<SharedString>> {
    Rc::new(
        text.lines()
            .take(MAX_DUMP_LINES)
            .map(SharedString::from)
            .collect(),
    )
}

impl Render for PineEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("pineditor")
            .key_context(CONTEXT)
            // The rename prompt sits above the editor so the text it is about
            // stays visible, and disappears entirely when closed.
            .child(h_flex().when_some(self.rename.as_ref(), |this, bar| {
                this.gap_2()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(format!("重命名 `{}` →", bar.target))
                    .child(div().w(px(220.)).child(Input::new(&bar.input)))
                    .child(
                        Button::new("rename-apply")
                            .small()
                            .label("应用")
                            .on_click(cx.listener(|this, _, window, cx| this.apply_rename(window, cx))),
                    )
                    .child(
                        Button::new("rename-cancel")
                            .small()
                            .label("取消")
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_rename(cx))),
                    )
            }))
            .on_action(cx.listener(Self::open_file))
            .on_action(cx.listener(Self::save_file))
            .on_action(cx.listener(Self::format_document))
            .on_action(cx.listener(Self::find_references))
            .on_action(cx.listener(Self::document_symbols))
            .on_action(cx.listener(Self::rename_symbol))
            .on_action(cx.listener(Self::jump_back))
            .size_full()
            .child(
                h_resizable("pineditor-main")
                    .child(
                        resizable_panel().size(px(720.)).child(
                            Editor::new(&self.editor)
                                .bordered(false)
                                .h(relative(1.))
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_size(cx.theme().mono_font_size)
                                .into_any_element(),
                        ),
                    )
                    .child(resizable_panel().child(self.render_panel(window, cx))),
            )
            .child(StatusBar::new().left(
                h_flex()
                    .gap_3()
                    .px_2()
                    .child(self.document.title())
                    .child("Pine Script v5")
                    .child(self.panel.title())
                    .child(format!("{} tokens", self.token_count))
                    .when_some(self.status.as_ref(), |this, status| {
                        let (text, colour) = match status {
                            Status::Info(text) => (text.clone(), cx.theme().foreground),
                            Status::Error(text) => (text.clone(), cx.theme().danger),
                        };
                        this.child(text).text_color(colour)
                    }),
            ))
    }
}

fn main() {
    // Libraries named outside the document: `--lib <dir>` / `PINEDITOR_LIB_DIR`.
    // Read before anything else so the `--dump` path resolves imports too.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let lib_dir = std::env::var("PINEDITOR_LIB_DIR").ok();

    // `hover` changes the process CWD (pine_reference resolves its specs
    // relatively), so remember where we started for the save-as dialog.
    if let Ok(dir) = std::env::current_dir() {
        workspace::set_startup_dir(dir);
    }
    workspace::set_configured_roots(workspace::parse_roots(&args, lib_dir.as_deref()));

    // The bundled fixture is `include_str!`-ed, so the buffer starts with no
    // path — but it was compiled from a real file, and the demo script's own
    // `import mylib as lib` only resolves if `import` is looked up from that
    // file's directory. Opening another file replaces these roots.
    workspace::set_document_roots(workspace::fixture_roots());

    // `pineditor --dump` prints the token stream and AST to stdout and exits, so
    // the parse can be inspected without a display (and in CI).
    if std::env::args().any(|arg| arg == "--dump") {
        let analysis = analysis::Analysis::run(DEFAULT_SOURCE);
        println!("{}", analysis.lexer_dump());
        println!("\n{}", analysis.ast_dump());
        println!("\n-- diagnostics --\n{}", analysis.diagnostics_dump());
        return;
    }

    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);

    app.run(move |cx| {
        gpui_kit::init(cx);

        // `secondary` is Cmd on macOS and Ctrl elsewhere. The editor's actions
        // are bound under its own key context so they do not collide with the
        // input's bindings.
        cx.bind_keys([
            // gpui-base defines `GoToDefinition` but never binds a key for it:
            // it is reachable from the context menu and from Ctrl+click, and the
            // click path only works once the symbol has been hovered. F12 asks
            // the provider directly, so it works immediately.
            KeyBinding::new("f12", GoToDefinition, Some("Input")),
            KeyBinding::new("secondary-o", OpenFile, Some(CONTEXT)),
            KeyBinding::new("secondary-s", SaveFile, Some(CONTEXT)),
            KeyBinding::new("secondary-shift-f", FormatDocument, Some(CONTEXT)),
            KeyBinding::new("secondary-shift-g", FindReferences, Some(CONTEXT)),
            KeyBinding::new("secondary-shift-o", DocumentSymbols, Some(CONTEXT)),
            KeyBinding::new("secondary-shift-r", RenameSymbol, Some(CONTEXT)),
            // F12 jumps; Ctrl+Shift+B comes back.
            //
            // The obvious choices are taken: `alt-left`, `ctrl-left` and
            // `ctrl-shift-left` are all bound by the text editor under the nearer
            // `Input` context (move/select by word), so they never reach this
            // one, and `ctrl-alt-left` is grabbed by most window managers for
            // workspace switching. `ctrl-shift-b` is bound by nobody.
            KeyBinding::new("ctrl-shift-b", JumpBack, Some(CONTEXT)),
        ]);

        // Pine has no tree-sitter grammar. Registering it as a plain language
        // stops the editor warning about an unknown language and falling back to
        // "text"; the color comes from semantic tokens (see `highlight.rs`).
        LanguageRegistry::singleton().register("pine", &GrammarConfig::plain("pine"));

        if std::env::var_os("PINEDITOR_TRACE").is_some() {
            use gpui_kit::component::input::HighlightStyleResolver;
            eprintln!(
                "[pineditor] theme keyword color: {:?}",
                cx.theme()
                    .highlight_theme
                    .style("keyword")
                    .and_then(|style| style.color)
            );
        }

        cx.activate(true);

        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| PineEditor::new(window, cx))
        })
        .expect("open the editor window");
    });
}

#[cfg(test)]
mod tests {
    // Note: no `use super::*` here — the glob also pulls in `gpui_kit::test`,
    // which shadows the built-in `#[test]` attribute and sends the macro into
    // unbounded recursion.

    #[test]
    fn the_bundled_fixture_imports_a_library_next_to_it() {
        let fixture =
            crate::workspace::fixture_path().expect("fixtures/example.pine should exist");
        assert!(fixture.with_file_name("mylib.pine").is_file(), "the library the fixture imports");
    }
}
