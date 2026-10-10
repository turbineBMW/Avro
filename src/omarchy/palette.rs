//! The Omarchy desktop theme, resolved into libadwaita CSS.
//! Ported from Rustle's `rustle-core/src/omarchy.rs`.
//!
//! Omarchy keeps the active theme at `~/.local/state/omarchy/current/theme`
//! and swaps that directory wholesale on every `omarchy theme set`. The
//! palette lives in `colors.toml`; every themed app derives its own config
//! from those keys. Avro does the same, in-process: [`load`] resolves the
//! palette and renders it as libadwaita's colour variables, which the GTK
//! layer hands to a provider stacked above the app stylesheet. No omarchy
//! binary is invoked, so a theme switch costs a file read.
//!
//! A theme may also ship its own `avro.css` -- either by hand or from a
//! `~/.config/omarchy/themed/avro.css.tpl` template -- and that file is used
//! verbatim in place of everything derived here.
//!
//! The resolution cascade below mirrors `omarchy-theme-color` so Avro and
//! the rest of the desktop read an identical palette out of the same file,
//! including the legacy themes that only define `color0`..`color15`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// The state directory omarchy swaps on a theme change, below the user's
/// home. `theme` is replaced by a rename, so this stable parent is what a
/// file monitor can watch.
pub fn state_dir(home: &Path) -> PathBuf {
    home.join(".local/state/omarchy/current")
}

pub fn theme_dir(home: &Path) -> PathBuf {
    state_dir(home).join("theme")
}

/// Whether this machine has an Omarchy theme to follow at all.
pub fn detected(home: &Path) -> bool {
    theme_dir(home).is_dir()
}

