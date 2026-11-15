//! The shortcut registry: every keyboard shortcut the app knows about, and
//! how a keypress resolves against it.
//!
//! One registry serves both features that share the vocabulary:
//!
//! * **`/settings`** — every registered shortcut becomes one row (host
//!   shortcuts on the host card, a plugin's on its own card), generated from
//!   the registry rather than restated. A spec registered with `fixed` set
//!   renders read-only: the binding is locked at registration.
//! * **Input** — `App::handle_key` asks the [`Keymap`] whether a keypress
//!   denotes a registered shortcut; the host's own actions dispatch to their
//!   handlers, a plugin's to [`crate::plugins::PluginHost::shortcut_triggered`].
//!
//! The registry merges the host's [`HOST_SHORTCUTS`] with every plugin's
//! declarations. Validation is strict, like everything else that reads
//! `config.toml`: a spec whose default combo is malformed, reserved by the
//! composer/interrupt guard, or already claimed by another registration is a
//! startup error, and so is a `[shortcuts]` override that names an unknown
//! id, overrides a fixed binding, or collides with another entry. Silence
//! would hide mistakes; a message names the two ids.
//!
//! Combos are parsed by [`cst_plugin_api::shortcuts`], which also owns the
//! grammar and the reserved set; this module is the registry bookkeeping on
//! top of it, plus the override handling for `config.toml`'s `[shortcuts]`
//! table. The front-end converts its key events to
//! [`cst_plugin_api::shortcuts::KeyCombo`]s (the TUI keeps its adapter in
//! [`crate::tui::keymap`]) and asks this module whether a combo fires a
//! shortcut — matching is terminal-agnostic, like the rest of `core`.

use std::collections::BTreeMap;

use cst_plugin_api::settings::SettingField;
use cst_plugin_api::shortcuts::KeyCombo;
use cst_plugin_api::shortcuts::ShortcutSpec;
use cst_plugin_api::shortcuts::combo_list_string;
use cst_plugin_api::shortcuts::is_reserved;
use cst_plugin_api::shortcuts::parse_combo_list;

/// The host's own shortcuts, in `/settings` display order. All of them are
/// rebindable; the keys the host owns outright (interrupt, the composer's
/// editing keys, completion) are not registered at all — they live in
/// [`cst_plugin_api::shortcuts::is_reserved`] so nothing can claim them.
pub static HOST_SHORTCUTS: &[ShortcutSpec] = &[
    ShortcutSpec::new(
        "thinking",
        "Thinking cards",
        "Fold or expand the thinking cards (as `/thinking` does).",
        "ctrl+t",
        false,
    ),
    ShortcutSpec::new(
        "tool_detail",
        "Tool output",
        "Fold or expand tool-call output blocks.",
        "ctrl+o",
        false,
    ),
    ShortcutSpec::new(
        "page_up",
        "Page up",
        "Scroll the transcript up a page (fullscreen mode).",
        "pageup",
        false,
    ),
    ShortcutSpec::new(
        "page_down",
        "Page down",
        "Scroll the transcript down a page (fullscreen mode).",
        "pagedown",
        false,
    ),
    ShortcutSpec::new(
        "jump_bottom",
        "Jump to latest",
        "Scroll to the newest transcript line and re-arm follow mode.",
        "ctrl+end",
        false,
    ),
    ShortcutSpec::new(
        "history_prev",
        "History previous",
        "Recall the previous input from this session's history.",
        "ctrl+p",
        false,
    ),
    ShortcutSpec::new(
        "history_next",
        "History next",
        "Recall the next input from this session's history.",
        "ctrl+n",
        false,
    ),
    ShortcutSpec::new(
        "quit",
        "Quit",
        "Exit the app (when the composer is empty).",
        "ctrl+d",
        false,
    ),
];

/// One registry entry: a spec plus its owner. `owner` is `None` for the
/// host's own shortcuts, the plugin id otherwise — the same split the
/// status bar uses between host segments and `plugin:key` ones.
#[derive(Debug, Clone)]
pub struct ShortcutEntry {
    /// The id a `[shortcuts]` override names: bare for the host
    /// (`"thinking"`), `plugin:id` for a plugin (`"tps:rate"`).
    pub full_id: String,
    pub owner: Option<&'static str>,
    pub spec: &'static ShortcutSpec,
}

