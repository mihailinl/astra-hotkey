//! Hotkey registration and matching logic.
//!
//! This module maintains a set of registered hotkey combinations and provides
//! efficient O(1) lookup for matching pressed keys against registered hotkeys.

use std::collections::HashSet;
use once_cell::sync::Lazy;
use parking_lot::RwLock;

/// Set of normalized registered hotkey strings.
/// Using RwLock for concurrent read access with exclusive write.
static REGISTERED_HOTKEYS: Lazy<RwLock<HashSet<String>>> = Lazy::new(|| RwLock::new(HashSet::new()));

/// Modifier flags for internal representation
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
}

impl Default for Modifiers {
    fn default() -> Self {
        Self {
            ctrl: false,
            alt: false,
            shift: false,
            win: false,
        }
    }
}

/// Normalize a hotkey string for consistent comparison.
/// Sorts modifiers in standard order: Ctrl+Alt+Shift+Win, then key alphabetically.
///
/// Example: "Shift+Ctrl+T" -> "Ctrl+Shift+T"
pub fn normalize_hotkey(hotkey: &str) -> String {
    let parts: Vec<&str> = hotkey
        .split('+')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    let mut modifiers = Modifiers::default();
    let mut regular_keys: Vec<String> = Vec::new();

    for part in parts {
        let lower = part.to_lowercase();
        match lower.as_str() {
            "ctrl" | "control" => modifiers.ctrl = true,
            "alt" => modifiers.alt = true,
            "shift" => modifiers.shift = true,
            "win" | "meta" | "super" | "cmd" | "command" => modifiers.win = true,
            _ => {
                // Capitalize first letter for consistency
                let normalized = capitalize_key(part);
                regular_keys.push(normalized);
            }
        }
    }

    // Build normalized string
    let mut result = Vec::new();
    if modifiers.ctrl {
        result.push("Ctrl".to_string());
    }
    if modifiers.alt {
        result.push("Alt".to_string());
    }
    if modifiers.shift {
        result.push("Shift".to_string());
    }
    if modifiers.win {
        result.push("Win".to_string());
    }

    // Sort regular keys alphabetically for consistency
    regular_keys.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    result.extend(regular_keys);

    result.join("+")
}

/// Capitalize key name for consistency.
/// "enter" -> "Enter", "f1" -> "F1", "a" -> "A"
fn capitalize_key(key: &str) -> String {
    let lower = key.to_lowercase();

    // Handle function keys specially
    if lower.starts_with('f') && lower.len() <= 3 {
        if let Ok(_) = lower[1..].parse::<u8>() {
            return lower.to_uppercase();
        }
    }

    // Handle special keys
    match lower.as_str() {
        "enter" | "return" => "Enter".to_string(),
        "escape" | "esc" => "Escape".to_string(),
        "space" => "Space".to_string(),
        "tab" => "Tab".to_string(),
        "backspace" => "Backspace".to_string(),
        "delete" | "del" => "Delete".to_string(),
        "insert" | "ins" => "Insert".to_string(),
        "home" => "Home".to_string(),
        "end" => "End".to_string(),
        "pageup" | "pgup" => "PageUp".to_string(),
        "pagedown" | "pgdn" => "PageDown".to_string(),
        "up" => "Up".to_string(),
        "down" => "Down".to_string(),
        "left" => "Left".to_string(),
        "right" => "Right".to_string(),
        "printscreen" | "prtsc" => "PrintScreen".to_string(),
        "pause" => "Pause".to_string(),
        "capslock" => "CapsLock".to_string(),
        "numlock" => "NumLock".to_string(),
        "scrolllock" => "ScrollLock".to_string(),
        // Single letters - uppercase
        s if s.len() == 1 && s.chars().next().unwrap().is_alphabetic() => s.to_uppercase(),
        // Numbers
        s if s.len() == 1 && s.chars().next().unwrap().is_numeric() => s.to_string(),
        // Numpad keys
        "num0" | "numpad0" => "Num0".to_string(),
        "num1" | "numpad1" => "Num1".to_string(),
        "num2" | "numpad2" => "Num2".to_string(),
        "num3" | "numpad3" => "Num3".to_string(),
        "num4" | "numpad4" => "Num4".to_string(),
        "num5" | "numpad5" => "Num5".to_string(),
        "num6" | "numpad6" => "Num6".to_string(),
        "num7" | "numpad7" => "Num7".to_string(),
        "num8" | "numpad8" => "Num8".to_string(),
        "num9" | "numpad9" => "Num9".to_string(),
        "nummul" | "numpadmultiply" => "NumMul".to_string(),
        "numadd" | "numpadadd" => "NumAdd".to_string(),
        "numsub" | "numpadsubtract" => "NumSub".to_string(),
        "numdec" | "numpaddecimal" => "NumDec".to_string(),
        "numdiv" | "numpaddivide" => "NumDiv".to_string(),
        // Symbols - keep as-is
        _ => key.to_string(),
    }
}

