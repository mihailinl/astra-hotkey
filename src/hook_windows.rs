//! Windows-specific low-level keyboard hook implementation.
//!
//! Uses SetWindowsHookEx with WH_KEYBOARD_LL to intercept keyboard events.
//! Only registered hotkey combinations trigger the callback.

use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::thread;
use std::ptr;

use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
    DispatchMessageW, TranslateMessage,
    HHOOK, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN,
};

use crate::registry::{self, Modifiers};
use crate::invoke_callback;

/// Global hook handle
static HOOK_HANDLE: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(ptr::null_mut());

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
            // Install the hook
            let hook = SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(keyboard_hook_proc),
                HINSTANCE::default(),
                0,
            );

            match hook {
                Ok(h) => {
                    HOOK_HANDLE.store(h.0 as *mut _, Ordering::SeqCst);

                    // Message loop to keep the hook alive
                    let mut msg = MSG::default();
                    while RUNNING.load(Ordering::SeqCst) {
                        // Use GetMessage with a timeout by checking PeekMessage
                        let result = GetMessageW(&mut msg, None, 0, 0);
                        if result.0 == 0 || result.0 == -1 {
                            break;
                        }
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }

                    // Cleanup
                    let _ = UnhookWindowsHookEx(h);
                    HOOK_HANDLE.store(ptr::null_mut(), Ordering::SeqCst);
                }
                Err(_) => {
                    RUNNING.store(false, Ordering::SeqCst);
                }
            }
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

        // Only process key down events
        if event_type == WM_KEYDOWN || event_type == WM_SYSKEYDOWN {
            let kb_struct = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let vk_code = kb_struct.vkCode;

            // Skip modifier-only keys
            if !is_modifier_key(vk_code) {
                // Get current modifier state
                let modifiers = get_current_modifiers();

                // Convert virtual key to key name
                if let Some(key_name) = vk_to_key_name(vk_code) {
                    // Check if this combination is registered
                    if let Some(hotkey) = registry::check_hotkey(modifiers, &key_name) {
                        // Invoke callback
                        invoke_callback(&hotkey);
                    }
                }
            }
        }
    }

    // Always call next hook
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

        // Numbers 0-9 (0x30-0x39)
        0x30..=0x39 => ((vk as u8 - 0x30) as u8 + b'0').to_string(),

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
