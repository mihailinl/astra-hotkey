//! Windows-specific low-level keyboard + mouse hook implementation.
//!
//! Uses SetWindowsHookEx with WH_KEYBOARD_LL to intercept keyboard events
//! and WH_MOUSE_LL for side mouse buttons (right / middle / X1 / X2).
//! Only registered hotkey combinations trigger the callback.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::thread;
use std::ptr;

use once_cell::sync::Lazy;
use parking_lot::Mutex;

use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
    DispatchMessageW, TranslateMessage,
    HHOOK, KBDLLHOOKSTRUCT, MSLLHOOKSTRUCT, MSG,
    WH_KEYBOARD_LL, WH_MOUSE_LL,
    WM_KEYDOWN, WM_SYSKEYDOWN, WM_KEYUP, WM_SYSKEYUP,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_XBUTTONDOWN, WM_XBUTTONUP,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN,
};

use crate::registry::{self, Modifiers};
use crate::invoke_callback;

/// Tracks which VK codes are currently pressed and the matched hotkey string.
/// Populated on key-down when a registered combo matches; consumed on key-up
/// so we can fire the up event even if the user released modifiers first.
static PRESSED_KEYS: Lazy<Mutex<HashMap<u32, String>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Same as `PRESSED_KEYS` but keyed by mouse button id (2..5) for the mouse hook.
static PRESSED_MOUSE: Lazy<Mutex<HashMap<u32, String>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Global keyboard hook handle
static HOOK_HANDLE: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(ptr::null_mut());

/// Global mouse hook handle
static MOUSE_HOOK_HANDLE: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(ptr::null_mut());

