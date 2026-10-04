//! macOS global-hotkey backend — Carbon `RegisterEventHotKey`.
//!
//! # Why Carbon and not a CGEventTap
//! `RegisterEventHotKey` is the one global-shortcut API on macOS that needs **no
//! TCC permission** (no Accessibility, no Input Monitoring): the WindowServer
//! matches the exact registered combo and tells only us about it, which is
//! precisely this library's privacy promise. It also reports BOTH edges
//! (`kEventHotKeyPressed` / `kEventHotKeyReleased`), so push-to-talk works. A
//! CGEventTap would see every keystroke and needs Input Monitoring — we do not
//! need it, so we do not use it. What Carbon cannot do, and therefore this
//! backend refuses (`hotkey_register` returns `false`):
//! - mouse buttons (`MOUSE2..5`) — would need a CGEventTap;
//! - modifier-only combos (no non-modifier key);
//! - key names with no macOS virtual keycode (`Insert`, `PrintScreen`,
//!   `Pause`, `ScrollLock`, `NumLock`, `CapsLock`).
//!
//! # Threading — works in a GUI-less daemon
//! Carbon is not thread-safe, so EVERY Carbon call (install handler, register,
//! unregister) happens on one dedicated thread, `astra-hotkey-carbon`, which
//! runs the Carbon application event loop (`RunApplicationEventLoop`) as its
//! own loop. The host process does NOT need a main-thread run loop and does not
//! need to be an NSApplication: hot-key events are delivered to, and dispatched
//! on, this thread (measured with the main thread blocked, which is the daemon's
//! shape — see the MEASURED note in `thread_main`, and the `macos_hotkey`
//! example, which reproduces it). The process does not become a LaunchServices
//! app (no Dock icon, no menu bar). The C ABI threads hand work to the thread
//! by posting a private Carbon event to its event queue (`PostEventToQueue` is
//! thread-safe) and wait for the answer, so `hotkey_register` returns the real
//! OS result. Registration is NON-exclusive (options `0`): measured, two
//! processes registering the same combo BOTH receive it — the same shared
//! semantics as the Windows low-level hook. A combo the system itself reserves
//! (e.g. ⌘Space for Spotlight) is consumed by the system first.
//!
//! Callbacks (`"<combo>|down"` / `"<combo>|up"`) fire ON that thread.
//!
//! # Modifier mapping (literal, matches the UI recorder's `KeyboardEvent` flags)
//! `Ctrl` → Control (⌃), `Alt` → Option (⌥), `Shift` → Shift (⇧),
//! `Win` / `Meta` / `Super` / `Cmd` / `Command` → Command (⌘). `Ctrl` is NOT
//! rewritten to Command: the UI records ⌘ as `meta`, so a combo recorded on a
//! Mac already says `Win`, and a `Ctrl+…` default means the Control key.
//!
//! Keycodes are the `kVK_*` *positional* codes (ANSI layout), the same way the
//! Windows backend uses US-layout VK codes.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use once_cell::sync::Lazy;
use parking_lot::Mutex;

use crate::invoke_callback;

// ───────────────────────────── Carbon / CF FFI ─────────────────────────────

type OSStatus = i32;
type EventTargetRef = *mut c_void;
type EventHandlerRef = *mut c_void;
type EventHandlerCallRef = *mut c_void;
type EventRef = *mut c_void;
type EventHotKeyRef = *mut c_void;
type EventLoopRef = *mut c_void;
type EventQueueRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

type EventHandlerProc = extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> OSStatus;

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn GetEventDispatcherTarget() -> EventTargetRef;
    fn InstallEventHandler(
        target: EventTargetRef,
        handler: EventHandlerProc,
        num_types: usize,
        list: *const EventTypeSpec,
        user_data: *mut c_void,
        out_ref: *mut EventHandlerRef,
    ) -> OSStatus;
    fn RemoveEventHandler(handler: EventHandlerRef) -> OSStatus;
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        id: EventHotKeyID,
        target: EventTargetRef,
        options: u32,
        out_ref: *mut EventHotKeyRef,
    ) -> OSStatus;
    fn UnregisterEventHotKey(hot_key: EventHotKeyRef) -> OSStatus;
    fn GetEventKind(event: EventRef) -> u32;
    fn GetEventParameter(
        event: EventRef,
        name: u32,
        desired_type: u32,
        actual_type: *mut u32,
        buffer_size: usize,
        actual_size: *mut usize,
        data: *mut c_void,
    ) -> OSStatus;
    fn RunApplicationEventLoop();
    fn QuitApplicationEventLoop();
    fn GetCurrentEventLoop() -> EventLoopRef;
    fn QuitEventLoop(event_loop: EventLoopRef) -> OSStatus;
    fn GetCurrentEventQueue() -> EventQueueRef;
    fn CreateEvent(
        allocator: *const c_void,
        class_id: u32,
        kind: u32,
        when: f64,
        flags: u32,
        out_event: *mut EventRef,
    ) -> OSStatus;
    fn PostEventToQueue(queue: EventQueueRef, event: EventRef, priority: i16) -> OSStatus;
    fn ReleaseEvent(event: EventRef);
}

