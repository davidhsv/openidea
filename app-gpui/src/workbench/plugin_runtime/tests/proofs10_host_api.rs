use super::*;

/// What `command` of api14 answers for the window, run to its end.
fn api14(cx: &mut VisualTestContext, root: &Path, command: &str, arg: Option<&str>) -> IdeResult<Option<String>> {
    let running = run_in_window(cx, root, "api14", command, arg);
    wait(cx, "the command ends", |_| running.is_finished());
    running.join().expect("the command ran")
}

fn open(workbench: &Entity<Workbench>, path: &Path, cx: &mut VisualTestContext) -> Entity<crate::editor::view::Editor> {
    let at = path.to_path_buf();
    workbench.update_in(cx, |w, window, cx| w.open_project_file(at, window, cx));
    wait(cx, "the file opens", |cx| {
        workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == path))
    });
    workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor")
}

/// The plugin's code writes to its output channel and reveals it: the window opens the channel's tab, titled after
/// it, showing what was written.
#[gpui_kit::test]
async fn a_plugins_output_channel_is_revealed_in_a_tab_with_what_it_wrote(cx: &mut TestAppContext) {
    let home = start(cx);
    let folder = plugin_copy(home.path(), "api14", str::to_owned);
    let id = link(cx, &folder, &[], "output");
    let (_folder, _root, workbench, cx) = project(cx, &[("a.txt", "a\n")]);
    run_action(cx, &plugin_action_id(&id, "output"), Some(Value::from("hello from the plugin")));
    wait(cx, "the channel's tab opens", |cx| {
        workbench.read_with(cx, |w, _| w.tab_titles().iter().any(|tab| tab.name == "Output: test" && tab.active))
    });
    let tab = workbench.read_with(cx, |w, _| w.plugin_log_tabs().into_iter().next().map(|(_, view)| view));
    let view = tab.expect("the output tab");
    wait(cx, "the tab shows the text", |cx| view.read_with(cx, |v, _| v.text().contains("hello from the plugin")));
}

/// The plugin's code stores a declared secret and deletes it: the host keeps it in the file store of the test's home
/// and says it is set, then not.
#[gpui_kit::test]
async fn a_plugins_secret_is_stored_and_deleted_through_the_host(cx: &mut TestAppContext) {
    let home = start(cx);
    let folder = plugin_copy(home.path(), "api14", str::to_owned);
    let id = link(cx, &folder, &["secrets"], "secret");
    let (_folder, root, _workbench, cx) = project(cx, &[("a.txt", "a\n")]);
    let host = cx.update(|_, cx| cx.global::<Plugins>().host.clone());
    assert_eq!(host.secrets(&id).expect("the secrets' state"), [("token".to_owned(), false)]);
    api14(cx, &root, "secret", Some("s3cr3t")).expect("the secret is stored");
    assert_eq!(host.secrets(&id).expect("the secrets' state"), [("token".to_owned(), true)]);
    let file = home.path().join("data/plugins/.data").join(&id).join("secrets.json");
    assert!(file.is_file(), "kept in a file under the home: {}", file.display());
    api14(cx, &root, "delete-secret", None).expect("the secret is deleted");
    assert_eq!(host.secrets(&id).expect("the secrets' state"), [("token".to_owned(), false)]);
    assert!(api14(cx, &root, "undeclared-secret", Some("x")).is_err(), "an undeclared secret is refused");
}

/// The plugin's code runs the program it was granted three ways (run, run a command, start and wait): each answers
/// with the program's output, which comes from the script in the test's `bin`.
#[cfg(unix)]
#[gpui_kit::test]
async fn a_plugin_runs_its_granted_program_and_reads_its_output(cx: &mut TestAppContext) {
    let home = start(cx);
    let bin = home.path().join("bin");
    program(&bin, "api14-tool", "#!/bin/sh\necho api14-tool ran\n");
    programs_in(&bin, cx);
    let folder = plugin_copy(home.path(), "api14", |text| {
        const LISTED: &str = r#"optional = ["ide.commands","#;
        let edited = text.replacen(LISTED, r#"optional = ["process:api14-tool", "ide.commands","#, 1);
        assert_ne!(edited, text, "the manifest lists its optional permissions so");
        edited
    });
    link(cx, &folder, &["process:api14-tool"], "process-run");
    let (_folder, root, _workbench, cx) = project(cx, &[("a.txt", "a\n")]);
    for command in ["process-run", "process-run-command", "process-start"] {
        let output = api14(cx, &root, command, None).unwrap_or_else(|error| panic!("{command}: {error:?}"));
        assert_eq!(output.as_deref(), Some("api14-tool ran\n"), "{command}");
    }
}

/// The plugin's code lists the window's open editors: the list names the file open in the window.
#[gpui_kit::test]
async fn a_plugin_lists_the_windows_open_editors(cx: &mut TestAppContext) {
    let home = start(cx);
    let folder = plugin_copy(home.path(), "api14", str::to_owned);
    link(cx, &folder, &["editor.read"], "open-editors");
    let (_folder, root, workbench, cx) = project(cx, &[("a.txt", "one\n"), ("b.txt", "b\n")]);
    open(&workbench, &root.join("a.txt"), cx);
    let listed = |cx: &mut VisualTestContext| api14(cx, &root, "open-editors", None).ok().flatten().unwrap_or_default();
    wait(cx, "the list names the open file", |cx| listed(cx).contains("a.txt"));
    assert!(!listed(cx).contains("b.txt"), "a file not open is not listed");
}

/// The plugin's code opens a project file and selects in the focused editor: the file opens in the window, and the
/// caret goes where the plugin put it.
#[gpui_kit::test]
async fn a_plugin_opens_a_file_and_selects_in_the_focused_editor(cx: &mut TestAppContext) {
    let home = start(cx);
    let folder = plugin_copy(home.path(), "api14", str::to_owned);
    let id = link(cx, &folder, &["project.read"], "open-editor");
    let (_folder, root, workbench, cx) = project(cx, &[("a.txt", "one\ntwo\n"), ("b.txt", "b\n")]);
    api14(cx, &root, "open-editor", Some("b.txt")).expect("it opens the file");
    wait(cx, "the file opens", |cx| {
        workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == root.join("b.txt")))
    });

    let editor = open(&workbench, &root.join("a.txt"), cx);
    editor.update(cx, |editor, cx| editor.set_selections(vec![(5, 5)], 0, cx));
    assert_eq!(editor.read_with(cx, |e, _| e.caret_position()), (1, 1));
    // Run from the registry, the command gets the focused editor; the fixture selects the document's start.
    run_action(cx, &plugin_action_id(&id, "set-selections"), None);
    wait(cx, "the plugin's selection", |cx| editor.read_with(cx, |e, _| e.caret_position()) == (0, 0));
}

