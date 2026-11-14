use super::*;
use crate::platform::settings;
use ide_workspace::settings::SettingsScope;

/// A row of the Model Profiles fixture's `profiles` table, every column given as its schema asks.
fn profile(name: &str) -> Value {
    json!({
        "name": name,
        "provider": "local",
        "baseUrl": "http://localhost:11434",
        "model": "example-model",
        "enabled": true,
        "temperature": 0.7,
        "limit": 2048,
    })
}

// src/features/plugins/__tests__/plugins.test.tsx › bridge events
/// The plugin's entry in the Project view's menu of a file runs its command with that file as the selection: the
/// command's notification names the file.
#[cfg(unix)]
#[gpui_kit::test]
async fn forwards_a_menus_path_and_selection_to_the_command(cx: &mut TestAppContext) {
    use crate::project_view::context_menu::ProjectCommand;
    let home = start_with_plugins(cx);
    let bin = home.path().join("bin");
    program(&bin, "jobs", "#!/bin/sh\nexit 0\n");
    programs_in(&bin, cx);
    let folder = plugin_copy(home.path(), "jobs", |text| {
        let text = text.replace("engines.openidea = \"^1.0\"", "engines.openidea = \"^1.3\"");
        format!("{text}\n[[contributes.menus]]\nmenu = \"projectView.context\"\ncommand = \"selection\"\n")
    });
    link(cx, &folder, &[], "selection");
    let (_folder, root, workbench, cx) = trusted_project(cx, LABEL, &[("notes.txt", "one\n")]);
    let file = root.join("notes.txt");
    let at = file.to_string_lossy().into_owned();
    let tree = workbench.read_with(cx, |w, _| w.project_tree().cloned()).expect("the tree");
    wait_long(cx, "the tree lists the file", |cx| tree.update(cx, |tree, cx| tree.reveal_path(&at, cx)));
    cx.update(|window, cx| window.focus(&tree.focus_handle(cx), cx));
    cx.run_until_parked();
    let selector: &'static str = Box::leak(format!("project-row:{at}").into_boxed_str());
    let row = cx.debug_bounds(selector).expect("the row is drawn");
    cx.simulate_mouse_down(row.center(), MouseButton::Right, Modifiers::default());
    cx.simulate_mouse_up(row.center(), MouseButton::Right, Modifiers::default());
    cx.run_until_parked();
    let menu = workbench.read_with(cx, |w, _| w.open_menu::<ProjectCommand>()).expect("the context menu is open");
    let lines = menu.read_with(cx, |menu, _| menu.lines()[0].clone());
    let entry = "Jobs: Show Selected Paths";
    let index = lines.iter().position(|line| line.trim_start_matches("> ") == entry).expect("the plugin's entry");
    click(cx, &format!("menu-row-0-{index}"));
    wait_long(cx, "the plugin names the selection", |cx| {
        notice_titled(&workbench, "Selected paths", cx).is_some_and(|notice| notice.body.as_deref() == Some(&*at))
    });
    cx.update(|_, cx| cx.global::<Backend>().host.close_project(LABEL));
}

// src/features/plugins/__tests__/settings.test.tsx
/// A plugin command run in a project's window reads the project's value of its setting over the global one: with
/// one profile saved globally and two for the project, the command reads two.
#[gpui_kit::test]
async fn a_plugin_command_reads_the_projects_setting_over_the_global_one(cx: &mut TestAppContext) {
    let home = start_with_plugins(cx);
    let folder = plugin_copy(home.path(), "settings-pages", str::to_owned);
    link(cx, &folder, &[], "test");
    let (_folder, _root, workbench, cx) = trusted_project(cx, LABEL, &[("a.txt", "a\n")]);
    const KEY: &str = "plugins.settings-pages.profiles";
    let global = json!([profile("everywhere")]);
    let own = json!([profile("one"), profile("two")]);
    cx.update(|_, cx| settings::set(LABEL, SettingsScope::Global, KEY, &global, cx)).expect("the global value");
    cx.update(|_, cx| settings::set(LABEL, SettingsScope::Project, KEY, &own, cx)).expect("the project's value");
    run_action(cx, "plugin.settings-pages.test", None);
    wait_long(cx, "the plugin read the rows", |cx| notice_titled(&workbench, "Model Profiles", cx).is_some());
    let notice = notice_titled(&workbench, "Model Profiles", cx).expect("the plugin's notification");
    assert_eq!(notice.body.as_deref(), Some("Read 2 saved profile(s)"));
    cx.update(|_, cx| cx.global::<Backend>().host.close_project(LABEL));
}