const fn four_cc(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
}

const K_EVENT_CLASS_KEYBOARD: u32 = four_cc(b"keyb");
const K_EVENT_HOT_KEY_PRESSED: u32 = 5;
const K_EVENT_HOT_KEY_RELEASED: u32 = 6;
const K_EVENT_PARAM_DIRECT_OBJECT: u32 = four_cc(b"----");
const TYPE_EVENT_HOT_KEY_ID: u32 = four_cc(b"hkid");
/// Our hot-key signature, so we never act on an id some other code registered.
const SIGNATURE: u32 = four_cc(b"AsHk");
/// Private event (class = our signature) that wakes the Carbon thread to
/// drain its command queue.
const K_EVENT_WAKE: u32 = 1;
const K_EVENT_PRIORITY_STANDARD: i16 = 1;
const EVENT_NOT_HANDLED_ERR: OSStatus = -9874;
const EVENT_HOT_KEY_EXISTS_ERR: OSStatus = -9878;

// Carbon modifier masks (Events.h).
const CMD_KEY: u32 = 1 << 8;
const SHIFT_KEY: u32 = 1 << 9;
const OPTION_KEY: u32 = 1 << 11;
const CONTROL_KEY: u32 = 1 << 12;

// ─────────────────────────── combo → (keycode, mods) ───────────────────────────

/// Parse a **normalized** combo (`registry::normalize_hotkey` output, e.g.
/// `"Ctrl+Shift+T"`) into a macOS virtual keycode + Carbon modifier mask.
/// `None` when the combo has no key, more than one key, or a key macOS has no
/// keycode for (see the module docs).
pub fn parse_combo(combo: &str) -> Option<(u32, u32)> {
    let mut mods = 0u32;
    let mut key: Option<u32> = None;
    for part in combo.split('+').map(str::trim).filter(|s| !s.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= CONTROL_KEY,
            "alt" | "option" | "opt" => mods |= OPTION_KEY,
            "shift" => mods |= SHIFT_KEY,
            "win" | "meta" | "super" | "cmd" | "command" => mods |= CMD_KEY,
            _ => {
                if key.is_some() {
                    return None; // two non-modifier keys — not expressible
                }
                key = Some(key_to_vk(part)?);
            }
        }
    }
    key.map(|k| (k, mods))
}

