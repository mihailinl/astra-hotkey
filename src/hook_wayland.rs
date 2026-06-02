//! Linux **Wayland** global-hotkey backend — the `org.freedesktop.portal.GlobalShortcuts`
//! XDG portal via [`ashpd`].
//!
//! On Wayland an app **cannot** silently grab global keys (the compositor owns
//! input, by design — anti-keylogger). The blessed path is the GlobalShortcuts
//! portal: we register `{id, description, preferred_trigger}`, the **compositor**
//! decides/binds the actual key (and may show its own dialog), and it streams
//! activations back to us *by id*. We never learn a raw keysym and never grab.
//!
//! ## Why an actor on its own runtime
//! The portal is async D-Bus and the proxy/session/signal-streams are neither
//! `Send` nor cheap to recreate, so they live on **one** dedicated thread running
//! a current-thread tokio runtime — the "portal actor". The C ABI (called from
//! the daemon's threads) talks to it over a channel:
//!
//! - `set_shortcuts` / `configure` are **fire-and-forget** commands. This is
//!   deliberate: KDE's `BindShortcuts` can pop a modal dialog and not return
//!   until the user dismisses it. Blocking the daemon thread for user-interaction
//!   time would be a bug, so we send the command and return immediately. The
//!   *result* arrives later via the `on_bindings_changed` callback → the daemon
//!   re-reads `hotkey_get_bindings`. That asynchronous-truth path is exactly what
//!   makes "I rebound it in System Settings but Astra shows the old key"
//!   structurally impossible.
//! - Activations arrive on the portal's `Activated` / `Deactivated` signals and
//!   are funneled straight to `on_activated(id, pressed)`.
//! - `ShortcutsChanged` (user rebound a key) refreshes the cache and fires
//!   `on_bindings_changed`.
//!
//! ## Session lifecycle
//! The GlobalShortcuts portal binds a set of shortcuts **once per session**;
//! several compositors ignore a second `BindShortcuts` on the same session. So to
//! reliably *change* the set (or to re-present the compositor's config UI via
//! `configure`) we **recreate the session** and bind again. Bindings persist by
//! app-id across sessions, so the compositor re-applies the user's saved keys
//! silently — recreating is safe and does not re-prompt for unchanged shortcuts.
//! We only rebind when the desired set actually changed, so routine no-op syncs
//! never churn a session or pop a dialog.
//!
//! Selection between this and the X11 backend happens in `linux.rs`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;

use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut, Shortcut};
use ashpd::desktop::Session;
use ashpd::WindowIdentifier;
use futures_util::StreamExt;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::backend::{self, BindingStatus, Provider, ShortcutBinding, ShortcutSpec};

/// Commands the C ABI thread sends to the portal actor.
enum Command {
    /// Replace the whole shortcut set (fire-and-forget; result via callback).
    Set(Vec<ShortcutSpec>),
    /// Re-present the compositor's bind UI (fire-and-forget).
    Configure(String),
    /// Tear the actor down (drops the session, ending the portal subscription).
    Shutdown,
}

/// The live command channel. `Mutex<Option<_>>` (not a `OnceCell`) so it can be
/// cleared on shutdown / actor death and re-created on a later `start()` — a
/// `OnceCell` would latch a dead sender forever.
static CMD_TX: Lazy<Mutex<Option<mpsc::UnboundedSender<Command>>>> = Lazy::new(|| Mutex::new(None));
/// `true` between a successful `start()` and the actor's exit.
static ACTOR_ALIVE: AtomicBool = AtomicBool::new(false);

/// Bound on `CreateSession` at startup — it does not show a dialog, so a short
/// wait is safe; if the portal is missing/slow we fall back to Unsupported.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
/// Safety bound on a single `BindShortcuts` await so an abandoned compositor
/// dialog cannot park the actor forever.
const BIND_TIMEOUT: Duration = Duration::from_secs(180);

