//! Linux backend selection and the id-based dispatch shared by both Linux
//! backends.
//!
//! At `hotkey_init2` time we decide — once — between:
//!   * **Portal** (`hook_wayland`): Wayland session with a working GlobalShortcuts
//!     portal. The compositor owns the key; "Configure" opens its dialog.
//!   * **X11** (`hook_linux`): X11 session. We grab silently via `global-hotkey`;
//!     "Configure" is handled in-app (the daemon shows its recorder).
//!   * **Unsupported**: Wayland without a portal. We still answer `provider` and
//!     `get_bindings` so the UI can explain it.
//!
//! The portal is id-native. X11 is combo-native, so its id support is a thin
//! translation: we register each spec's `preferred` combo (reusing the existing
//! grab in `registry`/`hook_linux`) and record combo→id in `backend`, so the
//! shared `invoke_callback` funnel turns a fired combo into `on_activated(id)`.

use crate::backend::{self, BindingStatus, PortalModel, Provider, ShortcutBinding, ShortcutSpec};
use crate::{hook_linux, hook_wayland, hyprland, registry};

/// Is this a Wayland session? Prefer the explicit session type, fall back to the
/// presence of a Wayland display socket.
fn is_wayland() -> bool {
    if let Ok(t) = std::env::var("XDG_SESSION_TYPE") {
        if t.eq_ignore_ascii_case("wayland") {
            return true;
        }
        if t.eq_ignore_ascii_case("x11") {
            return false;
        }
    }
    std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// Does an X11 display exist (native X11, or XWayland for an X11 session)?
fn has_x11() -> bool {
    std::env::var_os("DISPLAY").is_some()
}

/// Decide the backend and start it. Called from `hotkey_init2`. Returns `true`
/// if the library is usable enough to answer queries (always true except on a
/// hard failure), and sets `backend::provider()` to the real selection.
///
/// Note we do **not** fall back from a portal-less Wayland session to X11:
/// `XGrabKey` on XWayland does not see keys routed to native Wayland windows, so
/// it would be a silent half-working trap. Honest "Unsupported" is better.
pub fn init() -> bool {
    if is_wayland() {
        if hook_wayland::start() {
            backend::set_provider(Provider::Portal);
            // Pick the portal binding model up front (the daemon queries it via
            // `hotkey_portal_model()` to choose the shortcut-id scheme BEFORE the
            // first bind): Hyprland/wlroots register the id but expect the user to
            // bind a key in their config (Manual); KDE/GNOME pop a dialog and bind
            // it themselves (Dialog).
            backend::set_portal_model(if hyprland::is_hyprland() {
                PortalModel::Manual
            } else {
                PortalModel::Dialog
            });
            return true;
        }
        // Wayland but no usable portal.
        backend::set_provider(Provider::Unsupported);
        return true;
    }

    // X11 session.
    if has_x11() && hook_linux::start_hook() {
        backend::set_provider(Provider::X11);
        return true;
    }

    backend::set_provider(Provider::Unsupported);
    true
}

/// Replace the whole shortcut set. Routes to the active backend.
pub fn set_shortcuts(specs: Vec<ShortcutSpec>) -> bool {
    match backend::provider() {
        Provider::Portal => hook_wayland::set_shortcuts(specs),
        Provider::X11 => {
            set_owned(specs);
            true
        }
        // Unsupported: still record the desired set as the bindings cache (all
        // Unsupported) so the UI can list the shortcuts and explain why none work.
        _ => {
            let bindings = specs
                .into_iter()
                .map(|s| ShortcutBinding {
                    id: s.id,
                    combo: None,
                    status: BindingStatus::Unsupported,
                    description: s.description,
                    bind_hint: None,
                })
                .collect();
            backend::set_bindings(bindings);
            false
        }
    }
}

/// Open the OS config UI for a shortcut. Only meaningful on the portal; owned
/// backends use the daemon's in-app recorder, so this is a no-op there.
pub fn configure(id: &str) -> bool {
    match backend::provider() {
        Provider::Portal => match backend::portal_model() {
            // Manual-bind (Hyprland): "configure" means run the one-time config
            // setup — show the native consent diff to add the include line, then
            // reload. The bind lines themselves were already written on sync.
            PortalModel::Manual => match hyprland::detect() {
                Some(cfg) => hyprland::run_consent(&cfg),
                None => false,
            },
            // Dialog (KDE/GNOME): re-present the compositor's own bind dialog.
            _ => hook_wayland::configure(id),
        },
        _ => false,
    }
}

/// X11 (owned) `set_shortcuts`: grab each spec's preferred combo and record
/// combo→id so activations dispatch by id.
///
/// Ordering matters: we populate the combo→id table (which also flips
/// `OWNED_ID_MODE`) **before** issuing any `XGrabKey`, so a press landing the
/// instant a combo is grabbed resolves to its id instead of being dropped by the
/// `invoke_callback` funnel. We also read the **actual** grab result from
/// `hook_linux::register` (not a set-insert bool), so a combo already held by
/// another app is honestly reported `NeedsBinding` rather than a phantom `Bound`.
fn set_owned(specs: Vec<ShortcutSpec>) {
    // Drop previous grabs (and any stale legacy set state).
    hook_linux::unregister_all();

    // The X11 listener emits the *normalized* combo string, so key the table by
    // exactly that — `registry::normalize_hotkey` produces the same form
    // `hook_linux::register` stores it under.
    let prepared: Vec<(ShortcutSpec, Option<String>)> = specs
        .into_iter()
        .map(|spec| {
            let normalized = spec
                .preferred
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(registry::normalize_hotkey);
            (spec, normalized)
        })
        .collect();

    let map: Vec<(String, String)> = prepared
        .iter()
        .filter_map(|(spec, combo)| combo.clone().map(|c| (c, spec.id.clone())))
        .collect();
    backend::owned_set_map(map); // table + OWNED_ID_MODE live BEFORE any grab

    let bindings: Vec<ShortcutBinding> = prepared
        .into_iter()
        .map(|(spec, combo)| match combo {
            None => ShortcutBinding {
                id: spec.id,
                combo: None,
                status: BindingStatus::NeedsBinding,
                description: spec.description,
                bind_hint: None,
            },
            Some(normalized) => {
                let ok = hook_linux::register(&normalized); // real XGrabKey result
                ShortcutBinding {
                    id: spec.id,
                    combo: Some(normalized),
                    status: if ok {
                        BindingStatus::Bound
                    } else {
                        BindingStatus::NeedsBinding
                    },
                    description: spec.description,
                    bind_hint: None,
                }
            }
        })
        .collect();

    backend::set_bindings(bindings);
    backend::invoke_bindings_changed();
}