/// Astra key name → `kVK_*` (HIToolbox/Events.h). Case-insensitive.
fn key_to_vk(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    let vk = match lower.as_str() {
        "a" => 0x00,
        "s" => 0x01,
        "d" => 0x02,
        "f" => 0x03,
        "h" => 0x04,
        "g" => 0x05,
        "z" => 0x06,
        "x" => 0x07,
        "c" => 0x08,
        "v" => 0x09,
        "b" => 0x0B,
        "q" => 0x0C,
        "w" => 0x0D,
        "e" => 0x0E,
        "r" => 0x0F,
        "y" => 0x10,
        "t" => 0x11,
        "o" => 0x1F,
        "u" => 0x20,
        "i" => 0x22,
        "p" => 0x23,
        "l" => 0x25,
        "j" => 0x26,
        "k" => 0x28,
        "n" => 0x2D,
        "m" => 0x2E,

        "1" => 0x12,
        "2" => 0x13,
        "3" => 0x14,
        "4" => 0x15,
        "5" => 0x17,
        "6" => 0x16,
        "7" => 0x1A,
        "8" => 0x1C,
        "9" => 0x19,
        "0" => 0x1D,

        "=" => 0x18,
        "-" => 0x1B,
        "]" => 0x1E,
        "[" => 0x21,
        "'" => 0x27,
        ";" => 0x29,
        "\\" => 0x2A,
        "," => 0x2B,
        "/" => 0x2C,
        "." => 0x2F,
        "`" => 0x32,

        "enter" | "return" => 0x24,
        "tab" => 0x30,
        "space" => 0x31,
        "backspace" => 0x33, // kVK_Delete (the ⌫ key)
        "escape" | "esc" => 0x35,
        "delete" | "del" => 0x75, // kVK_ForwardDelete
        "home" => 0x73,
        "end" => 0x77,
        "pageup" | "pgup" => 0x74,
        "pagedown" | "pgdn" => 0x79,
        "left" => 0x7B,
        "right" => 0x7C,
        "down" => 0x7D,
        "up" => 0x7E,

        "f1" => 0x7A,
        "f2" => 0x78,
        "f3" => 0x63,
        "f4" => 0x76,
        "f5" => 0x60,
        "f6" => 0x61,
        "f7" => 0x62,
        "f8" => 0x64,
        "f9" => 0x65,
        "f10" => 0x6D,
        "f11" => 0x67,
        "f12" => 0x6F,
        "f13" => 0x69,
        "f14" => 0x6B,
        "f15" => 0x71,
        "f16" => 0x6A,
        "f17" => 0x40,
        "f18" => 0x4F,
        "f19" => 0x50,
        "f20" => 0x5A,

        "num0" | "numpad0" => 0x52,
        "num1" | "numpad1" => 0x53,
        "num2" | "numpad2" => 0x54,
        "num3" | "numpad3" => 0x55,
        "num4" | "numpad4" => 0x56,
        "num5" | "numpad5" => 0x57,
        "num6" | "numpad6" => 0x58,
        "num7" | "numpad7" => 0x59,
        "num8" | "numpad8" => 0x5B,
        "num9" | "numpad9" => 0x5C,
        "nummul" | "numpadmultiply" => 0x43,
        "numadd" | "numpadadd" => 0x45,
        "numsub" | "numpadsubtract" => 0x4E,
        "numdec" | "numpaddecimal" => 0x41,
        "numdiv" | "numpaddivide" => 0x4B,

        _ => return None,
    };
    Some(vk)
}

// ───────────────────────────── the Carbon thread ─────────────────────────────

enum Cmd {
    Register(String, mpsc::Sender<bool>),
    Unregister(String, mpsc::Sender<bool>),
    UnregisterAll(mpsc::Sender<bool>),
    Stop,
}

/// Handle the ABI threads use to reach the Carbon thread.
struct Remote {
    /// The Carbon thread's `EventQueueRef` (an integer so `Remote` is `Send`).
    event_queue: usize,
    queue: mpsc::Sender<Cmd>,
    thread: thread::ThreadId,
    join: Option<thread::JoinHandle<()>>,
}

static REMOTE: Lazy<Mutex<Option<Remote>>> = Lazy::new(|| Mutex::new(None));
static RUNNING: AtomicBool = AtomicBool::new(false);

/// State that lives on (and is only touched from) the Carbon thread.
struct Local {
    rx: mpsc::Receiver<Cmd>,
    target: EventTargetRef,
    next_id: u32,
    by_combo: HashMap<String, (u32, EventHotKeyRef)>,
    by_id: HashMap<u32, String>,
    /// Ids whose key is currently down — Carbon can re-send Pressed on
    /// auto-repeat; we report one `down` per physical press.
    down: HashSet<u32>,
}

thread_local! {
    static LOCAL: std::cell::RefCell<Option<Local>> = const { std::cell::RefCell::new(None) };
}

impl Local {
    fn register(&mut self, combo: &str) -> bool {
        if self.by_combo.contains_key(combo) {
            return true;
        }
        let Some((vk, mods)) = parse_combo(combo) else {
            return false;
        };
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let id = self.next_id;
        let mut hk: EventHotKeyRef = std::ptr::null_mut();
        let st = unsafe {
            RegisterEventHotKey(
                vk,
                mods,
                EventHotKeyID {
                    signature: SIGNATURE,
                    id,
                },
                self.target,
                0,
                &mut hk,
            )
        };
        if st != 0 || hk.is_null() {
            let why = if st == EVENT_HOT_KEY_EXISTS_ERR {
                " (already registered in this process)"
            } else {
                ""
            };
            crate::backend::log_warn(format!(
                "macOS RegisterEventHotKey({combo}) failed: OSStatus {st}{why}"
            ));
            return false;
        }
        self.by_combo.insert(combo.to_string(), (id, hk));
        self.by_id.insert(id, combo.to_string());
        true
    }

