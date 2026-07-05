//! Input synthesis: key-spec parsing/keysym resolution plus the pointer/keyboard
//! action result JSON shapes. The actual portal `notify_*` calls live in
//! `portal.rs` (they need the live RemoteDesktop session); this module owns the
//! CU-contract parsing and the serializable action results.

pub mod keymap;

use serde::Serialize;

/// A mouse button, mapped to its evdev button code for the portal.
///
/// The XDG RemoteDesktop portal takes Linux evdev button codes:
/// left = BTN_LEFT (272), right = BTN_RIGHT (273), middle = BTN_MIDDLE (274).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    /// Parse a CU button name (`left`/`right`/`middle`, case-insensitive).
    pub fn parse(name: &str) -> anyhow::Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "left" | "1" => Ok(Self::Left),
            "right" | "3" => Ok(Self::Right),
            "middle" | "2" => Ok(Self::Middle),
            other => anyhow::bail!("unsupported mouse button `{other}`"),
        }
    }

    /// The evdev button code the portal expects.
    pub fn evdev_code(self) -> i32 {
        match self {
            Self::Left => 272,
            Self::Right => 273,
            Self::Middle => 274,
        }
    }
}

/// Result of a pointer action (move/click/scroll). JSON body is ignored by the
/// JS (it only checks exit code), but emitted for parity and CLI debugging.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PointerActionResult {
    pub action: String,
    pub x: i32,
    pub y: i32,
}

/// Result of a drag action.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DragActionResult {
    pub action: String,
    pub from_x: i32,
    pub from_y: i32,
    pub to_x: i32,
    pub to_y: i32,
}

/// Result of a held-button state change (`left-mouse-down` / `-up`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ButtonStateResult {
    pub action: String,
    pub button: String,
    pub is_held: bool,
}

/// Result of a keyboard action (key-sequence / hold-key).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyboardActionResult {
    pub action: String,
    pub keys: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// Result of a `type` action.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeActionResult {
    pub action: String,
    pub text: String,
    pub char_count: usize,
}
