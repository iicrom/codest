//! The TUI's adapter between crossterm key events and the core keymap
//! ([`crate::core::keymap`]). Matching itself is terminal-agnostic — this is
//! only the one place that knows both vocabularies.

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;

use cst_plugin_api::shortcuts::ComboKey;
use cst_plugin_api::shortcuts::KeyCombo;

/// One keypress as a [`KeyCombo`], in the grammar's canonical terms:
/// characters are lowercased (the shift rides in the flag), and key kinds
/// this vocabulary does not name (media keys, keypad, …) simply never match.
pub fn combo_from_event(key: &KeyEvent) -> Option<KeyCombo> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let key = match key.code {
        KeyCode::Char(c) => ComboKey::Char(c.to_ascii_lowercase()),
        KeyCode::Backspace => ComboKey::Backspace,
        KeyCode::Enter => ComboKey::Enter,
        KeyCode::Tab => ComboKey::Tab,
        KeyCode::BackTab => ComboKey::BackTab,
        KeyCode::Esc => ComboKey::Esc,
        KeyCode::Left => ComboKey::Left,
        KeyCode::Right => ComboKey::Right,
        KeyCode::Up => ComboKey::Up,
        KeyCode::Down => ComboKey::Down,
        KeyCode::Home => ComboKey::Home,
        KeyCode::End => ComboKey::End,
        KeyCode::PageUp => ComboKey::PageUp,
        KeyCode::PageDown => ComboKey::PageDown,
        KeyCode::Insert => ComboKey::Insert,
        KeyCode::Delete => ComboKey::Delete,
        KeyCode::F(n) => ComboKey::F(n),
        KeyCode::Null
        | KeyCode::CapsLock
        | KeyCode::ScrollLock
        | KeyCode::NumLock
        | KeyCode::PrintScreen
        | KeyCode::Pause
        | KeyCode::Menu
        | KeyCode::KeypadBegin
        | KeyCode::Modifier(_)
        | KeyCode::Media(_) => return None,
    };
    Some(KeyCombo::new(ctrl, alt, shift, key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn events_convert_into_the_grammar_terms() {
        let combo = combo_from_event(&key(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .expect("plain press");
        assert!(combo.ctrl && !combo.shift);
        assert_eq!(combo.canonical_string(), "ctrl+t");

        // A terminal that reports ctrl+shift+T lands on the same combo.
        let combo = combo_from_event(&key(
            KeyCode::Char('T'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("plain press");
        assert!(combo.matches(&cst_plugin_api::shortcuts::parse_combo("ctrl+t").expect("combo")));

        // Release events are not presses.
        assert!(
            combo_from_event(&KeyEvent {
                code: KeyCode::Char('t'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Release,
                state: KeyEventState::NONE,
            })
            .is_none()
        );
        // Unnamed kinds never match.
        assert!(combo_from_event(&key(KeyCode::Null, KeyModifiers::NONE)).is_none());
        assert!(combo_from_event(&key(KeyCode::CapsLock, KeyModifiers::NONE)).is_none());
        // BackTab keeps its own kind.
        assert_eq!(
            combo_from_event(&key(KeyCode::BackTab, KeyModifiers::SHIFT))
                .expect("backtab")
                .key,
            ComboKey::BackTab
        );
    }
}
