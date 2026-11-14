use super::*;

/// What `command` of `plugin` answers for the window, run to its end.
fn ask(cx: &mut VisualTestContext, root: &Path, plugin: &str, command: &str, arg: Option<&str>) -> Option<String> {
    run_in_window(cx, root, plugin, command, arg).join().expect("the command ran").ok().flatten()
}

fn status_of(plugin: &str, cx: &mut VisualTestContext) -> Option<PluginStatus> {
    let host = cx.update(|_, cx| cx.global::<Plugins>().host.clone());
    host.list(None).plugins.into_iter().find(|p| p.id == plugin).map(|p| p.status)
}

fn has_item(workbench: &Entity<Workbench>, id: &str, cx: &mut VisualTestContext) -> bool {
    workbench.read_with(cx, |w, cx| w.status_bar().read(cx).model().spec(id).is_some())
}

/// Opens `path` in a tab of the window and waits until its editor is the focused one.
fn open(workbench: &Entity<Workbench>, path: &Path, cx: &mut VisualTestContext) {
    let at = path.to_path_buf();
    workbench.update_in(cx, |w, window, cx| w.open_project_file(at, window, cx));
    wait(cx, "the file opens", |cx| {
        workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == path))
    });
}

/// Closes the focused editor's tab and opens `path` again: the window reports the file as closed, then opened.
/// The window posts only the kinds the plugins want, which it learns after the plugins load; a test repeats
/// this until the plugin heard what it waits for.
fn reopen(workbench: &Entity<Workbench>, path: &Path, cx: &mut VisualTestContext) {
    if workbench.read_with(cx, |w, _| w.editor().is_some()) {
        run_action(cx, "editor.closeTab", None);
        wait(cx, "the tab closes", |cx| workbench.read_with(cx, |w, _| w.editor().is_none()));
    }
    open(workbench, path, cx);
}

/// A file the window opens reaches the plugin subscribed to `file-opened` for its path, which counts the events it
/// hears; once the plugin unsubscribes, opening the file again reaches it no more.
#[gpui_kit::test]
async fn a_file_the_window_opens_reaches_the_plugin_subscribed_to_file_opened(cx: &mut TestAppContext) {
    let home = start(cx);
    let folder = plugin_copy(home.path(), "path-events", str::to_owned);
    link(cx, &folder, &["events", "project.read"], "count");
    let (_folder, root, workbench, cx) = project(cx, &[("config/app.yaml", "a: 1\n")]);
    const PLUGIN: &str = "test.path-events";
    // Its first command loads the plugin, which subscribes to openings of `config/*.yaml`.
    assert_eq!(ask(cx, &root, PLUGIN, "count", None), None, "nothing heard yet");
    let file = root.join("config/app.yaml");
    wait(cx, "the plugin heard the file open", |cx| {
        let heard = ask(cx, &root, PLUGIN, "count", None).and_then(|n| n.parse::<usize>().ok()).unwrap_or(0) > 0;
        if !heard {
            reopen(&workbench, &file, cx);
        }
        heard
    });
    ask(cx, &root, PLUGIN, "off", None);
    let before = ask(cx, &root, PLUGIN, "count", None);
    reopen(&workbench, &file, cx);
    // Two of the window's posting intervals and the host's delivery pass.
    let until = std::time::Instant::now() + Duration::from_secs(2);
    wait(cx, "two seconds", |_| std::time::Instant::now() >= until);
    assert_eq!(ask(cx, &root, PLUGIN, "count", None), before, "an unsubscribed plugin hears nothing");
}