/// Why a shortcut registry or a `[shortcuts]` override was refused.
#[derive(Debug, thiserror::Error)]
pub enum KeymapError {
    #[error("shortcut id `{id}` is not a valid id (letters, digits, `-`, `_`)")]
    InvalidSpecId { id: String },
    #[error("shortcut `{id}`: {message}")]
    BadDefault { id: String, message: String },
    #[error("shortcut `{id}` is registered more than once")]
    DuplicateId { id: String },
    #[error("`{first}` and `{second}` both claim {combo}")]
    RegistryConflict {
        first: String,
        second: String,
        combo: String,
    },
    #[error("[shortcuts] names unknown shortcut `{id}` (known: {known})")]
    UnknownId { id: String, known: String },
    #[error("[shortcuts].{id}: {message}")]
    BadOverride { id: String, message: String },
    #[error("[shortcuts].{id}: that shortcut is fixed and cannot be rebound")]
    OverrideOnFixed { id: String },
    #[error("[shortcuts].{id} claims {combo}, already bound by `{other}`")]
    OverrideConflict {
        id: String,
        other: String,
        combo: String,
    },
}

/// Validate and merge the host's and the plugins' shortcut declarations into
/// one registry, in registration order (host first, then each plugin's).
pub fn build_registry(
    plugin_specs: &[(&'static str, &'static [ShortcutSpec])],
) -> Result<Vec<ShortcutEntry>, KeymapError> {
    let mut entries = Vec::new();
    for spec in HOST_SHORTCUTS {
        push_entry(&mut entries, None, spec)?;
    }
    for (plugin_id, specs) in plugin_specs {
        for spec in *specs {
            push_entry(&mut entries, Some(plugin_id), spec)?;
        }
    }
    Ok(entries)
}

fn push_entry(
    entries: &mut Vec<ShortcutEntry>,
    owner: Option<&'static str>,
    spec: &'static ShortcutSpec,
) -> Result<(), KeymapError> {
    if !valid_spec_id(spec.id) {
        return Err(KeymapError::InvalidSpecId {
            id: spec.id.to_string(),
        });
    }
    let full_id = match owner {
        None => spec.id.to_string(),
        Some(plugin_id) => format!("{plugin_id}:{}", spec.id),
    };
    if entries.iter().any(|entry| entry.full_id == full_id) {
        return Err(KeymapError::DuplicateId { id: full_id });
    }
    let combos = parse_combo_list(spec.default)
        .map_err(|message| KeymapError::BadDefault {
            id: full_id.clone(),
            message,
        })?;
    for combo in &combos {
        if is_reserved(combo) {
            return Err(KeymapError::BadDefault {
                id: full_id.clone(),
                message: format!(
                    "default `{}` is reserved by the host",
                    combo.canonical_string()
                ),
            });
        }
    }
    // Two registrations claiming the same default combo would make one of
    // them a lie: refuse the build rather than let dispatch order decide.
    for combo in &combos {
        for entry in entries.iter() {
            if entry
                .combos()
                .iter()
                .any(|existing| existing.matches(combo))
            {
                return Err(KeymapError::RegistryConflict {
                    first: entry.full_id.clone(),
                    second: full_id.clone(),
                    combo: combo.canonical_string(),
                });
            }
        }
    }
    entries.push(ShortcutEntry {
        full_id,
        owner,
        spec,
    });
    Ok(())
}

/// A spec id is the plugin-local half of a `plugin:id` pair: no `:` (the
/// scope separator) and no `.` (the settings-key path separator).
fn valid_spec_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl ShortcutEntry {
    /// The entry's default combos, parsed once here (the registry build has
    /// already proven them well-formed).
    pub fn combos(&self) -> Vec<KeyCombo> {
        // `parse_combo_list` was checked at registration; the copy is cheap
        // and keeps this type free of cached state.
        parse_combo_list(self.spec.default).unwrap_or_default()
    }
}

