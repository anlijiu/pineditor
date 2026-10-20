//! Headless layout checks.
//!
//! `uniform_list` lays its rows out itself and hands taffy no children, so in a
//! flex column it has no intrinsic height: without an explicit `size_full()` it
//! collapses to its own padding and the panel looks empty. That regression is
//! invisible without a display, which is why this test renders a real frame and
//! measures the element.

use gpui::{VisualTestContext, px};

const DUMP_LIST: &str = "dump-list";

#[gpui::test]
fn the_dump_panel_fills_the_height_it_is_given(cx: &mut gpui::TestAppContext) {
    // The same two calls `main` makes before opening a window.
    cx.update(|cx| {
        gpui_kit::init(cx);
        gpui_kit::component::highlighter::LanguageRegistry::singleton()
            .register("pine", &gpui_kit::component::highlighter::GrammarConfig::plain("pine"));
    });

    let window = cx.add_window(crate::PineEditor::new);
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    cx.run_until_parked();

    let list = cx
        .debug_bounds(DUMP_LIST)
        .expect("the dump list should be laid out");

    assert!(
        list.size.height > px(200.),
        "the virtual list collapsed instead of filling its container: {list:?}"
    );
    assert!(
        list.size.width > px(200.),
        "the virtual list has no width: {list:?}"
    );
}
