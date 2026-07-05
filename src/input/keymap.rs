//! CU key-spec parsing and keysym resolution.
//!
//! Turns strings like `ctrl+shift+tab` into modifier keysyms plus a base key,
//! and maps CU key-names / literal chars to X11 keysym *values*. Unlike the
//! X11 bridge, GNOME injection goes through the portal's
//! `notify_keyboard_keysym` call, which takes a keysym value directly, so there
//! is no keycode lookup or spare-keycode remapping here.
//!
//! The name table below is extracted from `_mapKey` in
//! `js/cu_linux_executor.js` (the ydotool/xdotool-era mapping) and mirrors the
//! x11-bridge `keymap.rs` so all three bridges accept the same key specs.
//!
//! Key-spec grammar (matches the JS `combo.split("+")`):
//!   spec        := token ("+" token)*
//!   token       := modifier | keyname
//!   modifier    := "ctrl" | "control" | "alt" | "shift"
//!                | "super" | "meta" | "cmd" | "command"   (cmd/meta/command -> super)
//!   keyname     := one of the CU_KEY_NAMES table entries, OR a single literal
//!                  character (e.g. "a", "1", "/") passed through verbatim.
//! Matching is case-insensitive; the base key is the last non-modifier token.

use anyhow::{Result, bail};
use xkeysym::Keysym as XKeysym;

/// A parsed key spec: modifier keysym-names plus the base key name.
#[derive(Debug, Clone)]
pub struct KeySpec {
    pub modifiers: Vec<String>,
    pub base: String,
}

/// Modifier tokens (lowercased) recognized at the front of a key spec.
/// `cmd`/`command`/`meta` normalize to `super`.
pub const MODIFIER_TOKENS: &[&str] = &[
    "ctrl", "control", "alt", "shift", "super", "meta", "cmd", "command",
];

/// CU key-name -> X11 keysym-name table, extracted from `_mapKey`.
/// Underscore forms (`page_up`, `page_down`) are included as aliases.
pub const CU_KEY_NAMES: &[(&str, &str)] = &[
    ("enter", "Return"),
    ("return", "Return"),
    ("backspace", "BackSpace"),
    ("delete", "Delete"),
    ("escape", "Escape"),
    ("esc", "Escape"),
    ("tab", "Tab"),
    ("space", "space"),
    ("up", "Up"),
    ("down", "Down"),
    ("left", "Left"),
    ("right", "Right"),
    ("home", "Home"),
    ("end", "End"),
    ("pageup", "Prior"),
    ("page_up", "Prior"),
    ("pagedown", "Next"),
    ("page_down", "Next"),
    ("capslock", "Caps_Lock"),
];

