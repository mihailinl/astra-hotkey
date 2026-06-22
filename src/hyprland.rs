//! Hyprland (and wlroots) **manual-bind** integration for the GlobalShortcuts
//! portal.
//!
//! On Hyprland the portal registers a shortcut *id* but binds **no** key and
//! shows **no** dialog (unlike KDE) — the user must bind a key to the id via the
//! compositor's `global` dispatcher in their config. `preferred_trigger` is
//! ignored. So for the [`PortalModel::Manual`](crate::backend::PortalModel) case
//! we:
//!
//! 1. Register **stable** shortcut ids via the portal (done by `hook_wayland`).
//! 2. Write the `bind …, global, <appid>:<id>` lines to an **Astra-owned** config
//!    file in the right dialect (legacy `hyprland.conf` vs the new Lua config —
//!    since Hyprland 0.55 `hyprland.lua` is loaded in preference to `.conf`).
//! 3. Ask the user **once** (via a native GTK consent dialog showing a diff) to
//!    add a single `source`/`require` line that includes our file, then
//!    `hyprctl reload`.
//!
//! Steps 2–3 live here (Rust, testable). The GTK dialog is a separate, optional
//! helper binary (`bin/consent.rs`, feature `consent-gtk`) so the dlopened cdylib
//! stays free of a GTK dependency and GTK runs in its own process with its own
//! main loop. Verified end-to-end on Hyprland 0.55.4: a stable id registered via
//! the portal + a `global` bind in the config delivers `Activated` to us.

use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
use parking_lot::Mutex;

use crate::backend::{self, BindingStatus, ShortcutBinding, ShortcutSpec};

/// The last desired set applied on the manual path, so the consent flow can
/// re-apply (recompute status + reload) once the include line is added without
/// the portal actor having to round-trip a fresh `BindShortcuts`.
static LAST_DESIRED: Lazy<Mutex<Vec<ShortcutSpec>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// Stable application id we register with the portal (see `hook_wayland::APP_ID`).
/// Hyprland files our shortcuts under `<APP_ID>:<shortcut id>`; the `global` bind
/// target must use exactly this prefix. MUST stay in lockstep with
/// `hook_wayland::APP_ID`.
const APP_ID: &str = "tech.knicetech.astra-daemon";

/// Which config dialect the user's Hyprland loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parser {
    /// New Lua config (`hyprland.lua`) — Hyprland ≥ 0.55, loaded in preference.
    Lua,
    /// Legacy hyprlang (`hyprland.conf`).
    Legacy,
}

impl Parser {
    fn as_str(self) -> &'static str {
        match self {
            Parser::Lua => "lua",
            Parser::Legacy => "legacy",
        }
    }
}

/// A resolved view of the user's Hyprland config layout: which dialect, the
/// Astra-owned file we write, and the user file we add the one-time include to.
#[derive(Debug, Clone)]
pub struct HyprConfig {
    pub parser: Parser,
    /// `~/.config/hypr` (kept for diagnostics / future use, e.g. the consent UI).
    #[allow(dead_code)]
    pub hypr_dir: PathBuf,
    /// The Astra-owned file we (re)generate (`custom/astra.lua` / `astra.conf`).
    pub managed_file: PathBuf,
    /// The user file the one-time include line is appended to.
    pub include_target: PathBuf,
    /// The exact one-time include line (`require("custom.astra")` / `source = …`).
    pub include_line: String,
}

/// True when we're running under Hyprland (env signal — available before the
/// first portal bind, so the daemon can pick the stable-id scheme upfront).
pub fn is_hyprland() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
        || std::env::var("XDG_CURRENT_DESKTOP")
            .map(|d| d.to_ascii_lowercase().contains("hyprland"))
            .unwrap_or(false)
}

/// `~/.config/hypr`, honoring `XDG_CONFIG_HOME`.
fn hypr_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ => {
            let home = std::env::var_os("HOME")?;
            PathBuf::from(home).join(".config")
        }
    };
    Some(base.join("hypr"))
}

