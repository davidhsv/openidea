use super::*;

/// The Jobs fixture linked with `optional`, its pinned program a script in the home's `bin` that does nothing.
#[cfg(unix)]
fn link_jobs(home: &Path, optional: &[&str], cx: &mut TestAppContext) -> String {
    link_jobs_as(home, optional, str::to_owned, cx)
}

/// [`link_jobs`] with the manifest rewritten by `edit`.
#[cfg(unix)]
fn link_jobs_as(home: &Path, optional: &[&str], edit: impl Fn(&str) -> String, cx: &mut TestAppContext) -> String {
    let bin = home.join("bin");
    program(&bin, "jobs", "#!/bin/sh\nexit 0\n");
    programs_in(&bin, cx);
    let folder = plugin_copy(home, "jobs", edit);
    link(cx, &folder, optional, "follow")
}

/// The Jobs manifest with its text question declared as a command, so other plugins can run it.
#[cfg(unix)]
fn input_declared(text: &str) -> String {
    format!("{text}\n[[contributes.commands]]\nid = \"input\"\ntitle = \"Jobs: Ask for a Name\"\n")
}

#[cfg(unix)]
fn asked(workbench: &Entity<Workbench>, cx: &mut VisualTestContext) -> Option<Entity<PluginPrompt>> {
    workbench.read_with(cx, |w, _| w.plugin_prompt().cloned())
}

/// What the command running on `running` answered, once it ends.
#[cfg(unix)]
fn answer(running: std::thread::JoinHandle<IdeResult<Option<String>>>, cx: &mut VisualTestContext) -> Option<String> {
    wait(cx, "the command ends", |_| running.is_finished());
    running.join().expect("the command ran").expect("it answers")
}

/// Whether `workbench` has registered the status bar item `id`.
fn has_item(workbench: &Entity<Workbench>, id: &str, cx: &mut VisualTestContext) -> bool {
    workbench.read_with(cx, |w, cx| w.status_bar().read(cx).model().spec(id).is_some())
}

/// The plugin's code asks a yes / no question while its command waits: it shows with its title and message; OK
/// answers yes, Cancel no, and that is what the command gets back.
#[cfg(unix)]
#[gpui_kit::test]
async fn a_plugins_yes_no_question_shows_its_message_and_the_command_gets_the_answer(cx: &mut TestAppContext) {
    let home = start(cx);
    link_jobs(home.path(), &[], cx);
    let (_folder, root, workbench, cx) = project(cx, &[("a.txt", "a\n")]);

    let running = run_in_window(cx, &root, "jobs", "confirm", None);
    wait(cx, "the question shows", |cx| asked(&workbench, cx).is_some());
    let prompt = asked(&workbench, cx).expect("a question");
    let shown = prompt.read_with(cx, |p, _| (p.title().to_owned(), p.message().map(str::to_owned)));
    assert_eq!(shown, ("Delete".to_owned(), Some("Really?".to_owned())));
    let ok = cx.debug_bounds("plugin-prompt-ok").expect("OK is drawn");
    cx.simulate_click(ok.center(), Modifiers::none());
    assert_eq!(answer(running, cx).as_deref(), Some("true"));
    assert!(asked(&workbench, cx).is_none(), "the question closed");

    let running = run_in_window(cx, &root, "jobs", "confirm", None);
    wait(cx, "the question shows again", |cx| asked(&workbench, cx).is_some());
    let cancel = cx.debug_bounds("plugin-prompt-cancel").expect("Cancel is drawn");
    cx.simulate_click(cancel.center(), Modifiers::none());
    assert_eq!(answer(running, cx).as_deref(), Some("false"));
}

// src/features/plugins/__tests__/plugins.test.tsx › plugin prompts
/// The plugin's code asks for several lines of text: the question shows a multi-line field with the text the
/// plugin suggests; Enter adds a line and Ctrl+Enter confirms, and the command gets both lines.
#[cfg(unix)]
#[gpui_kit::test]
async fn asks_multi_line_text_and_confirms_on_ctrl_enter(cx: &mut TestAppContext) {
    let home = start(cx);
    link_jobs(home.path(), &[], cx);
    let (_folder, root, workbench, cx) = project(cx, &[("a.txt", "a\n")]);
    let running = run_in_window(cx, &root, "jobs", "input-multiline", Some("one"));
    wait(cx, "the question shows", |cx| asked(&workbench, cx).is_some());
    let prompt = asked(&workbench, cx).expect("a question");
    assert!(prompt.read_with(cx, |p, _| p.is_multiline()));
    assert_eq!(prompt.read_with(cx, |p, cx| p.text(cx)).as_deref(), Some("one"));
    let area = prompt.read_with(cx, |p, _| p.area().cloned()).expect("a multi-line field");
    assert!(cx.update(|window, cx| area.focus_handle(cx).is_focused(window)), "the field has the keyboard");
    area.update(cx, |area, cx| area.set_text("", cx));
    cx.simulate_input("a");
    cx.simulate_keystrokes("enter");
    cx.simulate_input("b");
    cx.run_until_parked();
    assert!(!running.is_finished(), "Enter adds a line");
    cx.simulate_keystrokes("ctrl-enter");
    assert_eq!(answer(running, cx).as_deref(), Some("a\nb"));
}

