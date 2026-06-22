//! AstraHotkey - Native global hotkey service for Astra AI assistant
//!
//! This library provides efficient global hotkey detection using a registration model.
//! Only registered hotkey combinations trigger callbacks - no data is captured or transmitted.
//!
//! # Privacy Guarantee
//! - Only registered hotkey combinations are processed
//! - No keyboard input is logged, stored, or transmitted
//! - Source code is public for audit

mod backend;
mod registry;

#[cfg(windows)]
mod hook_windows;

#[cfg(target_os = "linux")]
mod hook_linux;
#[cfg(target_os = "linux")]
mod hook_wayland;
#[cfg(target_os = "linux")]
mod hyprland;
#[cfg(target_os = "linux")]
mod linux;

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

    // Idempotent across legacy X11, id-based X11, and the portal actor — each
    // stop is a safe no-op if that backend was never started.
    #[cfg(target_os = "linux")]
    {
        hook_linux::stop_hook();
        hook_wayland::stop();
    }

    // Clear callback
    if let Some(cb_cell) = CALLBACK.get() {
        let mut guard = cb_cell.lock();
        *guard = None;
    }

    // Clear all registrations
    registry::unregister_all();
}

/// Internal activation funnel for the **owned** backends (Win32, X11). The hook
/// threads call this with `"<combo>|down"` / `"<combo>|up"`.
///
/// When the id-based ABI is active (`hotkey_init2` was used) and the owned
/// combo→id table is populated, we translate the fired combo into its shortcut
/// id and dispatch via `on_activated(id, pressed)`. Otherwise we fall back to
/// the legacy combo callback. The portal backend never comes through here — it
/// fires by id directly via `backend::invoke_activated`.
pub(crate) fn invoke_callback(keys: &str) {
    if backend::id_mode_active() {
        if let Some((combo, edge)) = keys.rsplit_once('|') {
            if let Some(id) = backend::owned_lookup(combo) {
                backend::invoke_activated(&id, edge == "down");
            }
            // In id-mode a combo with no mapping isn't one of ours — drop it
            // rather than leaking it to a (now unused) legacy callback.
            return;
        }
        return;
    }
    legacy_invoke_callback(keys);
}

/// The legacy combo callback path (`hotkey_init`).
fn legacy_invoke_callback(keys: &str) {
    if let Some(cb_cell) = CALLBACK.get() {
        let guard = cb_cell.lock();
        if let Some(callback) = *guard {
            if let Ok(c_string) = CString::new(keys) {
                callback(c_string.as_ptr());
            }
        }
    }
}

// ===========================================================================
// id-based ABI (the portal needs an id-keyed model; owned backends adopt it too)
//
// `hotkey_init2` + `hotkey_set_shortcuts` + `hotkey_get_bindings` +
// `hotkey_configure` + `hotkey_backend` form the new ABI the daemon uses on
// Linux. On other platforms these are honest no-ops (`hotkey_init2` returns
// false → the daemon keeps using the legacy combo ABI). The symbols always
// exist so the daemon can resolve the same set everywhere.
// ===========================================================================

/// Which native backend the library selected: `"windows" | "x11" | "portal" | "none"`.
/// Returns a `'static` C string — do **not** pass it to `hotkey_free`.
#[no_mangle]
pub extern "C" fn hotkey_backend() -> *const c_char {
    #[cfg(windows)]
    {
        c"windows".as_ptr()
    }
    #[cfg(target_os = "linux")]
    {
        backend::provider().as_cstr().as_ptr()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        c"none".as_ptr()
    }
}

/// The portal **binding model** when the backend is `portal`: `"dialog"` (KDE /
/// GNOME — the compositor's dialog binds the key), `"manual"` (Hyprland / wlroots
/// — the user binds the key in their config via the `global` dispatcher; Astra
/// writes the lines + offers a consent dialog), or `"none"`. The daemon queries
/// this right after `hotkey_init2` to choose the shortcut-id scheme (stable on
/// manual, `#combo`-suffixed on dialog). Returns a `'static` C string — do **not**
/// pass it to `hotkey_free`.
#[no_mangle]
pub extern "C" fn hotkey_portal_model() -> *const c_char {
    #[cfg(target_os = "linux")]
    {
        backend::portal_model().as_cstr().as_ptr()
    }
    #[cfg(not(target_os = "linux"))]
    {
        c"none".as_ptr()
    }
}