/// Resolve the config layout from what's on disk. `None` if we can't locate the
/// hypr dir. Detection: `hyprland.lua` present → Lua (Hyprland prefers it); else
/// Legacy. For the include target we prefer the framework's user-keybinds file
/// (`custom/keybinds.lua`, the end-4 / dots-hyprland convention) so we never
/// touch a framework-owned entry file.
pub fn detect() -> Option<HyprConfig> {
    let dir = hypr_dir()?;
    let lua_entry = dir.join("hyprland.lua");
    let conf_entry = dir.join("hyprland.conf");

    if lua_entry.exists() {
        // Lua. Prefer the `custom/` override dir if present.
        let custom = dir.join("custom");
        if custom.is_dir() {
            let managed = custom.join("astra.lua");
            // The framework already `require`s `custom/keybinds.lua`; appending our
            // include there keeps it inside the user-owned, update-safe area.
            let target = {
                let kb = custom.join("keybinds.lua");
                if kb.exists() { kb } else { lua_entry.clone() }
            };
            return Some(HyprConfig {
                parser: Parser::Lua,
                hypr_dir: dir,
                managed_file: managed,
                include_target: target,
                include_line: "require(\"custom.astra\")".to_string(),
            });
        }
        let managed = dir.join("astra.lua");
        return Some(HyprConfig {
            parser: Parser::Lua,
            hypr_dir: dir,
            managed_file: managed,
            include_target: lua_entry,
            include_line: "require(\"astra\")".to_string(),
        });
    }

    // Legacy (or no file yet — default to legacy `.conf`, the historical layout).
    let managed = dir.join("astra.conf");
    let include_line = format!("source = {}", managed.display());
    Some(HyprConfig {
        parser: Parser::Legacy,
        hypr_dir: dir,
        managed_file: managed,
        include_target: conf_entry,
        include_line,
    })
}

/// A shortcut to emit into the managed file.
pub struct Bind<'a> {
    pub id: &'a str,
    pub description: &'a str,
    pub combo: &'a str,
}

/// Convert an Astra combo (`"Ctrl+Alt+P"`) into Hyprland `(mods, key)`, where
/// `mods` are Hyprland modifier tokens (`CTRL`/`ALT`/`SHIFT`/`SUPER`) and `key`
/// is an XKB keysym Hyprland accepts. `None` if the combo has no non-modifier key.
fn combo_to_hypr(combo: &str) -> Option<(Vec<&'static str>, String)> {
    let mut mods: Vec<&'static str> = Vec::new();
    let mut key: Option<String> = None;
    for part in combo.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => push_unique(&mut mods, "CTRL"),
            "alt" => push_unique(&mut mods, "ALT"),
            "shift" => push_unique(&mut mods, "SHIFT"),
            "win" | "super" | "meta" | "cmd" | "command" | "logo" => push_unique(&mut mods, "SUPER"),
            _ => key = Some(hypr_keysym(part)),
        }
    }
    key.map(|k| (mods, k))
}

fn push_unique(v: &mut Vec<&'static str>, m: &'static str) {
    if !v.contains(&m) {
        v.push(m);
    }
}

/// Map an Astra key token to an XKB keysym name Hyprland accepts. Letters stay
/// upper-case (Hyprland is case-insensitive for letter keys); symbols/named keys
/// map to their keysym. Best-effort — an unmapped token passes through and the
/// user can adjust the shown line.
fn hypr_keysym(key: &str) -> String {
    if key.len() == 1 {
        let c = key.chars().next().unwrap();
        if c.is_ascii_alphabetic() {
            return c.to_ascii_uppercase().to_string();
        }
        if c.is_ascii_digit() {
            return c.to_string();
        }
    }
    // Function keys keep F1..F24.
    if (key.starts_with('F') || key.starts_with('f')) && key.len() <= 3 && key[1..].parse::<u8>().is_ok()
    {
        return key.to_ascii_uppercase();
    }
    match key.to_ascii_lowercase().as_str() {
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
        "-" => "minus".to_string(),
        "=" => "equal".to_string(),
        "[" => "bracketleft".to_string(),
        "]" => "bracketright".to_string(),
        "\\" => "backslash".to_string(),
        ";" => "semicolon".to_string(),
        "'" => "apostrophe".to_string(),
        "," => "comma".to_string(),
        "." => "period".to_string(),
        "/" => "slash".to_string(),
        "`" => "grave".to_string(),
        other => other.to_string(),
    }
}

/// The exact config line that binds `combo` to shortcut `id` in the user's
/// dialect — what the UI shows for copy/paste and what the managed file holds.
/// `None` if the combo lacks a real key.
pub fn bind_line(parser: Parser, b: &Bind) -> Option<String> {
    let (mods, key) = combo_to_hypr(b.combo)?;
    let target = format!("{APP_ID}:{}", b.id);
    Some(match parser {
        Parser::Lua => {
            let mut chord: Vec<String> = mods.iter().map(|m| m.to_string()).collect();
            chord.push(key);
            format!(
                "hl.bind(\"{}\", hl.dsp.global(\"{}\"), {{description = \"{}\"}})",
                chord.join("+"),
                target,
                lua_escape(b.description),
            )
        }
        Parser::Legacy => {
            format!("bind = {}, {}, global, {}", mods.join(" "), key, target)
        }
    })
}