/// The `/settings` row for one registered shortcut: a text field at
/// `shortcuts.<full_id>` whose default shows the registered combo. All of
/// these rows live on the dedicated **Shortcuts** card, so a plugin's
/// shortcut carries its owner in the label (`"Meter (tps)"`) — two plugins
/// registering the same label stay distinguishable. The key, label, and
/// help text are `&'static str`s, so they are leaked — call this once per
/// registry entry and store the field (the registry is fixed at build time;
/// `App` builds the fields once and `/settings` reuses them on every open).
/// A few dozen bytes per registered shortcut per run.
pub fn setting_field(entry: &ShortcutEntry) -> SettingField {
    let key: &'static str =
        Box::leak(format!("shortcuts.{}", entry.full_id).into_boxed_str());
    let label: &'static str = match entry.owner {
        None => entry.spec.label,
        Some(plugin_id) => Box::leak(
            format!("{} ({plugin_id})", entry.spec.label).into_boxed_str(),
        ),
    };
    let description: &'static str = Box::leak(
        format!(
            "{} Press the new combo to rebind — further presses add \
             alternatives; Backspace disables the shortcut, Esc cancels.",
            entry.spec.description
        )
        .into_boxed_str(),
    );
    SettingField::text(key, label, description, Some(entry.spec.default))
}

/// The runtime half: the registry plus the `[shortcuts]` overrides, ready to
/// answer `does this keypress mean this shortcut?`.
#[derive(Debug, Clone)]
pub struct Keymap {
    registry: Vec<ShortcutEntry>,
    effective: BTreeMap<String, Vec<KeyCombo>>,
}

