//! The look: Raven Glass, the stylesheet shared with Raven Settings, Store,
//! Viewer and Power (`data/raven-glass.css`), plus the classes only the
//! player draws (`data/owl-player.css`). Accent and light/dark come from
//! desktop.toml, exactly as in the other Raven apps.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::config::{Appearance, DEFAULT_ACCENT, Desktop, ThemeMode};

const BASE_CSS: &str =
    concat!(include_str!("../../../data/raven-glass.css"), include_str!("../../../data/owl-player.css"));

/// How long desktop.toml has to be quiet before it is re-read: one save is
/// a burst of events (create, write, rename).
const DESKTOP_SETTLE: Duration = Duration::from_millis(150);

thread_local! {
    /// The appearance last applied, so the renderer can ask for the accent
    /// and backdrop every frame without touching the disk.
    static CURRENT: RefCell<Appearance> = RefCell::new(Desktop::load().appearance);
    static OVERLAY: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
    static DESKTOP_MONITOR: RefCell<Option<gio::FileMonitor>> = const { RefCell::new(None) };
    static LISTENERS: RefCell<Vec<Box<dyn Fn()>>> = const { RefCell::new(Vec::new()) };
}

/// Load the stylesheets once, apply the desktop's look, and follow it.
pub fn apply() {
    let display = gtk::gdk::Display::default().expect("no display");
    let base = gtk::CssProvider::new();
    base.load_from_string(BASE_CSS);
    gtk::style_context_add_provider_for_display(&display, &base, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    refresh();
    watch_desktop();
}

/// Run `f` after every re-application of desktop.toml — for what GTK does
/// not draw (the GL stage).
pub fn connect_changed(f: impl Fn() + 'static) {
    LISTENERS.with(|l| l.borrow_mut().push(Box::new(f)));
}

/// Re-read desktop.toml and apply it: light/dark, the accent, the glass
/// theme, and glass on the open windows. The overlay provider is replaced, never stacked.
fn refresh() {
    let appearance = Desktop::load().appearance;
    adw::StyleManager::default().set_color_scheme(match appearance.theme_mode {
        ThemeMode::Dark => adw::ColorScheme::ForceDark,
        ThemeMode::Light => adw::ColorScheme::ForceLight,
        ThemeMode::Auto => adw::ColorScheme::PreferDark,
    });
    let accent = if is_hex(&appearance.accent) { appearance.accent.as_str() } else { DEFAULT_ACCENT };
    let light = appearance.theme_mode == ThemeMode::Light;
    let css = format!(
        "@define-color accent_bg_color {accent};\n@define-color accent_color {accent};\n{}{}",
        if light {
            concat!(
                include_str!("../../../data/raven-glass-light.css"),
                include_str!("../../../data/owl-player-light.css")
            )
        } else {
            ""
        },
        crate::glass_tint::css(&appearance.glass_theme, light),
    );
    if let Some(display) = gtk::gdk::Display::default() {
        OVERLAY.with(|slot| {
            if let Some(old) = slot.borrow_mut().take() {
                gtk::style_context_remove_provider_for_display(&display, &old);
            }
            let overlay = gtk::CssProvider::new();
            overlay.load_from_string(&css);
            gtk::style_context_add_provider_for_display(
                &display,
                &overlay,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            );
            *slot.borrow_mut() = Some(overlay);
        });
    }

    // Glass is the material of the player's own windows; dialogs stay opaque.
    let toplevels = gtk::Window::toplevels();
    for i in 0..toplevels.n_items() {
        let Some(window) = toplevels.item(i).and_downcast::<gtk::Window>() else {
            continue;
        };
        if !window.has_css_class("raven") {
            continue;
        }
        if appearance.transparency && window.transient_for().is_none() {
            window.add_css_class("glass");
        } else {
            window.remove_css_class("glass");
        }
    }

    CURRENT.with(|c| *c.borrow_mut() = appearance);
    LISTENERS.with(|l| l.borrow().iter().for_each(|f| f()));
}

/// Follow desktop.toml, so a change made in Raven Settings shows here at
/// once. The directory is watched rather than the file: the file may not
/// exist yet, and is replaced by renaming a new one over it.
fn watch_desktop() {
    let path = Desktop::path();
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return;
    };
    let name = name.to_os_string();
    let Ok(monitor) = gio::File::for_path(dir)
        .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
    else {
        return;
    };
    let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    monitor.connect_changed(move |_, file, other, event| {
        if matches!(
            event,
            gio::FileMonitorEvent::AttributeChanged
                | gio::FileMonitorEvent::PreUnmount
                | gio::FileMonitorEvent::Unmounted
        ) {
            return;
        }
        let names_desktop = |f: Option<&gio::File>| {
            f.and_then(|f| f.basename()).is_some_and(|b| b.as_os_str() == name.as_os_str())
        };
        if !names_desktop(Some(file)) && !names_desktop(other) {
            return;
        }
        if let Some(id) = pending.borrow_mut().take() {
            id.remove();
        }
        let fired = pending.clone();
        let id = glib::timeout_add_local_once(DESKTOP_SETTLE, move || {
            fired.borrow_mut().take();
            refresh();
        });
        *pending.borrow_mut() = Some(id);
    });
    DESKTOP_MONITOR.with(|m| *m.borrow_mut() = Some(monitor));
}

/// The window background, as the GL stage should clear to when no film is
/// loaded. Matches `@window_bg_color` in whichever of the two Raven
/// stylesheets is in force, re-tinted by the glass theme. Auto is dark in
/// every Raven app.
pub fn backdrop() -> [f32; 3] {
    let (light, tint) = CURRENT.with(|c| {
        let c = c.borrow();
        let light = c.theme_mode == ThemeMode::Light;
        (light, crate::glass_tint::css(&c.glass_theme, light))
    });
    if let Some(ground) = tinted_ground(&tint) {
        return ground;
    }
    // #17171d and #f2f2f7.
    if light { [0.949, 0.949, 0.969] } else { [0.090, 0.090, 0.114] }
}

/// The `window_bg_color` a glass theme defines, or `None` for Black Glass.
fn tinted_ground(tint: &str) -> Option<[f32; 3]> {
    let hex = tint.split("@define-color window_bg_color #").nth(1)?.get(..6)?;
    let channel =
        |from: usize| u8::from_str_radix(&hex[from..from + 2], 16).ok().map(|v| v as f32 / 255.0);
    Some([channel(0)?, channel(2)?, channel(4)?])
}

/// The desktop's accent colour, for the parts of the picture GTK does not
/// draw — the visualiser is ours, so it has to read the same value the
/// stylesheet does.
pub fn accent_rgb() -> [f32; 3] {
    let hex = CURRENT.with(|c| {
        let accent = &c.borrow().accent;
        if is_hex(accent) { accent.clone() } else { DEFAULT_ACCENT.into() }
    });
    let channel = |from: usize| {
        u8::from_str_radix(&hex[from..from + 2], 16).unwrap_or(0) as f32 / 255.0
    };
    [channel(1), channel(3), channel(5)]
}

/// Whether the desktop asked for translucent windows.
pub fn glass() -> bool {
    CURRENT.with(|c| c.borrow().transparency)
}

fn is_hex(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}