/// The active theme's slug, for display in Preferences.
pub fn theme_name(home: &Path) -> Option<String> {
    let name = fs::read_to_string(state_dir(home).join("theme.name")).ok()?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

pub struct Theme {
    /// CSS ready for a `gtk::CssProvider`.
    pub css: String,
    /// The theme's `mode`, which decides the libadwaita colour scheme.
    pub light: bool,
}

/// Resolve the active theme into CSS, or `None` when omarchy isn't installed
/// or the theme carries no palette.
pub fn load(home: &Path) -> Option<Theme> {
    let dir = theme_dir(home);
    let colors = fs::read_to_string(dir.join("colors.toml")).ok()?;
    let palette = Palette::resolve(&colors, dir.join("light.mode").exists());

    // A theme that speaks Avro directly outranks anything derived here.
    let css = match fs::read_to_string(dir.join("avro.css")) {
        Ok(css) => css,
        Err(_) => palette.to_css(),
    };
    Some(Theme {
        css,
        light: palette.light,
    })
}

struct Palette {
    colors: HashMap<String, String>,
    light: bool,
}

impl Palette {
    /// Port of `omarchy-theme-color`'s parse-and-resolve. Keep the two in
    /// step: a theme that renders correctly in Alacritty must render the same
    /// here, and the fallbacks are the only thing standing between a
    /// pre-`colors.toml` theme and a half-empty palette.
    fn resolve(toml: &str, light_mode_file: bool) -> Self {
        let mut colors = parse(toml);
        let mut palette = Palette {
            light: false,
            colors: HashMap::new(),
        };

        // The short palette names an older theme may use instead.
        for (canonical, legacy) in [
            ("background", "bg"),
            ("dark_background", "dark_bg"),
            ("darker_background", "darker_bg"),
            ("lighter_background", "lighter_bg"),
            ("foreground", "fg"),
            ("dark_foreground", "dark_fg"),
            ("light_foreground", "light_fg"),
            ("bright_foreground", "bright_fg"),
        ] {
            alias(&mut colors, canonical, legacy);
        }

        // Themes predating the semantic palette define only ANSI names.
        alias(&mut colors, "background", "color0");
        alias(&mut colors, "foreground", "color7");
        mirror(&mut colors, "color0", "background");
        mirror(&mut colors, "color7", "foreground");

        for (name, ansi) in [
            ("red", "color1"),
            ("green", "color2"),
            ("yellow", "color3"),
            ("blue", "color4"),
            ("magenta", "color5"),
            ("cyan", "color6"),
            ("bright_red", "color9"),
            ("bright_green", "color10"),
            ("bright_yellow", "color11"),
            ("bright_blue", "color12"),
            ("bright_magenta", "color13"),
            ("bright_cyan", "color14"),
        ] {
            alias(&mut colors, name, ansi);
        }
        alias(&mut colors, "magenta", "purple");
        alias(&mut colors, "bright_magenta", "bright_purple");

        alias_any(&mut colors, "light_foreground", &["color7", "foreground"]);
        alias_any(&mut colors, "bright_foreground", &["color15", "foreground"]);
        // Unconditional, exactly as omarchy resolves it: the cursor is the
        // brightest foreground, not a key a theme sets on its own.
        mirror(&mut colors, "cursor", "bright_foreground");
        alias_any(&mut colors, "lighter_background", &["color0", "background"]);
        alias_any(&mut colors, "dark_foreground", &["color8", "foreground"]);
        alias_any(&mut colors, "muted", &["color8", "dark_foreground"]);
        alias_any(
            &mut colors,
            "selection",
            &["selection_background", "color8", "color0", "background"],
        );
        alias(&mut colors, "selection_background", "selection");
        alias(&mut colors, "selection_foreground", "bright_foreground");
        alias(&mut colors, "orange", "yellow");
        derive(&mut colors, "brown", "orange", "#000000", 0.5);

        derive(
            &mut colors,
            "dark_background",
            "background",
            "#000000",
            0.25,
        );
        derive(
            &mut colors,
            "darker_background",
            "background",
            "#000000",
            0.5,
        );
        for (bright, base) in [
            ("bright_red", "red"),
            ("bright_yellow", "yellow"),
            ("bright_green", "green"),
            ("bright_cyan", "cyan"),
            ("bright_blue", "blue"),
            ("bright_magenta", "magenta"),
        ] {
            derive(&mut colors, bright, base, "#ffffff", 0.2);
        }

        for (ansi, name) in [
            ("color0", "background"),
            ("color1", "red"),
            ("color2", "green"),
            ("color3", "yellow"),
            ("color4", "blue"),
            ("color5", "magenta"),
            ("color6", "cyan"),
            ("color7", "foreground"),
            ("color8", "muted"),
            ("color9", "bright_red"),
            ("color10", "bright_green"),
            ("color11", "bright_yellow"),
            ("color12", "bright_blue"),
            ("color13", "bright_magenta"),
            ("color14", "bright_cyan"),
            ("color15", "bright_foreground"),
        ] {
            alias(&mut colors, ansi, name);
        }

        palette.light = resolve_mode(&colors, light_mode_file);
        palette.colors = colors;
        palette
    }

    /// A palette key, or the value of `fallback` when the theme is missing it
    /// even after the cascade above.
    fn get(&self, key: &str, fallback: &str) -> String {
        self.colors
            .get(key)
            .filter(|value| !value.is_empty())
            .cloned()
            .unwrap_or_else(|| {
                self.colors
                    .get(fallback)
                    .cloned()
                    .unwrap_or_else(|| fallback.to_string())
            })
    }

    /// Render the palette as the libadwaita variables the app stylesheet and
    /// every stock widget already read.
    ///
    /// The palette itself is omarchy's, key for key; what changes here is
    /// how it is spent on a window. The one value that cannot be spent as
    /// written is a light theme's foreground -- see [`ink`].
    fn to_css(&self) -> String {
        let c = |key: &str, fallback: &str| self.get(key, fallback);
        let bg = c("background", "#1d1d20");
        let fg = c("foreground", "#d0cfcc");
        let fg = if self.light { ink(&fg, &bg) } else { fg };
        let accent = c("accent", "blue");
        // Chrome sits one step off the content background in both modes:
        // omarchy's dark_* shades are darker than `background` for a dark
        // theme and for a light one alike.
        let chrome = c("dark_background", "background");
        let backdrop = c("darker_background", "background");
        let raised = c("lighter_background", "background");
        // Hairlines are the foreground at `--border-opacity`, as in stock
        // libadwaita, but its 15% assumes a near-black or near-white
        // foreground. A light theme's text is often a mid-tone, and 15% of
        // that vanishes, so the opacity is raised until the line shows.
        let opacity = border_opacity(&fg, &[&bg, &chrome]);
        let border = format!("color-mix(in srgb, {fg} {opacity}%, transparent)");

        let mut css = String::from(
            "/* Generated by Avro from the active Omarchy theme; rebuilt on\n\
             \x20* every theme change. Ship a avro.css in the theme to replace it. */\n\
             :root {\n",
        );
        let mut property = |name: &str, value: &str| {
            css.push_str(&format!("  {name}: {value};\n"));
        };
        property("--accent-bg-color", &accent);
        property("--accent-fg-color", readable_on(&accent));
        property("--accent-color", &accent);
        property("--window-bg-color", &bg);
        property("--window-fg-color", &fg);
        property("--view-bg-color", &bg);
        property("--view-fg-color", &fg);
        property("--border-opacity", &format!("{opacity}%"));
        for prefix in ["--headerbar", "--sidebar", "--secondary-sidebar"] {
            property(&format!("{prefix}-bg-color"), &chrome);
            property(&format!("{prefix}-fg-color"), &fg);
            property(&format!("{prefix}-backdrop-color"), &backdrop);
            // libadwaita applies `--border-opacity` to the headerbar's border
            // itself; the sidebars' are used as written.
            property(
                &format!("{prefix}-border-color"),
                if prefix == "--headerbar" {
                    &fg
                } else {
                    &border
                },
            );
        }
        for prefix in ["--card", "--popover", "--dialog"] {
            property(
                &format!("{prefix}-bg-color"),
                if prefix == "--card" { &raised } else { &chrome },
            );
            property(&format!("{prefix}-fg-color"), &fg);
        }
        for (prefix, base, bright) in [
            ("--success", "green", "bright_green"),
            ("--warning", "yellow", "bright_yellow"),
            ("--error", "red", "bright_red"),
            ("--destructive", "red", "bright_red"),
        ] {
            let base = c(base, base);
            property(&format!("{prefix}-bg-color"), &base);
            property(&format!("{prefix}-fg-color"), readable_on(&base));
            property(&format!("{prefix}-color"), &c(bright, base.as_str()));
        }
        css.push_str("}\n");
        css
    }
}

/// `key = "value"` pairs, quotes and inline comments stripped. Keys and
/// values outside the charset omarchy accepts are dropped rather than
/// smuggled into CSS.
fn parse(toml: &str) -> HashMap<String, String> {
    let mut colors = HashMap::new();
    for line in toml.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key: String = key
            .chars()
            .filter(|ch| !matches!(ch, '"' | '\'' | ' ' | '\t'))
            .collect();
        if key.is_empty() || key.starts_with('#') {
            continue;
        }
        let value = match value.split_once(['"', '\'']) {
            Some((_, rest)) => rest
                .split(['"', '\''])
                .next()
                .unwrap_or_default()
                .to_string(),
            None => value.trim().to_string(),
        };
        if !key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
        {
            continue;
        }
        if !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "#(),._+/% -".contains(ch))
        {
            continue;
        }
        colors.insert(key, value);
    }
    colors
}