/// X11 keysym-name -> keysym value table.
///
/// Covers the CU table's target names plus the modifier keysyms and common
/// extra names (function keys, navigation) a raw keysym-name passthrough might
/// carry. Names are matched case-sensitively (X keysym names are, e.g. `Return`
/// vs `return`).
const KEYSYM_NAMES: &[(&str, u32)] = &[
    // Modifiers (resolved as base keys too, e.g. hold-key --key ctrl).
    ("Control_L", xkeysym::key::Control_L),
    ("Control_R", xkeysym::key::Control_R),
    ("Shift_L", xkeysym::key::Shift_L),
    ("Shift_R", xkeysym::key::Shift_R),
    ("Alt_L", xkeysym::key::Alt_L),
    ("Alt_R", xkeysym::key::Alt_R),
    ("Super_L", xkeysym::key::Super_L),
    ("Super_R", xkeysym::key::Super_R),
    ("Meta_L", xkeysym::key::Meta_L),
    ("Meta_R", xkeysym::key::Meta_R),
    ("Caps_Lock", xkeysym::key::Caps_Lock),
    ("Num_Lock", xkeysym::key::Num_Lock),
    // CU named keys.
    ("Return", xkeysym::key::Return),
    ("BackSpace", xkeysym::key::BackSpace),
    ("Delete", xkeysym::key::Delete),
    ("Escape", xkeysym::key::Escape),
    ("Tab", xkeysym::key::Tab),
    ("space", xkeysym::key::space),
    ("Up", xkeysym::key::Up),
    ("Down", xkeysym::key::Down),
    ("Left", xkeysym::key::Left),
    ("Right", xkeysym::key::Right),
    ("Home", xkeysym::key::Home),
    ("End", xkeysym::key::End),
    ("Prior", xkeysym::key::Prior),
    ("Next", xkeysym::key::Next),
    ("Insert", xkeysym::key::Insert),
    ("Menu", xkeysym::key::Menu),
    ("Print", xkeysym::key::Print),
    ("Pause", xkeysym::key::Pause),
    // Function keys F1..F24.
    ("F1", xkeysym::key::F1),
    ("F2", xkeysym::key::F2),
    ("F3", xkeysym::key::F3),
    ("F4", xkeysym::key::F4),
    ("F5", xkeysym::key::F5),
    ("F6", xkeysym::key::F6),
    ("F7", xkeysym::key::F7),
    ("F8", xkeysym::key::F8),
    ("F9", xkeysym::key::F9),
    ("F10", xkeysym::key::F10),
    ("F11", xkeysym::key::F11),
    ("F12", xkeysym::key::F12),
    ("F13", xkeysym::key::F13),
    ("F14", xkeysym::key::F14),
    ("F15", xkeysym::key::F15),
    ("F16", xkeysym::key::F16),
    ("F17", xkeysym::key::F17),
    ("F18", xkeysym::key::F18),
    ("F19", xkeysym::key::F19),
    ("F20", xkeysym::key::F20),
    ("F21", xkeysym::key::F21),
    ("F22", xkeysym::key::F22),
    ("F23", xkeysym::key::F23),
    ("F24", xkeysym::key::F24),
];

/// Normalize a token for CU-table / modifier lookup: trim, lowercase, and drop
/// underscores (so `page_down` == `pagedown`).
fn normalize_token(token: &str) -> String {
    token
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|&c| c != '_')
        .collect()
}

/// True if `token` names a modifier. `cmd`/`command`/`meta`/`super` all count.
pub fn is_modifier(token: &str) -> bool {
    MODIFIER_TOKENS.contains(&normalize_token(token).as_str())
}

/// Canonical X11 keysym-name for a modifier token (`ctrl` -> `Control_L`, ...).
fn modifier_keysym_name(token: &str) -> Option<&'static str> {
    match normalize_token(token).as_str() {
        "ctrl" | "control" => Some("Control_L"),
        "alt" => Some("Alt_L"),
        "shift" => Some("Shift_L"),
        "super" | "meta" | "cmd" | "command" => Some("Super_L"),
        _ => None,
    }
}

/// Parse a CU key spec (`ctrl+shift+tab`) into modifiers + base key.
///
/// Tokens are split on `+`, trimmed, and empty tokens dropped. All-but-last
/// modifier tokens become modifiers; the last non-modifier token is the base. A
/// spec that is *all* modifiers (e.g. `ctrl`) uses its last token as the base.
pub fn parse_spec(spec: &str) -> Result<KeySpec> {
    let tokens: Vec<&str> = spec
        .split('+')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .collect();

    if tokens.is_empty() {
        bail!("key spec is empty");
    }

    let base_idx = tokens
        .iter()
        .rposition(|token| !is_modifier(token))
        .unwrap_or(tokens.len() - 1);

    let mut modifiers = Vec::new();
    for (idx, token) in tokens.iter().enumerate() {
        if idx == base_idx {
            continue;
        }
        if !is_modifier(token) {
            bail!("unexpected non-modifier token `{token}` before base key in `{spec}`");
        }
        modifiers.push((*token).to_owned());
    }

    Ok(KeySpec {
        modifiers,
        base: tokens[base_idx].to_owned(),
    })
}

