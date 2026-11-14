//! The Markdown editor's layout host, IDEA's split editor: the text editor (a slot the owner
//! fills), a live preview, or both side by side, switched by Editor Only / Editor and Preview /
//! Preview Only ([`MODE_ACTIONS`]) from the tab strip or from the host's own floating toolbar.
//!
//! The editor stays alive whatever the layout, so its document view is never rebuilt. The
//! preview is built when a layout shows it and dropped when Editor Only hides it; it is drawn
//! from the shared [`LiveSources`] the editor publishes to (debounced edits, its scrolling, caret
//! moves, its font zoom). Side by side, the preview follows the editor's scrolling; on its own,
//! it follows navigation in the hidden editor and reports the line on screen.
//!
//! The host forwards what its preview asks of the owner as [`MarkdownViewEvent`]s and records the
//! preview's own scroll offset in [`LiveSources`].

use super::markdown_view::{Highlighter, MarkdownView, MarkdownViewEvent};
use super::preview::{LiveSources, MODE_ACTIONS, MarkdownMode, MarkdownModes, PreviewImages};
use crate::platform::storage::UiStorage;
use crate::theme;
use crate::ui::tooltip::tooltip;
use gpui_kit::{
    AnyView, Context, Div, Entity, EventEmitter, IntoElement, Render, Stateful, Subscription, Window, div, prelude::*,
    px,
};

/// A layout button of the toolbar: its action, and whether its layout is the current one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutButton {
    pub id: &'static str,
    pub title: &'static str,
    pub pressed: bool,
}

/// The icon of each layout action, as [`MODE_ACTIONS`] orders them.
const MODE_ICONS: [&str; 3] = ["editor-only", "editor-preview", "preview-only"];

pub struct PreviewHost {
    path: String,
    editor: AnyView,
    modes: Entity<MarkdownModes>,
    live: Entity<LiveSources>,
    images: Entity<PreviewImages>,
    font_size: f64,
    highlighter: Option<Highlighter>,
    mode: MarkdownMode,
    preview: Option<Entity<MarkdownView>>,
    /// The layout buttons are drawn by the tab strip: the floating toolbar is not.
    actions_in_tabs: bool,
    /// [`PreviewHost::load`] ran: the layout is applied from then on.
    started: bool,
    _subscriptions: Vec<Subscription>,
    preview_subscription: Option<Subscription>,
}

impl EventEmitter<MarkdownViewEvent> for PreviewHost {}

impl PreviewHost {
    /// The host of the Markdown file `path` around its text editor `editor`. The layouts, the live
    /// sources and the images are shared by every Markdown editor; `font_size` is the preview font
    /// size ([`super::preview::markdown_preview_font_size`]).
    pub fn new(
        path: String,
        editor: AnyView,
        modes: Entity<MarkdownModes>,
        live: Entity<LiveSources>,
        images: Entity<PreviewImages>,
        font_size: f64,
        cx: &mut Context<Self>,
    ) -> Self {
        let mode = modes.read(cx).mode_of(&path);
        let subscriptions = vec![
            cx.observe(&modes, |host, _, cx| host.layout_changed(cx)),
            cx.observe(&live, |host, _, cx| host.live_changed(cx)),
        ];
        Self {
            path,
            editor,
            modes,
            live,
            images,
            font_size,
            highlighter: None,
            mode,
            preview: None,
            actions_in_tabs: false,
            started: false,
            _subscriptions: subscriptions,
            preview_subscription: None,
        }
    }

    /// Shows the file's layout: call once the owner listens.
    pub fn load(&mut self, cx: &mut Context<Self>) {
        self.started = true;
        self.mode = self.modes.read(cx).mode_of(&self.path);
        self.apply_layout(cx);
    }

    /// Runs layout action `id` ([`MODE_ACTIONS`]) for this file; `false` for another action.
    pub fn run_action(&mut self, id: &str, cx: &mut Context<Self>) -> bool {
        let path = self.path.clone();
        self.modes.update(cx, |modes, cx| {
            let ran = modes.run_action(id, Some(&path), cx.default_global::<UiStorage>());
            if ran {
                cx.notify();
            }
            ran
        })
    }

    /// The layout buttons drawn in the tab strip, so the floating toolbar is not.
    pub fn set_actions_in_tabs(&mut self, in_tabs: bool, cx: &mut Context<Self>) {
        self.actions_in_tabs = in_tabs;
        cx.notify();
    }