/// Start the portal actor. Blocks (up to [`READY_TIMEOUT`]) until the actor has
/// created its session, so the caller learns whether the portal is usable.
///
/// Returns `true` if the GlobalShortcuts portal is available (provider = Portal);
/// `false` otherwise (caller falls back to Unsupported).
pub fn start() -> bool {
    if ACTOR_ALIVE.load(Ordering::SeqCst) {
        return true; // already running
    }

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
    let (ready_tx, ready_rx) = std_mpsc::channel::<bool>();

    thread::Builder::new()
        .name("astra-hotkey-portal".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("[astra-hotkey] portal: tokio runtime build failed: {e}");
                    let _ = ready_tx.send(false);
                    return;
                }
            };
            rt.block_on(actor_main(cmd_rx, ready_tx));
        })
        .ok();

    let ok = ready_rx.recv_timeout(READY_TIMEOUT).unwrap_or(false);
    if ok {
        *CMD_TX.lock() = Some(cmd_tx);
        ACTOR_ALIVE.store(true, Ordering::SeqCst);
    }
    ok
}

/// Send the full desired set to the actor. Returns immediately; the real
/// bindings arrive asynchronously via `on_bindings_changed`.
pub fn set_shortcuts(specs: Vec<ShortcutSpec>) -> bool {
    send(Command::Set(specs))
}

/// Ask the compositor to (re-)present its bind UI for the shortcut set.
pub fn configure(id: &str) -> bool {
    send(Command::Configure(id.to_string()))
}

/// Stop the actor and drop the portal session. Clears the channel so a later
/// `start()` can re-bootstrap.
pub fn stop() {
    if let Some(tx) = CMD_TX.lock().take() {
        let _ = tx.send(Command::Shutdown);
    }
    ACTOR_ALIVE.store(false, Ordering::SeqCst);
}

/// Send a command, clearing dead state if the actor's receiver is gone.
fn send(cmd: Command) -> bool {
    let tx = CMD_TX.lock().clone();
    match tx {
        Some(tx) if tx.send(cmd).is_ok() => true,
        _ => {
            // Receiver dropped (actor died) or never started — mark dead so a
            // future `start()` re-bootstraps rather than no-ops forever.
            mark_dead();
            false
        }
    }
}

/// Clear the live channel + alive flag (actor is gone).
fn mark_dead() {
    *CMD_TX.lock() = None;
    ACTOR_ALIVE.store(false, Ordering::SeqCst);
}

