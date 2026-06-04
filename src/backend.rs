//! Shared, cross-platform pieces of the **id-based** hotkey model.
//!
//! The legacy ABI (`hotkey_register("Ctrl+A")` + a `"Ctrl+A|down"` callback) is
//! combo-keyed and good enough for the silent-grab backends (Win32, X11). The
//! Wayland GlobalShortcuts portal, however, is **id-keyed**: you register
//! `{id, description, preferred}`, the compositor owns the actual key, and it
//! fires activations *by id*. To serve all three backends behind one daemon
//! model we add an id-based ABI on top; this module holds the parts every
//! backend touches:
//!
//! - [`Provider`] — which backend was actually selected (reported to the UI so
//!   it can pick "in-app recorder" vs "Configure in the compositor" vs
//!   "unsupported").
//! - [`ShortcutSpec`] / [`ShortcutBinding`] — the JSON shapes crossing the C
//!   boundary (`hotkey_set_shortcuts` in, `hotkey_get_bindings` out).
//! - The id-based callbacks (`on_activated(id, pressed)`, `on_bindings_changed`)
//!   and their funnels.
//! - The **owned-backend** translation table (combo → id) plus the live
//!   bindings cache the UI reads. "Owned" = a backend where *we* hold the key
//!   grab and thus know the combo (Win32, X11); the portal is *not* owned (the
//!   compositor holds it), so it bypasses the combo table and dispatches by id.

use std::collections::HashMap;
use std::ffi::{c_char, CStr};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use once_cell::sync::{Lazy, OnceCell};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Which backend the library actually selected at runtime. Reported to the UI
/// (via the daemon) so it can render the right affordance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Provider {
    /// `hotkey_init2` not called yet / selection not finished.
    Unknown = 0,
    /// Win32 low-level hook (owned, silent, in-app recorder).
    Windows = 1,
    /// X11 `XGrabKey` via `global-hotkey` (owned, silent, in-app recorder).
    X11 = 2,
    /// Wayland GlobalShortcuts portal (compositor-owned, "Configure" dialog).
    Portal = 3,
    /// No working global-hotkey path on this session (e.g. Wayland without the
    /// portal). The lib still answers `provider`/`get_bindings` so the UI can
    /// explain the situation honestly.
    Unsupported = 4,
}

impl Provider {
    fn from_u8(v: u8) -> Provider {
        match v {
            1 => Provider::Windows,
            2 => Provider::X11,
            3 => Provider::Portal,
            4 => Provider::Unsupported,
            _ => Provider::Unknown,
        }
    }

    /// Stable wire string for `hotkey_backend()` — a `'static` C string, so the
    /// caller never frees it.
    pub fn as_cstr(self) -> &'static CStr {
        match self {
            Provider::Windows => c"windows",
            Provider::X11 => c"x11",
            Provider::Portal => c"portal",
            // Both "no backend yet" and "no backend possible" report "none" to
            // the daemon; the daemon distinguishes via get_bindings status.
            Provider::Unknown | Provider::Unsupported => c"none",
        }
    }
}

static PROVIDER: AtomicU8 = AtomicU8::new(Provider::Unknown as u8);

/// Record which backend was selected (called once during `hotkey_init2`).
pub fn set_provider(p: Provider) {
    PROVIDER.store(p as u8, Ordering::SeqCst);
}

/// The backend currently in effect.
pub fn provider() -> Provider {
    Provider::from_u8(PROVIDER.load(Ordering::SeqCst))
}

/// Desired shortcut, as sent by the daemon through `hotkey_set_shortcuts`.
///
/// `preferred` is only a *suggestion* — on Wayland the compositor may bind a
/// different key (or none). The actual key always comes back via
/// [`ShortcutBinding::combo`], never from here. This is the anti-desync rule.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ShortcutSpec {
    pub id: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub preferred: Option<String>,
}

/// The status of a single binding, as the UI should render it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingStatus {
    /// The OS has a real key bound to this shortcut.
    Bound,
    /// Registered with the OS but no key is bound yet (portal: user hasn't
    /// accepted; owned: the grab failed, e.g. the combo is held elsewhere).
    NeedsBinding,
    /// This backend can't bind global shortcuts at all.
    Unsupported,
}