/// A file whose tab the window closes reaches the plugin activated by `file-closed`, with its path.
#[gpui_kit::test]
async fn a_file_whose_tab_closes_reaches_the_plugin_activated_by_file_closed(cx: &mut TestAppContext) {
    let home = start(cx);
    let folder = plugin_copy(home.path(), "api14", str::to_owned);
    // The fixture records the focused editor's selections with each event, which needs `editor.read`.
    link(cx, &folder, &["events", "editor.read"], "events");
    let (_folder, root, workbench, cx) = project(cx, &[("a.txt", "a\n")]);
    let file = root.join("a.txt");
    let name = file.to_string_lossy().into_owned();
    let closed = |cx: &mut VisualTestContext| {
        let listed = ask(cx, &root, "api14", "events", None);
        let events: Value = listed.and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default();
        events.as_array().is_some_and(|events| {
            events.iter().any(|e| e["kind"] == "file-closed" && e["data"].as_str().is_some_and(|d| d.contains(&name)))
        })
    };
    open(&workbench, &file, cx);
    wait(cx, "the plugin heard the tab close", |cx| {
        let heard = closed(cx);
        if !heard {
            reopen(&workbench, &file, cx);
        }
        heard
    });
}

/// A file created in the project on disk reaches the plugin that wants `files-changed`, among the batch's created
/// paths.
#[gpui_kit::test]
async fn a_file_created_on_disk_reaches_the_plugin_as_files_changed(cx: &mut TestAppContext) {
    let home = start(cx);
    let folder = plugin_copy(home.path(), "files-events", str::to_owned);
    link(cx, &folder, &["project.read"], "events");
    let (_folder, root, _workbench, cx) = project(cx, &[("a.txt", "a\n")]);
    let created = |cx: &mut VisualTestContext| {
        let listed = ask(cx, &root, "files-events", "events", None);
        let batches: Value = listed.and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default();
        batches.as_array().is_some_and(|batches| {
            batches.iter().any(|batch| {
                batch["created"]
                    .as_array()
                    .is_some_and(|paths| paths.iter().any(|p| p.as_str().is_some_and(|p| p.ends_with("/created.txt"))))
            })
        })
    };
    // The plugin is live before the file comes, so the event has someone to go to.
    ask(cx, &root, "files-events", "events", None);
    std::fs::write(root.join("created.txt"), "new\n").expect("the file is written");
    wait(cx, "the plugin heard the new file", created);
}