/// The portal actor: owns the proxy, session and signal streams for its whole
/// life and never lets them cross a thread boundary.
async fn actor_main(mut cmd_rx: mpsc::UnboundedReceiver<Command>, ready_tx: std_mpsc::Sender<bool>) {
    let shortcuts = match GlobalShortcuts::new().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[astra-hotkey] portal: GlobalShortcuts proxy failed: {e}");
            let _ = ready_tx.send(false);
            return;
        }
    };
    let mut session = match shortcuts.create_session().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[astra-hotkey] portal: CreateSession failed (portal unavailable?): {e}");
            let _ = ready_tx.send(false);
            return;
        }
    };

    // `Activated` is essential; if it can't be subscribed the portal is unusable.
    // `Deactivated` (PTT release) and `ShortcutsChanged` (live rebind) are
    // best-effort — log and continue without them.
    let mut activated = match shortcuts.receive_activated().await {
        Ok(s) => Box::pin(s),
        Err(e) => {
            eprintln!("[astra-hotkey] portal: receive_activated failed: {e}");
            let _ = ready_tx.send(false);
            return;
        }
    };
    let mut deactivated = match shortcuts.receive_deactivated().await {
        Ok(s) => Some(Box::pin(s)),
        Err(e) => {
            eprintln!("[astra-hotkey] portal: receive_deactivated unavailable: {e}");
            None
        }
    };
    let mut changed = match shortcuts.receive_shortcuts_changed().await {
        Ok(s) => Some(Box::pin(s)),
        Err(e) => {
            eprintln!("[astra-hotkey] portal: receive_shortcuts_changed unavailable: {e}");
            None
        }
    };

    // Session created — the portal is usable.
    let _ = ready_tx.send(true);

    // The set we last bound. Kept so we can (a) skip no-op rebinds, (b) decide
    // whether a new Set requires a fresh session, and (c) let ShortcutsChanged
    // preserve descriptions / surface declined shortcuts as NeedsBinding.
    let mut current: Vec<ShortcutSpec> = Vec::new();
    // Set when the Activated stream terminates (bus dropped): downgrade on exit.
    let mut bus_dropped = false;

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(Command::Set(specs)) => {
                    if specs == current {
                        continue; // no-op sync: never churn a session or pop a dialog
                    }
                    // A non-first change needs a fresh session (portal binds once
                    // per session). The first bind reuses the startup session.
                    if !current.is_empty() {
                        match recreate_session(&shortcuts, &mut session).await {
                            Ok(()) => {}
                            Err(e) => {
                                eprintln!("[astra-hotkey] portal: session recreate failed: {e}");
                                continue;
                            }
                        }
                    }
                    current = specs;
                    bind_set(&shortcuts, &session, &current).await;
                }
                Some(Command::Configure(_id)) => {
                    // No per-shortcut configure verb exists; recreate the session
                    // and re-bind the whole set to re-present the compositor's UI.
                    if let Err(e) = recreate_session(&shortcuts, &mut session).await {
                        eprintln!("[astra-hotkey] portal: session recreate (configure) failed: {e}");
                        continue;
                    }
                    let specs = current.clone();
                    bind_set(&shortcuts, &session, &specs).await;
                }
                Some(Command::Shutdown) | None => break,
            },

            act = activated.next() => match act {
                Some(a) => backend::invoke_activated(a.shortcut_id(), true),
                None => {
                    eprintln!("[astra-hotkey] portal: Activated stream ended (D-Bus connection dropped)");
                    bus_dropped = true;
                    break;
                }
            },

            // Optional streams: a `None` means the stream terminated — drop it so
            // we stop polling a dead stream (which would otherwise busy-spin).
            maybe_deact = next_opt(&mut deactivated) => match maybe_deact {
                Some(d) => backend::invoke_activated(d.shortcut_id(), false),
                None => deactivated = None,
            },
            maybe_changed = next_opt(&mut changed) => match maybe_changed {
                Some(ch) => apply_shortcuts(&current, ch.shortcuts()),
                None => changed = None,
            },
        }
    }

    // Dropping `session` ends the portal subscription cleanly.
    let _ = session.close().await;

    if bus_dropped {
        // The portal is gone: stop reporting phantom-bound shortcuts and let the
        // daemon/UI learn the backend died.
        backend::set_provider(Provider::Unsupported);
        backend::mark_all_unsupported();
        backend::invoke_bindings_changed();
        mark_dead();
    }
}

/// Close the current session and create a fresh one in its place.
async fn recreate_session<'a>(
    shortcuts: &GlobalShortcuts<'a>,
    session: &mut Session<'a, GlobalShortcuts<'a>>,
) -> Result<(), ashpd::Error> {
    // `close()` consumes by ref; we then overwrite `*session`. Closing the old
    // one first unbinds its shortcuts so the new bind is authoritative.
    let _ = session.close().await;
    *session = shortcuts.create_session().await?;
    Ok(())
}

/// Poll an optional stream: if absent, never resolves (so `select!` ignores it).
async fn next_opt<S, T>(s: &mut Option<std::pin::Pin<Box<S>>>) -> Option<T>
where
    S: futures_util::Stream<Item = T> + ?Sized,
{
    match s {
        Some(stream) => stream.next().await,
        None => std::future::pending().await,
    }
}

/// Bind `desired` via the portal, then refresh the cache from the result and
/// fire the change callback. Bounded by [`BIND_TIMEOUT`] so an abandoned
/// compositor dialog can't park the actor forever.
async fn bind_set(
    shortcuts: &GlobalShortcuts<'_>,
    session: &Session<'_, GlobalShortcuts<'_>>,
    desired: &[ShortcutSpec],
) {
    let new_shortcuts: Vec<NewShortcut> = desired
        .iter()
        .map(|s| {
            let ns = NewShortcut::new(&s.id, &s.description);
            match &s.preferred {
                Some(p) if !p.trim().is_empty() => {
                    // The compositor reads triggers in the XDG "shortcuts" spec
                    // form (CTRL+LOGO+a), not Astra's "Ctrl+Win+A".
                    let trigger = astra_to_xdg_trigger(p);
                    ns.preferred_trigger(Some(trigger.as_str()))
                }
                _ => ns,
            }
        })
        .collect();

    let win = WindowIdentifier::default();
    let fut = shortcuts.bind_shortcuts(session, &new_shortcuts, &win);
    match tokio::time::timeout(BIND_TIMEOUT, fut).await {
        Ok(Ok(request)) => match request.response() {
            Ok(resp) => apply_shortcuts(desired, resp.shortcuts()),
            Err(e) => eprintln!("[astra-hotkey] portal: BindShortcuts response error: {e}"),
        },
        Ok(Err(e)) => eprintln!("[astra-hotkey] portal: BindShortcuts failed: {e}"),
        Err(_) => eprintln!(
            "[astra-hotkey] portal: BindShortcuts timed out after {BIND_TIMEOUT:?} (dialog abandoned?)"
        ),
    }
}