    /// Languages & Frameworks › Markdown's preview font size changed.
    pub fn set_font_size(&mut self, font_size: f64, cx: &mut Context<Self>) {
        self.font_size = font_size;
        if let Some(preview) = &self.preview {
            preview.update(cx, |preview, cx| preview.set_font_size(font_size, cx));
        }
    }

    /// Colours the preview's code blocks.
    pub fn set_highlighter(&mut self, highlighter: Option<Highlighter>, cx: &mut Context<Self>) {
        self.highlighter.clone_from(&highlighter);
        if let Some(preview) = &self.preview {
            preview.update(cx, |preview, cx| preview.set_highlighter(highlighter, cx));
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn mode(&self) -> MarkdownMode {
        self.mode
    }

    /// The preview, while a layout shows it.
    pub fn preview(&self) -> Option<&Entity<MarkdownView>> {
        self.preview.as_ref()
    }

    /// The layout buttons, left to right.
    pub fn toolbar(&self) -> Vec<LayoutButton> {
        MODE_ACTIONS.iter().map(|&(id, title, mode)| LayoutButton { id, title, pressed: mode == self.mode }).collect()
    }

    /// The layout buttons as the tab strip draws them.
    pub fn layout_actions(&self, cx: &mut Context<Self>) -> Div {
        let buttons: Vec<Stateful<Div>> = self
            .toolbar()
            .into_iter()
            .zip(MODE_ICONS)
            .map(|(button, icon)| {
                let id = button.id;
                let selector = match id {
                    "markdown.editorOnly" => "markdown-layout-editor-only",
                    "markdown.editorAndPreview" => "markdown-layout-split",
                    _ => "markdown-layout-preview-only",
                };
                let color = theme::color(cx, if button.pressed { "fg-strong" } else { "fg-muted" });
                div()
                    .id(selector)
                    .debug_selector(move || selector.to_owned())
                    .size(theme::length(cx, "h-floating-toolbar-button"))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme::length(cx, "radius-toolbar-button"))
                    .when(button.pressed, |el| el.bg(theme::color(cx, "bg-selected")))
                    .hover(|el| el.bg(theme::color(cx, "bg-hover")))
                    .tooltip(tooltip(button.title))
                    .on_click(cx.listener(move |host, _, _, cx| {
                        host.run_action(id, cx);
                    }))
                    .child(theme::icon(cx, icon, 16.0, color))
            })
            .collect();
        div().flex().items_center().children(buttons)
    }

    // ---------------------------------------------------------------------------------------------
    // Layout and live state

    fn layout_changed(&mut self, cx: &mut Context<Self>) {
        let mode = self.modes.read(cx).mode_of(&self.path);
        if mode != self.mode {
            self.mode = mode;
            if self.started {
                self.apply_layout(cx);
            }
            cx.notify();
        }
    }

    /// Builds, switches or drops the preview for the current layout.
    fn apply_layout(&mut self, cx: &mut Context<Self>) {
        if !self.mode.shows_preview() {
            let preview = self.preview.take();
            if let Some(preview) = &preview {
                preview.update(cx, |preview, cx| preview.close(cx));
            }
            // The preview's last `ShownLine` is still queued, and the events of a released entity are
            // dropped: the preview and its subscription go once that event is delivered.
            let subscription = self.preview_subscription.take();
            cx.defer(move |_| drop((preview, subscription)));
            cx.notify();
            return;
        }
        let sync = self.mode == MarkdownMode::Split;
        let preview = match &self.preview {
            Some(preview) => preview.clone(),
            None => {
                let (path, images, font_size) = (self.path.clone(), self.images.clone(), self.font_size);
                let highlighter = self.highlighter.clone();
                let preview = cx.new(|cx| {
                    let mut preview = MarkdownView::new(path, images, font_size, cx);
                    preview.set_highlighter(highlighter, cx);
                    preview
                });
                let subscription = cx.subscribe(&preview, |host, _, event, cx| host.forward(event, cx));
                self.preview_subscription = Some(subscription);
                self.preview = Some(preview.clone());
                preview
            }
        };
        let live = self.live.read(cx).get(&self.path).cloned();
        preview.update(cx, |preview, cx| {
            if let Some(live) = &live {
                preview.update_live(live, cx);
            }
            preview.show(sync, cx);
        });
        cx.notify();
    }

    fn live_changed(&mut self, cx: &mut Context<Self>) {
        let Some(preview) = self.preview.clone() else { return };
        let Some(live) = self.live.read(cx).get(&self.path).cloned() else { return };
        preview.update(cx, |preview, cx| preview.update_live(&live, cx));
    }

    fn forward(&mut self, event: &MarkdownViewEvent, cx: &mut Context<Self>) {
        if let MarkdownViewEvent::Scrolled(top) = event {
            // Kept without a notification: nothing redraws from it until the preview is rebuilt.
            let path = self.path.clone();
            self.live.update(cx, |live, _| live.preview_scrolled(&path, *top));
        }
        cx.emit(event.clone());
    }
}

impl Render for PreviewHost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let split = self.mode == MarkdownMode::Split;
        let toolbar = (!self.actions_in_tabs).then(|| {
            div()
                .debug_selector(|| "markdown-layout-actions".to_owned())
                .absolute()
                .top(px(6.0))
                .right(px(12.0))
                .p(px(2.0))
                .rounded(theme::length(cx, "radius-control"))
                .bg(theme::color(cx, "bg-elev"))
                .border_1()
                .border_color(theme::color(cx, "border"))
                .child(self.layout_actions(cx))
        });
        div()
            .relative()
            .size_full()
            .flex()
            .when(self.mode != MarkdownMode::Preview, |el| {
                el.child(
                    div()
                        .debug_selector(|| "markdown-editor-slot".to_owned())
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .flex()
                        .flex_col()
                        .when(split, |el| el.border_r_1().border_color(theme::color(cx, "border-editor-tabs")))
                        .child(self.editor.clone()),
                )
            })
            .children(self.preview.clone().map(|preview| {
                div().debug_selector(|| "markdown-preview-slot".to_owned()).flex_1().min_w_0().h_full().child(preview)
            }))
            .children(toolbar)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features_b::viewers::font_zoom::{FontZooms, ZoomAction, ZoomEffect};
    use crate::features_b::viewers::preview::DEFAULT_MODE_KEY;
    use crate::platform::input;
    use gpui_kit::{Modifiers, ScrollDelta, ScrollEvent, TestAppContext, VisualTestContext, point};
    use std::cell::RefCell;
    use std::rc::Rc;
    mod proofs8_preview_only;

