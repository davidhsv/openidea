//! Where the failures of a window go: the log, under the target `frontend`, so that the log
//! attached to a report shows what failed in the UI. A failure nothing handled (a task whose error
//! no one waits for) is written once per window with the window's label, at most
//! [`MAX_REPORTS`](super::misc::error_log::MAX_REPORTS) times per window; a caught error worth
//! keeping is written with what was being done.

use super::misc::error_log::{ErrorReporter, Reason, UNCAUGHT_ERROR, UNHANDLED_REJECTION, error_message};
use gpui_kit::{App, Global};
use std::collections::HashMap;

/// The log target the lines are written under.
pub const TARGET: &str = "frontend";

/// One reporter per window label.
#[derive(Default)]
pub struct ErrorLog {
    windows: HashMap<String, ErrorReporter>,
}

impl Global for ErrorLog {}

/// Writes the failure of `kind` of the window `label` to the log, unless it repeats the window's
/// last one or the window has written its share. Answers whether a line was written.
pub fn report(label: &str, kind: &str, reason: Reason<'_>, cx: &mut App) -> bool {
    let log = cx.default_global::<ErrorLog>();
    let reporter = log.windows.entry(label.to_owned()).or_insert_with(|| ErrorReporter::new(Some(label)));
    let Some(line) = reporter.report(kind, reason) else { return false };
    tracing::error!(target: TARGET, "{line}");
    true
}

/// An error nothing handled: a task of the window `label` failed and no one waited for it.
pub fn report_unhandled(label: &str, error: &anyhow::Error, cx: &mut App) -> bool {
    let text = format!("{error:#}");
    report(label, UNHANDLED_REJECTION, Reason::Text(&text), cx)
}

/// A failure that escaped the window's own handling (a panic message caught at its edge).
pub fn report_uncaught(label: &str, message: &str, cx: &mut App) -> bool {
    report(label, UNCAUGHT_ERROR, Reason::Text(message), cx)
}

/// The window `label` closed: a window opened later under the same label starts afresh.
pub fn forget_window(label: &str, cx: &mut App) {
    if cx.has_global::<ErrorLog>() {
        cx.global_mut::<ErrorLog>().windows.remove(label);
    }
}

/// A caught error worth keeping: written with `context`, what was being done.
pub fn log_error(context: &str, reason: Reason<'_>) {
    tracing::error!(target: TARGET, "{}", error_message(context, reason));
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::TestAppContext;
    use std::sync::Arc;

    /// What a tracing subscriber writes, kept to be read back.
    #[derive(Clone, Default)]
    struct Captured(Arc<parking_lot::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl Captured {
        fn lines(&self) -> Vec<String> {
            String::from_utf8_lossy(&self.0.lock()).lines().map(str::to_owned).collect()
        }
    }

    // src/platform/errorLog.test.ts › frontend errors in the log › writes uncaught errors and unhandled rejections once each, with the window
    #[gpui_kit::test]
    async fn writes_uncaught_errors_and_unhandled_rejections_once_each_with_the_window(cx: &mut TestAppContext) {
        let log = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .without_time()
            .finish();
        let _logging = tracing::subscriber::set_default(subscriber);
        cx.update(|cx| {
            assert!(report_uncaught("project-2", "boom", cx));
            assert!(!report_uncaught("project-2", "boom", cx), "a repeat is not written");
            assert!(report_unhandled("project-2", &anyhow::anyhow!("no project"), cx));
            // Another window has its own reporter.
            assert!(report_uncaught("main", "boom", cx));
            forget_window("project-2", cx);
            assert!(report_uncaught("project-2", "boom", cx), "a window opened again starts afresh");
        });
        let lines = log.lines();
        let written: Vec<&str> =
            lines.iter().filter_map(|line| line.split_once("frontend: ").map(|(_, l)| l)).collect();
        assert_eq!(
            written,
            [
                "[project-2] uncaught error: boom",
                "[project-2] unhandled rejection: no project",
                "[main] uncaught error: boom",
                "[project-2] uncaught error: boom",
            ]
        );
        assert!(lines.iter().all(|line| line.contains("ERROR")), "{lines:?}");
    }

    // Not from a spec: a window stops writing after its share of failures.
    #[gpui_kit::test]
    async fn a_window_writes_at_most_its_share_of_failures(cx: &mut TestAppContext) {
        use crate::platform::misc::error_log::MAX_REPORTS;
        cx.update(|cx| {
            let written =
                (0..MAX_REPORTS + 10).filter(|n| report_uncaught("main", &format!("failure {n}"), cx)).count();
            assert_eq!(written, MAX_REPORTS);
        });
    }
}
