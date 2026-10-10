//! Follow the active Omarchy palette, using the same resolution as Rustle.
mod palette;

use adw::prelude::*;
use gtk4::{self as gtk, gio, glib};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

thread_local! {
    static THEME: RefCell<Option<Rc<Theme>>> = const { RefCell::new(None) };
}

struct Theme {
    home: PathBuf,
    enabled: Cell<bool>,
    provider: gtk::CssProvider,
    monitors: RefCell<Vec<gio::FileMonitor>>,
    pending: RefCell<Option<glib::SourceId>>,
}

pub fn detected() -> bool {
    palette::detected(&glib::home_dir())
}

pub fn theme_name() -> Option<String> {
    palette::theme_name(&glib::home_dir())
}

/// Install once during application startup, above the system accent fallback.
pub fn install(display: &gtk::gdk::Display) {
    THEME.with(|cell| {
        if cell.borrow().is_some() {
            return;
        }
        let theme = Rc::new(Theme {
            home: glib::home_dir(),
            enabled: Cell::new(crate::settings::Settings::load().follow_omarchy_theme),
            provider: gtk::CssProvider::new(),
            monitors: RefCell::new(Vec::new()),
            pending: RefCell::new(None),
        });
        gtk::style_context_add_provider_for_display(
            display,
            &theme.provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 2,
        );
        theme.provider.connect_parsing_error(|_, section, error| {
            tracing::warn!(
                "Omarchy CSS line {}: {error}",
                section.start_location().lines() + 1
            );
        });
        theme.reload();
        theme.watch();
        cell.replace(Some(theme));
    });
}

pub fn set_follow(enabled: bool) {
    THEME.with(|cell| {
        if let Some(theme) = cell.borrow().as_ref() {
            theme.enabled.set(enabled);
            theme.reload();
        }
    });
}

impl Theme {
    fn reload(&self) {
        let palette = self
            .enabled
            .get()
            .then(|| palette::load(&self.home))
            .flatten();
        self.provider
            .load_from_string(palette.as_ref().map_or("", |theme| theme.css.as_str()));
        adw::StyleManager::default().set_color_scheme(match palette {
            Some(theme) if theme.light => adw::ColorScheme::ForceLight,
            Some(_) => adw::ColorScheme::ForceDark,
            None => adw::ColorScheme::Default,
        });
    }

    fn watch(self: &Rc<Self>) {
        for monitor in self.monitors.take() {
            monitor.cancel();
        }
        // The parent survives Omarchy's directory replacement. Also watch the
        // current theme for edits to colors.toml, light.mode, or avro.css.
        for dir in [
            palette::state_dir(&self.home),
            palette::theme_dir(&self.home),
        ] {
            if !dir.is_dir() {
                continue;
            }
            let monitor = match gio::File::for_path(&dir)
                .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
            {
                Ok(monitor) => monitor,
                Err(error) => {
                    tracing::warn!("cannot watch {}: {error}", dir.display());
                    continue;
                }
            };
            let weak = Rc::downgrade(self);
            monitor.connect_changed(move |_, _, _, _| {
                let Some(theme) = weak.upgrade() else { return };
                if let Some(pending) = theme.pending.take() {
                    pending.remove();
                }
                let weak = Rc::downgrade(&theme);
                theme.pending.replace(Some(glib::timeout_add_local_once(
                    Duration::from_millis(120),
                    move || {
                        if let Some(theme) = weak.upgrade() {
                            theme.pending.take();
                            theme.reload();
                            theme.watch();
                        }
                    },
                )));
            });
            self.monitors.borrow_mut().push(monitor);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    #[ignore = "requires a GTK display; run explicitly with --ignored"]
    fn live_theme_changes_and_opt_out() {
        adw::init().unwrap();
        let home = tempfile::tempdir().unwrap();
        let dir = palette::theme_dir(home.path());
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("colors.toml"),
            "mode = \"dark\"\nbackground = \"#111c18\"\n",
        )
        .unwrap();
        let theme = Rc::new(Theme {
            home: home.path().into(),
            enabled: Cell::new(true),
            provider: gtk::CssProvider::new(),
            monitors: RefCell::new(Vec::new()),
            pending: RefCell::new(None),
        });
        let errors = Rc::new(RefCell::new(Vec::new()));
        let errors_clone = errors.clone();
        theme.provider.connect_parsing_error(move |_, _, error| {
            errors_clone.borrow_mut().push(error.to_string());
        });
        if let Some(active) = palette::load(&glib::home_dir()) {
            theme.provider.load_from_string(&active.css);
        }
        theme.reload();
        theme.watch();
        let manager = adw::StyleManager::default();
        assert!(manager.is_dark());

        fn wait_until(predicate: impl Fn() -> bool) {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            let context = glib::MainContext::default();
            while !predicate() {
                assert!(std::time::Instant::now() < deadline, "theme did not reload");
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        // An in-place palette edit and a directory swap must both reload.
        fs::write(
            dir.join("colors.toml"),
            "mode = \"light\"\nbackground = \"#eff1f5\"\n",
        )
        .unwrap();
        wait_until(|| !manager.is_dark());
        let staging = dir.with_file_name("next-theme");
        fs::create_dir(&staging).unwrap();
        fs::write(
            staging.join("colors.toml"),
            "mode = \"dark\"\nbackground = \"#111c18\"\n",
        )
        .unwrap();
        fs::rename(&dir, dir.with_file_name("old-theme")).unwrap();
        fs::rename(staging, &dir).unwrap();
        wait_until(|| manager.is_dark());
        fs::write(
            dir.join("colors.toml"),
            "mode = \"light\"\nbackground = \"#eff1f5\"\n",
        )
        .unwrap();
        wait_until(|| !manager.is_dark());

        fs::write(
            dir.join("avro.css"),
            ":root { --accent-bg-color: #abcdef; }",
        )
        .unwrap();
        wait_until(|| theme.provider.to_str().contains("#abcdef"));

        theme.enabled.set(false);
        theme.reload();
        assert_eq!(manager.color_scheme(), adw::ColorScheme::Default);
        assert!(theme.provider.to_str().trim().is_empty());
        theme.enabled.set(true);
        theme.reload();
        assert_eq!(manager.color_scheme(), adw::ColorScheme::ForceLight);
        fs::remove_file(dir.join("colors.toml")).unwrap();
        wait_until(|| manager.color_scheme() == adw::ColorScheme::Default);
        assert!(theme.provider.to_str().trim().is_empty());
        assert!(
            errors.borrow().is_empty(),
            "CSS errors: {:?}",
            errors.borrow()
        );
    }
}
