//! The shortcut registry vocabulary: how the host and the plugins declare
//! keyboard shortcuts.
//!
//! A shortcut is one [`ShortcutSpec`]: a stable id, a row label for
//! `/settings`, a default combo string, and whether the registration locks it
//! against rebinding. The host merges its own specs with every plugin's
//! ([`Plugin::shortcuts`](crate::plugin::Plugin::shortcuts)) into one
//! registry; `/settings` generates one editable row per entry (a `fixed`
//! entry renders read-only), and a keypress resolves through the same table.
//!
//! Combos are carried as *text* (`"ctrl+alt+t"`, comma-separated for
//! multiple) — the same convention as [`crate::settings`], so both the
//! config file and the settings editor speak one grammar. [`KeyCombo`] is
//! the parsed form: modifiers plus a [`ComboKey`], canonicalized so a
//! terminal that reports `Ctrl+Shift+T` matches a stored `ctrl+t`.
//!
//! The grammar deliberately keeps typing safe: a bare character is input,
//! never a shortcut, and the keys the composer and the interrupt guard own
//! ([`is_reserved`]) cannot be bound by anyone.

/// One registered shortcut.
///
/// `id` is unique within its owner: the host's own ids are bare
/// (`"thinking"`), a plugin's are scoped to `plugin:id` by the host (the same
/// `plugin:key` convention as status segments), so `id` itself only has to be
/// unique inside the declaring plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortcutSpec {
    /// Plugin-local id (`"thinking"`, `"rate_toggle"`). Letters, digits, `-`
    /// and `_`; no `:` or `.`, which carry structural meaning elsewhere.
    pub id: &'static str,
    /// Row label, kept short — the row is one line.
    pub label: &'static str,
    /// One-line explanation shown in the help bar while the row is focused.
    pub description: &'static str,
    /// The default combo string (`"ctrl+t"`; comma-separated for several).
    /// Parsed and validated when the host loads the plugin.
    pub default: &'static str,
    /// The registration locks the binding: `/settings` renders the row
    /// read-only and a stored override for it is refused.
    pub fixed: bool,
}

impl ShortcutSpec {
    pub const fn new(
        id: &'static str,
        label: &'static str,
        description: &'static str,
        default: &'static str,
        fixed: bool,
    ) -> Self {
        Self {
            id,
            label,
            description,
            default,
            fixed,
        }
    }
}

/// One key of a [`KeyCombo`]. A small vocabulary of its own so this crate
/// stays independent of any terminal library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComboKey {
    Char(char),
    Backspace,
    Delete,
    Enter,
    Tab,
    BackTab,
    Esc,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    F(u8),
}

/// One parsed key combo: modifiers plus a key.
///
/// Equality goes through [`KeyCombo::canonical`], so a terminal that reports
/// `Ctrl+Shift+T` matches a stored `ctrl+t` (terminals rarely report the
/// shift distinctly once ctrl is held).
#[derive(Debug, Clone, Copy)]
pub struct KeyCombo {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: ComboKey,
}

impl PartialEq for KeyCombo {
    fn eq(&self, other: &Self) -> bool {
        let (a, b) = (self.canonical(), other.canonical());
        a.ctrl == b.ctrl && a.alt == b.alt && a.shift == b.shift && a.key == b.key
    }
}

impl Eq for KeyCombo {}

impl KeyCombo {
    pub const fn new(ctrl: bool, alt: bool, shift: bool, key: ComboKey) -> Self {
        Self {
            ctrl,
            alt,
            shift,
            key,
        }
    }

    /// The comparison form: character keys drop `shift` when `ctrl` is held.
    pub fn canonical(&self) -> Self {
        let shift = self.shift && !(self.ctrl && matches!(self.key, ComboKey::Char(_)));
        Self {
            shift,
            ..*self
        }
    }

    /// Whether `self` and `other` denote the same physical combo.
    pub fn matches(&self, other: &KeyCombo) -> bool {
        self.canonical() == other.canonical()
    }

