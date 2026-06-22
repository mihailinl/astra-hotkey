//! `astra-hotkey-consent` — a small native GTK4 consent dialog.
//!
//! Hyprland's GlobalShortcuts portal registers a shortcut id but binds no key
//! (and shows no dialog); the user must add ONE `source`/`require` line to their
//! Hyprland config so it includes Astra's managed bind file. Rather than silently
//! editing the user's config, the daemon (via the `astra-hotkey` crate) spawns
//! this helper to show a GitHub-style **diff** of that one-line change and ask
//! for confirmation.
//!
//! Pure UI: shows the diff, exits `0` (confirm) / non-`0` (cancel). The crate
//! performs the actual file append + `hyprctl reload` after a confirm, so the
//! file-mutation logic stays in one tested place. Separate process (+ behind the
//! `consent-gtk` feature) so the dlopened cdylib never links GTK.
//!
//! Localized from the system locale (en/ru/uk; falls back to en). Layout is fully
//! adaptive — the content scrolls, the action buttons are pinned at the bottom,
//! so nothing clips at any window/tile size.
//!
//! Args: `--target <file>` (the config file the line is added to), `--line <s>`
//! (the include line), `--managed <file>` (Astra's generated binds file, shown in
//! the file manager by the "View" button).

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

use gtk4::gdk::Display;
use gtk4::gio::ApplicationFlags;
use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, CssProvider, HeaderBar, Label,
    Orientation, PolicyType, ScrolledWindow, TextBuffer, TextView,
};

/// The GTK app id — also the Wayland app_id Hyprland matches with `class:`.
const APP_ID: &str = "tech.knicetech.astra.hotkeyconsent";

struct Args {
    target: PathBuf,
    line: String,
    managed: PathBuf,
    /// Explicit UI locale from the daemon (matches the app); overrides system LANG.
    lang: Option<String>,
}

fn parse_args() -> Args {
    let mut target = PathBuf::new();
    let mut line = String::new();
    let mut managed = PathBuf::new();
    let mut lang = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--target" => target = PathBuf::from(it.next().unwrap_or_default()),
            "--line" => line = it.next().unwrap_or_default(),
            "--managed" => managed = PathBuf::from(it.next().unwrap_or_default()),
            "--lang" => lang = it.next().filter(|s| !s.is_empty()),
            _ => {}
        }
    }
    Args { target, line, managed, lang }
}

/// Localized strings. Self-contained (the helper is a standalone binary) — mirror
/// the daemon UI's primary locales; everything else falls back to English.
struct Strings {
    title: &'static str,
    explain: &'static str,
    file_label: &'static str,
    view: &'static str,
    cancel: &'static str,
    add: &'static str,
}

/// Normalize a locale string to one of our supported codes, or `None`.
fn norm_lang(v: &str) -> Option<&'static str> {
    let l = v.to_ascii_lowercase();
    if l.starts_with("ru") {
        Some("ru")
    } else if l.starts_with("uk") {
        Some("uk")
    } else if l.starts_with("en") {
        Some("en")
    } else {
        None
    }
}

/// Resolve the locale: the explicit `--lang` (the Astra UI language, passed by the
/// daemon) wins; otherwise fall back to the environment (`LC_ALL`/`LC_MESSAGES`/
/// `LANG`); finally English.
fn resolve_lang(explicit: Option<&str>) -> &'static str {
    if let Some(code) = explicit.and_then(norm_lang) {
        return code;
    }
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(var) {
            if let Some(code) = norm_lang(&v) {
                return code;
            }
        }
    }
    "en"
}