/// Merge the compositor's bound shortcuts with our desired specs into the
/// bindings cache, then notify the daemon. A desired spec missing from `bound`
/// is reported as `NeedsBinding` (compositor declined / user hasn't accepted).
fn apply_shortcuts(desired: &[ShortcutSpec], bound: &[Shortcut]) {
    let bindings: Vec<ShortcutBinding> = desired
        .iter()
        .map(|spec| match bound.iter().find(|b| b.id() == spec.id) {
            Some(b) => ShortcutBinding {
                id: spec.id.clone(),
                combo: Some(b.trigger_description().to_string()),
                status: BindingStatus::Bound,
                description: spec.description.clone(),
            },
            None => ShortcutBinding {
                id: spec.id.clone(),
                combo: None,
                status: BindingStatus::NeedsBinding,
                description: spec.description.clone(),
            },
        })
        .collect();

    backend::set_bindings(bindings);
    backend::invoke_bindings_changed();
}

/// Convert an Astra combo (`"Ctrl+Alt+A"`, `"Win+E"`) to the XDG "shortcuts"
/// spec trigger form the portal's `preferred_trigger` expects: modifiers
/// upper-cased (`CTRL`/`ALT`/`SHIFT`/`LOGO`) and the key as an XKB keysym name
/// (single letters lower-cased: `a`). Best-effort — if the compositor can't
/// parse it, the shortcut simply comes back `NeedsBinding` and the user binds it
/// manually, so an imperfect mapping degrades gracefully.
fn astra_to_xdg_trigger(combo: &str) -> String {
    combo
        .split('+')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|part| match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => "CTRL".to_string(),
            "alt" => "ALT".to_string(),
            "shift" => "SHIFT".to_string(),
            "win" | "super" | "meta" | "cmd" | "command" | "logo" => "LOGO".to_string(),
            other => keysym_name(other),
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Map an Astra key token to a plausible XKB keysym name.
fn keysym_name(key: &str) -> String {
    // Single ASCII letter → lower-case keysym ("a").
    if key.len() == 1 {
        let c = key.chars().next().unwrap();
        if c.is_ascii_alphabetic() {
            return c.to_ascii_lowercase().to_string();
        }
        if c.is_ascii_digit() {
            return c.to_string();
        }
    }
    // Function keys keep their upper-case keysym form ("F1").
    if (key.starts_with('f') || key.starts_with('F')) && key.len() <= 3 {
        if key[1..].parse::<u8>().is_ok() {
            return key.to_ascii_uppercase();
        }
    }
    match key {
        "space" => "space".to_string(),
        "enter" | "return" => "Return".to_string(),
        "tab" => "Tab".to_string(),
        "escape" | "esc" => "Escape".to_string(),
        "backspace" => "BackSpace".to_string(),
        "delete" | "del" => "Delete".to_string(),
        "insert" | "ins" => "Insert".to_string(),
        "home" => "Home".to_string(),
        "end" => "End".to_string(),
        "pageup" | "pgup" => "Prior".to_string(),
        "pagedown" | "pgdn" => "Next".to_string(),
        "up" => "Up".to_string(),
        "down" => "Down".to_string(),
        "left" => "Left".to_string(),
        "right" => "Right".to_string(),
        // Unknown: pass through lower-cased (best effort).
        other => other.to_string(),
    }
}