/// The *actual* binding, as returned by `hotkey_get_bindings` for the UI.
#[derive(Debug, Clone, Serialize)]
pub struct ShortcutBinding {
    pub id: String,
    /// Human-readable trigger the compositor reports (portal) or the normalized
    /// combo we grabbed (owned). `None` when nothing is bound.
    pub combo: Option<String>,
    pub status: BindingStatus,
    pub description: String,
}

// ---------------------------------------------------------------------------
// id-based callbacks (set once by `hotkey_init2`) + their funnels.
// ---------------------------------------------------------------------------

type ActivatedCb = extern "C" fn(*const c_char, bool);
type ChangedCb = extern "C" fn();

static ACTIVATED_CB: OnceCell<Mutex<Option<ActivatedCb>>> = OnceCell::new();
static CHANGED_CB: OnceCell<Mutex<Option<ChangedCb>>> = OnceCell::new();

/// Install the id-based callbacks. Returns `false` if they were already set
/// (double init).
pub fn set_callbacks(on_activated: ActivatedCb, on_changed: ChangedCb) -> bool {
    let a = ACTIVATED_CB.get_or_init(|| Mutex::new(None));
    let c = CHANGED_CB.get_or_init(|| Mutex::new(None));
    let mut ag = a.lock();
    let mut cg = c.lock();
    if ag.is_some() {
        return false;
    }
    *ag = Some(on_activated);
    *cg = Some(on_changed);
    true
}

/// `true` once the id-based ABI is in use, i.e. `hotkey_init2` ran. The shared
/// `invoke_callback` funnel uses this to decide whether owned-backend combo
/// events should be translated to ids instead of going to the legacy callback.
pub fn id_mode_active() -> bool {
    ACTIVATED_CB
        .get()
        .map(|m| m.lock().is_some())
        .unwrap_or(false)
}

/// Fire `on_activated(id, pressed)`. Used by the portal directly and by the
/// owned backends after combo→id translation.
///
/// The fn-pointer is copied out and the guard dropped **before** invoking the
/// daemon callback: `parking_lot::Mutex` is non-reentrant, and the callback may
/// synchronously re-enter the lib (e.g. read bindings), so holding the lock
/// across the call would self-deadlock the calling thread.
pub fn invoke_activated(id: &str, pressed: bool) {
    let cb = ACTIVATED_CB.get().and_then(|cell| *cell.lock());
    if let Some(cb) = cb {
        if let Ok(c) = std::ffi::CString::new(id) {
            cb(c.as_ptr(), pressed);
        }
    }
}

/// Fire `on_bindings_changed()` — the actual bindings changed (portal rebind,
/// `ShortcutsChanged`, or an owned re-register), so the daemon should re-read
/// `hotkey_get_bindings` and re-emit its UI event. This is the live anti-desync
/// signal. Same lock-release-before-call discipline as [`invoke_activated`].
pub fn invoke_bindings_changed() {
    let cb = CHANGED_CB.get().and_then(|cell| *cell.lock());
    if let Some(cb) = cb {
        cb();
    }
}

// ---------------------------------------------------------------------------
// Diagnostic log funnel → the daemon's `tracing` sink.
//
// The portal actor / X11 hook used to `eprintln!` straight to the process
// stderr fd, which the daemon's `tracing_appender` file layer never captures —
// so every Wayland bind/activation failure was invisible in `daemon.log`. We
// now route diagnostics through an optional C callback the daemon installs
// (`hotkey_set_log_callback`); it forwards each line to `tracing` under target
// `astra_daemon::ffi::hotkey`. If no callback is installed (older daemon, or a
// direct test harness) we fall back to `eprintln!` so messages are never lost.
// ---------------------------------------------------------------------------