    type Events = Rc<RefCell<Vec<MarkdownViewEvent>>>;

    const PATH: &str = "/p/README.md";

    struct Slot;

    impl Render for Slot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().debug_selector(|| "editor".to_owned())
        }
    }

    /// Eighty sections of a heading and a paragraph: section `i`'s heading is on line `4 * i`.
    fn document() -> String {
        (0..80).map(|i| format!("# Section {i}\n\nText of section {i}.\n\n")).collect()
    }

    struct World<'a> {
        host: Entity<PreviewHost>,
        live: Entity<LiveSources>,
        modes: Entity<MarkdownModes>,
        events: Events,
        cx: &'a mut VisualTestContext,
    }

    fn open(cx: &mut TestAppContext, mode: MarkdownMode) -> World<'_> {
        cx.update(|cx| {
            cx.set_global(theme::ActiveTheme::default());
            cx.set_global(UiStorage::default());
            input::init(cx);
        });
        let (modes, live, images, editor) = cx.update(|cx| {
            let mut modes = MarkdownModes::load(&UiStorage::default());
            modes.set_mode(PATH, mode, &mut UiStorage::default());
            let mut live = LiveSources::default();
            live.opened(PATH, &document(), 0.0);
            let editor: AnyView = cx.new(|_| Slot).into();
            (cx.new(|_| modes), cx.new(|_| live), cx.new(|_| PreviewImages::default()), editor)
        });
        let (shared_modes, shared_live) = (modes.clone(), live.clone());
        let (host, cx) = cx.add_window_view(|_, cx| {
            PreviewHost::new(PATH.to_owned(), editor, shared_modes, shared_live, images, 13.0, cx)
        });
        let events: Events = Rc::default();
        let seen = Rc::clone(&events);
        cx.update(|_, cx| {
            cx.subscribe(&host, move |_, event: &MarkdownViewEvent, _| seen.borrow_mut().push(event.clone())).detach();
        });
        host.update(cx, |host, cx| host.load(cx));
        cx.run_until_parked();
        World { host, live, modes, events, cx }
    }

    impl World<'_> {
        fn preview(&mut self) -> Entity<MarkdownView> {
            self.host.read_with(self.cx, |host, _| host.preview().cloned()).expect("the preview is shown")
        }

        fn scroll_top(&mut self) -> f64 {
            let preview = self.preview();
            preview.read_with(self.cx, |preview, _| preview.scroll_top())
        }

        fn shown(&mut self) -> Option<u32> {
            let preview = self.preview();
            preview.read_with(self.cx, |preview, _| preview.shown_line())
        }

        /// Where the preview shows source `line` at the top, from its layout.
        fn top_of(&mut self, line: u32) -> f64 {
            let preview = self.preview();
            preview.read_with(self.cx, |preview, _| {
                let (blocks, _) = preview.layout();
                blocks.iter().find(|block| block.line == f64::from(line)).map_or(0.0, |block| block.top)
            })
        }

        fn change_live(&mut self, change: impl FnOnce(&mut LiveSources)) {
            self.live.update(self.cx, |live, cx| {
                change(live);
                cx.notify();
            });
            self.cx.run_until_parked();
        }

        fn go_to(&mut self, line: u32) {
            self.change_live(|live| live.caret_moved(PATH, line));
        }

        fn set_mode(&mut self, mode: MarkdownMode) {
            self.modes.update(self.cx, |modes, cx| {
                modes.set_mode(PATH, mode, &mut UiStorage::default());
                cx.notify();
            });
            self.cx.run_until_parked();
        }

        /// The `ShownLine` events since event `from`.
        fn shown_events(&self, from: usize) -> Vec<Option<u32>> {
            self.events.borrow()[from..]
                .iter()
                .filter_map(|event| match event {
                    MarkdownViewEvent::ShownLine(line) => Some(*line),
                    _ => None,
                })
                .collect()
        }

        fn wheel(&mut self, dy: f32) {
            let bounds = self.cx.debug_bounds("markdown-preview").expect("the preview is drawn");
            self.cx.simulate_event(ScrollEvent {
                position: bounds.center(),
                delta: ScrollDelta::Pixels(point(px(0.0), px(dy))),
                ..Default::default()
            });
            self.cx.run_until_parked();
        }
    }

    // src/features/viewers/markdown/previewOnly.test.tsx › Markdown Preview only › scrolls to where the hidden
    // editor's caret is sent, again on the same place
    #[gpui_kit::test]
    async fn scrolls_to_where_the_hidden_editors_caret_is_sent_again_on_the_same_place(cx: &mut TestAppContext) {
        let mut world = open(cx, MarkdownMode::Preview);
        assert_eq!(world.shown(), Some(0));
        assert_eq!(world.host.read_with(world.cx, |host, _| host.mode()), MarkdownMode::Preview);
        assert!(world.cx.debug_bounds("editor").is_none());

        world.go_to(80);
        let top = world.top_of(80);
        assert!(top > 0.0);
        assert!((world.scroll_top() - top).abs() < 1.0);
        assert_eq!(world.shown(), Some(80));

        world.wheel(200.0);
        assert!(world.scroll_top() < top);
        world.go_to(80);
        assert!((world.scroll_top() - top).abs() < 1.0);
    }

    // src/features/viewers/markdown/previewOnly.test.tsx › Markdown Preview only › reports the line on screen as
    // the user scrolls, not after navigating
    #[gpui_kit::test]
    async fn reports_the_line_on_screen_as_the_user_scrolls_not_after_navigating(cx: &mut TestAppContext) {
        let mut world = open(cx, MarkdownMode::Preview);
        let before = world.events.borrow().len();
        world.go_to(120);
        assert_eq!(world.shown_events(before), [Some(120)]);

        let delta = world.scroll_top() as f32 - world.top_of(60) as f32;
        world.wheel(delta);
        assert_eq!(world.shown(), Some(60));
    }

    // src/features/viewers/markdown/previewOnly.test.tsx › Markdown Preview only › leaves the split preview to the
    // editor's scroll sync
    #[gpui_kit::test]
    async fn leaves_the_split_preview_to_the_editors_scroll_sync(cx: &mut TestAppContext) {
        let mut world = open(cx, MarkdownMode::Split);
        assert!(world.cx.debug_bounds("editor").is_some());
        world.go_to(80);
        assert_eq!(world.scroll_top(), 0.0);
        assert_eq!(world.shown(), None);

        world.change_live(|live| live.scrolled(PATH, 80.0));
        let top = world.top_of(80);
        assert!((world.scroll_top() - top).abs() < 1.0);
        world.wheel(-50.0);
        assert_eq!(world.shown(), None);
        assert!(world.shown_events(0).is_empty());
    }

    // src/features/viewers/markdown/previewOnly.test.tsx › Markdown Preview only › stops standing in for the editor
    // when the editor comes back or the preview closes
    #[gpui_kit::test]
    async fn stops_standing_in_for_the_editor_when_the_editor_comes_back_or_the_preview_closes(
        cx: &mut TestAppContext,
    ) {
        let mut world = open(cx, MarkdownMode::Preview);
        world.go_to(80);
        assert_eq!(world.shown(), Some(80));
        let before = world.events.borrow().len();
        world.set_mode(MarkdownMode::Split);
        assert_eq!(world.shown(), None);
        assert_eq!(world.shown_events(before), [None]);

        world.set_mode(MarkdownMode::Preview);
        assert!(world.shown().is_some());
        let before = world.events.borrow().len();
        world.set_mode(MarkdownMode::Editor);
        assert!(world.host.read_with(world.cx, |host, _| host.preview().is_none()));
        assert_eq!(world.shown_events(before), [None]);
    }

    // src/features/viewers/markdown/previewScroll.test.tsx › preview-only scroll position › restores the preview's
    // own scroll after a remount
    #[gpui_kit::test]
    async fn restores_the_previews_own_scroll_after_a_remount(cx: &mut TestAppContext) {
        let mut world = open(cx, MarkdownMode::Preview);
        world.wheel(-600.0);
        let top = world.scroll_top();
        assert!(top > 0.0);
        let saved = world.live.read_with(world.cx, |live, _| live.get(PATH).map(|live| live.preview_top));
        assert_eq!(saved, Some(top));

        world.set_mode(MarkdownMode::Editor);
        world.set_mode(MarkdownMode::Preview);
        assert!((world.scroll_top() - top).abs() < 1.0);
    }

    // src/features/viewers/markdown/zoom.test.tsx › editor font size actions › scales the Markdown preview with its
    // editor and resets with it
    #[gpui_kit::test]
    async fn scales_the_markdown_preview_with_its_editor_and_resets_with_it(cx: &mut TestAppContext) {
        let mut world = open(cx, MarkdownMode::Split);
        let font_size = |world: &mut World<'_>| {
            let preview = world.preview();
            preview.read_with(world.cx, |preview, _| preview.font_size())
        };
        assert_eq!(font_size(&mut world), 13.0);
        let mut zooms = FontZooms::default();
        let publish = |world: &mut World<'_>, effects: Vec<ZoomEffect>| {
            for effect in effects {
                if let ZoomEffect::Editor { zoom, .. } = effect {
                    world.change_live(|live| live.font_zoomed(PATH, zoom as f32));
                }
            }
        };
        for _ in 0..2 {
            let effects = zooms.run(ZoomAction::Increase, None, Some(1));
            publish(&mut world, effects);
        }
        assert_eq!(font_size(&mut world), 15.0);
        let effects = zooms.run(ZoomAction::Reset, None, Some(1));
        publish(&mut world, effects);
        assert_eq!(font_size(&mut world), 13.0);
    }

    #[gpui_kit::test]
    async fn switches_the_layout_from_its_toolbar_and_remembers_it(cx: &mut TestAppContext) {
        let mut world = open(cx, MarkdownMode::Split);
        let pressed = |world: &mut World<'_>| {
            let toolbar = world.host.read_with(world.cx, |host, _| host.toolbar());
            toolbar.iter().map(|button| button.pressed).collect::<Vec<_>>()
        };
        assert_eq!(pressed(&mut world), [false, true, false]);
        let bounds = world.cx.debug_bounds("markdown-layout-editor-only").expect("the toolbar is drawn");
        world.cx.simulate_click(bounds.center(), Modifiers::default());
        world.cx.run_until_parked();
        assert_eq!(pressed(&mut world), [true, false, false]);
        assert!(world.host.read_with(world.cx, |host, _| host.preview().is_none()));
        let stored = world.cx.update(|_, cx| cx.global::<UiStorage>().get(DEFAULT_MODE_KEY).map(str::to_owned));
        assert_eq!(stored.as_deref(), Some("editor"));

        assert!(world.host.update(world.cx, |host, cx| host.run_action("markdown.previewOnly", cx)));
        world.cx.run_until_parked();
        assert_eq!(pressed(&mut world), [false, false, true]);
        assert!(!world.host.update(world.cx, |host, cx| host.run_action("image.zoomIn", cx)));
    }
}
