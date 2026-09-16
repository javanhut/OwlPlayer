//! The look: Raven Glass, the stylesheet shared with Raven Settings, Store,
//! Viewer and Power (`data/raven-glass.css`), plus the classes only the
//! player draws (`data/owl-player.css`). Accent and light/dark come from
//! desktop.toml, exactly as in the other Raven apps.

use gtk4 as gtk;
use libadwaita as adw;

use crate::config::{DEFAULT_ACCENT, Desktop, ThemeMode};

const BASE_CSS: &str =
    concat!(include_str!("../../../data/raven-glass.css"), include_str!("../../../data/owl-player.css"));

pub fn apply() {
    let display = gtk::gdk::Display::default().expect("no display");
    let base = gtk::CssProvider::new();
    base.load_from_string(BASE_CSS);
    gtk::style_context_add_provider_for_display(&display, &base, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);

    let appearance = Desktop::load().appearance;
    adw::StyleManager::default().set_color_scheme(match appearance.theme_mode {
        ThemeMode::Dark => adw::ColorScheme::ForceDark,
        ThemeMode::Light => adw::ColorScheme::ForceLight,
        ThemeMode::Auto => adw::ColorScheme::PreferDark,
    });
    let accent = if is_hex(&appearance.accent) { appearance.accent.as_str() } else { DEFAULT_ACCENT };
    let css = format!(
        "@define-color accent_bg_color {accent};\n@define-color accent_color {accent};\n{}",
        if appearance.theme_mode == ThemeMode::Light {
            include_str!("../../../data/raven-glass-light.css")
        } else {
            ""
        }
    );
    let overlay = gtk::CssProvider::new();
    overlay.load_from_string(&css);
    gtk::style_context_add_provider_for_display(
        &display,
        &overlay,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
}

/// The window background, as the GL stage should clear to when no film is
/// loaded. Matches `@window_bg_color` in whichever of the two Raven
/// stylesheets is in force.
pub fn backdrop() -> [f32; 3] {
    let dark = match Desktop::load().appearance.theme_mode {
        ThemeMode::Light => false,
        ThemeMode::Dark => true,
        ThemeMode::Auto => !adw::StyleManager::default().is_dark(),
    };
    // #17171d and #f2f2f7.
    if dark { [0.090, 0.090, 0.114] } else { [0.949, 0.949, 0.969] }
}

/// The desktop's accent colour, for the parts of the picture GTK does not
/// draw — the visualiser is ours, so it has to read the same value the
/// stylesheet does.
pub fn accent_rgb() -> [f32; 3] {
    let appearance = Desktop::load().appearance;
    let hex = if is_hex(&appearance.accent) { appearance.accent.clone() } else { DEFAULT_ACCENT.into() };
    let channel = |from: usize| {
        u8::from_str_radix(&hex[from..from + 2], 16).unwrap_or(0) as f32 / 255.0
    };
    [channel(1), channel(3), channel(5)]
}

/// Whether the desktop asked for translucent windows.
pub fn glass() -> bool {
    Desktop::load().appearance.transparency
}

fn is_hex(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}