/// Log levels crossing the C boundary (kept in lockstep with the daemon-side
/// `hotkey_log_callback` match in `ffi/hotkey.rs`).
pub const LOG_ERROR: u8 = 0;
pub const LOG_WARN: u8 = 1;
pub const LOG_INFO: u8 = 2;
pub const LOG_DEBUG: u8 = 3;

type LogCb = extern "C" fn(u8, *const c_char);
static LOG_CB: OnceCell<Mutex<Option<LogCb>>> = OnceCell::new();

/// Install the daemon's log callback (idempotent — last writer wins, harmless).
pub fn set_log_callback(cb: LogCb) {
    let cell = LOG_CB.get_or_init(|| Mutex::new(None));
    *cell.lock() = Some(cb);
}

/// Emit a diagnostic line at `level`. Copies the fn-pointer out before calling
/// (same non-reentrant-lock discipline as the other funnels).
pub fn log(level: u8, msg: &str) {
    let cb = LOG_CB.get().and_then(|cell| *cell.lock());
    match cb {
        Some(cb) => {
            if let Ok(c) = std::ffi::CString::new(msg) {
                cb(level, c.as_ptr());
            }
        }
        // No daemon callback installed → keep the legacy stderr breadcrumb.
        None => eprintln!("[astra-hotkey] {msg}"),
    }
}

/// Convenience wrappers so call sites read like `tracing` macros.
pub fn log_error(msg: String) {
    log(LOG_ERROR, &msg);
}
pub fn log_warn(msg: String) {
    log(LOG_WARN, &msg);
}
pub fn log_info(msg: String) {
    log(LOG_INFO, &msg);
}
pub fn log_debug(msg: String) {
    log(LOG_DEBUG, &msg);
}

// ---------------------------------------------------------------------------
// Owned-backend (Win32 / X11) combo→id table + the live bindings cache.
// ---------------------------------------------------------------------------

/// combo (normalized) → our shortcut id. Populated by the owned `set_shortcuts`
/// path; read by `invoke_callback` to translate a fired combo into an id.
static OWNED_COMBO_TO_ID: Lazy<Mutex<HashMap<String, String>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// `true` once an owned backend has populated the combo→id table, so combo
/// events should dispatch by id. Kept separate from [`id_mode_active`] because
/// the portal sets `id_mode_active` (callbacks) but never touches this table.
static OWNED_ID_MODE: AtomicBool = AtomicBool::new(false);

/// The live bindings the UI reads via `hotkey_get_bindings`. Updated by both the
/// owned path (synchronously) and the portal actor (on bind / ShortcutsChanged).
static BINDINGS: Lazy<Mutex<Vec<ShortcutBinding>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// Replace the owned combo→id table (called by the owned `set_shortcuts`).
pub fn owned_set_map(entries: Vec<(String, String)>) {
    let mut m = OWNED_COMBO_TO_ID.lock();
    m.clear();
    for (combo, id) in entries {
        m.insert(combo, id);
    }
    OWNED_ID_MODE.store(true, Ordering::SeqCst);
}

/// Translate a fired combo to its shortcut id, if the owned id-table is active.
pub fn owned_lookup(combo: &str) -> Option<String> {
    if !OWNED_ID_MODE.load(Ordering::SeqCst) {
        return None;
    }
    OWNED_COMBO_TO_ID.lock().get(combo).cloned()
}

/// Overwrite the live bindings cache.
pub fn set_bindings(bindings: Vec<ShortcutBinding>) {
    *BINDINGS.lock() = bindings;
}

/// Re-mark every cached binding as `Unsupported` with no combo. Called when a
/// backend dies (e.g. the portal's D-Bus connection drops) so the UI stops
/// reporting phantom-bound shortcuts instead of leaving the stale `Bound` cache.
pub fn mark_all_unsupported() {
    let mut b = BINDINGS.lock();
    for binding in b.iter_mut() {
        binding.combo = None;
        binding.status = BindingStatus::Unsupported;
    }
}

/// Current bindings as a JSON array string for `hotkey_get_bindings`.
pub fn bindings_json() -> String {
    let b = BINDINGS.lock();
    serde_json::to_string(&*b).unwrap_or_else(|_| "[]".to_string())
}