/// Fill `key` from `from` only when the theme left it unset.
fn alias(colors: &mut HashMap<String, String>, key: &str, from: &str) {
    alias_any(colors, key, &[from]);
}

fn alias_any(colors: &mut HashMap<String, String>, key: &str, from: &[&str]) {
    if colors.get(key).is_some_and(|value| !value.is_empty()) {
        return;
    }
    for candidate in from {
        if let Some(value) = colors.get(*candidate).filter(|value| !value.is_empty()) {
            let value = value.clone();
            colors.insert(key.to_string(), value);
            return;
        }
    }
}

/// Overwrite `key` with `from` whenever `from` is set.
fn mirror(colors: &mut HashMap<String, String>, key: &str, from: &str) {
    if let Some(value) = colors.get(from).filter(|value| !value.is_empty()) {
        let value = value.clone();
        colors.insert(key.to_string(), value);
    }
}

/// Fill `key` by blending `base` toward `toward`, when the theme left it unset.
fn derive(colors: &mut HashMap<String, String>, key: &str, base: &str, toward: &str, amount: f64) {
    if colors.get(key).is_some_and(|value| !value.is_empty()) {
        return;
    }
    let Some(base) = colors.get(base).cloned() else {
        return;
    };
    if let Some(value) = mix(&base, toward, amount) {
        colors.insert(key.to_string(), value);
    }
}