fn lua_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Render the full Astra-owned config file from the desired binds.
pub fn render_managed(parser: Parser, binds: &[Bind]) -> String {
    let mut out = String::new();
    match parser {
        Parser::Lua => {
            out.push_str("-- Astra-managed Hyprland global shortcuts — DO NOT EDIT.\n");
            out.push_str("-- Regenerated by Astra whenever you change a hotkey.\n");
            out.push_str("-- Included from your config via: require(\"custom.astra\")\n\n");
        }
        Parser::Legacy => {
            out.push_str("# Astra-managed Hyprland global shortcuts — DO NOT EDIT.\n");
            out.push_str("# Regenerated by Astra whenever you change a hotkey.\n");
            out.push_str("# Included from your config via: source = this file\n\n");
        }
    }
    for b in binds {
        match bind_line(parser, b) {
            Some(line) => {
                out.push_str(&line);
                out.push('\n');
            }
            None => {
                let c = if parser == Parser::Lua { "--" } else { "#" };
                out.push_str(&format!("{c} (skipped {}: no key in combo {:?})\n", b.id, b.combo));
            }
        }
    }
    out
}

/// Is our include line already present in the target file?
pub fn include_present(cfg: &HyprConfig) -> bool {
    let needle = cfg.include_line.trim();
    match std::fs::read_to_string(&cfg.include_target) {
        Ok(content) => content.lines().any(|l| {
            let l = l.trim();
            // Ignore commented-out occurrences.
            !l.starts_with('#') && !l.starts_with("--") && l == needle
        }),
        Err(_) => false,
    }
}

/// Write the managed file from the binds, only if the content changed (so we
/// don't churn the file or trigger needless reloads). Returns whether a write
/// happened. Creates parent dirs as needed.
pub fn write_managed(cfg: &HyprConfig, binds: &[Bind]) -> std::io::Result<bool> {
    let content = render_managed(cfg.parser, binds);
    if let Ok(existing) = std::fs::read_to_string(&cfg.managed_file) {
        if existing == content {
            return Ok(false);
        }
    }
    if let Some(parent) = cfg.managed_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&cfg.managed_file, content)?;
    Ok(true)
}

