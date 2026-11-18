//! Keyboard input handling for the terminal emulator.
//! Dauerbot modifications adapt Herdr App's Apache-2.0 terminal key behavior;
//! see DAUERBOT-PATCHES.md for the source revision and compatibility limits.
//!
//! This module provides [`keystroke_to_bytes`], which converts GPUI keyboard
//! events into terminal escape sequences that can be written to the PTY.
//!
//! # Key Mappings
//!
//! ## Special Keys
//!
//! | Key | Sequence | Notes |
//! |-----|----------|-------|
//! | Enter | `\r` (0x0D) | Carriage return |
//! | Escape | `\x1b` (0x1B) | ESC |
//! | Backspace | `\x7f` (0x7F) | DEL |
//! | Tab | `\t` (0x09) | Horizontal tab |
//! | Shift+Tab | `\x1b[Z` | Backtab |
//! | Space | ` ` (0x20) | Space |
//! | Ctrl+Space | `\x00` | NUL |
//!
//! ## Arrow Keys
//!
//! Arrow key sequences depend on application cursor mode:
//!
//! | Key | Normal Mode | App Cursor Mode |
//! |-----|-------------|-----------------|
//! | Up | `\x1b[A` | `\x1bOA` |
//! | Down | `\x1b[B` | `\x1bOB` |
//! | Right | `\x1b[C` | `\x1bOC` |
//! | Left | `\x1b[D` | `\x1bOD` |
//!
//! ## Navigation Keys
//!
//! | Key | Sequence |
//! |-----|----------|
//! | Home | `\x1b[H` |
//! | End | `\x1b[F` |
//! | PageUp | `\x1b[5~` |
//! | PageDown | `\x1b[6~` |
//! | Insert | `\x1b[2~` |
//! | Delete | `\x1b[3~` |
//!
//! ## Function Keys
//!
//! | Key | Sequence |
//! |-----|----------|
//! | F1-F4 | `\x1bOP` - `\x1bOS` |
//! | F5-F12 | `\x1b[15~` - `\x1b[24~` |
//!
//! ## Control Combinations
//!
//! Ctrl+A through Ctrl+Z map to ASCII control characters 0x01-0x1A:
//!
//! | Combination | Byte |
//! |-------------|------|
//! | Ctrl+A | 0x01 |
//! | Ctrl+C | 0x03 (interrupt) |
//! | Ctrl+D | 0x04 (EOF) |
//! | Ctrl+Z | 0x1A (suspend) |
//!
//! ## Alt Combinations
//!
//! Alt+key sends ESC followed by the key: `\x1b` + key
//!
//! # Terminal Mode Effects
//!
//! The [`TermMode`] flags affect key sequences:
//!
//! - **APP_CURSOR**: Changes arrow key sequences from CSI to SS3 format
//!
//! # Example
//!
//! ```
//! use gpui::Keystroke;
//! use alacritty_terminal::term::TermMode;
//! use gpui_terminal::input::keystroke_to_bytes;
//!
//! // Enter key
//! let keystroke = Keystroke::parse("enter").unwrap();
//! assert_eq!(keystroke_to_bytes(&keystroke, TermMode::empty()), Some(b"\r".to_vec()));
//!
//! // Ctrl+C (interrupt)
//! let keystroke = Keystroke::parse("ctrl-c").unwrap();
//! assert_eq!(keystroke_to_bytes(&keystroke, TermMode::empty()), Some(vec![0x03]));
//! ```

use alacritty_terminal::term::TermMode;
use gpui::Keystroke;

// macOS supplies key_char for Enter and Tab as well as printable text.
pub(crate) fn uses_text_input(key: &Keystroke) -> bool {
    !key.modifiers.control
        && !key.modifiers.platform
        && !key.modifiers.function
        && !agy_meta_shortcut(key)
        && (key.key_char.as_ref().is_some_and(|text| !text.chars().any(char::is_control)
            && (!key.modifiers.alt || cfg!(target_os = "macos") || !text.is_ascii()))
            // A dead Option key can have no committed text yet. Let AppKit's
            // input context start composition instead of manufacturing ESC+key.
            || (cfg!(target_os = "macos") && key.modifiers.alt && key.key.chars().count() == 1))
}

