//! What Vim mode decides on top of a Vim engine: the IDE's Ex commands (`:w`, `:q`, `:action`…)
//! and the rc file's mappings, including which single-key built-ins a `<leader>` shadows while a
//! mapping starts with them.
//!
//! The engine itself (motions, operators, registers) is not here: a key sequence no rc mapping
//! claims is the engine's.

use super::rc::{MapMode, RcCommand, SetValue, resolve_action_id};
use crate::platform::i18n::{I18n, Param};
use std::collections::BTreeMap;

/// What an Ex command line comes to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExOutcome {
    /// Run these actions, one after the other, once the key handling is over.
    Run(Vec<String>),
    /// Show this in the status bar.
    Status(String),
    /// Not one of the IDE's commands: the engine's own.
    Engine,
}

/// The IDE's Ex commands: full name, shortest abbreviation, the actions they run.
const EX_COMMANDS: [(&str, &str, &[&str]); 6] = [
    ("write", "w", &["file.save"]),
    ("wall", "wa", &["file.save"]),
    ("quit", "q", &["editor.closeTab"]),
    ("qall", "qa", &["editor.closeAllTabs"]),
    ("wq", "wq", &["file.save", "editor.closeTab"]),
    ("xit", "x", &["file.save", "editor.closeTab"]),
];

/// The command a typed name means: the longest prefix of it that is a command's name or
/// abbreviation, when that command's name starts with what was typed.
fn ex_command(name: &str) -> Option<&'static [&'static str]> {
    (1..=name.len()).rev().filter(|&i| name.is_char_boundary(i)).find_map(|i| {
        let prefix = &name[..i];
        EX_COMMANDS
            .iter()
            .find(|(full, short, _)| (*full == prefix || *short == prefix) && full.starts_with(name))
            .map(|(_, _, actions)| *actions)
    })
}

/// `:action <id>`: an IntelliJ action id or an OpenIDEA one.
pub fn run_ide_action(id: Option<&str>, action_exists: &dyn Fn(&str) -> bool, i18n: &mut I18n) -> ExOutcome {
    let Some(id) = id else { return ExOutcome::Status(i18n.tx("Usage: :action <action id>")) };
    let resolved = resolve_action_id(id);
    if !action_exists(resolved) {
        return ExOutcome::Status(i18n.tx_built(&["Vim: no action \"", "\""], &[Param::from(id)]));
    }
    ExOutcome::Run(vec![resolved.to_owned()])
}

/// An Ex command line typed after `:` (or run by a mapping).
pub fn handle_ex(input: &str, action_exists: &dyn Fn(&str) -> bool, i18n: &mut I18n) -> ExOutcome {
    let input = input.trim_start_matches([':', ' ']);
    let name_len = input.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(input.len());
    let (name, args) = input.split_at(name_len);
    if name.is_empty() {
        return ExOutcome::Engine;
    }
    if name == "action" {
        return run_ide_action(args.split_whitespace().next(), action_exists, i18n);
    }
    match ex_command(name) {
        Some(actions) => ExOutcome::Run(actions.iter().map(|a| (*a).to_owned()).collect()),
        None => ExOutcome::Engine,
    }
}

/// The modes a key sequence is typed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyMode {
    Normal,
    Insert,
    Visual,
    OperatorPending,
}

/// Single-key built-ins a leader may reuse, with what they do: while an rc mapping starts with one
/// of them, the built-in is set aside, or it would fire before the mapping can complete.
/// Operators and prefixes (`d`, `c`, `y`, `<C-d>`…) are never set aside: every command built on
/// them would break.
pub const LEADER_BUILTINS: [(&str, &str); 9] = [
    ("<Space>", "l"),
    ("<S-Space>", "w"),
    ("<C-Space>", "W"),
    ("<BS>", "h"),
    ("<S-BS>", "b"),
    ("<C-BS>", "B"),
    ("<Del>", "x"),
    (",", "repeat the last character search backwards"),
    (";", "repeat the last character search"),
];

