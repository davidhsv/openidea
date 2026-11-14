//! Where a popup opens when it belongs to a place on screen: under the caret it acts on (Show
//! Usages, the bookmark mnemonic chooser, Select In) or under the button that opened it (Add
//! Service). Each kind keeps its own gap and, without that place, its own spot in the window. The
//! points returned are the popup's top left corner in window coordinates; the popup is still
//! moved to stay inside the window when it is drawn.

use crate::editor::view::Editor;
use gpui_kit::{App, Bounds, Pixels, Point, Size, point, px};

/// The gap between a floating popup and the point it is anchored at.
pub const POPUP_OFFSET: f32 = 4.0;
/// The gap a chooser leaves under the caret's line or under its button.
pub const CHOOSER_GAP: f32 = 2.0;

/// The bottom left corner of the main caret's line, in the window; `None` while that line is not
/// painted (scrolled away, folded, or the editor not drawn yet).
pub fn caret_bottom_left(editor: &Editor, cx: &App) -> Option<Point<Pixels>> {
    let top = editor.position_of(editor.caret())?;
    Some(point(top.x, top.y + editor.line_height(cx)))
}

/// `gap` pixels below `at`.
pub fn below(at: Point<Pixels>, gap: f32) -> Point<Pixels> {
    point(at.x, at.y + px(gap))
}

/// The point `left` pixels left of the window's middle, `top` pixels from its top, kept inside it.
fn from_middle(viewport: Size<Pixels>, left: f32, top: f32) -> Point<Pixels> {
    point(px((f32::from(viewport.width) / 2.0 - left).max(0.0)), px(top.max(0.0)))
}

/// Show Usages: under the caret's line; without a painted caret, near the editor's top left corner;
/// without an editor, left of the window's middle a third of the way down.
pub fn usages_popup(editor: Option<&Editor>, viewport: Size<Pixels>, cx: &App) -> Point<Pixels> {
    let at = match editor {
        Some(editor) => caret_bottom_left(editor, cx)
            .or_else(|| editor.painted_bounds().map(|bounds| point(bounds.left() + px(40.0), bounds.top() + px(40.0)))),
        None => None,
    };
    let at = at.unwrap_or_else(|| from_middle(viewport, 200.0, f32::from(viewport.height) / 3.0));
    below(at, POPUP_OFFSET)
}

/// The mnemonic chooser of Toggle Bookmark with Mnemonic: just under the caret's line; else left of
/// the window's middle a third of the way down.
pub fn mnemonic_chooser(editor: Option<&Editor>, viewport: Size<Pixels>, cx: &App) -> Point<Pixels> {
    let at = editor
        .and_then(|editor| caret_bottom_left(editor, cx))
        .map(|at| below(at, CHOOSER_GAP))
        .unwrap_or_else(|| from_middle(viewport, 160.0, f32::from(viewport.height) / 3.0));
    below(at, POPUP_OFFSET)
}

/// The Select In chooser: just under the caret's line when the file chosen for is the edited one
/// (`editor` is then its editor); else near the top of the window, left of its middle.
pub fn select_in_chooser(editor: Option<&Editor>, viewport: Size<Pixels>, cx: &App) -> Point<Pixels> {
    editor
        .and_then(|editor| caret_bottom_left(editor, cx))
        .map(|at| below(at, CHOOSER_GAP))
        .unwrap_or_else(|| from_middle(viewport, 100.0, 80.0))
}

/// A menu a button opens: under the button's left edge; at (100, 102) while the button is not drawn.
pub fn button_menu(button: Option<Bounds<Pixels>>) -> Point<Pixels> {
    let at = button.map_or_else(|| point(px(100.0), px(100.0)), |bounds| bounds.bottom_left());
    below(at, CHOOSER_GAP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::size;

    // Not from a spec: a button's menu opens under its left edge, and at a fixed spot without one.
    #[test]
    fn a_buttons_menu_opens_under_its_left_edge() {
        let button = Bounds { origin: point(px(30.0), px(50.0)), size: size(px(24.0), px(24.0)) };
        assert_eq!(button_menu(Some(button)), point(px(30.0), px(76.0)));
        assert_eq!(button_menu(None), point(px(100.0), px(102.0)));
    }

    // Not from a spec: without an editor each popup has its own spot, inside a narrow window too.
    #[gpui_kit::test]
    async fn without_an_editor_each_popup_opens_at_its_own_spot(cx: &mut gpui_kit::TestAppContext) {
        let viewport = size(px(1000.0), px(600.0));
        cx.update(|cx| {
            assert_eq!(usages_popup(None, viewport, cx), point(px(300.0), px(204.0)));
            assert_eq!(mnemonic_chooser(None, viewport, cx), point(px(340.0), px(204.0)));
            assert_eq!(select_in_chooser(None, viewport, cx), point(px(400.0), px(80.0)));
            let narrow = size(px(150.0), px(90.0));
            assert_eq!(select_in_chooser(None, narrow, cx), point(px(0.0), px(80.0)));
            assert_eq!(usages_popup(None, narrow, cx), point(px(0.0), px(34.0)));
        });
    }
}