/// The plugin's code asks for a folder: the system's folder dialog opens; the folder chosen there is what the
/// command gets, and a dismissed dialog answers none.
#[cfg(unix)]
#[gpui_kit::test]
async fn a_plugins_folder_question_opens_the_folder_dialog_and_the_command_gets_the_choice(cx: &mut TestAppContext) {
    let home = start(cx);
    link_jobs(home.path(), &[], cx);
    let (_folder, root, _workbench, cx) = project(cx, &[("a.txt", "a\n")]);
    let target = root.join("target");
    std::fs::create_dir_all(&target).expect("the folder is made");

    let running = run_in_window(cx, &root, "jobs", "pick-directory", None);
    wait(cx, "the folder dialog opens", |cx| cx.did_prompt_for_paths());
    let chosen = target.clone();
    cx.simulate_path_prompt_response(move |_| Some(vec![chosen.clone()]));
    assert_eq!(answer(running, cx), Some(target.to_string_lossy().into_owned()));

    let running = run_in_window(cx, &root, "jobs", "pick-directory", None);
    wait(cx, "the folder dialog opens again", |cx| cx.did_prompt_for_paths());
    cx.simulate_path_prompt_response(|_| None);
    assert_eq!(answer(running, cx).as_deref(), Some("none"));
}

/// A context key the plugin's code sets is on in the window's `when` clauses, and once the code removes it the
/// key is gone from the window.
#[cfg(unix)]
#[gpui_kit::test]
async fn a_context_key_the_plugin_sets_is_on_and_removing_it_takes_it_away(cx: &mut TestAppContext) {
    let home = start(cx);
    let id = link_jobs(home.path(), &[], cx);
    let (_folder, _root, _workbench, cx) = project(cx, &[("a.txt", "a\n")]);
    const KEY: &str = "plugin.jobs.flag";
    assert_eq!(context(KEY, cx), None);
    run_action(cx, &plugin_action_id(&id, "context"), Some(Value::from("flag=true")));
    wait(cx, "the key is on", |cx| context(KEY, cx) == Some(ContextValue::Bool(true)));
    run_action(cx, &plugin_action_id(&id, "context"), Some(Value::from("flag=")));
    wait(cx, "the key is gone", |cx| context(KEY, cx).is_none());
}

// src/features/plugins/__tests__/plugins.test.tsx › bridge events
/// A plugin command that runs another plugin's command through the host has it run in its window and gets that
/// command's result: the other plugin's question shows there, and the text it is answered with is what the first
/// command returns.
#[cfg(unix)]
#[gpui_kit::test]
async fn executes_host_command_requests_through_actions_and_returns_plugin_command_results(cx: &mut TestAppContext) {
    let home = start(cx);
    let jobs = link_jobs_as(home.path(), &[], input_declared, cx);
    let folder = plugin_copy(home.path(), "api14", str::to_owned);
    link(cx, &folder, &["ide.commands"], "execute");
    let (_folder, root, workbench, cx) = project(cx, &[("a.txt", "a\n")]);
    let target = plugin_action_id(&jobs, "input");
    let running = run_in_window(cx, &root, "api14", "execute", Some(target.as_str()));
    wait(cx, "the other plugin's question shows", |cx| asked(&workbench, cx).is_some());
    let prompt = asked(&workbench, cx).expect("a question");
    // The api14 fixture passes "argument" as the argument of the command it runs.
    let shown = prompt.read_with(cx, |p, cx| (p.title().to_owned(), p.text(cx)));
    assert_eq!(shown, ("Name".to_owned(), Some("argument".to_owned())));
    cx.simulate_keystrokes("enter");
    assert_eq!(answer(running, cx).as_deref(), Some("argument"), "the jobs command's result reaches api14");
}

/// A status item the plugin's code sets for one window shows in that window only: another window with a project
/// of its own does not register it. An item the code sets for every window shows in both.
#[gpui_kit::test]
async fn a_status_item_set_for_one_window_stays_out_of_another_window(cx: &mut TestAppContext) {
    let home = start(cx);
    let folder = plugin_copy(home.path(), "api14", str::to_owned);
    let id = link(cx, &folder, &[], "window-status");
    let (_folder, root, first, cx) = project(cx, &[("a.txt", "a\n")]);
    let (_other, _other_root, second, cx) = trusted_project(cx, "w2", &[("b.txt", "b\n")]);
    const ITEM: &str = "plugin.api14/state";

    // Commands of the test run for the first window (`w1`).
    let set = run_in_window(cx, &root, &id, "window-status", Some("only w1")).join();
    set.expect("the command ran").expect("the item is set");
    wait(cx, "the first window shows the item", |cx| has_item(&first, ITEM, cx));
    // Both windows hear an event in the same delivery: the second has heard this one and kept out of it.
    assert!(!has_item(&second, ITEM, cx), "the second window does not register the first one's item");
    // The second window has heard every event by now: the host's events go out in order, and it listens to them.
    let set = run_in_window(cx, &root, &id, "global-status", Some("everywhere")).join();
    set.expect("the command ran").expect("the item is set");
    wait(cx, "the second window shows the global item", |cx| has_item(&second, ITEM, cx));
    let shown = cx.update(|_, cx| {
        cx.global::<Plugins>().host.status_items().into_iter().any(|(_, _, item)| item.text == "everywhere")
    });
    assert!(shown, "the global item is listed");
    // The first window's own item stays in front of the global one there; the second sees the global one.
    let second_texts = status_texts(&second, cx);
    assert!(second_texts.iter().any(|t| t == "everywhere"), "{second_texts:?}");
    assert!(!second_texts.iter().any(|t| t == "only w1"), "the first window's item stays out: {second_texts:?}");
    cx.update(|_, cx| {
        let host = &cx.global::<Backend>().host;
        host.close_project("w2");
        host.close_project(LABEL);
    });
}
