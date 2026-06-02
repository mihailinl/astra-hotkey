//! Linux **X11** global-hotkey backend.
//!
//! Replaces the old non-functional stub (which opened a display and looped on
//! `XNextEvent` but never called `XGrabKey`/`XSelectInput`, so it received zero
//! key events). This uses the [`global-hotkey`](https://crates.io/crates/global-hotkey)
//! crate, which performs a real `XGrabKey` and runs its own internal X11 event
//! thread; the manager is `Send`, so we `register`/`unregister` from the C ABI
//! thread and read activations from a listener thread.
//!
//! Combos arrive in Astra's format (`"Ctrl+Alt+A"`); `global-hotkey`'s parser
//! accepts it directly (we only map `Win` → `Super`). Activations are delivered
//! to the C callback as `"<combo>|down"` / `"<combo>|up"` (matching the daemon's
//! `hotkey_callback` parser, so push-to-talk gets both edges).
//!
//! **X11 only.** On a Wayland session `XGrabKey` via XWayland does not deliver
//! global keys for native Wayland windows — that path uses the GlobalShortcuts
//! portal backend (`hook_wayland.rs`).

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use once_cell::sync::OnceCell;
use parking_lot::Mutex;

use crate::invoke_callback;

struct State {
    manager: GlobalHotKeyManager,
    /// combo string → HotKey (so we can unregister by combo).
    by_combo: HashMap<String, HotKey>,
}

static STATE: OnceCell<Mutex<State>> = OnceCell::new();
/// hotkey id → combo string, read by the listener thread to label activations.
static ID_TO_COMBO: OnceCell<Mutex<HashMap<u32, String>>> = OnceCell::new();
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Initialize the X11 hotkey manager and start the activation-listener thread.
pub fn start_hook() -> bool {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return true; // already running
    }

    let manager = match GlobalHotKeyManager::new() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[astra-hotkey] X11 GlobalHotKeyManager init failed: {e}");
            RUNNING.store(false, Ordering::SeqCst);
            return false;
        }
    };

    let _ = STATE.set(Mutex::new(State {
        manager,
        by_combo: HashMap::new(),
    }));
    let _ = ID_TO_COMBO.set(Mutex::new(HashMap::new()));

    thread::Builder::new()
        .name("astra-hotkey-x11".into())
        .spawn(|| {
            let rx = GlobalHotKeyEvent::receiver();
            while RUNNING.load(Ordering::SeqCst) {
                // Bounded wait so we notice `stop_hook()` promptly.
                let ev = match rx.recv_timeout(Duration::from_millis(150)) {
                    Ok(ev) => ev,
                    Err(_) => continue,
                };
                let combo = ID_TO_COMBO
                    .get()
                    .and_then(|m| m.lock().get(&ev.id).cloned());
                if let Some(combo) = combo {
                    let suffix = if matches!(ev.state, HotKeyState::Pressed) {
                        "down"
                    } else {
                        "up"
                    };
                    invoke_callback(&format!("{combo}|{suffix}"));
                }
            }
        })
        .ok();

    true
}

/// Stop the listener thread and release all grabs.
pub fn stop_hook() {
    RUNNING.store(false, Ordering::SeqCst);
    unregister_all();
}

/// `global-hotkey`'s parser accepts `"Ctrl+Alt+A"`; we only normalize the
/// Windows/Super key name (`Win` → `Super`).
fn parse_combo(combo: &str) -> Option<HotKey> {
    HotKey::from_str(&combo.replace("Win", "Super")).ok()
}

/// Register (grab) a combo. Returns `true` if the grab succeeded.
pub fn register(combo: &str) -> bool {
    let Some(hk) = parse_combo(combo) else {
        eprintln!("[astra-hotkey] cannot parse hotkey '{combo}'");
        return false;
    };
    let Some(state) = STATE.get() else { return false };
    let mut st = state.lock();
    match st.manager.register(hk) {
        Ok(()) => {
            if let Some(m) = ID_TO_COMBO.get() {
                m.lock().insert(hk.id, combo.to_string());
            }
            st.by_combo.insert(combo.to_string(), hk);
            true
        }
        Err(e) => {
            eprintln!("[astra-hotkey] failed to register '{combo}': {e}");
            false
        }
    }
}

/// Unregister (ungrab) a previously registered combo.
pub fn unregister(combo: &str) -> bool {
    let Some(state) = STATE.get() else { return false };
    let mut st = state.lock();
    if let Some(hk) = st.by_combo.remove(combo) {
        if let Some(m) = ID_TO_COMBO.get() {
            m.lock().remove(&hk.id);
        }
        let _ = st.manager.unregister(hk);
        true
    } else {
        false
    }
}

/// Unregister all combos.
pub fn unregister_all() {
    let Some(state) = STATE.get() else { return };
    let mut st = state.lock();
    let all: Vec<HotKey> = st.by_combo.drain().map(|(_, hk)| hk).collect();
    if let Some(m) = ID_TO_COMBO.get() {
        m.lock().clear();
    }
    if !all.is_empty() {
        let _ = st.manager.unregister_all(&all);
    }
}
