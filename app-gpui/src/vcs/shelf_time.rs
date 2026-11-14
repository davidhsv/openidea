//! When a shelf was made, as its row in the Shelf tab shows it after the number of files: a list
//! timestamp in the Date Formats settings ("5 min ago", "Today 09:07 AM", "Nov 3, 2026").

use crate::platform::date_format::{DateFormatOptions, date_format_options};
use crate::platform::i18n::I18n;
use crate::platform::local_time::{now_ms, utc_offset_minutes};
use crate::platform::settings;
use crate::vcs_diff::model::{file_count, format_time};
use gpui_kit::App;

/// The clock shelf rows are dated against: the time now, and the offset from UTC in force at a
/// moment. Views use the wall clock; tests a fixed one, so that "Today" does not depend on when
/// and where they run.
#[derive(Clone, Copy)]
pub struct ShelfClock {
    pub now_ms: fn() -> f64,
    pub utc_offset_minutes: fn(f64) -> i32,
}

impl Default for ShelfClock {
    fn default() -> Self {
        Self { now_ms, utc_offset_minutes }
    }
}

/// The Date Formats settings (`general.dateFormat`) the rows are written in.
pub fn date_options(cx: &App) -> DateFormatOptions {
    date_format_options(serde_json::to_value(&settings::global(cx).general.date_format).ok().as_ref())
}

/// The detail of a shelf's row: its number of files, then ` · ` and the time it was shelved. A
/// shelf without a time (0) shows the number alone.
pub fn shelf_detail(
    files: usize,
    time_ms: f64,
    clock: ShelfClock,
    options: &DateFormatOptions,
    i18n: &mut I18n,
) -> String {
    let count = file_count(files, i18n);
    let time = format_time(Some(time_ms), (clock.now_ms)(), (clock.utc_offset_minutes)(time_ms), options, i18n);
    if time.is_empty() { count } else { format!("{count} · {time}") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::input;
    use crate::theme;
    use crate::vcs::shelf_view::ShelfPanel;
    use gpui_kit::{Focusable, TestAppContext};
    use ide_vcs::{ChangeKind, ChangedFile, ShelvedChangeList};

    /// 2026-11-05 12:00 UTC.
    const NOW: f64 = 1_793_880_000_000.0;

    fn fixed_now() -> f64 {
        NOW
    }

    fn utc(_: f64) -> i32 {
        0
    }

    const FIXED: ShelfClock = ShelfClock { now_ms: fixed_now, utc_offset_minutes: utc };

    fn shelf(id: &str, time_ms: f64) -> ShelvedChangeList {
        ShelvedChangeList {
            id: id.to_owned(),
            name: id.to_owned(),
            time_ms,
            branch: None,
            repo: None,
            files: vec![ChangedFile { path: "a.txt".to_owned(), kind: ChangeKind::Modified, old_path: None }],
        }
    }

    // Not from a spec: the time follows the number of files, as a list timestamp; no time, no suffix.
    #[test]
    fn a_shelf_row_shows_its_files_then_when_it_was_shelved() {
        let mut i18n = I18n::default();
        let options = DateFormatOptions::default();
        let detail = |files, ms, i18n: &mut I18n| shelf_detail(files, ms, FIXED, &options, i18n);
        assert_eq!(detail(2, 1_793_879_700_000.0, &mut i18n), "2 files · 5 min ago");
        assert_eq!(detail(1, 1_793_869_620_000.0, &mut i18n), "1 file · Today 09:07 AM");
        assert_eq!(detail(1, 1_793_830_200_000.0, &mut i18n), "1 file · Yesterday 10:10 PM");
        assert_eq!(detail(3, 1_793_719_800_000.0, &mut i18n), "3 files · Nov 3, 2026");
        assert_eq!(detail(2, 0.0, &mut i18n), "2 files");
        assert_eq!(detail(2, f64::NAN, &mut i18n), "2 files");
        let plain = DateFormatOptions { pretty: false, override_system: true, ..DateFormatOptions::default() };
        assert_eq!(shelf_detail(1, 1_793_869_620_000.0, FIXED, &plain, &mut i18n), "1 file · 05.11.2026 09:07");
    }

    // Not from a spec: the Shelf tab dates each shelf against its clock.
    #[gpui_kit::test]
    async fn the_shelf_tab_shows_each_shelfs_time_after_its_files(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_global(theme::ActiveTheme::default());
            input::init(cx);
        });
        let (panel, cx) = cx.add_window_view(|_, cx| ShelfPanel::new(cx));
        cx.update(|window, cx| {
            window.activate_window();
            let focus = panel.focus_handle(cx);
            window.focus(&focus, cx);
        });
        panel.update(cx, |panel, cx| {
            panel.set_clock(FIXED, cx);
            panel.set_shelves(vec![shelf("Work", 1_793_879_700_000.0), shelf("Old", 0.0)], true, cx);
        });
        cx.run_until_parked();
        assert_eq!(panel.update(cx, |panel, cx| panel.lines(cx)), ["Work 1 file · 5 min ago", "Old 1 file"]);
        // The row is drawn with the same text.
        assert!(cx.debug_bounds("shelf-row-Work").is_some(), "the shelf's row is drawn");
    }
}