    /// The canonical config form: lowercase tokens joined by `+`
    /// (`"ctrl+alt+t"`). Built from the canonical form, so a stored
    /// `ctrl+shift+t` reads back as the `ctrl+t` it matches.
    pub fn canonical_string(&self) -> String {
        let canonical = self.canonical();
        let mut parts: Vec<String> = Vec::new();
        if canonical.ctrl {
            parts.push("ctrl".to_string());
        }
        if canonical.alt {
            parts.push("alt".to_string());
        }
        if canonical.shift {
            parts.push("shift".to_string());
        }
        parts.push(key_name(canonical.key));
        parts.join("+")
    }

    /// The display form: `Ctrl+Alt+T`, `PgUp` — for footer hints and rows.
    pub fn display(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".to_string());
        }
        if self.alt {
            parts.push("Alt".to_string());
        }
        if self.shift {
            parts.push("Shift".to_string());
        }
        let key = self.key;
        parts.push(match key {
            ComboKey::Char(' ') => "Space".to_string(),
            ComboKey::Char(c) => c.to_uppercase().to_string(),
            ComboKey::PageUp => "PgUp".to_string(),
            ComboKey::PageDown => "PgDn".to_string(),
            ComboKey::BackTab => "Shift+Tab".to_string(),
            other => {
                let name = key_name(other);
                let mut chars = name.chars();
                match chars.next() {
                    Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                    None => String::new(),
                }
            }
        });
        parts.join("+")
    }
}

/// The lowercase grammar name of a key (`"t"`, `"pageup"`, `"space"`).
fn key_name(key: ComboKey) -> String {
    match key {
        ComboKey::Char(' ') => "space".to_string(),
        ComboKey::Char('+') => "plus".to_string(),
        ComboKey::Char('-') => "minus".to_string(),
        ComboKey::Char(',') => "comma".to_string(),
        ComboKey::Char('.') => "period".to_string(),
        ComboKey::Char(c) => c.to_string(),
        ComboKey::Backspace => "backspace".to_string(),
        ComboKey::Delete => "delete".to_string(),
        ComboKey::Enter => "enter".to_string(),
        ComboKey::Tab => "tab".to_string(),
        ComboKey::BackTab => "backtab".to_string(),
        ComboKey::Esc => "esc".to_string(),
        ComboKey::Left => "left".to_string(),
        ComboKey::Right => "right".to_string(),
        ComboKey::Up => "up".to_string(),
        ComboKey::Down => "down".to_string(),
        ComboKey::Home => "home".to_string(),
        ComboKey::End => "end".to_string(),
        ComboKey::PageUp => "pageup".to_string(),
        ComboKey::PageDown => "pagedown".to_string(),
        ComboKey::Insert => "insert".to_string(),
        ComboKey::F(n) => format!("f{n}"),
    }
}

/// Parse one combo token (`"ctrl+alt+t"`). The modifiers may come in any
/// order; the final token names the key.
pub fn parse_combo(text: &str) -> Result<KeyCombo, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("empty combo".to_string());
    }
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut key = None;
    for token in trimmed.split('+') {
        let token = token.trim();
        if token.is_empty() {
            return Err(format!("`{trimmed}` is not a valid combo"));
        }
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => ctrl = true,
            "alt" | "opt" | "option" => alt = true,
            "shift" => shift = true,
            name => {
                if key.is_some() {
                    return Err(format!("`{trimmed}` names more than one key"));
                }
                key = Some(parse_key(name)?);
            }
        }
    }
    let Some(key) = key else {
        return Err(format!("`{trimmed}` names no key"));
    };
    let combo = KeyCombo::new(ctrl, alt, shift, key);
    validate_combo(&combo).map_err(|message| format!("`{trimmed}`: {message}"))?;
    Ok(combo)
}