/// Flag to signal hook thread to stop
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Start the keyboard hook in a separate thread.
pub fn start_hook() -> bool {
    if RUNNING.swap(true, Ordering::SeqCst) {
        // Already running
        return false;
    }

    thread::spawn(|| {
        unsafe {
            // Install the keyboard hook
            let kbd_hook = match SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(keyboard_hook_proc),
                HINSTANCE::default(),
                0,
            ) {
                Ok(h) => h,
                Err(_) => {
                    RUNNING.store(false, Ordering::SeqCst);
                    return;
                }
            };

            // Install the mouse hook on the same thread so a single message
            // pump services both. If this fails, tear the keyboard hook down
            // so init() reports the failure cleanly.
            let mouse_hook = match SetWindowsHookExW(
                WH_MOUSE_LL,
                Some(mouse_hook_proc),
                HINSTANCE::default(),
                0,
            ) {
                Ok(h) => h,
                Err(_) => {
                    let _ = UnhookWindowsHookEx(kbd_hook);
                    RUNNING.store(false, Ordering::SeqCst);
                    return;
                }
            };

            HOOK_HANDLE.store(kbd_hook.0 as *mut _, Ordering::SeqCst);
            MOUSE_HOOK_HANDLE.store(mouse_hook.0 as *mut _, Ordering::SeqCst);

            // Message loop to keep both hooks alive
            let mut msg = MSG::default();
            while RUNNING.load(Ordering::SeqCst) {
                let result = GetMessageW(&mut msg, None, 0, 0);
                if result.0 == 0 || result.0 == -1 {
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            // Cleanup
            let _ = UnhookWindowsHookEx(kbd_hook);
            let _ = UnhookWindowsHookEx(mouse_hook);
            HOOK_HANDLE.store(ptr::null_mut(), Ordering::SeqCst);
            MOUSE_HOOK_HANDLE.store(ptr::null_mut(), Ordering::SeqCst);
        }
    });

    // Give the hook thread time to start
    thread::sleep(std::time::Duration::from_millis(50));

    RUNNING.load(Ordering::SeqCst)
}

/// Stop the keyboard hook.
pub fn stop_hook() {
    RUNNING.store(false, Ordering::SeqCst);

    // Wait a bit for cleanup (the loop will exit on next iteration)
    thread::sleep(std::time::Duration::from_millis(100));
}

/// Low-level keyboard hook procedure.
unsafe extern "system" fn keyboard_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // If code < 0, we must pass to CallNextHookEx
    if code >= 0 {
        let event_type = wparam.0 as u32;
        let kb_struct = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let vk_code = kb_struct.vkCode;

        if event_type == WM_KEYDOWN || event_type == WM_SYSKEYDOWN {
            // Skip modifier-only keys
            if !is_modifier_key(vk_code) {
                // Only fire once per physical press (ignore auto-repeat)
                let already_pressed = PRESSED_KEYS.lock().contains_key(&vk_code);
                if !already_pressed {
                    let modifiers = get_current_modifiers();
                    if let Some(key_name) = vk_to_key_name(vk_code) {
                        if let Some(hotkey) = registry::check_hotkey(modifiers, &key_name) {
                            PRESSED_KEYS.lock().insert(vk_code, hotkey.clone());
                            invoke_callback(&format!("{}|down", hotkey));
                        }
                    }
                }
            }
        } else if event_type == WM_KEYUP || event_type == WM_SYSKEYUP {
            // Fire up event if we had tracked this vk_code as a matched hotkey
            let hotkey = PRESSED_KEYS.lock().remove(&vk_code);
            if let Some(hotkey) = hotkey {
                invoke_callback(&format!("{}|up", hotkey));
            }
        }
    }

    // Always call next hook
    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

/// Low-level mouse hook procedure. Maps side buttons to MOUSE2..5 names and
/// reuses `registry::check_hotkey` so a hotkey like "Ctrl+MOUSE2" fires the
/// same callback path as a keyboard combo. Left button is intentionally
/// ignored — the UI also skips it to avoid hijacking ordinary clicks.
unsafe extern "system" fn mouse_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code >= 0 {
        let msg = wparam.0 as u32;
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        // For X-button events, HIWORD of mouseData is XBUTTON1 (1) or XBUTTON2 (2).
        let xbtn = ((info.mouseData >> 16) & 0xFFFF) as u16;

        let mapped: Option<(u32, &'static str, bool)> = match msg {
            WM_RBUTTONDOWN => Some((2, "MOUSE2", true)),
            WM_RBUTTONUP   => Some((2, "MOUSE2", false)),
            WM_MBUTTONDOWN => Some((3, "MOUSE3", true)),
            WM_MBUTTONUP   => Some((3, "MOUSE3", false)),
            WM_XBUTTONDOWN if xbtn == 1 => Some((4, "MOUSE4", true)),
            WM_XBUTTONUP   if xbtn == 1 => Some((4, "MOUSE4", false)),
            WM_XBUTTONDOWN if xbtn == 2 => Some((5, "MOUSE5", true)),
            WM_XBUTTONUP   if xbtn == 2 => Some((5, "MOUSE5", false)),
            _ => None,
        };

        if let Some((id, key_name, is_down)) = mapped {
            if is_down {
                let already_pressed = PRESSED_MOUSE.lock().contains_key(&id);
                if !already_pressed {
                    let modifiers = get_current_modifiers();
                    if let Some(hotkey) = registry::check_hotkey(modifiers, key_name) {
                        PRESSED_MOUSE.lock().insert(id, hotkey.clone());
                        invoke_callback(&format!("{}|down", hotkey));
                    }
                }
            } else {
                let hotkey = PRESSED_MOUSE.lock().remove(&id);
                if let Some(hotkey) = hotkey {
                    invoke_callback(&format!("{}|up", hotkey));
                }
            }
        }
    }

    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

/// Check if a virtual key code is a modifier key.
fn is_modifier_key(vk: u32) -> bool {
    matches!(
        vk,
        0xA0 | 0xA1 | // VK_LSHIFT, VK_RSHIFT
        0xA2 | 0xA3 | // VK_LCONTROL, VK_RCONTROL
        0xA4 | 0xA5 | // VK_LMENU, VK_RMENU (Alt)
        0x5B | 0x5C | // VK_LWIN, VK_RWIN
        0x10 | 0x11 | 0x12 // VK_SHIFT, VK_CONTROL, VK_MENU
    )
}

/// Get current state of modifier keys.
fn get_current_modifiers() -> Modifiers {
    unsafe {
        Modifiers {
            ctrl: (GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0,
            alt: (GetAsyncKeyState(VK_MENU.0 as i32) as u16 & 0x8000) != 0,
            shift: (GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0,
            win: (GetAsyncKeyState(VK_LWIN.0 as i32) as u16 & 0x8000) != 0
                || (GetAsyncKeyState(VK_RWIN.0 as i32) as u16 & 0x8000) != 0,
        }
    }
}

/// Convert Windows virtual key code to key name string.
fn vk_to_key_name(vk: u32) -> Option<String> {
    let name = match vk {
        // Letters A-Z (0x41-0x5A)
        0x41..=0x5A => ((vk as u8) as char).to_string(),

        // Numbers 0-9 (0x30-0x39) — VK codes match ASCII, same as letters
        0x30..=0x39 => ((vk as u8) as char).to_string(),

        // Function keys F1-F12 (0x70-0x7B)
        0x70..=0x7B => format!("F{}", vk - 0x70 + 1),

        // Numpad 0-9 (0x60-0x69)
        0x60..=0x69 => format!("Num{}", vk - 0x60),

        // Numpad operators
        0x6A => "NumMul".to_string(),
        0x6B => "NumAdd".to_string(),
        0x6D => "NumSub".to_string(),
        0x6E => "NumDec".to_string(),
        0x6F => "NumDiv".to_string(),

        // Special keys
        0x0D => "Enter".to_string(),
        0x1B => "Escape".to_string(),
        0x20 => "Space".to_string(),
        0x09 => "Tab".to_string(),
        0x08 => "Backspace".to_string(),
        0x2E => "Delete".to_string(),
        0x2D => "Insert".to_string(),
        0x24 => "Home".to_string(),
        0x23 => "End".to_string(),
        0x21 => "PageUp".to_string(),
        0x22 => "PageDown".to_string(),
        0x26 => "Up".to_string(),
        0x28 => "Down".to_string(),
        0x25 => "Left".to_string(),
        0x27 => "Right".to_string(),
        0x2C => "PrintScreen".to_string(),
        0x13 => "Pause".to_string(),
        0x14 => "CapsLock".to_string(),
        0x90 => "NumLock".to_string(),
        0x91 => "ScrollLock".to_string(),

        // OEM keys (US keyboard layout)
        0xC0 => "`".to_string(),
        0xBD => "-".to_string(),
        0xBB => "=".to_string(),
        0xDB => "[".to_string(),
        0xDD => "]".to_string(),
        0xDC => "\\".to_string(),
        0xBA => ";".to_string(),
        0xDE => "'".to_string(),
        0xBC => ",".to_string(),
        0xBE => ".".to_string(),
        0xBF => "/".to_string(),

        // Unknown key
        _ => return None,
    };

    Some(name)
}
