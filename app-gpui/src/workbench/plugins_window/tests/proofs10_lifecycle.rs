use super::*;
use ide_plugin::Invocation;

/// What `command` of `plugin` answers when run for the window `label` in the project at `root`.
fn ask(host: &Arc<PluginHost>, label: &str, root: &Path, plugin: &str, command: &str) -> Option<String> {
    let invocation = Invocation {
        command_chain: Vec::new(),
        client: Some(label.to_owned()),
        workspace_root: Some(root.to_path_buf()),
        editor: None,
        selected_paths: Vec::new(),
        global_settings: false,
    };
    let (host, plugin, command) = (Arc::clone(host), plugin.to_owned(), command.to_owned());
    let running = std::thread::spawn(move || host.run_command(&plugin, &command, None, invocation));
    running.join().expect("the command ran").ok().flatten()
}

/// Turning a running plugin off in Settings › Plugins stops its code, which runs its `deactivate` first: once the
/// plugin is on again, it reports what its `deactivate` stored.
#[gpui_kit::test]
async fn turning_a_running_plugin_off_runs_its_deactivate(cx: &mut TestAppContext) {
    let home = start_with_plugins(cx);
    let folder = plugin_copy(home.path(), "api14", str::to_owned);
    let id = link(cx, &folder, &[], "events");
    let (_folder, root, workbench, cx) = trusted_project(cx, "w1", &[("a.txt", "a\n")]);
    let host = cx.update(|_, cx| cx.global::<Plugins>().host.clone());
    let status = |host: &PluginHost| installed(host, &id).map(|plugin| plugin.status);
    run_action(cx, &plugin_action_id(&id, "events"), None);
    wait_long(cx, "the plugin's code runs", |_| status(&host) == Some(PluginStatus::Active));
    assert_eq!(ask(&host, "w1", &root, &id, "deactivated"), None, "not deactivated while it runs");

    run_action(cx, "plugins.settings", None);
    let page = workbench.read_with(cx, |w, _| w.settings_plugins_page().cloned()).expect("the Plugins page");
    wait_long(cx, "the plugin is listed", |cx| {
        let rows = page.update(cx, |page, cx| page.rows(cx));
        rows.into_iter().any(|row| matches!(row, ListRow::Item(item) if item.id == id))
    });
    let check: &'static str = Box::leak(format!("plugins-check-{id}").into_boxed_str());
    click_on(cx, check);
    wait_long(cx, "the plugin is off", |_| installed(&host, &id).is_some_and(|plugin| !plugin.enabled));

    host.set_enabled(&id, true).expect("the plugin turns on");
    wait_long(cx, "the plugin is on", |_| installed(&host, &id).is_some_and(|plugin| plugin.enabled));
    assert_eq!(ask(&host, "w1", &root, &id, "deactivated").as_deref(), Some("yes"));
    cx.update(|_, cx| cx.global::<Backend>().host.close_project("w1"));
}