/// Ask Hyprland to reload its config (re-reads our managed file). Best-effort.
pub fn reload() -> bool {
    match std::process::Command::new("hyprctl").arg("reload").output() {
        Ok(out) if out.status.success() => {
            backend::log_info("hyprland: hyprctl reload ok".to_string());
            true
        }
        Ok(out) => {
            backend::log_warn(format!(
                "hyprland: hyprctl reload failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
            false
        }
        Err(e) => {
            backend::log_warn(format!("hyprland: hyprctl reload not run: {e}"));
            false
        }
    }
}

/// Apply the desired set on the **manual-bind** path: (re)write the Astra-owned
/// config file, refresh the UI bindings cache (with the desired combo + the exact
/// `bind_hint`), reload Hyprland if the file changed and the include is active,
/// and notify the daemon. This is the manual-model counterpart to
/// `hook_wayland::apply_shortcuts`.
pub fn sync_manual(desired: &[ShortcutSpec]) {
    *LAST_DESIRED.lock() = desired.to_vec();
    sync_manual_inner(desired, false);
}

/// Re-apply the last desired set after the consent flow added the include line —
/// recompute status (now `Bound`) and reload so the new binds go live.
pub fn reapply() {
    let desired = LAST_DESIRED.lock().clone();
    sync_manual_inner(&desired, true);
}

fn sync_manual_inner(desired: &[ShortcutSpec], force_reload: bool) {
    let cfg = detect();

    // Build the Hyprland binds for specs that carry a real key combo.
    let binds: Vec<Bind> = desired
        .iter()
        .filter_map(|s| {
            let combo = s.preferred.as_deref().map(str::trim).filter(|c| !c.is_empty())?;
            Some(Bind { id: &s.id, description: &s.description, combo })
        })
        .collect();

    let mut changed = false;
    let present = match &cfg {
        Some(cfg) => {
            match write_managed(cfg, &binds) {
                Ok(c) => changed = c,
                Err(e) => backend::log_error(format!("hyprland: write {:?} failed: {e}", cfg.managed_file)),
            }
            include_present(cfg)
        }
        None => false,
    };

    // Reload only when the binds actually changed (or we just enabled the include)
    // AND the include is active — otherwise a reload would apply nothing.
    if present && (changed || force_reload) {
        reload();
    }

    let parser = cfg.as_ref().map(|c| c.parser);
    let bindings: Vec<ShortcutBinding> = desired
        .iter()
        .map(|s| {
            let combo = s.preferred.as_deref().map(str::trim).filter(|c| !c.is_empty());
            let hint = match (parser, combo) {
                (Some(parser), Some(combo)) => bind_line(
                    parser,
                    &Bind { id: &s.id, description: &s.description, combo },
                ),
                _ => None,
            };
            ShortcutBinding {
                id: s.id.clone(),
                combo: combo.map(|c| c.to_string()),
                // Once the include is active the key IS wired up (verified: a
                // `global` bind in the config delivers `Activated`), so report it
                // bound; otherwise it awaits the one-time compositor setup.
                status: if present {
                    BindingStatus::Bound
                } else {
                    BindingStatus::NeedsCompositorBind
                },
                description: s.description.clone(),
                bind_hint: hint,
            }
        })
        .collect();

    for b in &bindings {
        match b.status {
            BindingStatus::Bound => backend::log_info(format!(
                "hyprland: {} -> {} (via compositor global bind)",
                b.id,
                b.combo.as_deref().unwrap_or("?")
            )),
            BindingStatus::NeedsCompositorBind => backend::log_warn(format!(
                "hyprland: {} awaiting one-time config setup (suggested: {})",
                b.id,
                b.bind_hint.as_deref().unwrap_or("?")
            )),
            _ => {}
        }
    }

    backend::set_bindings(bindings);
    backend::invoke_bindings_changed();
}

/// Locate the GTK consent helper binary. Co-located with the host executable
/// (deployed next to the daemon + the `.so`), or overridden via
/// `ASTRA_HOTKEY_CONSENT_BIN` for dev.
fn consent_helper() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("ASTRA_HOTKEY_CONSENT_BIN") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    for name in ["astra-hotkey-consent", "astra_hotkey_consent"] {
        let cand = dir.join(name);
        if cand.exists() {
            return Some(cand);
        }
    }
    None
}

/// Spawn the native GTK consent dialog (pure UI: shows a diff of the one-time
/// include edit, returns exit 0 = confirm / non-0 = cancel), and on confirm
/// append the include line **here** (the tested [`append_include`]) and
/// [`reapply`] so the new binds go live. Returns `false` if no helper is
/// available — the caller falls back to showing the manual line in the UI.
///
/// Runs the wait on a detached thread so the portal actor is never blocked on
/// user-interaction time.
pub fn run_consent(cfg: &HyprConfig) -> bool {
    let Some(helper) = consent_helper() else {
        backend::log_warn(
            "hyprland: consent helper not found — UI will show the manual config line".to_string(),
        );
        return false;
    };
    let target = cfg.include_target.clone();
    let line = cfg.include_line.clone();
    let managed = cfg.managed_file.clone();
    std::thread::spawn(move || {
        let status = std::process::Command::new(&helper)
            .arg("--target")
            .arg(&target)
            .arg("--line")
            .arg(&line)
            .arg("--managed")
            .arg(&managed)
            .status();
        match status {
            Ok(s) if s.success() => match append_include(&target, &line) {
                Ok(_) => {
                    backend::log_info(format!(
                        "hyprland: consent confirmed — include added to {}",
                        target.display()
                    ));
                    // Recompute status (now bound) + reload so the binds go live.
                    reapply();
                }
                Err(e) => backend::log_error(format!(
                    "hyprland: failed to add include to {}: {e}",
                    target.display()
                )),
            },
            Ok(_) => backend::log_info("hyprland: consent dialog cancelled".to_string()),
            Err(e) => backend::log_warn(format!("hyprland: consent helper failed: {e}")),
        }
    });
    true
}

/// Compositor-config state for the UI (`hotkey_compositor_info`).
pub fn compositor_info_json() -> String {
    let model = backend::portal_model();
    let parser_str;
    let managed;
    let include_target;
    let present;
    match detect() {
        Some(cfg) => {
            parser_str = cfg.parser.as_str();
            managed = cfg.managed_file.display().to_string();
            include_target = cfg.include_target.display().to_string();
            present = include_present(&cfg);
        }
        None => {
            parser_str = "unknown";
            managed = String::new();
            include_target = String::new();
            present = false;
        }
    }
    let needs_setup = model == backend::PortalModel::Manual && !present;
    format!(
        "{{\"model\":\"{}\",\"parser\":\"{}\",\"managed_file\":{},\"include_target\":{},\"include_present\":{},\"needs_setup\":{}}}",
        model.as_str(),
        parser_str,
        json_str(&managed),
        json_str(&include_target),
        present,
        needs_setup,
    )
}

/// Minimal JSON string encoder (we only emit a couple of file paths).
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Is `line` already an active (uncommented) entry in `content`?
fn line_present(content: &str, line: &str) -> bool {
    let needle = line.trim();
    content.lines().any(|l| {
        let l = l.trim();
        !l.starts_with('#') && !l.starts_with("--") && l == needle
    })
}

/// Append the include line to the target file if missing. Used by the consent
/// helper (and unit-tested here). Idempotent. Creates the file if absent ONLY
/// when its parent dir already exists (never fabricates a brand-new entry config
/// out of nowhere). Returns whether a write happened.
pub fn append_include(target: &Path, line: &str) -> std::io::Result<bool> {
    // Hyprland's legacy `.conf` comments with `#`; the Lua config with `--`.
    let comment = if target.extension().map(|e| e == "conf").unwrap_or(false) {
        "#"
    } else {
        "--"
    };
    let block = format!("\n{comment} Added by Astra: include its managed global shortcuts.\n{line}\n");

    match std::fs::read_to_string(target) {
        Ok(content) => {
            // Re-check presence to stay idempotent even across races.
            if line_present(&content, line) {
                return Ok(false);
            }
            let mut new = content;
            if !new.is_empty() && !new.ends_with('\n') {
                new.push('\n');
            }
            new.push_str(&block);
            std::fs::write(target, new)?;
            Ok(true)
        }
        // No file yet: only create inside an established hypr dir (never fabricate
        // a brand-new entry config — an almost-empty one would strip Hyprland's
        // compiled-in defaults).
        Err(_) => match target.parent() {
            Some(parent) if parent.is_dir() => {
                std::fs::write(target, block.trim_start_matches('\n'))?;
                Ok(true)
            }
            _ => Ok(false),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combo_converts_to_hypr_lua_and_legacy() {
        let b = Bind { id: "astra.overlay", description: "Astra: Overlay", combo: "Ctrl+Alt+P" };
        assert_eq!(
            bind_line(Parser::Lua, &b).unwrap(),
            "hl.bind(\"CTRL+ALT+P\", hl.dsp.global(\"tech.knicetech.astra-daemon:astra.overlay\"), {description = \"Astra: Overlay\"})"
        );
        assert_eq!(
            bind_line(Parser::Legacy, &b).unwrap(),
            "bind = CTRL ALT, P, global, tech.knicetech.astra-daemon:astra.overlay"
        );
    }

    #[test]
    fn space_and_symbol_keys_map_to_keysyms() {
        let ptt = Bind { id: "astra.ptt", description: "Astra: PTT", combo: "Ctrl+Space" };
        assert_eq!(
            bind_line(Parser::Legacy, &ptt).unwrap(),
            "bind = CTRL, space, global, tech.knicetech.astra-daemon:astra.ptt"
        );
        let slash = Bind { id: "x", description: "d", combo: "Super+/" };
        assert_eq!(
            bind_line(Parser::Lua, &slash).unwrap(),
            "hl.bind(\"SUPER+slash\", hl.dsp.global(\"tech.knicetech.astra-daemon:x\"), {description = \"d\"})"
        );
    }

    #[test]
    fn no_key_combo_yields_none() {
        let b = Bind { id: "x", description: "d", combo: "Ctrl+Alt" };
        assert!(bind_line(Parser::Lua, &b).is_none());
    }

    #[test]
    fn managed_file_has_header_and_lines() {
        let binds = [
            Bind { id: "astra.overlay", description: "Astra: Overlay", combo: "Ctrl+Alt+P" },
            Bind { id: "astra.ptt", description: "Astra: PTT", combo: "Alt+Space" },
        ];
        let lua = render_managed(Parser::Lua, &binds);
        assert!(lua.contains("require(\"custom.astra\")"));
        assert!(lua.contains("astra.overlay"));
        assert!(lua.contains("astra.ptt"));
        let legacy = render_managed(Parser::Legacy, &binds);
        assert!(legacy.starts_with("# Astra-managed"));
        assert!(legacy.contains("bind = CTRL ALT, P, global, tech.knicetech.astra-daemon:astra.overlay"));
    }

    #[test]
    fn append_include_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("astra-hk-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("keybinds.lua");
        std::fs::write(&target, "hl.bind(\"F1\", hl.dsp.exec_cmd(\"x\"))\n").unwrap();
        let line = "require(\"custom.astra\")";
        assert!(append_include(&target, line).unwrap());
        // Second call: already present → no write.
        assert!(!append_include(&target, line).unwrap());
        let content = std::fs::read_to_string(&target).unwrap();
        assert_eq!(content.matches(line).count(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
