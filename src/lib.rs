//! AstraHotkey - Native global hotkey service for Astra AI assistant
//!
//! This library provides efficient global hotkey detection using a registration model.
//! Only registered hotkey combinations trigger callbacks - no data is captured or transmitted.
//!
//! # Privacy Guarantee
//! - Only registered hotkey combinations are processed
//! - No keyboard input is logged, stored, or transmitted
//! - Source code is public for audit

mod registry;

#[cfg(windows)]
mod hook_windows;

#[cfg(target_os = "linux")]
mod hook_linux;

use std::ffi::{c_char, CStr, CString};

use once_cell::sync::OnceCell;
use parking_lot::Mutex;

/// Global callback function pointer
static CALLBACK: OnceCell<Mutex<Option<extern "C" fn(*const c_char)>>> = OnceCell::new();

/// Initialize the hotkey service with a callback function.
/// The callback receives the matched hotkey string (e.g., "Ctrl+Shift+T").
///
/// # Safety
/// The callback must remain valid for the lifetime of the hotkey service.
///
/// # Returns
/// - `true` if initialization succeeded
/// - `false` if already initialized or hook installation failed
#[no_mangle]
pub extern "C" fn hotkey_init(callback: extern "C" fn(*const c_char)) -> bool {
    // Store callback
    let cb_cell = CALLBACK.get_or_init(|| Mutex::new(None));
    {
        let mut guard = cb_cell.lock();
        if guard.is_some() {
            // Already initialized
            return false;
        }
        *guard = Some(callback);
    }

    // Start the platform-specific hook
    #[cfg(windows)]
    {
        hook_windows::start_hook()
    }

    #[cfg(target_os = "linux")]
    {
        hook_linux::start_hook()
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    {
        false
    }
}

/// Register a hotkey combination to listen for.
/// Format: "Ctrl+Shift+T", "Alt+F4", "Win+E", etc.
///
/// Modifiers: Ctrl, Alt, Shift, Win
/// Keys: A-Z, 0-9, F1-F12, Enter, Escape, Space, Tab, etc.
///
/// # Returns
/// - `true` if registration succeeded
/// - `false` if the hotkey is invalid or already registered
#[no_mangle]
pub extern "C" fn hotkey_register(keys: *const c_char) -> bool {
    if keys.is_null() {
        return false;
    }

    let keys_str = unsafe {
        match CStr::from_ptr(keys).to_str() {
            Ok(s) => s,
            Err(_) => return false,
        }
    };

    registry::register_hotkey(keys_str)
}

/// Unregister a previously registered hotkey combination.
///
/// # Returns
/// - `true` if unregistration succeeded
/// - `false` if the hotkey was not registered
#[no_mangle]
pub extern "C" fn hotkey_unregister(keys: *const c_char) -> bool {
    if keys.is_null() {
        return false;
    }

    let keys_str = unsafe {
        match CStr::from_ptr(keys).to_str() {
            Ok(s) => s,
            Err(_) => return false,
        }
    };

    registry::unregister_hotkey(keys_str)
}

/// Unregister all hotkeys.
#[no_mangle]
pub extern "C" fn hotkey_unregister_all() {
    registry::unregister_all();
}

/// Get the number of currently registered hotkeys.
#[no_mangle]
pub extern "C" fn hotkey_count() -> u32 {
    registry::count() as u32
}

/// Shutdown the hotkey service and release all resources.
#[no_mangle]
pub extern "C" fn hotkey_shutdown() {
    // Stop the platform-specific hook
    #[cfg(windows)]
    hook_windows::stop_hook();

    #[cfg(target_os = "linux")]
    hook_linux::stop_hook();

    // Clear callback
    if let Some(cb_cell) = CALLBACK.get() {
        let mut guard = cb_cell.lock();
        *guard = None;
    }

    // Clear all registrations
    registry::unregister_all();
}

/// Internal function to invoke the callback when a registered hotkey is detected.
/// Called by platform-specific hook implementations.
pub(crate) fn invoke_callback(keys: &str) {
    if let Some(cb_cell) = CALLBACK.get() {
        let guard = cb_cell.lock();
        if let Some(callback) = *guard {
            if let Ok(c_string) = CString::new(keys) {
                callback(c_string.as_ptr());
            }
        }
    }
}
