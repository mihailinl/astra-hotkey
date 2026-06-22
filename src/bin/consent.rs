//! `astra-hotkey-consent` — a tiny native GTK4 consent dialog.
//!
//! Hyprland's GlobalShortcuts portal registers a shortcut id but never binds a
//! key (and shows no dialog); the user must add one `source`/`require` line to
//! their Hyprland config so it includes Astra's managed bind file. Rather than
//! silently editing the user's config, the daemon (via the `astra-hotkey` crate)
//! spawns this helper to show a **GitHub-Desktop-style diff** of that one-line
//! change and ask for confirmation.
//!
//! This binary is **pure UI**: it shows the diff and exits `0` (confirm) or `1`
//! (cancel). The crate performs the actual file append + `hyprctl reload` after a
//! confirm, so the file-mutation logic stays in one tested place. Kept as a
//! separate process (and behind the `consent-gtk` feature) so the dlopened
//! cdylib never links GTK and GTK runs on its own main loop.
//!
//! Args: `--target <file>` (the config file the line is added to), `--line <s>`
//! (the include line), `--managed <file>` (Astra's generated binds file, opened
//! by the "View" button).

use std::cell::Cell;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;

use gtk4::gio::ApplicationFlags;
use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, Label, Orientation,
    ScrolledWindow, TextBuffer, TextView,
};

struct Args {
    target: PathBuf,
    line: String,
    managed: PathBuf,
}

fn parse_args() -> Args {
    let mut target = PathBuf::new();
    let mut line = String::new();
    let mut managed = PathBuf::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--target" => target = PathBuf::from(it.next().unwrap_or_default()),
            "--line" => line = it.next().unwrap_or_default(),
            "--managed" => managed = PathBuf::from(it.next().unwrap_or_default()),
            _ => {}
        }
    }
    Args { target, line, managed }
}

fn main() {
    let args = Rc::new(parse_args());
    // NON_UNIQUE: this is a one-shot dialog, not a singleton app — never hand off
    // to an existing instance. A valid reverse-DNS id keeps the WM/portal happy.
    let app = Application::new(
        Some("tech.knicetech.astra.hotkeyconsent"),
        ApplicationFlags::NON_UNIQUE,
    );
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

fn build_ui(app: &Application, args: &Args, confirmed: Rc<Cell<bool>>) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Astra · Enable global hotkeys")
        .default_width(620)
        .default_height(460)
        .resizable(true)
        .build();

    let root = GtkBox::new(Orientation::Vertical, 12);
    root.set_margin_top(16);
    root.set_margin_bottom(16);
    root.set_margin_start(16);
    root.set_margin_end(16);

    let heading = Label::new(None);
    heading.set_markup("<span size='large' weight='bold'>Enable Astra global hotkeys on Hyprland</span>");
    heading.set_halign(Align::Start);
    root.append(&heading);

    let explain = Label::new(Some(&format!(
        "Hyprland doesn't bind keys to app shortcuts automatically. Astra wrote its \
         shortcut binds to:\n  {}\n\nTo activate them, this one line will be added to your \
         Hyprland config ({}). Review the change below — nothing is written until you confirm.",
        args.managed.display(),
        args.target.display(),
    )));
    explain.set_wrap(true);
    explain.set_halign(Align::Start);
    explain.set_xalign(0.0);
    root.append(&explain);

    // Diff view (context lines + the added line).
    let buffer = TextBuffer::new(None);
    let ctx_tag = buffer
        .create_tag(Some("ctx"), &[("foreground", &"#8b949e")])
        .expect("ctx tag");
    let add_tag = buffer
        .create_tag(
            Some("add"),
            &[("foreground", &"#2ea043"), ("weight", &700i32)],
        )
        .expect("add tag");
    let head_tag = buffer
        .create_tag(Some("head"), &[("foreground", &"#6e7781")])
        .expect("head tag");

    let existing = std::fs::read_to_string(&args.target).unwrap_or_default();
    let mut iter = buffer.start_iter();
    let fname = args
        .target
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| args.target.display().to_string());
    buffer.insert_with_tags(&mut iter, &format!("--- {fname}\n"), &[&head_tag]);
    if existing.trim().is_empty() {
        buffer.insert_with_tags(&mut iter, "  (new file)\n", &[&ctx_tag]);
    } else {
        let lines: Vec<&str> = existing.lines().collect();
        let start = lines.len().saturating_sub(8);
        if start > 0 {
            buffer.insert_with_tags(&mut iter, "  …\n", &[&ctx_tag]);
        }
        for l in &lines[start..] {
            buffer.insert_with_tags(&mut iter, &format!("  {l}\n"), &[&ctx_tag]);
        }
    }
    buffer.insert_with_tags(&mut iter, &format!("+ {}\n", args.line), &[&add_tag]);

    let text = TextView::with_buffer(&buffer);
    text.set_editable(false);
    text.set_cursor_visible(false);
    text.set_monospace(true);
    text.set_left_margin(8);
    text.set_top_margin(8);

    let scroll = ScrolledWindow::builder()
        .child(&text)
        .vexpand(true)
        .hexpand(true)
        .min_content_height(160)
        .build();
    scroll.add_css_class("frame");
    root.append(&scroll);

    // Buttons.
    let buttons = GtkBox::new(Orientation::Horizontal, 8);
    buttons.set_halign(Align::End);

    let view_btn = Button::with_label("View Astra config");
    {
        let managed = args.managed.clone();
        view_btn.connect_clicked(move |_| {
            let _ = Command::new("xdg-open").arg(&managed).spawn();
        });
    }
    view_btn.set_halign(Align::Start);
    view_btn.set_hexpand(true);

    let cancel_btn = Button::with_label("Cancel");
    {
        let window = window.clone();
        cancel_btn.connect_clicked(move |_| window.close());
    }

    let add_btn = Button::with_label("Add to my config");
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
    root.append(&buttons);

    window.set_child(Some(&root));
    window.present();
}