    fn unregister(&mut self, combo: &str) -> bool {
        match self.by_combo.remove(combo) {
            Some((id, hk)) => {
                unsafe { UnregisterEventHotKey(hk) };
                self.by_id.remove(&id);
                self.down.remove(&id);
                true
            }
            None => false,
        }
    }

    fn unregister_all(&mut self) {
        for (_, (_, hk)) in self.by_combo.drain() {
            unsafe { UnregisterEventHotKey(hk) };
        }
        self.by_id.clear();
        self.down.clear();
    }
}

/// End `RunApplicationEventLoop` on the Carbon thread (call ON that thread).
fn quit_loop() {
    unsafe {
        QuitApplicationEventLoop();
        QuitEventLoop(GetCurrentEventLoop());
    }
}

/// Wake the Carbon thread so it drains its command queue.
fn wake(event_queue: usize) {
    unsafe {
        let mut ev: EventRef = std::ptr::null_mut();
        if CreateEvent(std::ptr::null(), SIGNATURE, K_EVENT_WAKE, 0.0, 0, &mut ev) == 0 {
            PostEventToQueue(event_queue as EventQueueRef, ev, K_EVENT_PRIORITY_STANDARD);
            ReleaseEvent(ev);
        }
    }
}

/// Drain the command queue (on the Carbon thread).
fn drain_commands() {
    LOCAL.with(|cell| {
        let mut guard = cell.borrow_mut();
        let Some(local) = guard.as_mut() else { return };
        while let Ok(cmd) = local.rx.try_recv() {
            match cmd {
                Cmd::Register(c, tx) => {
                    let _ = tx.send(local.register(&c));
                }
                Cmd::Unregister(c, tx) => {
                    let _ = tx.send(local.unregister(&c));
                }
                Cmd::UnregisterAll(tx) => {
                    local.unregister_all();
                    let _ = tx.send(true);
                }
                Cmd::Stop => {
                    RUNNING.store(false, Ordering::SeqCst);
                    quit_loop();
                }
            }
        }
    });
}

/// Carbon hot-key handler (runs on the Carbon thread).
extern "C" fn on_hotkey(_call: EventHandlerCallRef, event: EventRef, _ud: *mut c_void) -> OSStatus {
    let mut hk = EventHotKeyID::default();
    let st = unsafe {
        GetEventParameter(
            event,
            K_EVENT_PARAM_DIRECT_OBJECT,
            TYPE_EVENT_HOT_KEY_ID,
            std::ptr::null_mut(),
            std::mem::size_of::<EventHotKeyID>(),
            std::ptr::null_mut(),
            &mut hk as *mut _ as *mut c_void,
        )
    };
    let kind = unsafe { GetEventKind(event) };
    if kind == K_EVENT_WAKE && st != 0 {
        // Our private wake event (it carries no hot-key id).
        drain_commands();
        return 0;
    }
    if st != 0 || hk.signature != SIGNATURE {
        return EVENT_NOT_HANDLED_ERR;
    }
    // Resolve + update state, then release the borrow BEFORE calling out, so a
    // callback that re-enters the ABI (handled inline, see `call`) can borrow.
    let fire = LOCAL.with(|cell| {
        let mut guard = cell.borrow_mut();
        let local = guard.as_mut()?;
        let combo = local.by_id.get(&hk.id)?.clone();
        match kind {
            K_EVENT_HOT_KEY_PRESSED if local.down.insert(hk.id) => Some(format!("{combo}|down")),
            K_EVENT_HOT_KEY_RELEASED if local.down.remove(&hk.id) => Some(format!("{combo}|up")),
            _ => None,
        }
    });
    if let Some(msg) = fire {
        invoke_callback(&msg);
    }
    0
}