fn rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).ok();
    Some((channel(0)?, channel(2)?, channel(4)?))
}

/// Blend two hex colors, `amount` of the way from `start` to `end`. A
/// non-hex value (an `rgba()` a theme wrote by hand) has nothing to blend.
fn mix(start: &str, end: &str, amount: f64) -> Option<String> {
    let (sr, sg, sb) = rgb(start)?;
    let (er, eg, eb) = rgb(end)?;
    let amount = amount.clamp(0.0, 1.0);
    let blend = |s: u8, e: u8| (s as f64 * (1.0 - amount) + e as f64 * amount + 0.5) as u8;
    Some(format!(
        "#{:02x}{:02x}{:02x}",
        blend(sr, er),
        blend(sg, eg),
        blend(sb, eb)
    ))
}

/// WCAG relative luminance.
fn luminance(color: &str) -> Option<f64> {
    let (r, g, b) = rgb(color)?;
    let linear = |channel: u8| {
        let c = channel as f64 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    Some(0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b))
}

/// The WCAG contrast ratio between two colours, or `None` when either is
/// not a colour this can measure.
fn contrast(a: &str, b: &str) -> Option<f64> {
    let (a, b) = (luminance(a)?, luminance(b)?);
    Some((a.max(b) + 0.05) / (a.min(b) + 0.05))
}

/// `#rrggbb` as hue, saturation and lightness, each `0.0..=1.0`.
fn hsl(color: &str) -> Option<(f64, f64, f64)> {
    let (r, g, b) = rgb(color)?;
    let (r, g, b) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let lightness = (max + min) / 2.0;
    let chroma = max - min;
    if chroma == 0.0 {
        return Some((0.0, 0.0, lightness));
    }
    let hue = if max == r {
        ((g - b) / chroma).rem_euclid(6.0)
    } else if max == g {
        (b - r) / chroma + 2.0
    } else {
        (r - g) / chroma + 4.0
    };
    Some((
        hue / 6.0,
        chroma / (1.0 - (2.0 * lightness - 1.0).abs()),
        lightness,
    ))
}