/// The plugin's code searches the project's text: the answer names the file holding the text.
#[gpui_kit::test]
async fn a_plugin_searches_the_projects_text(cx: &mut TestAppContext) {
    let home = start(cx);
    let folder = plugin_copy(home.path(), "api14", str::to_owned);
    link(cx, &folder, &["project.read"], "search-text");
    let (_folder, root, _workbench, cx) =
        project(cx, &[("src/haystack.txt", "there is a needle here\n"), ("other.txt", "nothing\n")]);
    let found = api14(cx, &root, "search-text", Some("needle")).expect("it searches").unwrap_or_default();
    assert!(found.contains("haystack.txt"), "{found}");
    assert!(!found.contains("other.txt"), "{found}");
}

/// The plugin's code reads the project's history: the log names the commit made in it, the blame of the file its
/// author, and the diff of the file the line changed since.
#[gpui_kit::test]
async fn a_plugin_reads_the_projects_log_blame_and_diff(cx: &mut TestAppContext) {
    let home = start(cx);
    let folder = plugin_copy(home.path(), "api14", str::to_owned);
    link(cx, &folder, &["project.read", "vcs.read"], "vcs-log");
    let repo = tempfile::tempdir().expect("a temporary folder");
    let root = repo.path().canonicalize().expect("the folder exists");
    init_repo(&root);
    std::fs::write(root.join("a.txt"), "a\n").expect("the file is written");
    git_in(&root, &["add", "a.txt"]);
    git_in(&root, &["commit", "-q", "-m", "Add the first file"]);
    cx.update(|cx| cx.global::<Backend>().host.trust.decide(&root, true, false)).expect("the project is trusted");
    let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new(LABEL, window, cx));
    cx.update(|window, _| window.activate_window());
    workbench.update_in(cx, |w, window, cx| w.open_project(root.clone(), window, cx));
    wait(cx, "the project opens", |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
    let log = api14(cx, &root, "vcs-log", None).expect("it reads the log").unwrap_or_default();
    assert!(log.contains("Add the first file"), "{log}");
    let blame = api14(cx, &root, "vcs-blame", Some("a.txt")).expect("it reads the blame").unwrap_or_default();
    assert!(blame.contains("Ada"), "{blame}");
    std::fs::write(root.join("a.txt"), "changed\n").expect("the file is changed");
    let diff = api14(cx, &root, "vcs-diff", Some("a.txt")).expect("it reads the diff").unwrap_or_default();
    assert!(diff.contains("+changed"), "{diff}");
    cx.update(|_, cx| cx.global::<Backend>().host.close_project(LABEL));
}

/// The plugin's code reads the UI language: with Chinese chosen in the settings, `locale()` answers that language's
/// tag.
#[gpui_kit::test]
async fn a_plugin_reads_the_ui_language_the_settings_choose(cx: &mut TestAppContext) {
    use ide_workspace::settings::SettingsScope;
    let home = start(cx);
    let folder = plugin_copy(home.path(), "api14", str::to_owned);
    link(cx, &folder, &[], "events");
    let (_folder, root, _workbench, cx) = project(cx, &[("a.txt", "a\n")]);
    cx.update(|_, cx| {
        crate::platform::settings::set(LABEL, SettingsScope::Global, "appearance.locale", &Value::from("zh"), cx)
    })
    .expect("the language is saved");
    wait(cx, "the plugin reads the chosen language", |cx| {
        api14(cx, &root, "locale", None).ok().flatten().as_deref() == Some("zh-CN")
    });
}