/// The first key of a mapping's lhs: `<Space>`, `,`, `x`…
pub fn first_key(lhs: &str) -> &str {
    if lhs.starts_with('<')
        && let Some(close) = lhs.find('>')
        && close > 1
        && !lhs[1..close].contains('<')
    {
        return &lhs[..=close];
    }
    lhs.chars().next().map_or(lhs, |c| &lhs[..c.len_utf8()])
}

/// The keys of a mapping side: `<…>` notations and single characters.
pub fn split_keys(keys: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = keys;
    while !rest.is_empty() {
        let key = first_key(rest);
        out.push(key);
        rest = &rest[key.len()..];
    }
    out
}

/// What a typed key sequence comes to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyMatch {
    /// An rc mapping to other keys.
    Keys { rhs: String, noremap: bool },
    /// An rc mapping to an Ex command line.
    Ex(String),
    /// The start of an rc mapping: wait for more keys.
    Pending,
    /// A leader built-in that is in effect (its key alone).
    BuiltIn(&'static str),
    /// Not an rc mapping: the engine's own keys.
    Engine,
}

fn applies(mode: Option<MapMode>, key_mode: KeyMode) -> bool {
    match mode {
        None => key_mode != KeyMode::Insert,
        Some(MapMode::Normal) => key_mode == KeyMode::Normal,
        Some(MapMode::Insert) => key_mode == KeyMode::Insert,
        Some(MapMode::Visual) => key_mode == KeyMode::Visual,
        Some(MapMode::OperatorPending) => key_mode == KeyMode::OperatorPending,
    }
}

/// The rc file as applied: its options and mappings, and the leader built-ins set aside. Applying
/// another rc replaces the mappings; a built-in comes back once no mapping starts with its key.
#[derive(Clone, Debug, Default)]
pub struct RcKeymap {
    options: Vec<(String, SetValue)>,
    mappings: Vec<RcCommand>,
    /// Built-ins ever set aside, by key: `true` while one is.
    shadowed: BTreeMap<&'static str, bool>,
}

impl RcKeymap {
    /// Applies rc commands, replacing earlier rc mappings.
    pub fn apply(&mut self, commands: &[RcCommand]) {
        self.options.clear();
        self.mappings.clear();
        let mut shadow: Vec<&'static str> = Vec::new();
        for c in commands {
            match c {
                RcCommand::Set { name, value } => self.options.push((name.clone(), value.clone())),
                RcCommand::Map { lhs, .. } | RcCommand::MapEx { lhs, .. } => {
                    let key = first_key(lhs);
                    if let Some((builtin, _)) = LEADER_BUILTINS.iter().find(|(k, _)| *k == key)
                        && !shadow.contains(builtin)
                    {
                        shadow.push(*builtin);
                    }
                    self.mappings.push(c.clone());
                }
            }
        }
        for (key, on) in &mut self.shadowed {
            *on = shadow.contains(key);
        }
        for key in shadow {
            self.shadowed.insert(key, true);
        }
    }

    /// The options to set on the engine.
    pub fn options(&self) -> &[(String, SetValue)] {
        &self.options
    }