fn from_hsl(hue: f64, saturation: f64, lightness: f64) -> String {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let hue = hue * 6.0;
    let second = chroma * (1.0 - (hue.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match hue as u32 {
        0 => (chroma, second, 0.0),
        1 => (second, chroma, 0.0),
        2 => (0.0, chroma, second),
        3 => (0.0, second, chroma),
        4 => (second, 0.0, chroma),
        _ => (chroma, 0.0, second),
    };
    let base = lightness - chroma / 2.0;
    let channel = |value: f64| ((value + base) * 255.0).round().clamp(0.0, 255.0) as u8;
    format!("#{:02x}{:02x}{:02x}", channel(r), channel(g), channel(b))
}

/// The chroma an ink may carry before dimming it tints the whole window:
/// libadwaita composites `.dim-label` at 55%, and 55% of a saturated blue
/// over a pale surface is lavender. Omarchy's own light themes all sit well
/// under this -- `catppuccin-latte` is the most coloured at 16%.
const INK_MAX_SATURATION: f64 = 0.30;
/// The contrast body text is held to. WCAG asks 4.5 for text alone, but the
/// dimmed half of it is drawn from the same ink, and only has room to stay
/// legible when what it is dimmed from has this much. Omarchy's own light
/// themes all clear it or come within a tenth.
const INK_CONTRAST: f64 = 7.0;
const INK_STEP: f64 = 0.005;

/// A light theme's `foreground` is the ink its terminal and editor write
/// with, where a saturated mid-tone is the theme's signature over a mostly
/// empty screen -- Tokyo Night Day's `#3760bf` is the blue it is on purpose.
/// A window is not a terminal: that one colour becomes every label at once,
/// and libadwaita dims a good share of them to 55%, which lands a mid-tone
/// blue on lavender. So the ink keeps the hue the theme chose, but is pulled
/// under [`INK_MAX_SATURATION`] and deepened until it reaches
/// [`INK_CONTRAST`]. A foreground that already reads as ink is returned
/// exactly as the theme wrote it, which is every light theme omarchy ships.
///
/// Dark themes are left alone: their foregrounds are pale by construction,
/// and a colour cast that would shout on white is what a dark palette is
/// for.
fn ink(fg: &str, bg: &str) -> String {
    let Some((hue, saturation, lightness)) = hsl(fg) else {
        return fg.to_string();
    };
    if saturation <= INK_MAX_SATURATION && contrast(fg, bg).is_none_or(|c| c >= INK_CONTRAST) {
        return fg.to_string();
    }
    let saturation = saturation.min(INK_MAX_SATURATION);
    let mut lightness = lightness;
    loop {
        let candidate = from_hsl(hue, saturation, lightness);
        if lightness <= 0.0 || contrast(&candidate, bg).is_none_or(|c| c >= INK_CONTRAST) {
            return candidate;
        }
        lightness = (lightness - INK_STEP).max(0.0);
    }
}

/// libadwaita's stock hairline opacity, and the ceiling past which a line
/// stops being a hairline.
const BORDER_OPACITY: u32 = 15;
const BORDER_OPACITY_MAX: u32 = 40;
/// The contrast a hairline needs against its surface to read as a line;
/// stock Adwaita sits between 1.3 (light) and 1.6 (dark).
const BORDER_CONTRAST: f64 = 1.4;

/// The lowest `--border-opacity`, in percent, at which `fg` painted over
/// each of `surfaces` reaches [`BORDER_CONTRAST`]. Colours that aren't hex
/// can't be measured and keep the stock opacity.
fn border_opacity(fg: &str, surfaces: &[&str]) -> u32 {
    let line_contrast =
        |surface: &str, opacity: u32| contrast(&mix(surface, fg, opacity as f64 / 100.0)?, surface);
    (BORDER_OPACITY..=BORDER_OPACITY_MAX)
        .find(|&opacity| {
            surfaces
                .iter()
                .all(|surface| line_contrast(surface, opacity).is_none_or(|c| c >= BORDER_CONTRAST))
        })
        .unwrap_or(BORDER_OPACITY_MAX)
}

/// Black or white, whichever stays legible on `color`. Used for text on the
/// saturated accent and status fills, where no palette key is guaranteed to
/// contrast with them in both light and dark themes.
fn readable_on(color: &str) -> &'static str {
    match rgb(color) {
        // Rec. 601 luma, the same threshold GTK uses to pick icon shades.
        Some((r, g, b)) if 0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64 > 140.0 => {
            "#000000"
        }
        _ => "#ffffff",
    }
}

/// `mode`, then the legacy `theme_type`, then a `light.mode` file beside
/// colors.toml, then the background's brightness.
fn resolve_mode(colors: &HashMap<String, String>, light_mode_file: bool) -> bool {
    for key in ["mode", "theme_type"] {
        if let Some(mode) = colors.get(key).filter(|mode| !mode.is_empty()) {
            return mode.eq_ignore_ascii_case("light");
        }
    }
    if light_mode_file {
        return true;
    }
    match colors.get("background").and_then(|bg| rgb(bg)) {
        Some((r, g, b)) => r as u32 + g as u32 + b as u32 > 382,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OSAKA_JADE: &str = r##"
mode = "dark"
accent = "#509475"
selection = "#32473B"
muted = "#53685B"
background = "#111c18"
foreground = "#C1C497"
bright_foreground = "#F7E8B2"
red = "#FF5345"
yellow = "#459451"
green = "#549e6a"
cyan = "#2DD5B7"
blue = "#509475"
magenta = "#D2689C"
bright_red = "#db9f9c"
"##;

    #[test]
    fn semantic_keys_reach_the_libadwaita_variables() {
        let palette = Palette::resolve(OSAKA_JADE, false);
        let css = palette.to_css();
        assert!(css.contains("--accent-bg-color: #509475;"));
        assert!(css.contains("--window-bg-color: #111c18;"));
        assert!(css.contains("--view-fg-color: #C1C497;"));
        // Chrome is derived a quarter of the way to black when the theme
        // leaves dark_background out.
        assert!(css.contains("--sidebar-bg-color: #0d1512;"));
        assert!(css.contains("--error-color: #db9f9c;"));
        assert!(!palette.light);
    }

    /// A theme that predates the semantic palette only has color0..color15.
    #[test]
    fn legacy_ansi_only_themes_still_resolve() {
        let css = Palette::resolve(
            "color0 = \"#101010\"\n\
             color1 = \"#ff0000\"\n\
             color4 = \"#3366ff\"\n\
             color7 = \"#e0e0e0\"\n",
            false,
        )
        .to_css();
        assert!(css.contains("--window-bg-color: #101010;"));
        assert!(css.contains("--window-fg-color: #e0e0e0;"));
        // No accent key: blue stands in, as it does across omarchy.
        assert!(css.contains("--accent-bg-color: #3366ff;"));
        assert!(css.contains("--error-bg-color: #ff0000;"));
        // bright_red is derived from red when the theme omits it.
        assert!(css.contains("--error-color: #ff3333;"));
    }

    #[test]
    fn light_themes_are_detected_by_mode_then_by_luminance() {
        assert!(Palette::resolve("mode = \"light\"\n", false).light);
        assert!(Palette::resolve("theme_type = \"light\"\n", false).light);
        assert!(Palette::resolve("background = \"#eff1f5\"\n", false).light);
        assert!(!Palette::resolve("background = \"#111c18\"\n", false).light);
        // A light.mode file decides only when the theme states no mode.
        assert!(Palette::resolve("background = \"#111c18\"\n", true).light);
        assert!(!Palette::resolve("mode = \"dark\"\n", true).light);
    }

    #[test]
    fn accent_text_flips_to_stay_legible() {
        assert_eq!(readable_on("#1e66f5"), "#ffffff");
        assert_eq!(readable_on("#e9ad0c"), "#000000");
        assert_eq!(readable_on("not-a-hex-color"), "#ffffff");
    }

    /// A light theme's mid-tone text makes a fainter hairline than a dark
    /// theme's pale text at the same opacity, so it is given more of it.
    #[test]
    fn hairlines_are_strengthened_until_they_show() {
        let dark = border_opacity("#a9b1d6", &["#1a1b26", "#13141c"]);
        let light = border_opacity("#3760bf", &["#e1e2e7", "#d4d6e0"]);
        assert!((BORDER_OPACITY..=20).contains(&dark), "{dark}");
        assert!(light > dark && light < BORDER_OPACITY_MAX, "{light}");
        // Near-black text on white already clears the bar close to stock.
        assert!(border_opacity("#000000", &["#ffffff"]) <= 20);
        assert_eq!(border_opacity("rgb(1, 2, 3)", &["#ffffff"]), BORDER_OPACITY);

        // The hairline of a light theme is drawn from the ink the window
        // actually uses, not the raw foreground.
        let css = Palette::resolve(
            "mode = \"light\"\nbackground = \"#e1e2e7\"\n\
             dark_background = \"#d4d6e0\"\nforeground = \"#3760bf\"\n",
            false,
        )
        .to_css();
        let ink = ink("#3760bf", "#e1e2e7");
        let opacity = border_opacity(&ink, &["#e1e2e7", "#d4d6e0"]);
        assert!(css.contains(&format!("--border-opacity: {opacity}%;")));
        assert!(css.contains(&format!("--headerbar-border-color: {ink};")));
        assert!(css.contains(&format!(
            "--sidebar-border-color: color-mix(in srgb, {ink} {opacity}%, transparent);"
        )));
    }

    /// The light themes omarchy ships are already written in ink, and come
    /// back byte for byte; a terminal palette's coloured mid-tone does not.
    #[test]
    fn a_light_themes_ink_is_deepened_only_when_it_has_to_be() {
        for (fg, bg) in [
            ("#4c4f69", "#eff1f5"), // catppuccin-latte
            ("#100F0F", "#FFFCF0"), // flexoki-light, casing and all
            ("#212121", "#fafafa"), // lupine
            ("#000000", "#ffffff"), // white
            ("rgb(1, 2, 3)", "#ffffff"),
        ] {
            assert_eq!(ink(fg, bg), fg, "{fg} was rewritten");
        }

        // Tokyo Night Day's terminal ink: 55% saturation at 4.5:1, which
        // dims to lavender. It keeps its hue and loses the cast.
        let deepened = ink("#3760bf", "#e1e2e7");
        let (hue, saturation, _) = hsl(&deepened).unwrap();
        assert!(
            contrast(&deepened, "#e1e2e7").unwrap() >= INK_CONTRAST,
            "{deepened}"
        );
        assert!(
            saturation <= INK_MAX_SATURATION + f64::EPSILON,
            "{deepened}"
        );
        assert!((hue - hsl("#3760bf").unwrap().0).abs() < 0.01, "{deepened}");
        // The dimmed half of the interface is what this is really for:
        // libadwaita's 55% over the background is a grey, not a pastel.
        let dimmed = mix("#e1e2e7", &deepened, 0.55).unwrap();
        assert!(hsl(&dimmed).unwrap().1 < 0.2, "{dimmed}");

        // A dark theme's pale foreground is spent as written.
        let palette = Palette::resolve(OSAKA_JADE, false);
        assert!(palette.to_css().contains("--window-fg-color: #C1C497;"));
    }

    #[test]
    fn hsl_round_trips_through_the_channels() {
        for color in ["#3760bf", "#e1e2e7", "#000000", "#ffffff", "#7f7f7f"] {
            let (hue, saturation, lightness) = hsl(color).unwrap();
            assert_eq!(from_hsl(hue, saturation, lightness), color.to_lowercase());
        }
        assert!(hsl("rgb(1, 2, 3)").is_none());
    }

    /// Values omarchy would refuse are not smuggled into a CSS declaration.
    /// A theme is a file from the internet: `omarchy theme install` clones
    /// one from any repo, and its colors.toml is kept verbatim.
    #[test]
    fn unsupported_keys_and_values_are_dropped() {
        let colors = parse(
            "background = \"#101010\"\n\
             evil = \"red; } * { color: blue\"\n\
             bad$key = \"#ffffff\"\n\
             # comment = \"#000000\"\n",
        );
        assert_eq!(
            colors.get("background").map(String::as_str),
            Some("#101010")
        );
        assert!(!colors.contains_key("evil"));
        assert!(!colors.contains_key("bad$key"));
        // Spaces around a key are stripped rather than rejected, as omarchy
        // does: `  accent = ...` is an ordinary way to write the file.
        assert_eq!(
            parse("  accent  =  \"#509475\"\n")
                .get("accent")
                .map(String::as_str),
            Some("#509475")
        );
    }

    /// The whole path, from a theme directory on disk: a theme's own
    /// avro.css replaces the derived CSS.
    #[test]
    fn a_theme_directory_loads_and_its_own_css_wins() {
        let home = tempfile::tempdir().unwrap();
        assert!(!detected(home.path()));
        assert!(load(home.path()).is_none());

        let dir = theme_dir(home.path());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("colors.toml"), OSAKA_JADE).unwrap();
        fs::write(state_dir(home.path()).join("theme.name"), "osaka-jade\n").unwrap();
        assert!(detected(home.path()));
        assert_eq!(theme_name(home.path()).as_deref(), Some("osaka-jade"));
        let theme = load(home.path()).unwrap();
        assert!(theme.css.contains("--accent-bg-color: #509475;"));

        fs::write(dir.join("avro.css"), ":root { --accent-color: red; }").unwrap();
        fs::write(dir.join("colors.toml"), "accent = \"rgb(1, 2, 3)\"\n").unwrap();
        let theme = load(home.path()).unwrap();
        assert_eq!(theme.css, ":root { --accent-color: red; }");
    }
}