/// Resolve a CU key-name (or literal char) to its X11 keysym-name.
///
/// Mirrors the JS `_mapKey`: known CU tokens map to their keysym name;
/// modifiers map to their `*_L` keysym name; single characters pass through;
/// anything else is treated as a raw X11 keysym name.
pub fn keysym_name(name: &str) -> Result<String> {
    if let Some(modifier) = modifier_keysym_name(name) {
        return Ok(modifier.to_owned());
    }

    let normalized = normalize_token(name);
    if let Some((_, keysym_name)) = CU_KEY_NAMES.iter().find(|(cu, _)| *cu == normalized) {
        return Ok((*keysym_name).to_owned());
    }

    let trimmed = name.trim();
    if trimmed.chars().count() == 1 {
        return Ok(trimmed.to_owned());
    }

    Ok(trimmed.to_owned())
}

/// Resolve a key-spec token (CU name / modifier / literal char / raw keysym
/// name) to an X11 keysym *value*.
pub fn keysym_for_name(name: &str) -> Result<u32> {
    let resolved = keysym_name(name)?;

    // Exact keysym-name table (case-sensitive: X keysym names are).
    if let Some((_, value)) = KEYSYM_NAMES.iter().find(|(n, _)| *n == resolved) {
        return Ok(*value);
    }

    // A single char: resolve via xkeysym's Unicode -> keysym mapping.
    if resolved.chars().count() == 1 {
        let ch = resolved.chars().next().expect("count == 1");
        return char_to_keysym(ch);
    }

    bail!("unsupported key name `{name}` (resolved keysym name `{resolved}`)")
}