fn agy_meta_shortcut(key: &Keystroke) -> bool {
    key.key == "m"
        && key.modifiers.alt
        && !key.modifiers.shift
        && !key.modifiers.control
        && !key.modifiers.platform
        && !key.modifiers.function
}

pub(crate) fn key_input_bytes(key: &Keystroke, mode: TermMode, composing: bool) -> Option<Vec<u8>> {
    // Pinned GPUI 0.2 has no prefer_character_input field. Its AppKit backend
    // checks marked_text_range before dispatching KeyDown (mac/window.rs).
    // Keep that native composition ownership here even for physical shortcuts.
    if composing || uses_text_input(key) {
        None
    } else {
        keystroke_to_bytes(key, mode)
    }
}

/// Convert a GPUI keystroke to terminal escape sequence bytes.
///
/// This function translates GPUI keyboard events into the appropriate byte sequences
/// expected by terminal applications. It handles special keys, control characters,
/// and application cursor mode.
///
/// # Arguments
///
/// * `keystroke` - The GPUI keystroke to convert
/// * `mode` - The current terminal mode (affects arrow key sequences)
///
/// # Returns
///
/// An optional vector of bytes representing the terminal escape sequence.
/// Returns `None` if the keystroke should not produce any output.
///
/// # Examples
///
/// ```
/// use gpui::Keystroke;
/// use alacritty_terminal::term::TermMode;
/// use gpui_terminal::input::keystroke_to_bytes;
///
/// let keystroke = Keystroke::parse("enter").unwrap();
/// let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
/// assert_eq!(bytes, Some(b"\r".to_vec()));
/// ```
// Key and modifier behavior follows Herdr App's Apache-2.0 terminal encoder
// (origin/master 321c9a84). Adapted for Alacritty and legacy HERDR's ANSI stream;
// Preserve native Option text; only AGY's requested Alt+M uses physical Meta.
pub fn keystroke_to_bytes(keystroke: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    let modifiers = keystroke.modifiers;
    if modifiers.platform {
        return None;
    }
    // Herdr App key_bytes consumes Option when it produced native text,
    // including ASCII layout symbols. Do not turn composed text into Meta.
    if modifiers.alt
        && !modifiers.control
        && !agy_meta_shortcut(keystroke)
        && let Some(text) = keystroke.key_char.as_ref().filter(|text| !text.is_empty())
        && (cfg!(target_os = "macos") || !text.is_ascii())
        && !text.chars().any(char::is_control)
    {
        return Some(text.as_bytes().to_vec());
    }
    let key = keystroke.key.as_str();
    let modifier = 1
        + u8::from(modifiers.shift)
        + 2 * u8::from(modifiers.alt)
        + 4 * u8::from(modifiers.control);
    let cursor = match key {
        "up" => Some('A'),
        "down" => Some('B'),
        "right" => Some('C'),
        "left" => Some('D'),
        "home" => Some('H'),
        "end" => Some('F'),
        _ => None,
    };
    if let Some(code) = cursor {
        return Some(if modifier != 1 {
            format!("\x1b[1;{modifier}{code}").into_bytes()
        } else if mode.contains(TermMode::APP_CURSOR) {
            format!("\x1bO{code}").into_bytes()
        } else {
            format!("\x1b[{code}").into_bytes()
        });
    }
    let function = match key {
        "insert" => Some(2),
        "delete" => Some(3),
        "pageup" => Some(5),
        "pagedown" => Some(6),
        "f5" => Some(15),
        "f6" => Some(17),
        "f7" => Some(18),
        "f8" => Some(19),
        "f9" => Some(20),
        "f10" => Some(21),
        "f11" => Some(23),
        "f12" => Some(24),
        _ => None,
    };
    if let Some(code) = function {
        return Some(
            if modifier == 1 {
                format!("\x1b[{code}~")
            } else {
                format!("\x1b[{code};{modifier}~")
            }
            .into_bytes(),
        );
    }
    if let Some(code) = match key {
        "f1" => Some('P'),
        "f2" => Some('Q'),
        "f3" => Some('R'),
        "f4" => Some('S'),
        _ => None,
    } {
        return Some(
            if modifier == 1 {
                format!("\x1bO{code}")
            } else {
                format!("\x1b[1;{modifier}{code}")
            }
            .into_bytes(),
        );
    }
    // Ghostty distinguishes Shift+Enter even without the Kitty protocol.
    if key == "enter" && modifiers.shift {
        return Some(format!("\x1b[27;{modifier};13~").into_bytes());
    }
    let mut bytes = match key {
        "enter" => b"\r".to_vec(),
        "escape" => b"\x1b".to_vec(),
        "backspace" => {
            if modifiers.control {
                vec![0x08]
            } else {
                vec![0x7f]
            }
        }
        "tab" if modifiers.shift => b"\x1b[Z".to_vec(),
        "tab" => b"\t".to_vec(),
        "space" if modifiers.control => vec![0],
        "space" => vec![b' '],
        _ if modifiers.control => {
            let mut chars = key.chars();
            let (Some(ch), None) = (chars.next(), chars.next()) else {
                return None;
            };
            let code = match ch {
                'a'..='z' | 'A'..='Z' => ch.to_ascii_uppercase() as u8 - b'@',
                '@' | '2' => 0,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '-' | '/' | '7' => 0x1f,
                '?' | '8' => 0x7f,
                _ => return None,
            };
            vec![code]
        }
        _ if modifiers.alt => {
            // Native text has already been preserved above. This is a physical
            // Meta shortcut or a non-text Alt chord, not an Option composition.
            let mut chars = key.chars();
            let (Some(ch), None) = (chars.next(), chars.next()) else {
                return None;
            };
            let ch = if modifiers.shift {
                ch.to_ascii_uppercase()
            } else {
                ch
            };
            ch.to_string().into_bytes()
        }
        _ => {
            if let Some(text) = &keystroke.key_char {
                text.as_bytes().to_vec()
            } else {
                let mut chars = key.chars();
                let (Some(ch), None) = (chars.next(), chars.next()) else {
                    return None;
                };
                let ch = if modifiers.shift {
                    ch.to_ascii_uppercase()
                } else {
                    ch
                };
                ch.to_string().into_bytes()
            }
        }
    };
    if modifiers.alt {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

/// Text committed by an input method may include macOS's newline for Return.
/// Terminal Return is CR; multiline paste is encoded separately as a paste.
pub(crate) fn committed_text_bytes(text: &str) -> Vec<u8> {
    text.replace("\r\n", "\n").replace('\n', "\r").into_bytes()
}

pub(crate) fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    if bracketed {
        format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', "")).into_bytes()
    } else {
        text.replace('\n', "\r").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enter_key() {
        let keystroke = Keystroke::parse("enter").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\r".to_vec()));
    }

    #[test]
    fn test_escape_key() {
        let keystroke = Keystroke::parse("escape").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\x1b".to_vec()));
    }

    #[test]
    fn test_backspace_key() {
        let keystroke = Keystroke::parse("backspace").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\x7f".to_vec()));
    }

    #[test]
    fn test_tab_key() {
        let keystroke = Keystroke::parse("tab").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\t".to_vec()));
    }

    #[test]
    fn test_shift_tab() {
        let keystroke = Keystroke::parse("shift-tab").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn test_arrow_keys_normal_mode() {
        let mode = TermMode::empty();

        let up = Keystroke::parse("up").unwrap();
        assert_eq!(keystroke_to_bytes(&up, mode), Some(b"\x1b[A".to_vec()));

        let down = Keystroke::parse("down").unwrap();
        assert_eq!(keystroke_to_bytes(&down, mode), Some(b"\x1b[B".to_vec()));

        let right = Keystroke::parse("right").unwrap();
        assert_eq!(keystroke_to_bytes(&right, mode), Some(b"\x1b[C".to_vec()));

        let left = Keystroke::parse("left").unwrap();
        assert_eq!(keystroke_to_bytes(&left, mode), Some(b"\x1b[D".to_vec()));
    }

    #[test]
    fn test_arrow_keys_app_cursor_mode() {
        let mode = TermMode::APP_CURSOR;

        let up = Keystroke::parse("up").unwrap();
        assert_eq!(keystroke_to_bytes(&up, mode), Some(b"\x1bOA".to_vec()));

        let down = Keystroke::parse("down").unwrap();
        assert_eq!(keystroke_to_bytes(&down, mode), Some(b"\x1bOB".to_vec()));

        let right = Keystroke::parse("right").unwrap();
        assert_eq!(keystroke_to_bytes(&right, mode), Some(b"\x1bOC".to_vec()));

        let left = Keystroke::parse("left").unwrap();
        assert_eq!(keystroke_to_bytes(&left, mode), Some(b"\x1bOD".to_vec()));
    }

    #[test]
    fn test_navigation_keys() {
        let mode = TermMode::empty();

        let home = Keystroke::parse("home").unwrap();
        assert_eq!(keystroke_to_bytes(&home, mode), Some(b"\x1b[H".to_vec()));

        let end = Keystroke::parse("end").unwrap();
        assert_eq!(keystroke_to_bytes(&end, mode), Some(b"\x1b[F".to_vec()));

        let pageup = Keystroke::parse("pageup").unwrap();
        assert_eq!(keystroke_to_bytes(&pageup, mode), Some(b"\x1b[5~".to_vec()));

        let pagedown = Keystroke::parse("pagedown").unwrap();
        assert_eq!(
            keystroke_to_bytes(&pagedown, mode),
            Some(b"\x1b[6~".to_vec())
        );

        let insert = Keystroke::parse("insert").unwrap();
        assert_eq!(keystroke_to_bytes(&insert, mode), Some(b"\x1b[2~".to_vec()));

        let delete = Keystroke::parse("delete").unwrap();
        assert_eq!(keystroke_to_bytes(&delete, mode), Some(b"\x1b[3~".to_vec()));
    }

    #[test]
    fn test_function_keys() {
        let mode = TermMode::empty();

        let f1 = Keystroke::parse("f1").unwrap();
        assert_eq!(keystroke_to_bytes(&f1, mode), Some(b"\x1bOP".to_vec()));

        let f2 = Keystroke::parse("f2").unwrap();
        assert_eq!(keystroke_to_bytes(&f2, mode), Some(b"\x1bOQ".to_vec()));

        let f5 = Keystroke::parse("f5").unwrap();
        assert_eq!(keystroke_to_bytes(&f5, mode), Some(b"\x1b[15~".to_vec()));

        let f12 = Keystroke::parse("f12").unwrap();
        assert_eq!(keystroke_to_bytes(&f12, mode), Some(b"\x1b[24~".to_vec()));
    }

    #[test]
    fn test_ctrl_combinations() {
        let mode = TermMode::empty();

        // Ctrl+A = 0x01
        let ctrl_a = Keystroke::parse("ctrl-a").unwrap();
        assert_eq!(keystroke_to_bytes(&ctrl_a, mode), Some(vec![0x01]));

        // Ctrl+C = 0x03
        let ctrl_c = Keystroke::parse("ctrl-c").unwrap();
        assert_eq!(keystroke_to_bytes(&ctrl_c, mode), Some(vec![0x03]));

        // Ctrl+Z = 0x1a
        let ctrl_z = Keystroke::parse("ctrl-z").unwrap();
        assert_eq!(keystroke_to_bytes(&ctrl_z, mode), Some(vec![0x1a]));

        // Ctrl+Space = 0x00
        let ctrl_space = Keystroke::parse("ctrl-space").unwrap();
        assert_eq!(keystroke_to_bytes(&ctrl_space, mode), Some(vec![0x00]));
    }

    #[test]
    fn test_alt_combinations() {
        let mode = TermMode::empty();

        // Alt+a sends ESC followed by 'a'
        let alt_a = Keystroke::parse("alt-a").unwrap();
        assert_eq!(keystroke_to_bytes(&alt_a, mode), Some(b"\x1ba".to_vec()));

        // Alt+x sends ESC followed by 'x'
        let alt_x = Keystroke::parse("alt-x").unwrap();
        assert_eq!(keystroke_to_bytes(&alt_x, mode), Some(b"\x1bx".to_vec()));
    }

    #[test]
    fn test_regular_characters() {
        let mode = TermMode::empty();

        let a = Keystroke::parse("a").unwrap();
        assert_eq!(keystroke_to_bytes(&a, mode), Some(b"a".to_vec()));

        let z = Keystroke::parse("z").unwrap();
        assert_eq!(keystroke_to_bytes(&z, mode), Some(b"z".to_vec()));

        let zero = Keystroke::parse("0").unwrap();
        assert_eq!(keystroke_to_bytes(&zero, mode), Some(b"0".to_vec()));
    }

    #[test]
    fn test_space_key() {
        let mode = TermMode::empty();

        let space = Keystroke::parse("space").unwrap();
        assert_eq!(keystroke_to_bytes(&space, mode), Some(b" ".to_vec()));
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;

    #[test]
    fn macos_enter_and_tab_are_terminal_keys_while_printable_text_uses_ime() {
        for (name, text, bytes) in [
            ("enter", "\n", b"\r".as_slice()),
            ("tab", "\t", b"\t".as_slice()),
        ] {
            let mut key = Keystroke::parse(name).unwrap();
            key.key_char = Some(text.into());
            assert!(!uses_text_input(&key));
            assert_eq!(
                keystroke_to_bytes(&key, TermMode::empty()).as_deref(),
                Some(bytes)
            );
        }
        let mut key = Keystroke::parse("a").unwrap();
        key.key_char = Some("a".into());
        assert!(uses_text_input(&key));
        key.key_char = Some("中".into());
        assert!(uses_text_input(&key));
    }

    #[test]
    fn native_meta_and_control_chords_keep_the_physical_key_and_modifiers() {
        let mut option_m = Keystroke::parse("alt-m").unwrap();
        option_m.key_char = Some("µ".into());
        assert!(!uses_text_input(&option_m));
        assert_eq!(
            keystroke_to_bytes(&option_m, TermMode::empty()).unwrap(),
            b"\x1bm"
        );
        for (key, expected) in [
            ("alt-shift-m", b"\x1bM".as_slice()),
            ("ctrl-alt-m", b"\x1b\r".as_slice()),
            ("alt-enter", b"\x1b\r".as_slice()),
            ("ctrl-left", b"\x1b[1;5D".as_slice()),
            ("alt-right", b"\x1b[1;3C".as_slice()),
            ("shift-enter", b"\x1b[27;2;13~".as_slice()),
            ("ctrl--", b"\x1f".as_slice()),
        ] {
            assert_eq!(
                keystroke_to_bytes(&Keystroke::parse(key).unwrap(), TermMode::empty()).unwrap(),
                expected,
                "{key}"
            );
        }
        assert!(
            keystroke_to_bytes(&Keystroke::parse("cmd-m").unwrap(), TermMode::empty()).is_none()
        );
    }

    #[test]
    fn option_symbols_and_dead_key_composition_keep_native_text_ownership() {
        for (chord, text) in [
            ("alt-e", "´"),
            ("alt-p", "π"),
            ("alt-8", "{"),
            ("alt-shift-2", "@"),
            ("alt-shift-m", "Â"),
        ] {
            let mut key = Keystroke::parse(chord).unwrap();
            key.key_char = Some(text.into());
            let native_text = cfg!(target_os = "macos") || !text.is_ascii();
            assert_eq!(uses_text_input(&key), native_text, "{chord}");
            assert_eq!(
                key_input_bytes(&key, TermMode::empty(), false).is_none(),
                native_text,
                "native text ownership for {chord}"
            );
            if native_text {
                assert_eq!(
                    keystroke_to_bytes(&key, TermMode::empty()).unwrap(),
                    text.as_bytes()
                );
            }
        }
        if cfg!(target_os = "macos") {
            let dead = Keystroke::parse("alt-e").unwrap();
            assert!(
                key_input_bytes(&dead, TermMode::empty(), false).is_none(),
                "no committed dead-key text yet"
            );
        }
        let mut shortcut = Keystroke::parse("alt-m").unwrap();
        shortcut.key_char = Some("µ".into());
        assert_eq!(
            key_input_bytes(&shortcut, TermMode::empty(), false).unwrap(),
            b"\x1bm"
        );
        assert!(
            key_input_bytes(&shortcut, TermMode::empty(), true).is_none(),
            "marked text owns even Alt+M"
        );
        assert!(
            key_input_bytes(&Keystroke::parse("enter").unwrap(), TermMode::empty(), true).is_none(),
            "Return confirms IME before it submits terminal input"
        );
        assert_eq!(committed_text_bytes("é"), "é".as_bytes());
    }

    #[test]
    fn committed_return_and_multiline_paste_keep_terminal_line_discipline() {
        assert_eq!(committed_text_bytes("中文\n"), "中文\r".as_bytes());
        assert_eq!(paste_bytes("first\r\nsecond\n", false), b"first\rsecond\r");
        assert_eq!(
            paste_bytes("first\r\nsecond\x1b[201~", true),
            b"\x1b[200~first\nsecond[201~\x1b[201~"
        );
    }
}