/// Parse a comma-separated combo list (`"ctrl+t, alt+y"`). At least one
/// combo is required; duplicates collapse.
pub fn parse_combo_list(text: &str) -> Result<Vec<KeyCombo>, String> {
    let mut combos: Vec<KeyCombo> = Vec::new();
    for token in text.split([',', ';']) {
        if token.trim().is_empty() {
            continue;
        }
        let combo = parse_combo(token)?;
        if !combos.iter().any(|existing| existing.matches(&combo)) {
            combos.push(combo);
        }
    }
    if combos.is_empty() {
        return Err("no key combos given".to_string());
    }
    Ok(combos)
}

/// The canonical config form of a parsed list, in registration order.
pub fn combo_list_string(combos: &[KeyCombo]) -> String {
    combos
        .iter()
        .map(KeyCombo::canonical_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_key(name: &str) -> Result<ComboKey, String> {
    let named = match name {
        "enter" => ComboKey::Enter,
        "tab" => ComboKey::Tab,
        "backtab" => ComboKey::BackTab,
        "esc" | "escape" => ComboKey::Esc,
        "backspace" => ComboKey::Backspace,
        "delete" | "del" => ComboKey::Delete,
        "left" => ComboKey::Left,
        "right" => ComboKey::Right,
        "up" => ComboKey::Up,
        "down" => ComboKey::Down,
        "home" => ComboKey::Home,
        "end" => ComboKey::End,
        "pageup" | "pgup" => ComboKey::PageUp,
        "pagedown" | "pgdn" => ComboKey::PageDown,
        "insert" | "ins" => ComboKey::Insert,
        "space" => ComboKey::Char(' '),
        "plus" => ComboKey::Char('+'),
        "minus" => ComboKey::Char('-'),
        "comma" => ComboKey::Char(','),
        "period" => ComboKey::Char('.'),
        other => {
            // F1..F12.
            if let Some(rest) = other.strip_prefix('f')
                && let Ok(number) = rest.parse::<u8>()
                && (1..=12).contains(&number)
            {
                return Ok(ComboKey::F(number));
            }
            // A single character otherwise; case is not meaningful in a
            // stored combo (`ctrl+T` is the key the terminal reports as
            // `ctrl+t`).
            let mut chars = other.chars();
            let (first, second) = (chars.next(), chars.next());
            match (first, second) {
                (Some(c), None) => ComboKey::Char(c.to_ascii_lowercase()),
                _ => return Err(format!("`{name}` is not a key name")),
            }
        }
    };
    Ok(named)
}

/// Whether `combo` is one of the keys the host owns outright: the interrupt
/// guard, the composer's editing keys, command completion, and plain typing.
/// Nothing may rebind these, and no registration may claim them.
pub fn is_reserved(combo: &KeyCombo) -> bool {
    match combo.key {
        // Esc closes dialogs and arms the interrupt guard; Enter delivers
        // (with Alt) or splits lines (Ctrl/Shift); Tab completes. All of it
        // is modal behavior the registry stays out of.
        ComboKey::Esc | ComboKey::Enter | ComboKey::Tab | ComboKey::BackTab => true,
        // Composer cursor and edit keys. Bare `Home`/`End` are the composer's
        // line movement; with Ctrl they belong to the transcript and stay
        // bindable (`ctrl+end` is the jump-to-latest default).
        ComboKey::Backspace | ComboKey::Delete => true,
        ComboKey::Left | ComboKey::Right | ComboKey::Up | ComboKey::Down => true,
        ComboKey::Home => !combo.ctrl,
        ComboKey::End => !combo.ctrl,
        ComboKey::Char(c) => {
            let lower = c.to_ascii_lowercase();
            if combo.ctrl {
                // Ctrl+C is the interrupt/quit guard; the rest are the
                // composer's readline editing keys.
                matches!(lower, 'c' | 'j' | 'a' | 'e' | 'u' | 'k' | 'w')
            } else {
                // Without ctrl or alt a character is typing.
                !combo.alt
            }
        }
        _ => false,
    }
}

/// Grammar-level checks every combo must pass, whatever its source: a
/// character key needs ctrl or alt (a bare or shift-only character is
/// typing), and reserved keys are refused outright. The combo parser runs
/// this on everything it accepts; a front-end that *captures* keypresses
/// directly (the `/settings` shortcut editor) runs it on the pressed key
/// before showing the candidate.
pub fn validate_combo(combo: &KeyCombo) -> Result<(), String> {
    if let ComboKey::Char(c) = combo.key
        && !combo.ctrl
        && !combo.alt
    {
        return Err(format!(
            "`{}` would fire while typing — a character key needs ctrl or alt",
            c
        ));
    }
    if is_reserved(combo) {
        return Err("that key is reserved by the host".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn combo(text: &str) -> KeyCombo {
        parse_combo(text).expect(text)
    }

    #[test]
    fn combos_parse_and_display() {
        let parsed = combo("ctrl+t");
        assert!(parsed.ctrl && !parsed.alt && !parsed.shift);
        assert_eq!(parsed.key, ComboKey::Char('t'));
        assert_eq!(parsed.canonical_string(), "ctrl+t");
        assert_eq!(parsed.display(), "Ctrl+T");

        assert_eq!(combo("alt+y").canonical_string(), "alt+y");
        assert_eq!(combo("pgup").key, ComboKey::PageUp);
        assert_eq!(combo("PgUp").display(), "PgUp");
        assert_eq!(combo("pagedown").display(), "PgDn");
        assert_eq!(combo("f3").key, ComboKey::F(3));
        assert_eq!(combo("ctrl+end").key, ComboKey::End);
        assert!(combo("ctrl+end").ctrl);
        assert_eq!(combo("ctrl+space").key, ComboKey::Char(' '));
        assert_eq!(combo("ctrl+space").display(), "Ctrl+Space");
    }

    #[test]
    fn modifiers_come_in_any_order_and_lists_split() {
        assert_eq!(combo("alt+ctrl+t").canonical_string(), "ctrl+alt+t");
        let list = parse_combo_list("ctrl+t ; alt+y , ctrl+t").expect("list");
        assert_eq!(combo_list_string(&list), "ctrl+t, alt+y");
        assert!(parse_combo_list("").is_err());
        assert!(parse_combo_list("  ").is_err());
    }

    #[test]
    fn terminal_quirks_canonicalize_to_the_stored_form() {
        // Terminals often report ctrl+shift+t without the shift.
        assert!(combo("ctrl+shift+t").matches(&combo("ctrl+t")));
        // Case in the stored text is normalized.
        assert!(combo("ctrl+T").matches(&combo("ctrl+t")));
        assert_eq!(combo("ctrl+T").canonical_string(), "ctrl+t");
        // Alt keeps its shift: no terminal quirk to paper over there.
        assert!(!combo("alt+shift+t").matches(&combo("alt+t")));
    }

    #[test]
    fn typing_and_reserved_keys_are_refused() {
        // A bare or shift-only character is input, not a shortcut.
        assert!(parse_combo("t").is_err());
        assert!(parse_combo("shift+t").is_err());
        assert!(parse_combo("?").is_err());
        // The host's own keys.
        for text in ["esc", "ctrl+c", "ctrl+j", "ctrl+a", "ctrl+e", "ctrl+u", "ctrl+k", "ctrl+w"] {
            assert!(parse_combo(text).is_err(), "{text}");
        }
        for text in ["enter", "alt+enter", "ctrl+enter", "tab", "backtab"] {
            assert!(parse_combo(text).is_err(), "{text}");
        }
        for text in ["backspace", "delete", "left", "right", "up", "down", "home"] {
            assert!(parse_combo(text).is_err(), "{text}");
        }
        // But ctrl+end is the transcript's, and free to rebind.
        assert!(parse_combo("ctrl+end").is_ok());
    }

    #[test]
    fn malformed_tokens_are_refused() {
        assert!(parse_combo("ctrl+").is_err());
        assert!(parse_combo("ctrl").is_err());
        assert!(parse_combo("ctrl+alt").is_err());
        assert!(parse_combo("ctrl+t+u").is_err());
        assert!(parse_combo("ctrl+foo").is_err());
        assert!(parse_combo("f13").is_err());
        assert!(parse_combo("").is_err());
        assert!(parse_combo("++").is_err());
    }
}