/// Resolve a single character to an X11 keysym value.
///
/// ASCII/Latin-1 characters and control characters map to their canonical
/// keysyms; everything else in the Unicode range uses the Unicode-plane keysym
/// encoding (`0x0100_0000 | codepoint`) that the portal understands. This
/// matches the kwin-portal-bridge `char_to_keysym` so `type` behaves the same
/// across bridges.
pub fn char_to_keysym(ch: char) -> Result<u32> {
    let codepoint = u32::from(ch);
    let keysym = match ch {
        '\u{8}' => 0xFF08,     // BackSpace
        '\t' => 0xFF09,        // Tab
        '\n' | '\r' => 0xFF0D, // Return
        '\u{1B}' => 0xFF1B,    // Escape
        '\u{7F}' => 0xFFFF,    // Delete
        _ if (0x20..=0x7E).contains(&codepoint) || (0xA0..=0xFF).contains(&codepoint) => codepoint,
        _ if codepoint <= 0x10_FFFF => {
            // Prefer a named keysym if xkeysym knows one, else Unicode plane.
            let ks = XKeysym::from_char(ch);
            if ks != XKeysym::NoSymbol {
                ks.raw()
            } else {
                0x0100_0000 | codepoint
            }
        }
        _ => bail!("character {ch:?} is outside the supported Unicode range"),
    };

    Ok(keysym)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifier_chord() {
        let spec = parse_spec("ctrl+shift+tab").unwrap();
        assert_eq!(spec.modifiers, vec!["ctrl", "shift"]);
        assert_eq!(spec.base, "tab");
    }

    #[test]
    fn parses_cmd_as_super_base_last() {
        let spec = parse_spec("cmd+a").unwrap();
        assert_eq!(spec.modifiers, vec!["cmd"]);
        assert_eq!(spec.base, "a");
    }

    #[test]
    fn parses_single_key() {
        let spec = parse_spec("return").unwrap();
        assert!(spec.modifiers.is_empty());
        assert_eq!(spec.base, "return");
    }

    #[test]
    fn parses_bare_modifier_as_base() {
        let spec = parse_spec("ctrl").unwrap();
        assert!(spec.modifiers.is_empty());
        assert_eq!(spec.base, "ctrl");
    }

    #[test]
    fn empty_spec_errors() {
        assert!(parse_spec("").is_err());
        assert!(parse_spec("+").is_err());
    }

    #[test]
    fn keysym_names_match_cu_table() {
        assert_eq!(keysym_name("return").unwrap(), "Return");
        assert_eq!(keysym_name("Enter").unwrap(), "Return");
        assert_eq!(keysym_name("escape").unwrap(), "Escape");
        assert_eq!(keysym_name("esc").unwrap(), "Escape");
        assert_eq!(keysym_name("page_down").unwrap(), "Next");
        assert_eq!(keysym_name("pagedown").unwrap(), "Next");
        assert_eq!(keysym_name("page_up").unwrap(), "Prior");
        assert_eq!(keysym_name("space").unwrap(), "space");
        assert_eq!(keysym_name("capslock").unwrap(), "Caps_Lock");
    }

    #[test]
    fn keysym_names_map_modifiers() {
        assert_eq!(keysym_name("ctrl").unwrap(), "Control_L");
        assert_eq!(keysym_name("control").unwrap(), "Control_L");
        assert_eq!(keysym_name("cmd").unwrap(), "Super_L");
        assert_eq!(keysym_name("command").unwrap(), "Super_L");
        assert_eq!(keysym_name("meta").unwrap(), "Super_L");
        assert_eq!(keysym_name("super").unwrap(), "Super_L");
        assert_eq!(keysym_name("alt").unwrap(), "Alt_L");
        assert_eq!(keysym_name("shift").unwrap(), "Shift_L");
    }

    #[test]
    fn keysym_names_pass_through_literals() {
        assert_eq!(keysym_name("a").unwrap(), "a");
        assert_eq!(keysym_name("/").unwrap(), "/");
        assert_eq!(keysym_name("F5").unwrap(), "F5");
    }

    #[test]
    fn keysym_values_resolve() {
        assert_eq!(keysym_for_name("return").unwrap(), xkeysym::key::Return);
        assert_eq!(keysym_for_name("tab").unwrap(), xkeysym::key::Tab);
        assert_eq!(keysym_for_name("F5").unwrap(), xkeysym::key::F5);
        assert_eq!(keysym_for_name("ctrl").unwrap(), xkeysym::key::Control_L);
        assert_eq!(keysym_for_name("a").unwrap(), u32::from('a'));
        assert_eq!(keysym_for_name("A").unwrap(), u32::from('A'));
        // Exotic glyph resolves to a keysym via xkeysym (é -> eacute 0xe9).
        assert_eq!(keysym_for_name("é").unwrap(), 0x0e9);
    }

    #[test]
    fn char_to_keysym_maps_ascii_and_control() {
        assert_eq!(char_to_keysym('a').unwrap(), u32::from('a'));
        assert_eq!(char_to_keysym(' ').unwrap(), u32::from(' '));
        assert_eq!(char_to_keysym('\n').unwrap(), 0xFF0D);
        assert_eq!(char_to_keysym('\t').unwrap(), 0xFF09);
        // Latin-1 supplement passes through as its codepoint.
        assert_eq!(char_to_keysym('é').unwrap(), 0xE9);
    }

    #[test]
    fn char_to_keysym_uses_unicode_plane_for_astral() {
        // An emoji has no named keysym: falls back to the Unicode plane.
        let ks = char_to_keysym('😀').unwrap();
        assert_eq!(ks, 0x0100_0000 | 0x1F600);
    }

    #[test]
    fn is_modifier_recognizes_aliases() {
        for token in [
            "ctrl", "Control", "ALT", "shift", "super", "meta", "cmd", "command",
        ] {
            assert!(is_modifier(token), "{token} should be a modifier");
        }
        assert!(!is_modifier("a"));
        assert!(!is_modifier("tab"));
    }
}