fn thread_main(rx: mpsc::Receiver<Cmd>, ready: mpsc::Sender<Result<usize, String>>) {
    unsafe {
        let target = GetEventDispatcherTarget();
        let types = [
            EventTypeSpec {
                event_class: K_EVENT_CLASS_KEYBOARD,
                event_kind: K_EVENT_HOT_KEY_PRESSED,
            },
            EventTypeSpec {
                event_class: K_EVENT_CLASS_KEYBOARD,
                event_kind: K_EVENT_HOT_KEY_RELEASED,
            },
            EventTypeSpec {
                event_class: SIGNATURE,
                event_kind: K_EVENT_WAKE,
            },
        ];
        let mut handler: EventHandlerRef = std::ptr::null_mut();
        let st = InstallEventHandler(
            target,
            on_hotkey,
            types.len(),
            types.as_ptr(),
            std::ptr::null_mut(),
            &mut handler,
        );
        if st != 0 {
            let _ = ready.send(Err(format!("InstallEventHandler failed: OSStatus {st}")));
            return;
        }

        let event_queue = GetCurrentEventQueue();

        LOCAL.with(|cell| {
            *cell.borrow_mut() = Some(Local {
                rx,
                target,
                next_id: 0,
                by_combo: HashMap::new(),
                by_id: HashMap::new(),
                down: HashSet::new(),
            })
        });
        let _ = ready.send(Ok(event_queue as usize));

        // MEASURED (macOS 26, a process with no NSApplication and an idle main
        // thread): `RunCurrentEventLoop` on this thread dispatches our posted
        // wake events but receives NO hot-key events — not even after a one-shot
        // `RunApplicationEventLoop` has initialised the app. `RunApplicationEventLoop`
        // run AS the loop of this thread receives both, and delivers them here
        // (not on main). It does not register the process with LaunchServices:
        // no Dock icon, no menu bar. It returns only on `QuitApplicationEventLoop`
        // (sent from our Stop command), so re-enter it on any spurious return.
        while RUNNING.load(Ordering::SeqCst) {
            RunApplicationEventLoop();
            drain_commands();
        }

        LOCAL.with(|cell| {
            if let Some(mut local) = cell.borrow_mut().take() {
                local.unregister_all();
            }
        });
        RemoveEventHandler(handler);
    }
}

/// Run `make(tx)` on the Carbon thread and wait for its answer. Called on the
/// Carbon thread itself (a callback re-entering the ABI) it runs inline.
fn call(make: impl FnOnce(mpsc::Sender<bool>) -> Cmd) -> Option<bool> {
    let (tx, rx) = mpsc::channel();
    let cmd = make(tx);
    {
        let guard = REMOTE.lock();
        let remote = guard.as_ref()?;
        if remote.thread == thread::current().id() {
            drop(guard);
            LOCAL.with(|cell| {
                if let Some(local) = cell.borrow_mut().as_mut() {
                    let _ = match cmd {
                        Cmd::Register(c, tx) => tx.send(local.register(&c)),
                        Cmd::Unregister(c, tx) => tx.send(local.unregister(&c)),
                        Cmd::UnregisterAll(tx) => {
                            local.unregister_all();
                            tx.send(true)
                        }
                        Cmd::Stop => Ok(()),
                    };
                }
            });
            return rx.try_recv().ok();
        }
        remote.queue.send(cmd).ok()?;
        wake(remote.event_queue);
    }
    rx.recv_timeout(Duration::from_secs(2)).ok()
}

// ───────────────────────────── public (crate) API ─────────────────────────────

/// Start the Carbon thread and (re-)register every combo already in the
/// registry (registrations made before `hotkey_init` are honoured, as on
/// Windows). Returns `false` only if the thread could not set up its handler.
pub fn start_hook() -> bool {
    let mut guard = REMOTE.lock();
    if guard.is_some() {
        return true;
    }
    let (qtx, qrx) = mpsc::channel();
    let (rtx, rrx) = mpsc::channel();
    RUNNING.store(true, Ordering::SeqCst);
    let join = match thread::Builder::new()
        .name("astra-hotkey-carbon".into())
        .spawn(move || thread_main(qrx, rtx))
    {
        Ok(j) => j,
        Err(e) => {
            RUNNING.store(false, Ordering::SeqCst);
            crate::backend::log_error(format!("macOS hotkey thread spawn failed: {e}"));
            return false;
        }
    };
    match rrx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(event_queue)) => {
            *guard = Some(Remote {
                event_queue,
                queue: qtx,
                thread: join.thread().id(),
                join: Some(join),
            });
        }
        Ok(Err(e)) => {
            RUNNING.store(false, Ordering::SeqCst);
            crate::backend::log_error(format!("macOS hotkey backend: {e}"));
            let _ = join.join();
            return false;
        }
        Err(_) => {
            RUNNING.store(false, Ordering::SeqCst);
            crate::backend::log_error("macOS hotkey backend: Carbon thread did not start".into());
            return false;
        }
    }
    drop(guard);

    for combo in crate::registry::snapshot() {
        if call(|tx| Cmd::Register(combo.clone(), tx)) != Some(true) {
            crate::backend::log_warn(format!("macOS: could not register pre-init hotkey {combo}"));
        }
    }
    true
}