fn strings(lang: &str) -> Strings {
    match lang {
        "ru" => Strings {
            title: "Включить глобальные хоткеи Astra в Hyprland",
            explain: "Hyprland не привязывает клавиши к шорткатам приложений автоматически. \
                      Astra записала свои привязки в отдельный файл конфигурации. Чтобы они \
                      заработали, в ваш конфиг Hyprland будет добавлена строка ниже, \
                      подключающая этот файл. Ничего не записывается без вашего подтверждения.",
            file_label: "Файл:",
            view: "Открыть файл Astra",
            cancel: "Отмена",
            add: "Добавить в конфиг",
        },
        "uk" => Strings {
            title: "Увімкнути глобальні гарячі клавіші Astra у Hyprland",
            explain: "Hyprland не прив'язує клавіші до скорочень застосунків автоматично. \
                      Astra записала свої прив'язки в окремий файл конфігурації. Щоб вони \
                      запрацювали, у ваш конфіг Hyprland буде додано рядок нижче, що підключає \
                      цей файл. Нічого не записується без вашого підтвердження.",
            file_label: "Файл:",
            view: "Відкрити файл Astra",
            cancel: "Скасувати",
            add: "Додати в конфіг",
        },
        _ => Strings {
            title: "Enable Astra global hotkeys on Hyprland",
            explain: "Hyprland doesn't bind keys to app shortcuts automatically. Astra wrote its \
                      shortcut binds to its own config file. To activate them, the line below \
                      will be added to your Hyprland config so it includes that file. Nothing is \
                      written until you confirm.",
            file_label: "File:",
            view: "View Astra file",
            cancel: "Cancel",
            add: "Add to config",
        },
    }
}

fn main() {
    let args = Rc::new(parse_args());
    let app = Application::new(Some(APP_ID), ApplicationFlags::NON_UNIQUE);
    let confirmed = Rc::new(Cell::new(false));
    {
        let args = args.clone();
        let confirmed = confirmed.clone();
        app.connect_activate(move |app| build_ui(app, &args, confirmed.clone()));
    }
    // Run with NO args so GApplication doesn't try to parse our `--target` flags.
    app.run_with_args::<&str>(&[]);
    std::process::exit(if confirmed.get() { 0 } else { 1 });
}