impl Keymap {
    /// Build from a registry and `config.toml`'s `[shortcuts]` table. Strict:
    /// every failure mode is an error naming the offending id, because a
    /// silently ignored override would leave the user pressing a key that
    /// does nothing.
    pub fn build(
        registry: &[ShortcutEntry],
        overrides: &BTreeMap<String, String>,
    ) -> Result<Self, KeymapError> {
        // Effective bindings start from the defaults; overrides are applied
        // one at a time, each checked against everything else currently in
        // force (defaults and earlier overrides alike).
        let mut effective: BTreeMap<String, Vec<KeyCombo>> = registry
            .iter()
            .map(|entry| (entry.full_id.clone(), entry.combos()))
            .collect();
        for (id, text) in overrides {
            let Some(entry) = registry.iter().find(|entry| &entry.full_id == id) else {
                let known = registry
                    .iter()
                    .map(|entry| entry.full_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(KeymapError::UnknownId {
                    id: id.clone(),
                    known,
                });
            };
            if entry.spec.fixed {
                return Err(KeymapError::OverrideOnFixed { id: id.clone() });
            }
            // An empty override disables the shortcut outright: it keeps no
            // key at all, distinct from an *absent* key, which means "run on
            // the registered default".
            let combos = if text.trim().is_empty() {
                Vec::new()
            } else {
                let combos =
                    parse_combo_list(text).map_err(|message| KeymapError::BadOverride {
                        id: id.clone(),
                        message,
                    })?;
                for combo in &combos {
                    if is_reserved(combo) {
                        return Err(KeymapError::BadOverride {
                            id: id.clone(),
                            message: format!(
                                "`{}` is reserved by the host",
                                combo.canonical_string()
                            ),
                        });
                    }
                }
                for combo in &combos {
                    for (other_id, other_combos) in &effective {
                        if other_id == id {
                            continue;
                        }
                        if other_combos.iter().any(|existing| existing.matches(combo)) {
                            return Err(KeymapError::OverrideConflict {
                                id: id.clone(),
                                other: other_id.clone(),
                                combo: combo.canonical_string(),
                            });
                        }
                    }
                }
                combos
            };
            effective.insert(id.clone(), combos);
        }
        Ok(Self {
            registry: registry.to_vec(),
            effective,
        })
    }

    pub fn registry(&self) -> &[ShortcutEntry] {
        &self.registry
    }

    pub fn entry(&self, full_id: &str) -> Option<&ShortcutEntry> {
        self.registry
            .iter()
            .find(|entry| entry.full_id == full_id)
    }

    /// Whether `combo` fires the shortcut named `full_id`.
    pub fn matches(&self, full_id: &str, combo: &KeyCombo) -> bool {
        self.effective
            .get(full_id)
            .is_some_and(|combos| combos.iter().any(|existing| existing.matches(combo)))
    }

    /// The plugin-owned shortcut `combo` fires, if any. The registry build
    /// has proven the bindings disjoint, so at most one entry can match.
    pub fn match_plugin(&self, combo: &KeyCombo) -> Option<String> {
        self.registry
            .iter()
            .filter(|entry| entry.owner.is_some())
            .find(|entry| {
                self.effective
                    .get(&entry.full_id)
                    .is_some_and(|combos| combos.iter().any(|existing| existing.matches(combo)))
            })
            .map(|entry| entry.full_id.clone())
    }

    /// The display form of a shortcut's first effective combo (`"Ctrl+T"`)
    /// — what footer hints show. A disabled shortcut reads `disabled`; an
    /// unknown id falls back to itself.
    pub fn primary_display(&self, full_id: &str) -> String {
        match self.effective.get(full_id) {
            Some(combos) if combos.is_empty() => "disabled".to_string(),
            Some(combos) => combos
                .first()
                .map(KeyCombo::display)
                .unwrap_or_else(|| full_id.to_string()),
            None => full_id.to_string(),
        }
    }

    /// The registered shortcut whose effective bindings already claim
    /// `combo`, excluding `full_id` itself — what a live capture editor
    /// checks before accepting a pressed key. Restating the shortcut's own
    /// binding is never a conflict, and the registry builds keep bindings
    /// disjoint, so at most one other entry can match.
    pub fn conflicts_with_other(&self, full_id: &str, combo: &KeyCombo) -> Option<String> {
        self.effective
            .iter()
            .filter(|(id, _)| id.as_str() != full_id)
            .find(|(_, combos)| combos.iter().any(|existing| existing.matches(combo)))
            .map(|(id, _)| id.clone())
    }

    /// The canonical config form of a shortcut's effective combos
    /// (`"ctrl+t, alt+y"`) — what conflict messages quote. A disabled
    /// shortcut reads empty, matching the stored override.
    pub fn effective_string(&self, full_id: &str) -> String {
        self.effective
            .get(full_id)
            .map(|combos| combo_list_string(combos))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cst_plugin_api::shortcuts::parse_combo;

    fn combo(text: &str) -> KeyCombo {
        parse_combo(text).expect(text)
    }

    fn registry() -> Vec<ShortcutEntry> {
        build_registry(&[]).expect("host registry")
    }

    fn keymap_with(overrides: &[(&str, &str)]) -> Keymap {
        let map = overrides
            .iter()
            .map(|(id, text)| (id.to_string(), text.to_string()))
            .collect();
        Keymap::build(&registry(), &map).expect("keymap")
    }

    #[test]
    fn the_host_registry_covers_the_eight_actions() {
        let entries = registry();
        let ids = entries
            .iter()
            .map(|entry| entry.full_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "thinking",
                "tool_detail",
                "page_up",
                "page_down",
                "jump_bottom",
                "history_prev",
                "history_next",
                "quit"
            ]
        );
        assert!(entries.iter().all(|entry| entry.owner.is_none()));
    }

    #[test]
    fn defaults_match_their_keys() {
        let keymap = keymap_with(&[]);
        assert!(keymap.matches("thinking", &combo("ctrl+t")));
        assert!(!keymap.matches("thinking", &combo("ctrl+y")));
        assert!(keymap.matches("page_up", &combo("pageup")));
        assert!(keymap.matches("jump_bottom", &combo("ctrl+end")));
        assert!(!keymap.matches("jump_bottom", &combo("ctrl+y")));
    }

    #[test]
    fn a_ctrl_shift_keypress_matches_a_ctrl_binding() {
        let keymap = keymap_with(&[]);
        assert!(keymap.matches("thinking", &combo("ctrl+shift+t")));
        assert!(keymap.matches("thinking", &combo("ctrl+T")));
    }

    #[test]
    fn overrides_rebind_and_conflicts_are_refused() {
        let keymap = keymap_with(&[("thinking", "alt+y")]);
        assert!(keymap.matches("thinking", &combo("alt+y")));
        assert!(!keymap.matches("thinking", &combo("ctrl+t")));
        assert_eq!(keymap.primary_display("thinking"), "Alt+Y");

        let map = [(
            "no_such_action".to_string(),
            "ctrl+z".to_string(),
        )]
        .into_iter()
        .collect::<BTreeMap<_, _>>();
        assert!(matches!(
            Keymap::build(&registry(), &map),
            Err(KeymapError::UnknownId { .. })
        ));
        let map = [("thinking".to_string(), "ctrl+c".to_string())]
            .into_iter()
            .collect();
        assert!(matches!(
            Keymap::build(&registry(), &map),
            Err(KeymapError::BadOverride { .. })
        ));
        let map = [("thinking".to_string(), "ctrl+o".to_string())]
            .into_iter()
            .collect();
        assert!(matches!(
            Keymap::build(&registry(), &map),
            Err(KeymapError::OverrideConflict { .. })
        ));
    }

    #[test]
    fn registry_validation_refuses_bad_specs() {
        static BAD_ID: [ShortcutSpec; 1] = [ShortcutSpec::new("a:b", "B", "…", "ctrl+z", false)];
        assert!(matches!(
            build_registry(&[("p", &BAD_ID)]),
            Err(KeymapError::InvalidSpecId { .. })
        ));

        static BAD_DEFAULT: [ShortcutSpec; 1] = [ShortcutSpec::new("thing", "T", "…", "t", false)];
        assert!(matches!(
            build_registry(&[("p", &BAD_DEFAULT)]),
            Err(KeymapError::BadDefault { .. })
        ));

        static RESERVED: [ShortcutSpec; 1] = [ShortcutSpec::new("thing", "T", "…", "ctrl+c", false)];
        assert!(matches!(
            build_registry(&[("p", &RESERVED)]),
            Err(KeymapError::BadDefault { .. })
        ));

        // A plugin registering the same local id twice is a build error;
        // scoped ids across plugins (`a:x` vs `b:x`) never collide.
        static DUP: [ShortcutSpec; 2] = [
            ShortcutSpec::new("meter", "M", "…", "ctrl+z", false),
            ShortcutSpec::new("meter", "M", "…", "ctrl+x", false),
        ];
        assert!(matches!(
            build_registry(&[("p", &DUP)]),
            Err(KeymapError::DuplicateId { .. })
        ));

        static CLASH: [ShortcutSpec; 1] = [ShortcutSpec::new("thing", "T", "…", "ctrl+t", false)];
        assert!(matches!(
            build_registry(&[("p", &CLASH)]),
            Err(KeymapError::RegistryConflict { .. })
        ));
    }

    #[test]
    fn plugin_entries_are_scoped_and_dispatchable() {
        static TOGGLE: [ShortcutSpec; 1] = [ShortcutSpec::new("meter", "Meter", "…", "ctrl+g", false)];
        let entries = build_registry(&[("tps", &TOGGLE)]).expect("registry");
        let entry = entries
            .iter()
            .find(|entry| entry.full_id == "tps:meter")
            .expect("scoped id");
        assert_eq!(entry.owner, Some("tps"));
        let keymap = Keymap::build(&entries, &BTreeMap::new()).expect("keymap");
        assert_eq!(
            keymap.match_plugin(&combo("ctrl+g")).as_deref(),
            Some("tps:meter")
        );
        assert_eq!(keymap.match_plugin(&combo("ctrl+t")), None);
    }

    #[test]
    fn an_empty_override_disables_the_shortcut() {
        let keymap = keymap_with(&[("thinking", "")]);
        // The default combo no longer fires: the shortcut keeps no key.
        assert!(!keymap.matches("thinking", &combo("ctrl+t")));
        assert!(!keymap.matches("thinking", &combo("alt+y")));
        // …and the hint shows the disabled state instead of a combo.
        assert_eq!(keymap.primary_display("thinking"), "disabled");
        assert!(keymap.conflicts_with_other("tool_detail", &combo("ctrl+t")).is_none(),
            "a disabled binding claims nothing, so the combo is free");
        // Other shortcuts are untouched.
        assert!(keymap.matches("tool_detail", &combo("ctrl+o")));

        // Whitespace-only text disables the same way.
        let keymap = keymap_with(&[("thinking", "   ")]);
        assert!(!keymap.matches("thinking", &combo("ctrl+t")));
        assert_eq!(keymap.primary_display("thinking"), "disabled");

        // A fixed shortcut refuses to be disabled through an override.
        static LOCKED: [ShortcutSpec; 1] =
            [ShortcutSpec::new("locked", "L", "…", "ctrl+q", true)];
        let registry = build_registry(&[("p", &LOCKED)]).expect("registry");
        let map = [("p:locked".to_string(), String::new())].into_iter().collect();
        assert!(matches!(
            Keymap::build(&registry, &map),
            Err(KeymapError::OverrideOnFixed { .. })
        ));
    }
}