/// Compositor-config state for the UI (manual-bind model). JSON object:
/// `{ "model", "parser", "managed_file", "include_target", "include_present",
/// "needs_setup" }`. `needs_setup` is `true` when the model is manual and the
/// one-time include line is not yet active — the UI surfaces a "set up" action
/// that calls `hotkey_configure`. The returned pointer is heap-allocated — free
/// it with [`hotkey_free`]. Returns an empty-state object off Linux.
#[no_mangle]
pub extern "C" fn hotkey_compositor_info() -> *mut c_char {
    #[cfg(target_os = "linux")]
    let json = hyprland::compositor_info_json();
    #[cfg(not(target_os = "linux"))]
    let json = "{\"model\":\"none\",\"parser\":\"unknown\",\"managed_file\":\"\",\"include_target\":\"\",\"include_present\":false,\"needs_setup\":false}".to_string();
    match CString::new(json) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Install a diagnostic log callback. The library routes all portal / X11
/// backend diagnostics (`CreateSession`/`BindShortcuts`/`Activated` failures,
/// JSON parse errors, X11 grab failures) through it so the daemon can forward
/// them into its `tracing` sink (`daemon.log`). `level`: 0=error, 1=warn,
/// 2=info, 3=debug. Without it the library falls back to `eprintln!`.
///
/// # Safety
/// The callback must remain valid for the lifetime of the hotkey service.
#[no_mangle]
pub extern "C" fn hotkey_set_log_callback(cb: extern "C" fn(u8, *const c_char)) {
    backend::set_log_callback(cb);
}

/// id-based init. `on_activated(id, pressed)` fires per shortcut activation;
/// `on_bindings_changed()` fires when the actual bindings change (portal rebind,
/// `ShortcutsChanged`, or an owned re-register) so the daemon re-reads
/// `hotkey_get_bindings`.
///
/// # Returns
/// `true` if the id-based ABI is active on this platform (currently Linux),
/// `false` otherwise — in which case the daemon should use the legacy ABI.
///
/// # Safety
/// The callbacks must remain valid for the lifetime of the hotkey service.
#[no_mangle]
pub extern "C" fn hotkey_init2(
    on_activated: extern "C" fn(*const c_char, bool),
    on_bindings_changed: extern "C" fn(),
) -> bool {
    #[cfg(target_os = "linux")]
    {
        if !backend::set_callbacks(on_activated, on_bindings_changed) {
            return false; // already initialized
        }
        linux::init()
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Keep the parameters "used" without installing callbacks: leaving the
        // id callbacks unset is what keeps `invoke_callback` on the legacy path.
        let _ = (on_activated, on_bindings_changed);
        false
    }
}

/// Replace the entire shortcut set. `json` is a UTF-8 array of
/// `{ "id": string, "description": string, "preferred": string|null }`.
///
/// Owned backends grab the `preferred` combos immediately; the portal sends them
/// to the compositor and the real bindings arrive later via `on_bindings_changed`.
#[no_mangle]
pub extern "C" fn hotkey_set_shortcuts(json: *const c_char) -> bool {
    if json.is_null() {
        return false;
    }
    let s = match unsafe { CStr::from_ptr(json) }.to_str() {
        Ok(s) => s,
        Err(_) => return false,
    };
    let specs: Vec<backend::ShortcutSpec> = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(e) => {
            backend::log_error(format!("hotkey_set_shortcuts: invalid JSON: {e}"));
            return false;
        }
    };

    #[cfg(target_os = "linux")]
    {
        linux::set_shortcuts(specs)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = specs;
        false
    }
}

/// Open the compositor's config UI for a shortcut (portal only). On owned
/// backends this returns `false` — the daemon shows its in-app recorder there.
#[no_mangle]
pub extern "C" fn hotkey_configure(id: *const c_char) -> bool {
    if id.is_null() {
        return false;
    }
    let s = match unsafe { CStr::from_ptr(id) }.to_str() {
        Ok(s) => s,
        Err(_) => return false,
    };

    #[cfg(target_os = "linux")]
    {
        linux::configure(s)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = s;
        false
    }
}

/// Current actual bindings as a JSON array of
/// `{ "id", "combo": string|null, "status": "bound"|"needs_binding"|"unsupported", "description" }`.
///
/// The returned pointer is heap-allocated — free it with [`hotkey_free`].
#[no_mangle]
pub extern "C" fn hotkey_get_bindings() -> *mut c_char {
    let json = backend::bindings_json();
    match CString::new(json) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Free a string returned by [`hotkey_get_bindings`].
///
/// # Safety
/// `s` must be a pointer previously returned by `hotkey_get_bindings` (and not
/// already freed).
#[no_mangle]
pub extern "C" fn hotkey_free(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    unsafe {
        let _ = CString::from_raw(s);
    }
}
