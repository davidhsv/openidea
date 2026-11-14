use super::*;

fn ids(store: &RunStore) -> Vec<String> {
    store.configs().iter().map(|config| config.id.clone()).collect()
}

/// A temporary configuration a run from `path` made, as the gutter makes it.
fn context(path: &str, kind: &str) -> RunConfig {
    let id = format!("context:file:{path}");
    RunConfig { detected: false, ..blank_config(&id, path.rsplit('/').next().unwrap_or(path), kind) }
}

/// Deleting a detected configuration of any kind from the chooser saves no user configuration and
/// suppresses the detection by its id; the selection it had goes.
#[test]
fn chooser_deletion_suppresses_detected_configurations() {
    for kind in ["spring-boot", "application", "maven", "gradle", "npm", "pytest", "cargo", "go"] {
        let id = format!("{kind}:app");
        let mut store = RunStore::new();
        store.project_root = Some("/p".into());
        store.set_configs(vec![cfg(&id, kind)]);
        store.select_config(Some(&id));
        assert!(store.can_delete_config(&id), "{kind}");
        let (persist, saved) = store.delete_config(&id);
        assert_eq!(saved, Some((Vec::new(), vec![id.clone()])), "{kind}");
        assert_eq!(store.selected_id, None, "{kind}");
        assert!(persist.writes.iter().any(|write| write.key == "openidea.run.selected:/p" && write.value.is_none()));
    }
}

/// Deleting a temporary configuration forgets it at once: nothing is saved or suppressed, and the
/// stored temporary list no longer has it.
#[test]
fn temporary_deletion_does_not_persist_suppression() {
    let mut store = RunStore::new();
    store.project_root = Some("/p".into());
    let (temporary, _) = store.adopt_temporary(RunConfig { detected: false, ..cfg("npm:app", "npm") }, &[]);
    assert_eq!(store.temporary, std::slice::from_ref(&temporary.id));
    let (persist, saved) = store.delete_config("npm:app");
    assert_eq!(saved, None, "nothing is saved and no detection is suppressed");
    assert!(ids(&store).is_empty());
    assert!(store.temporary.is_empty());
    let written = persist.writes.iter().find(|write| write.key == format!("{TEMPORARY_KEY}/p"));
    assert!(written.is_some_and(|write| write.value.as_deref().is_none_or(|value| !value.contains("npm:app"))));
}

/// A file moved away takes the temporary context configuration its path made, which is forgotten
/// without saving anything.
#[test]
fn a_moved_file_drops_its_stale_temporary_context_configuration() {
    let mut store = project_store();
    let old = context("src/main.cxx", "cc");
    store.adopt_temporary(old.clone(), &[]);
    assert!(ids(&store).contains(&old.id));
    let persist = store.drop_stale_context_configs(&strings(&["/p/src/main.cxx"]));
    assert!(!ids(&store).contains(&old.id));
    assert!(store.temporary.is_empty());
    assert!(persist.writes.iter().all(|write| write.key.starts_with("openidea.run.")), "only stored lists change");
}

/// Removing one file's context configuration keeps the other temporary ones and the detected and
/// saved configurations.
#[test]
fn keeps_other_temporary_and_non_temporary_configurations_on_removal() {
    let mut store = project_store();
    let moved = context("src/main.cxx", "cc");
    let other = context("app/main.go", "go");
    store.adopt_temporary(moved, &[]);
    store.adopt_temporary(other.clone(), &[]);
    store.drop_stale_context_configs(&strings(&["/p/src/main.cxx"]));
    assert_eq!(store.temporary, std::slice::from_ref(&other.id));
    let listed = ids(&store);
    assert!(listed.contains(&"maven:spring-boot:run".to_owned()) && listed.contains(&other.id), "{listed:?}");
}