    /// The leader built-ins set aside now.
    pub fn shadowed(&self) -> Vec<&'static str> {
        self.shadowed.iter().filter(|(_, on)| **on).map(|(k, _)| *k).collect()
    }

    /// Whether the built-in of `key` is in effect.
    pub fn builtin_active(&self, key: &str) -> bool {
        LEADER_BUILTINS.iter().any(|(k, _)| *k == key) && !self.shadowed.get(key).copied().unwrap_or(false)
    }

    /// What `keys`, typed in `mode`, come to. A mapping applied later wins over an earlier one
    /// with the same keys; a complete mapping wins over a longer one it starts.
    pub fn resolve(&self, mode: KeyMode, keys: &[&str]) -> KeyMatch {
        let mut pending = false;
        for c in self.mappings.iter().rev() {
            let (map_mode, lhs) = match c {
                RcCommand::Map { mode, lhs, .. } | RcCommand::MapEx { mode, lhs, .. } => (*mode, lhs),
                RcCommand::Set { .. } => continue,
            };
            if !applies(map_mode, mode) {
                continue;
            }
            let lhs_keys = split_keys(lhs);
            if lhs_keys == keys {
                return match c {
                    RcCommand::Map { rhs, noremap, .. } => KeyMatch::Keys { rhs: rhs.clone(), noremap: *noremap },
                    RcCommand::MapEx { ex, .. } => KeyMatch::Ex(ex.clone()),
                    RcCommand::Set { .. } => KeyMatch::Engine,
                };
            }
            if lhs_keys.len() > keys.len() && lhs_keys.starts_with(keys) {
                pending = true;
            }
        }
        if pending {
            return KeyMatch::Pending;
        }
        match keys {
            [key] if mode != KeyMode::Insert && self.builtin_active(key) => {
                LEADER_BUILTINS.iter().find(|(k, _)| k == key).map_or(KeyMatch::Engine, |(k, _)| KeyMatch::BuiltIn(k))
            }
            _ => KeyMatch::Engine,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::rc::parse_vimrc;
    use super::*;
    mod proofs8_vim;

    const ACTIONS: [&str; 4] = ["navigate.declaration", "code.reformat", "file.save", "editor.closeTab"];

    fn exists(id: &str) -> bool {
        ACTIONS.contains(&id)
    }

    fn keymap(rc: &str) -> RcKeymap {
        let mut keymap = RcKeymap::default();
        keymap.apply(&parse_vimrc(rc, None).commands);
        keymap
    }

    fn run(actions: &[&str]) -> ExOutcome {
        ExOutcome::Run(actions.iter().map(|a| (*a).to_owned()).collect())
    }

    #[test]
    fn action_runs_openidea_actions_by_idea_or_openidea_id() {
        let mut i18n = I18n::default();
        assert_eq!(handle_ex("action GotoDeclaration", &exists, &mut i18n), run(&["navigate.declaration"]));
        assert_eq!(handle_ex("action code.reformat", &exists, &mut i18n), run(&["code.reformat"]));
        assert_eq!(
            handle_ex("action NoSuchAction", &exists, &mut i18n),
            ExOutcome::Status("Vim: no action \"NoSuchAction\"".to_owned())
        );
        assert_eq!(handle_ex("action", &exists, &mut i18n), ExOutcome::Status("Usage: :action <action id>".to_owned()));
    }

    #[test]
    fn w_saves_q_closes_the_tab_wq_does_both() {
        let mut i18n = I18n::default();
        assert_eq!(handle_ex("w", &exists, &mut i18n), run(&["file.save"]));
        assert_eq!(handle_ex("q", &exists, &mut i18n), run(&["editor.closeTab"]));
        assert_eq!(handle_ex("wq", &exists, &mut i18n), run(&["file.save", "editor.closeTab"]));
        assert_eq!(handle_ex("write", &exists, &mut i18n), run(&["file.save"]));
        assert_eq!(handle_ex("s/a/b/", &exists, &mut i18n), ExOutcome::Engine);
    }

    #[test]
    fn applies_rc_mappings_including_action_ones() {
        let keymap = keymap("nmap gd <Action>(GotoDeclaration)\nnnoremap H ^");
        assert_eq!(keymap.resolve(KeyMode::Normal, &["g"]), KeyMatch::Pending);
        let KeyMatch::Ex(ex) = keymap.resolve(KeyMode::Normal, &["g", "d"]) else { panic!("an Ex mapping") };
        assert_eq!(handle_ex(&ex, &exists, &mut I18n::default()), run(&["navigate.declaration"]));
        assert_eq!(keymap.resolve(KeyMode::Normal, &["H"]), KeyMatch::Keys { rhs: "^".to_owned(), noremap: true });
    }

    #[test]
    fn applies_insert_mode_mappings_accepting_esc_in_any_case() {
        let keymap = keymap("inoremap jk <esc>");
        assert_eq!(keymap.resolve(KeyMode::Insert, &["j"]), KeyMatch::Pending);
        let escape = KeyMatch::Keys { rhs: "<Esc>".to_owned(), noremap: true };
        assert_eq!(keymap.resolve(KeyMode::Insert, &["j", "k"]), escape);
        assert_eq!(keymap.resolve(KeyMode::Normal, &["j", "k"]), KeyMatch::Engine);
    }

    #[test]
    fn runs_a_leader_mapping_whose_first_key_shadows_a_built_in() {
        for (leader, keys) in [
            ("let mapleader = \" \"", ["<Space>", "f", "f"]),
            ("let mapleader = \",\"", [",", "f", "f"]),
            ("let mapleader = \"\\\\\"", ["\\", "f", "f"]),
        ] {
            let keymap = keymap(&format!("{leader}\nnmap <leader>ff <Action>(GotoFile)"));
            assert_eq!(keymap.resolve(KeyMode::Normal, &keys[..1]), KeyMatch::Pending, "{leader}");
            assert_eq!(keymap.resolve(KeyMode::Normal, &keys), KeyMatch::Ex("action GotoFile".to_owned()), "{leader}");
        }
    }

    #[test]
    fn keeps_built_in_operators_a_mapping_starts_with() {
        for (rc, keys) in [
            ("nmap dx <Action>(GotoFile)", ["d", "w"]),
            ("nmap cs <Action>(GotoFile)", ["c", "w"]),
            ("nnoremap yz yy", ["y", "y"]),
        ] {
            let keymap = keymap(rc);
            assert!(keymap.shadowed().is_empty(), "{rc}");
            assert_eq!(keymap.resolve(KeyMode::Normal, &keys), KeyMatch::Engine, "{rc}");
        }
    }

    #[test]
    fn keeps_built_ins_and_user_mappings_consistent_across_rc_reloads_that_change_the_leader() {
        let mut keymap = RcKeymap::default();
        let apply = |keymap: &mut RcKeymap, rc: &str| keymap.apply(&parse_vimrc(rc, None).commands);
        apply(&mut keymap, "let mapleader = \" \"\nnmap <leader>ff <Action>(GotoFile)");
        apply(&mut keymap, "let mapleader = \",\"\nnmap <leader>ff <Action>(GotoFile)");
        assert_eq!(keymap.shadowed(), [","]);
        apply(&mut keymap, "let mapleader = \" \"\nnmap <leader>ff <Action>(GotoFile)\nnnoremap Q dd");
        assert_eq!(keymap.shadowed(), ["<Space>"]);
        assert!(keymap.builtin_active(","));
        assert_eq!(keymap.resolve(KeyMode::Normal, &["<Space>", "f", "f"]), KeyMatch::Ex("action GotoFile".to_owned()));
        // noremap still reaches the built-ins.
        assert_eq!(keymap.resolve(KeyMode::Normal, &["Q"]), KeyMatch::Keys { rhs: "dd".to_owned(), noremap: true });
        apply(&mut keymap, "");
        apply(&mut keymap, "");
        assert_eq!(keymap.resolve(KeyMode::Normal, &["<Space>"]), KeyMatch::BuiltIn("<Space>"));
        assert_eq!(keymap.resolve(KeyMode::Normal, &[","]), KeyMatch::BuiltIn(","));
        assert_eq!(keymap.resolve(KeyMode::Normal, &["<Space>", "f", "f"]), KeyMatch::Engine);
    }

    #[test]
    fn restores_the_built_in_key_the_leader_shadowed_once_the_rc_stops_using_it() {
        let mut keymap = keymap("let mapleader = \" \"\nnmap <leader>ff <Action>(GotoFile)");
        assert!(!keymap.builtin_active("<Space>"));
        keymap.apply(&parse_vimrc("nmap gd <Action>(GotoDeclaration)", None).commands);
        assert!(keymap.builtin_active("<Space>"));
        assert_eq!(keymap.resolve(KeyMode::Normal, &["<Space>"]), KeyMatch::BuiltIn("<Space>"));
    }

    #[test]
    fn splits_mapping_sides_into_keys() {
        assert_eq!(split_keys("<Space>ff"), ["<Space>", "f", "f"]);
        assert_eq!(split_keys("<<Esc>"), ["<", "<Esc>"]);
        assert_eq!(split_keys("<lt>Esc>"), ["<lt>", "E", "s", "c", ">"]);
    }
}