/// Stop the Carbon thread (unregisters everything). Idempotent.
pub fn stop_hook() {
    let remote = REMOTE.lock().take();
    let Some(mut remote) = remote else { return };
    if remote.thread == thread::current().id() {
        // Called from inside a hotkey callback: just tell the loop to exit.
        RUNNING.store(false, Ordering::SeqCst);
        quit_loop();
        return;
    }
    let _ = remote.queue.send(Cmd::Stop);
    wake(remote.event_queue);
    // Bounded join: the loop slices at 1 s and drains after each slice, so this
    // normally returns in milliseconds; never let shutdown hang the daemon.
    if let Some(j) = remote.join.take() {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !j.is_finished() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if j.is_finished() {
            let _ = j.join();
        } else {
            crate::backend::log_warn("macOS hotkey thread did not stop within 3 s".into());
        }
    }
}

/// Register a normalized combo with the OS. Before `hotkey_init` it only
/// validates (the combo is grabbed when the thread starts).
pub fn register(combo: &str) -> bool {
    if parse_combo(combo).is_none() {
        crate::backend::log_warn(format!(
            "macOS: hotkey {combo:?} cannot be registered (no key, or a key/mouse button Carbon cannot grab)"
        ));
        return false;
    }
    if REMOTE.lock().is_none() {
        return true;
    }
    call(|tx| Cmd::Register(combo.to_string(), tx)).unwrap_or(false)
}

pub fn unregister(combo: &str) -> bool {
    if REMOTE.lock().is_none() {
        return true;
    }
    call(|tx| Cmd::Unregister(combo.to_string(), tx)).unwrap_or(false)
}

pub fn unregister_all() {
    if REMOTE.lock().is_none() {
        return;
    }
    let _ = call(Cmd::UnregisterAll);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifiers_literally() {
        assert_eq!(
            parse_combo("Ctrl+Shift+T"),
            Some((0x11, CONTROL_KEY | SHIFT_KEY))
        );
        assert_eq!(parse_combo("Alt+Space"), Some((0x31, OPTION_KEY)));
        assert_eq!(parse_combo("Win+E"), Some((0x0E, CMD_KEY)));
        assert_eq!(parse_combo("F5"), Some((0x60, 0)));
        assert_eq!(
            parse_combo("Ctrl+Alt+Shift+Win+Num7"),
            Some((0x59, CONTROL_KEY | OPTION_KEY | SHIFT_KEY | CMD_KEY))
        );
    }

    #[test]
    fn refuses_what_carbon_cannot_grab() {
        assert_eq!(parse_combo("Ctrl+MOUSE3"), None);
        assert_eq!(parse_combo("Ctrl+Shift"), None);
        assert_eq!(parse_combo("PrintScreen"), None);
        assert_eq!(parse_combo("Ctrl+A+B"), None);
    }

    #[test]
    fn every_normalized_key_name_round_trips() {
        for k in [
            "A",
            "Z",
            "0",
            "9",
            "F1",
            "F12",
            "Enter",
            "Escape",
            "Space",
            "Tab",
            "Backspace",
            "Delete",
            "Home",
            "End",
            "PageUp",
            "PageDown",
            "Up",
            "Down",
            "Left",
            "Right",
            "NumAdd",
            "NumDiv",
            "`",
            "-",
            "=",
            "[",
            "]",
            "\\",
            ";",
            "'",
            ",",
            ".",
            "/",
        ] {
            let n = crate::registry::normalize_hotkey(&format!("Ctrl+{k}"));
            assert!(parse_combo(&n).is_some(), "{n}");
        }
    }
}
