//! System accent colour outside GNOME (ported from Rustle's `accent.rs`).
//!
//! libadwaita learns the accent from the settings portal, which only carries it
//! under GNOME's portal backend. Elsewhere it silently falls back to blue, so when
//! it reports no system support we read `org.gnome.desktop.interface accent-color`
//! ourselves and override its `--accent-*` CSS variables.
use adw::prelude::*;
use gtk4 as gtk;
use gtk::gio;
use std::cell::RefCell;

const INTERFACE_SCHEMA: &str = "org.gnome.desktop.interface";

thread_local! {
    static FALLBACK: RefCell<Option<Fallback>> = const { RefCell::new(None) };
}

struct Fallback {
    settings: gio::Settings,
    provider: gtk::CssProvider,
}

/// Install the GSettings fallback if libadwaita can't see the system accent.
/// Call once, after the display exists and before the first window.
pub fn install_fallback(display: &gtk::gdk::Display) {
    let manager = adw::StyleManager::default();
    if manager.is_system_supports_accent_colors() { return; }
    let source = gio::SettingsSchemaSource::default();
    if source.is_none_or(|s| s.lookup(INTERFACE_SCHEMA, true).is_none()) {
        tracing::info!("no {INTERFACE_SCHEMA} schema; keeping libadwaita's default accent");
        return;
    }
    let settings = gio::Settings::new(INTERFACE_SCHEMA);
    if !settings.settings_schema().is_some_and(|s| s.has_key("accent-color")) { return; }
    tracing::info!("portal has no accent colour; following {INTERFACE_SCHEMA} accent-color");

    let provider = gtk::CssProvider::new();
    gtk::style_context_add_provider_for_display(display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
    let fallback = Fallback { settings, provider };
    apply(&fallback, manager.is_dark());
    FALLBACK.with(|cell| *cell.borrow_mut() = Some(fallback));

    let refresh = || {
        let dark = adw::StyleManager::default().is_dark();
        FALLBACK.with(|cell| if let Some(f) = cell.borrow().as_ref() { apply(f, dark); });
    };
    FALLBACK.with(|cell| {
        if let Some(f) = cell.borrow().as_ref() {
            f.settings.connect_changed(Some("accent-color"), move |_, _| refresh());
        }
    });
    manager.connect_dark_notify(move |_| refresh());
}

fn apply(fallback: &Fallback, dark: bool) {
    let accent = parse(&fallback.settings.string("accent-color"));
    let css = format!(
        ":root {{ --accent-bg-color: {}; --accent-fg-color: #ffffff; --accent-color: {}; }}",
        rgba_hex(&accent.to_rgba()),
        rgba_hex(&accent.to_standalone_rgba(dark)),
    );
    fallback.provider.load_from_string(&css);
}

fn rgba_hex(c: &gtk::gdk::RGBA) -> String {
    format!("#{:02x}{:02x}{:02x}", (c.red() * 255.0).round() as u8, (c.green() * 255.0).round() as u8, (c.blue() * 255.0).round() as u8)
}

fn parse(name: &str) -> adw::AccentColor {
    use adw::AccentColor::*;
    match name {
        "teal" => Teal, "green" => Green, "yellow" => Yellow, "orange" => Orange,
        "red" => Red, "pink" => Pink, "purple" => Purple, "slate" => Slate, _ => Blue,
    }
}
