use super::*;

/// A new driver of window `label` (a reload of the window) over a fresh store, once it has listed
/// the configurations of the project at `root`.
async fn reloaded(cx: &mut TestAppContext, label: &str, root: &str) -> Entity<RunDriver> {
    let driver = cx.update(|cx| {
        let store = cx.new(|_| RunStore::new());
        cx.new(|_| RunDriver::new(label, store))
    });
    let (sender, done) = oneshot::channel();
    driver.update(cx, |driver, cx| {
        driver.load(root, cx);
        driver.when_loaded(
            move |_, _| {
                let _ = sender.send(());
            },
            cx,
        );
    });
    done.await.expect("the configurations are listed");
    driver
}

fn pinned(driver: &Entity<RunDriver>, cx: &mut TestAppContext) -> Vec<String> {
    driver.read_with(cx, |driver, cx| driver.store.read(cx).pinned.clone())
}

fn recent(driver: &Entity<RunDriver>, cx: &mut TestAppContext) -> Vec<String> {
    driver.read_with(cx, |driver, cx| driver.store.read(cx).recent.clone())
}

/// Pins are kept in pin order for the project and read back by the next window on it; an unpin is
/// kept too; another project has pins of its own (none).
#[gpui_kit::test]
async fn keeps_pins_per_project_across_reloads(cx: &mut TestAppContext) {
    let configs = [program("build", "/bin/echo", "build"), program("serve", "/bin/echo", "serve")];
    let (driver, _, folders) = project(cx, "w1", &configs).await;
    listed(&driver, &folders, cx).await;
    driver.update(cx, |driver, cx| {
        driver.toggle_pinned("serve", cx);
        driver.toggle_pinned("build", cx);
    });
    let root = folders.root_text();
    let again = reloaded(cx, "w1", &root).await;
    assert_eq!(pinned(&again, cx), ["serve", "build"]);
    again.update(cx, |driver, cx| driver.toggle_pinned("serve", cx));
    assert_eq!(pinned(&again, cx), ["build"]);
    let once_more = reloaded(cx, "w1", &root).await;
    assert_eq!(pinned(&once_more, cx), ["build"]);

    let other = tempfile::tempdir().expect("another project");
    let other_root = other.path().canonicalize().expect("the folder exists");
    ide_run::save_user_configs(&other_root, &configs).expect("the configurations are saved");
    cx.update(|cx| cx.global::<Backend>().host.trust.decide(&other_root, true, false)).expect("trusted");
    let open_root = other_root.clone();
    let opening = cx.update(|cx| {
        cx.global::<Backend>().with_host(move |host| async move {
            ide_host::ops::project::open(&host, "w2", &open_root).await.map(|_| ())
        })
    });
    opening.await.expect("the task completes").expect("the project opens");
    let elsewhere = reloaded(cx, "w2", &other_root.to_string_lossy()).await;
    assert!(pinned(&elsewhere, cx).is_empty(), "{:?}", pinned(&elsewhere, cx));
}

/// Running a configuration puts it first among the recent ones, which the next window on the
/// project reads back; a run of another one goes before it.
#[gpui_kit::test]
async fn marks_a_configuration_recent_when_a_run_session_starts(cx: &mut TestAppContext) {
    let configs = [program("hello", "/bin/echo", "hello"), program("bye", "/bin/echo", "bye")];
    let (driver, _, folders) = project(cx, "w1", &configs).await;
    listed(&driver, &folders, cx).await;
    assert!(recent(&driver, cx).is_empty());
    for id in ["hello", "bye"] {
        driver.update(cx, |driver, cx| driver.select_config(Some(id), cx));
        let launching = driver.update(cx, |driver, cx| driver.launch_selected(cx));
        launching.await.expect("a session started");
    }
    assert_eq!(recent(&driver, cx), ["bye", "hello"]);
    assert!(stored(cx, RECENT_KEY, &folders).is_some_and(|stored| stored.contains("bye")));
    let again = reloaded(cx, "w1", &folders.root_text()).await;
    assert_eq!(recent(&again, cx), ["bye", "hello"]);
}