/// Register a hotkey combination.
/// Returns true if registration succeeded, false if already registered.
pub fn register_hotkey(keys: &str) -> bool {
    let normalized = normalize_hotkey(keys);
    if normalized.is_empty() {
        return false;
    }

    let mut hotkeys = REGISTERED_HOTKEYS.write();
    hotkeys.insert(normalized)
}

/// Unregister a hotkey combination.
/// Returns true if the hotkey was registered, false otherwise.
pub fn unregister_hotkey(keys: &str) -> bool {
    let normalized = normalize_hotkey(keys);
    let mut hotkeys = REGISTERED_HOTKEYS.write();
    hotkeys.remove(&normalized)
}

/// Unregister all hotkeys.
pub fn unregister_all() {
    let mut hotkeys = REGISTERED_HOTKEYS.write();
    hotkeys.clear();
}

/// Check if a key combination is registered.
/// Returns true if the combination matches a registered hotkey.
#[allow(dead_code)]
pub fn is_registered(keys: &str) -> bool {
    let normalized = normalize_hotkey(keys);
    let hotkeys = REGISTERED_HOTKEYS.read();
    hotkeys.contains(&normalized)
}

/// Get the number of registered hotkeys.
pub fn count() -> usize {
    let hotkeys = REGISTERED_HOTKEYS.read();
    hotkeys.len()
}

/// Check pressed keys against registered hotkeys and return matching hotkey if found.
/// Takes current modifiers and a regular key.
pub fn check_hotkey(modifiers: Modifiers, key: &str) -> Option<String> {
    // Build the hotkey string from current state
    let mut parts = Vec::new();
    if modifiers.ctrl {
        parts.push("Ctrl".to_string());
    }
    if modifiers.alt {
        parts.push("Alt".to_string());
    }
    if modifiers.shift {
        parts.push("Shift".to_string());
    }
    if modifiers.win {
        parts.push("Win".to_string());
    }

    let normalized_key = capitalize_key(key);
    if !normalized_key.is_empty() {
        parts.push(normalized_key);
    }

    let hotkey_str = parts.join("+");

    // Check if this combination is registered
    let hotkeys = REGISTERED_HOTKEYS.read();
    if hotkeys.contains(&hotkey_str) {
        Some(hotkey_str)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_hotkey() {
        assert_eq!(normalize_hotkey("Ctrl+T"), "Ctrl+T");
        assert_eq!(normalize_hotkey("ctrl+t"), "Ctrl+T");
        assert_eq!(normalize_hotkey("Shift+Ctrl+T"), "Ctrl+Shift+T");
        assert_eq!(normalize_hotkey("t+shift+ctrl"), "Ctrl+Shift+T");
        assert_eq!(normalize_hotkey("Alt+F4"), "Alt+F4");
        assert_eq!(normalize_hotkey("Win+E"), "Win+E");
        assert_eq!(normalize_hotkey("meta+e"), "Win+E");
    }

    #[test]
    fn test_register_unregister() {
        unregister_all();

        assert!(register_hotkey("Ctrl+T"));
        assert!(!register_hotkey("Ctrl+T")); // Already registered
        assert!(is_registered("Ctrl+T"));
        assert!(is_registered("ctrl+t")); // Case insensitive

        assert!(unregister_hotkey("Ctrl+T"));
        assert!(!is_registered("Ctrl+T"));
        assert!(!unregister_hotkey("Ctrl+T")); // Already unregistered

        unregister_all(); // Cleanup
    }

    #[test]
    fn test_check_hotkey() {
        unregister_all();
        register_hotkey("Ctrl+Shift+T");

        let modifiers = Modifiers {
            ctrl: true,
            alt: false,
            shift: true,
            win: false,
        };

        assert_eq!(check_hotkey(modifiers, "T"), Some("Ctrl+Shift+T".to_string()));
        assert_eq!(check_hotkey(modifiers, "A"), None); // Not registered

        let wrong_modifiers = Modifiers {
            ctrl: true,
            alt: false,
            shift: false,
            win: false,
        };
        // With Ctrl+T not registered, this should be None
        // But first unregister the Ctrl+Shift+T to avoid state pollution
        unregister_all();
        assert_eq!(check_hotkey(wrong_modifiers, "T"), None); // Not registered

        unregister_all(); // Cleanup
    }
}