fn install_css() {
    let provider = CssProvider::new();
    provider.load_from_data(
        ".astra-diff { \
            font-family: \"JetBrains Mono\", \"Fira Code\", \"DejaVu Sans Mono\", monospace; \
            font-size: 11pt; \
            background-color: #0d1117; \
            padding: 10px; \
            border-radius: 8px; \
         } \
         .astra-diff text { background-color: #0d1117; }",
    );
    if let Some(display) = Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn build_ui(app: &Application, args: &Args, confirmed: Rc<Cell<bool>>) {
    install_css();
    let s = strings(resolve_lang(args.lang.as_deref()));

    let window = ApplicationWindow::builder()
        .application(app)
        .title(s.title)
        .default_width(700)
        .default_height(540)
        .resizable(true)
        .build();

    let header = HeaderBar::new();
    header.set_show_title_buttons(true);
    window.set_titlebar(Some(&header));

    // outer = [ scrollable content (grows) ] + [ pinned button bar ]
    let outer = GtkBox::new(Orientation::Vertical, 0);

    let content = GtkBox::new(Orientation::Vertical, 12);
    content.set_margin_top(16);
    content.set_margin_bottom(8);
    content.set_margin_start(18);
    content.set_margin_end(18);

    let heading = Label::new(None);
    heading.set_markup(&format!(
        "<span size='large' weight='bold'>{}</span>",
        markup_escape(s.title)
    ));
    heading.set_halign(Align::Start);
    heading.set_wrap(true);
    heading.set_xalign(0.0);
    content.append(&heading);

    let explain = Label::new(Some(s.explain));
    explain.set_wrap(true);
    explain.set_halign(Align::Start);
    explain.set_xalign(0.0);
    content.append(&explain);

    let target_lbl = Label::new(None);
    target_lbl.set_markup(&format!(
        "<span color='#9399b2'>{} <tt>{}</tt></span>",
        markup_escape(s.file_label),
        markup_escape(&args.target.display().to_string()),
    ));
    target_lbl.set_halign(Align::Start);
    target_lbl.set_wrap(true);
    target_lbl.set_xalign(0.0);
    content.append(&target_lbl);

    content.append(&build_diff(args));

    let scroll = ScrolledWindow::builder()
        .child(&content)
        .vexpand(true)
        .hexpand(true)
        .hscrollbar_policy(PolicyType::Automatic)
        .vscrollbar_policy(PolicyType::Automatic)
        .build();
    outer.append(&scroll);

    // Pinned action bar — always visible, never clipped.
    let buttons = GtkBox::new(Orientation::Horizontal, 8);
    buttons.set_margin_top(10);
    buttons.set_margin_bottom(14);
    buttons.set_margin_start(18);
    buttons.set_margin_end(18);

    let view_btn = Button::with_label(s.view);
    {
        let managed = args.managed.clone();
        view_btn.connect_clicked(move |_| show_in_file_manager(&managed));
    }
    view_btn.set_halign(Align::Start);
    view_btn.set_hexpand(true);

    let cancel_btn = Button::with_label(s.cancel);
    {
        let window = window.clone();
        cancel_btn.connect_clicked(move |_| window.close());
    }

    let add_btn = Button::with_label(s.add);
    add_btn.add_css_class("suggested-action");
    {
        let window = window.clone();
        let confirmed = confirmed.clone();
        add_btn.connect_clicked(move |_| {
            confirmed.set(true);
            window.close();
        });
    }

    buttons.append(&view_btn);
    buttons.append(&cancel_btn);
    buttons.append(&add_btn);
    outer.append(&buttons);

    window.set_child(Some(&outer));
    window.present();
}

/// A unified-diff TextView with a line-number gutter, GitHub-dark palette, and
/// full-row green (added) / red (removed) backgrounds. We only ever ADD lines, so
/// the red style is present for completeness but unused here.
fn build_diff(args: &Args) -> TextView {
    let buffer = TextBuffer::new(None);
    let gutter = buffer
        .create_tag(Some("gutter"), &[("foreground", &"#6e7681")])
        .unwrap();
    let ctx = buffer
        .create_tag(Some("ctx"), &[("foreground", &"#c9d1d9")])
        .unwrap();
    let add = buffer
        .create_tag(
            Some("add"),
            &[("foreground", &"#aff5b4"), ("paragraph-background", &"#13351f")],
        )
        .unwrap();
    let _del = buffer
        .create_tag(
            Some("del"),
            &[("foreground", &"#ffc1bb"), ("paragraph-background", &"#3a1a1f")],
        )
        .unwrap();

    let existing = std::fs::read_to_string(&args.target).unwrap_or_default();
    let lines: Vec<&str> = existing.lines().collect();
    let start = lines.len().saturating_sub(6);
    let mut iter = buffer.start_iter();

    if start > 0 {
        buffer.insert_with_tags(&mut iter, "   ⋮\n", &[&gutter]);
    }
    for (idx, l) in lines.iter().enumerate().skip(start) {
        buffer.insert_with_tags(&mut iter, &format!("{:>4}    ", idx + 1), &[&gutter]);
        buffer.insert_with_tags(&mut iter, &format!("{l}\n"), &[&ctx]);
    }
    // The added block (mirrors what the crate appends): a comment + the include.
    let base = lines.len();
    let comment = if args.target.extension().map(|e| e == "conf").unwrap_or(false) {
        "# Added by Astra: include its managed global shortcuts."
    } else {
        "-- Added by Astra: include its managed global shortcuts."
    };
    for (off, text) in [comment, args.line.as_str()].iter().enumerate() {
        buffer.insert_with_tags(&mut iter, &format!("{:>4}  + ", base + off + 1), &[&gutter]);
        buffer.insert_with_tags(&mut iter, &format!("{text}\n"), &[&add]);
    }

    let text = TextView::with_buffer(&buffer);
    text.set_editable(false);
    text.set_cursor_visible(false);
    text.set_monospace(true);
    text.add_css_class("astra-diff");
    text.set_size_request(-1, 130);
    text
}

/// Reveal `path` in the user's file manager with the item selected, via the
/// freedesktop `org.freedesktop.FileManager1.ShowItems` D-Bus call (Nautilus,
/// Dolphin, Nemo, Thunar, …). Falls back to opening the containing folder.
fn show_in_file_manager(path: &Path) {
    let uri = format!("file://{}", path.display());
    let ok = Command::new("dbus-send")
        .args([
            "--session",
            "--print-reply",
            "--dest=org.freedesktop.FileManager1",
            "--type=method_call",
            "/org/freedesktop/FileManager1",
            "org.freedesktop.FileManager1.ShowItems",
            &format!("array:string:{uri}"),
            "string:",
        ])
        .status()
        .map(|st| st.success())
        .unwrap_or(false);
    if !ok {
        if let Some(parent) = path.parent() {
            let _ = Command::new("xdg-open").arg(parent).spawn();
        }
    }
}

/// Minimal Pango-markup escaper for the strings we interpolate.
fn markup_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
