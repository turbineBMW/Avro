//! A see-through chat list.
//!
//! **Preferences → Appearance → Transparent sidebar** fades the chat list's background,
//! never its text, to the opacity of the slider below it. Both are Avro's own settings
//! (`transparent_sidebar`, `sidebar_opacity`), not the Omarchy theme's: the opacity is stated
//! in a provider above the theme's, so a theme's `avro.css` can't change it. Themes used to set
//! `--avro-sidebar-opacity` themselves; `settings` carries that value over once.
//!
//! Written with `rgb(from …)` rather than `alpha()`, which drops the whole background when
//! handed a percentage. Scoped to the split view's own pane, so sidebars nested in dialogs are
//! left alone.
//!
//! Below 1 the main window also gets the `avro-transparent` class, which clears its background
//! so the desktop shows through. Everything else that was drawn on that background is repainted
//! with it: the open chat (the content pane, whose header bar is flat), the split view should it
//! ever collapse into a single page, and the login, connecting and error pages. Dialogs,
//! popovers, toasts and banners paint their own backgrounds.
use adw::prelude::*;
use gtk4::{self as gtk, glib};
use std::cell::{Cell, RefCell};

/// The custom property the stylesheet reads.
pub const OPACITY: &str = "--avro-sidebar-opacity";

const RULES: &str = "
navigation-split-view > .sidebar-pane {
    background-color: rgb(from var(--sidebar-bg-color) r g b / calc(alpha * var(--avro-sidebar-opacity)));
}
navigation-split-view > .sidebar-pane:backdrop {
    background-color: rgb(from var(--sidebar-backdrop-color) r g b / calc(alpha * var(--avro-sidebar-opacity)));
}
window.avro-transparent { background-color: transparent; }
window.avro-transparent navigation-split-view > .content-pane,
window.avro-transparent navigation-split-view > navigation-view,
window.avro-transparent stack.avro-pages > :not(navigation-split-view) {
    background-color: var(--window-bg-color);
}
";

const TRANSPARENT: &str = "avro-transparent";

thread_local! {
    static PROVIDER: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
    static WINDOW: RefCell<glib::WeakRef<adw::ApplicationWindow>> = RefCell::new(glib::WeakRef::new());
    /// The opacity in force: 1 while the setting is off.
    static CURRENT: Cell<f32> = const { Cell::new(1.0) };
}


/// The whole stylesheet at `opacity`.
fn css(opacity: f32) -> String {
    format!(":root {{ {OPACITY}: {opacity:.2}; }}\n{RULES}")
}

/// The opacity the settings ask for: 1 when the sidebar isn't transparent.
pub fn opacity(transparent: bool, opacity: f32) -> f32 {
    if transparent { opacity.clamp(0.0, 1.0) } else { 1.0 }
}

/// Install once during application startup. Above the Omarchy provider
/// (`APPLICATION + 2`), so the setting wins over anything a theme states.
pub fn install(display: &gtk::gdk::Display) {
    let provider = gtk::CssProvider::new();
    gtk::style_context_add_provider_for_display(display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 3);
    PROVIDER.with(|cell| cell.replace(Some(provider)));
    let settings = crate::settings::Settings::load();
    apply(settings.transparent_sidebar, settings.sidebar_opacity);
}

/// The main window, whose background is cleared while the sidebar is see-through.
pub fn register(win: &adw::ApplicationWindow) {
    WINDOW.with(|cell| cell.borrow().set(Some(win)));
    sync();
}

/// State the opacity the settings ask for, and bring the main window in line.
pub fn apply(transparent: bool, sidebar_opacity: f32) {
    let current = opacity(transparent, sidebar_opacity);
    CURRENT.with(|cell| cell.set(current));
    PROVIDER.with(|cell| {
        if let Some(provider) = cell.borrow().as_ref() {
            provider.load_from_string(&css(current));
        }
    });
    sync();
}

fn sync() {
    let Some(win) = WINDOW.with(|cell| cell.borrow().upgrade()) else { return };
    tracing::debug!("{OPACITY} is {}", CURRENT.with(Cell::get));
    if CURRENT.with(Cell::get) < 1.0 {
        win.add_css_class(TRANSPARENT);
    } else {
        win.remove_css_class(TRANSPARENT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sidebar_is_only_see_through_while_the_setting_is_on() {
        assert_eq!(opacity(true, 0.8), 0.8);
        assert_eq!(opacity(false, 0.3), 1.0);
        assert_eq!(opacity(true, 1.5), 1.0);
        assert!(css(0.8).starts_with(":root { --avro-sidebar-opacity: 0.80; }"));
        // Both backgrounds read the property the provider states.
        assert_eq!(RULES.matches(&format!("var({OPACITY})")).count(), 2);
    }
}