/// The plugin activated only by its schedule runs once its period is over, with nothing else happening: its
/// scheduled refresh shows the branch item of the open project.
#[gpui_kit::test]
async fn a_plugin_on_a_schedule_runs_when_its_period_is_over(cx: &mut TestAppContext) {
    let home = start(cx);
    let folder = plugin_copy(home.path(), "git-toolbox", |text| {
        const DECLARED: &str =
            r#"activation = ["on-schedule:300", "on-event:editor-focused", "on-event:vcs-changed", "on-command:*"]"#;
        // The shortest schedule a manifest may declare.
        let edited = text.replace(DECLARED, r#"activation = ["on-schedule:60"]"#);
        assert_ne!(edited, text, "the manifest declares its activation so");
        edited
    });
    link(cx, &folder, &[], "refresh");
    let repo = tempfile::tempdir().expect("a temporary folder");
    let root = repo.path().canonicalize().expect("the folder exists");
    init_repo(&root);
    cx.update(|cx| cx.global::<Backend>().host.trust.decide(&root, true, false)).expect("the project is trusted");
    let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new(LABEL, window, cx));
    cx.update(|window, _| window.activate_window());
    workbench.update_in(cx, |w, window, cx| w.open_project(root.clone(), window, cx));
    wait(cx, "the project opens", |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
    const ITEM: &str = "plugin.openidea.git-toolbox/branch";
    assert_eq!(status_of("openidea.git-toolbox", cx), Some(PluginStatus::Inactive), "nothing ran it");
    assert!(!has_item(&workbench, ITEM, cx));
    // The host's upkeep as it runs an hour later each time: the first look starts the period if the app's own upkeep
    // has not, and the period is over at the next. Schedules wait for the host's startup to finish, hence the loop.
    let host = cx.update(|_, cx| cx.global::<Plugins>().host.clone());
    let (now, hours) = (std::time::Instant::now(), std::cell::Cell::new(0u64));
    wait(cx, "the scheduled run shows the branch", |cx| {
        hours.set(hours.get() + 1);
        host.tick_at(now + Duration::from_secs(3600 * hours.get()));
        has_item(&workbench, ITEM, cx)
    });
    cx.update(|_, cx| cx.global::<Backend>().host.close_project(LABEL));
}

/// The plugin's code removes its status item: the window takes the item away. The branch item shows in a
/// repository; once the project is no longer one, the plugin's refresh removes it.
#[gpui_kit::test]
async fn a_status_item_the_plugin_removes_leaves_the_status_bar(cx: &mut TestAppContext) {
    let home = start(cx);
    let folder = plugin_copy(home.path(), "git-toolbox", str::to_owned);
    let id = link(cx, &folder, &[], "refresh");
    let repo = tempfile::tempdir().expect("a temporary folder");
    let root = repo.path().canonicalize().expect("the folder exists");
    init_repo(&root);
    std::fs::write(root.join("a.txt"), "a\n").expect("the file is written");
    cx.update(|cx| cx.global::<Backend>().host.trust.decide(&root, true, false)).expect("the project is trusted");
    let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new(LABEL, window, cx));
    cx.update(|window, _| window.activate_window());
    workbench.update_in(cx, |w, window, cx| w.open_project(root.clone(), window, cx));
    wait(cx, "the project opens", |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
    const ITEM: &str = "plugin.openidea.git-toolbox/branch";
    run_action(cx, &plugin_action_id(&id, "refresh"), None);
    wait(cx, "the branch item shows", |cx| has_item(&workbench, ITEM, cx));
    std::fs::remove_dir_all(root.join(".git")).expect("the repository is removed");
    run_action(cx, &plugin_action_id(&id, "refresh"), None);
    wait(cx, "the item leaves the status bar", |cx| !has_item(&workbench, ITEM, cx));
    assert!(cx.debug_bounds("status:plugin.openidea.git-toolbox/branch").is_none(), "it is not drawn");
    cx.update(|_, cx| cx.global::<Backend>().host.close_project(LABEL));
}

/// A plugin's task reports its progress: the window's task list shows the message and the fraction done while it
/// runs, and the command's result comes once it ends.
#[cfg(unix)]
#[gpui_kit::test]
async fn a_plugins_task_shows_the_progress_and_message_it_reports(cx: &mut TestAppContext) {
    let home = start(cx);
    let bin = home.path().join("bin");
    // Reads its input until the plugin closes it, as `git hash-object --stdin` does.
    program(&bin, "git", "#!/bin/sh\nexec cat >/dev/null\n");
    programs_in(&bin, cx);
    let folder = plugin_copy(home.path(), "misbehave", str::to_owned);
    link(cx, &folder, &["process:git"], "echo");
    let (_folder, root, workbench, cx) = project(cx, &[("a.txt", "a\n")]);
    let running = run_in_window(cx, &root, "misbehave", "patient", None);
    let reported = |cx: &mut VisualTestContext| {
        workbench.read_with(cx, |w, _| {
            w.tasks().tasks().iter().any(|task| {
                task.title == "waiting"
                    && task.message.as_deref() == Some("waiting")
                    && task.fraction.is_some_and(|done| done > 0.0 && done < 1.0)
            })
        })
    };
    wait(cx, "the task shows its progress", reported);
    wait(cx, "the command ends", |_| running.is_finished());
    let result = running.join().expect("the command ran").expect("it answers");
    assert_eq!(result.as_deref(), Some("still here"));
    wait(cx, "the task leaves the list", |cx| {
        workbench.read_with(cx, |w, _| !w.tasks().tasks().iter().any(|task| task.title == "waiting" && !task.done))
    });
}
