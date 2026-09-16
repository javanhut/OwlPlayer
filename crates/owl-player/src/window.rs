//! The window. Sidebar, video stage, queue panel and transport, arranged
//! the way the design puts them: the picture runs edge to edge under
//! everything, and every piece of chrome floats on it as glass.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4 as gtk;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use owl_media::{Alignment, MediaInfo, Player, State, TrackKind, TrackPreference};

use crate::browse::Browser;
use crate::config::PlayerConfig;
use crate::queue::Queue;
use crate::stage::Stage;
use crate::theme;

pub const APP_ID: &str = "com.owlplayer.Raven";

/// Everything the callbacks need to reach. One `Rc` beats a dozen clones
/// of individual widgets threaded through every closure.
pub struct Ui {
    pub window: adw::ApplicationWindow,
    pub player: Rc<RefCell<Player>>,
    pub stage: Stage,
    pub queue: RefCell<Queue>,
    pub config: RefCell<PlayerConfig>,

    title: gtk::Label,
    chips: gtk::Box,
    scrubber: gtk::Scale,
    elapsed: gtk::Label,
    remaining: gtk::Label,
    play_button: gtk::Button,
    play_icon: gtk::Image,
    volume: gtk::Scale,
    volume_icon: gtk::Button,
    stage_stack: gtk::Stack,
    title_box: gtk::Box,
    browser: Rc<Browser>,
    nav_lists: Vec<gtk::ListBox>,
    sidebar_slot: gtk::Revealer,
    watermark: gtk::Label,
    top_bar: gtk::WindowHandle,
    transport: gtk::Box,
    /// Whether the viewer wants the queue panel at all, independent of
    /// whether the chrome is currently faded out.
    queue_wanted: Cell<bool>,
    /// Last time the pointer or keyboard did anything, used only to decide
    /// when to hide the pointer itself.
    last_activity: Cell<std::time::Instant>,
    /// Pointer position in window coordinates, and whether it is inside at
    /// all. Chrome visibility is a function of this rather than of a timer:
    /// each piece belongs to an edge and appears when the pointer is near
    /// that edge, which is predictable in a way a timeout is not.
    pointer: Cell<(f64, f64)>,
    pointer_inside: Cell<bool>,
    cursor_shown: Cell<bool>,
    subtitle: gtk::Label,
    /// The `text_revision` already rendered, so the label is rebuilt only
    /// when the cue changes rather than every frame.
    subtitle_revision: Cell<u64>,
    more_button: gtk::Button,
    queue_panel: gtk::Box,
    queue_list: gtk::ListBox,
    chapter_list: gtk::ListBox,
    details_list: gtk::Box,
    panel_foot: gtk::Label,
    /// Set while the scrubber is being dragged, so the per-frame update
    /// does not fight the pointer for the handle.
    scrubbing: Cell<bool>,
    fullscreen: Cell<bool>,
}

pub fn build(app: &adw::Application) -> Rc<Ui> {
    let config = PlayerConfig::load();
    let player = Rc::new(RefCell::new(Player::new()));
    player.borrow_mut().set_volume(config.volume);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(1320)
        .default_height(820)
        .title("Owl Player")
        .build();
    // `raven` is what the shared stylesheet keys off: every `window.raven`
    // rule in raven-glass.css — the window surface, the glass treatment,
    // the CSD radius, the dimmed-text fixes — matches on this class and on
    // nothing else. Without it the player loads the Raven sheet and then
    // ignores almost all of it.
    window.add_css_class("raven");
    window.add_css_class("owl");
    if theme::glass() {
        window.add_css_class("glass");
    }

    // No titlebar at all. AdwApplicationWindow draws its own decorations
    // around whatever `set_content` is given and refuses `set_titlebar`
    // outright, which is exactly what this design wants: the video runs to
    // all four edges and the window controls float on it, inside the
    // WindowHandle built in `build_top_bar`.

    let stage = Stage::new(Rc::clone(&player));

    let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let (sidebar, nav_lists) = build_sidebar();
    // Wrapped in a Revealer rather than faded in place: the sidebar is a
    // box-layout sibling of the video, so making it transparent would
    // leave its width reserved and the picture would keep playing in a
    // window with a dead column down one side.
    let sidebar_slot = gtk::Revealer::new();
    sidebar_slot.set_transition_type(gtk::RevealerTransitionType::SlideLeft);
    sidebar_slot.set_transition_duration(220);
    sidebar_slot.set_reveal_child(true);
    sidebar_slot.set_child(Some(&sidebar));
    root.append(&sidebar_slot);

    let browser = Browser::new();

    let stage_stack = gtk::Stack::new();
    stage_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stage_stack.set_transition_duration(140);
    stage_stack.add_named(&stage.widget, Some("player"));
    stage_stack.add_named(&browser.widget, Some("browse"));

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&stage_stack));
    overlay.set_hexpand(true);
    overlay.set_vexpand(true);

    let (top_bar, title, chips, title_box) = build_top_bar();
    top_bar.add_css_class("owl-chrome");
    overlay.add_overlay(&top_bar);

    let (queue_panel, _panel_stack, queue_list, chapter_list, details_list, panel_foot) = build_panel();
    queue_panel.add_css_class("owl-chrome");
    overlay.add_overlay(&queue_panel);

    let subtitle = gtk::Label::new(None);
    subtitle.add_css_class("owl-subtitle");
    subtitle.set_justify(gtk::Justification::Center);
    subtitle.set_wrap(true);
    subtitle.set_halign(gtk::Align::Center);
    subtitle.set_valign(gtk::Align::End);
    subtitle.set_visible(false);
    // Never a click target: the picture behind it still has to be
    // double-clickable for fullscreen.
    subtitle.set_can_target(false);
    overlay.add_overlay(&subtitle);

    let transport = TransportWidgets::build();
    transport.container.add_css_class("owl-chrome");
    overlay.add_overlay(&transport.container);

    let watermark = gtk::Label::new(Some("MORE THAN PLAYBACK"));
    watermark.add_css_class("owl-watermark");
    watermark.set_halign(gtk::Align::End);
    watermark.set_valign(gtk::Align::End);
    watermark.set_margin_end(26);
    // Clear of the scrubber: at 108 it sat directly on the remaining-time
    // label at the right end of the seek row.
    watermark.set_margin_bottom(146);
    watermark.set_can_target(false);
    watermark.add_css_class("owl-chrome");
    overlay.add_overlay(&watermark);

    root.append(&overlay);
    window.set_content(Some(&root));

    let ui = Rc::new(Ui {
        window: window.clone(),
        player,
        stage,
        queue: RefCell::new(Queue::default()),
        config: RefCell::new(config),
        title,
        chips,
        scrubber: transport.scrubber.clone(),
        elapsed: transport.elapsed.clone(),
        remaining: transport.remaining.clone(),
        play_button: transport.play.clone(),
        play_icon: transport.play_icon.clone(),
        volume: transport.volume.clone(),
        volume_icon: transport.volume_icon.clone(),
        stage_stack: stage_stack.clone(),
        title_box: title_box.clone(),
        browser: Rc::clone(&browser),
        nav_lists: nav_lists.clone(),
        sidebar_slot: sidebar_slot.clone(),
        watermark: watermark.clone(),
        top_bar: top_bar.clone(),
        transport: transport.container.clone(),
        queue_wanted: Cell::new(true),
        last_activity: Cell::new(std::time::Instant::now()),
        pointer: Cell::new((0.0, 0.0)),
        pointer_inside: Cell::new(false),
        cursor_shown: Cell::new(true),
        subtitle,
        subtitle_revision: Cell::new(u64::MAX),
        more_button: transport.more.clone(),
        queue_panel,
        queue_list,
        chapter_list,
        details_list,
        panel_foot,
        scrubbing: Cell::new(false),
        fullscreen: Cell::new(false),
    });

    wire(&ui, &transport);
    ui.refresh_transport();
    ui.show_player();
    ui
}

// ── Sidebar ─────────────────────────────────────────────────────────────

fn build_sidebar() -> (gtk::Box, Vec<gtk::ListBox>) {
    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
    // Both classes: `sidebar` so Raven's shared rules apply — nav rows,
    // separators, the search field, the glass treatment — and `owl-sidebar`
    // for the handful of things this app changes on top of them.
    sidebar.add_css_class("sidebar");
    sidebar.add_css_class("owl-sidebar");
    sidebar.set_size_request(268, -1);

    let brand = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    brand.add_css_class("owl-brand");
    let mark = gtk::Image::from_icon_name("com.owlplayer.Raven");
    mark.add_css_class("owl-mark");
    mark.set_pixel_size(40);
    brand.append(&mark);
    let words = gtk::Box::new(gtk::Orientation::Vertical, 0);
    words.set_valign(gtk::Align::Center);
    let name = gtk::Label::new(Some("Owl Player"));
    name.add_css_class("owl-wordmark");
    name.set_xalign(0.0);
    let tagline = gtk::Label::new(Some("See More."));
    tagline.add_css_class("owl-tagline");
    tagline.set_xalign(0.0);
    words.append(&name);
    words.append(&tagline);
    brand.append(&words);
    sidebar.append(&brand);

    let scroll = gtk::ScrolledWindow::new();
    scroll.add_css_class("owl-sidebar-scroll");
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_vexpand(true);

    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let primary = nav_list(&[
        ("Now Playing", "media-playback-start-symbolic", "player"),
        ("Queue", "view-list-symbolic", "queue"),
        ("Library", "folder-symbolic", "home"),
        ("Playlists", "view-list-symbolic", ""),
        ("Watch Later", "alarm-symbolic", ""),
        ("Favorites", "emblem-favorite-symbolic", ""),
    ]);
    column.append(&primary);

    let section = gtk::Label::new(Some("MEDIA"));
    section.add_css_class("owl-section");
    section.set_xalign(0.0);
    column.append(&section);

    let media = nav_list(&[
        ("Movies", "video-x-generic-symbolic", "videos"),
        ("TV Shows", "video-display-symbolic", "videos"),
        ("Music Videos", "emblem-music-symbolic", "music"),
        ("Local Files", "folder-open-symbolic", "home"),
        ("Network", "network-workgroup-symbolic", ""),
    ]);
    column.append(&media);
    scroll.set_child(Some(&column));
    sidebar.append(&scroll);

    let footer = gtk::Box::new(gtk::Orientation::Vertical, 10);
    footer.add_css_class("owl-sidebar-footer");
    let rule = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    rule.add_css_class("owl-rule");
    rule.set_halign(gtk::Align::Start);
    let blurb = gtk::Label::new(Some("Great stories\nlook better here."));
    blurb.add_css_class("owl-blurb");
    blurb.set_xalign(0.0);
    footer.append(&blurb);
    footer.append(&rule);
    sidebar.append(&footer);
    (sidebar, vec![primary, media])
}

/// Each row carries its destination as its widget name, which keeps the
/// mapping next to the label instead of in a parallel array that can drift.
/// An empty destination means the row is not wired to anything yet.
fn nav_list(items: &[(&str, &str, &str)]) -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.add_css_class("owl-nav");
    list.set_selection_mode(gtk::SelectionMode::Single);
    for (label, icon, action) in items {
        let row = gtk::ListBoxRow::new();
        row.set_widget_name(action);
        row.set_sensitive(!action.is_empty());
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 13);
        let image = gtk::Image::from_icon_name(icon);
        let text = gtk::Label::new(Some(label));
        text.set_xalign(0.0);
        line.append(&image);
        line.append(&text);
        row.set_child(Some(&line));
        list.append(&row);
    }
    list
}

// ── Title and chips, over the picture ───────────────────────────────────

fn build_top_bar() -> (gtk::WindowHandle, gtk::Label, gtk::Box, gtk::Box) {
    // A WindowHandle so the whole strip drags the window, which is the
    // only way to move it once the titlebar is empty.
    let handle = gtk::WindowHandle::new();
    handle.set_valign(gtk::Align::Start);

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.set_margin_top(14);
    row.set_margin_start(22);
    row.set_margin_end(10);

    let text = gtk::Box::new(gtk::Orientation::Vertical, 8);
    text.set_valign(gtk::Align::Start);
    let title = gtk::Label::new(Some("Nothing playing"));
    title.add_css_class("owl-stage-title");
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let chips = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    chips.set_halign(gtk::Align::Start);
    text.append(&title);
    text.append(&chips);

    row.append(&text);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    row.append(&spacer);

    let controls = gtk::WindowControls::new(gtk::PackType::End);
    controls.set_valign(gtk::Align::Start);
    row.append(&controls);

    handle.set_child(Some(&row));
    (handle, title, chips, text)
}

// ── Queue / Chapters / Details ──────────────────────────────────────────

type PanelParts = (gtk::Box, gtk::Stack, gtk::ListBox, gtk::ListBox, gtk::Box, gtk::Label);

fn build_panel() -> PanelParts {
    let panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
    panel.add_css_class("owl-panel");
    panel.set_halign(gtk::Align::End);
    panel.set_valign(gtk::Align::Start);
    panel.set_margin_top(62);
    panel.set_margin_end(18);
    panel.set_size_request(312, -1);
    panel.set_height_request(520);

    let tabs = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    tabs.add_css_class("owl-tabs");
    let stack = gtk::Stack::new();
    stack.set_vexpand(true);
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(140);

    let queue_list = gtk::ListBox::new();
    queue_list.add_css_class("owl-queue");
    queue_list.set_selection_mode(gtk::SelectionMode::Single);
    let queue_scroll = gtk::ScrolledWindow::new();
    queue_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    queue_scroll.set_child(Some(&queue_list));

    let chapter_list = gtk::ListBox::new();
    chapter_list.add_css_class("owl-queue");
    chapter_list.set_selection_mode(gtk::SelectionMode::Single);
    let chapter_scroll = gtk::ScrolledWindow::new();
    chapter_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    chapter_scroll.set_child(Some(&chapter_list));

    let details_list = gtk::Box::new(gtk::Orientation::Vertical, 4);
    details_list.set_margin_top(10);
    details_list.set_margin_start(14);
    details_list.set_margin_end(14);
    let details_scroll = gtk::ScrolledWindow::new();
    details_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    details_scroll.set_child(Some(&details_list));

    stack.add_named(&queue_scroll, Some("queue"));
    stack.add_named(&chapter_scroll, Some("chapters"));
    stack.add_named(&details_scroll, Some("details"));

    let mut first: Option<gtk::ToggleButton> = None;
    for (name, label) in [("queue", "Queue"), ("chapters", "Chapters"), ("details", "Details")] {
        let button = gtk::ToggleButton::with_label(label);
        match &first {
            None => {
                button.set_active(true);
                first = Some(button.clone());
            }
            Some(f) => button.set_group(Some(f)),
        }
        let stack = stack.clone();
        button.connect_toggled(move |b| {
            if b.is_active() {
                stack.set_visible_child_name(name);
            }
        });
        tabs.append(&button);
    }

    let foot = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    foot.add_css_class("owl-panel-foot");
    let summary = gtk::Label::new(Some("Nothing queued"));
    summary.set_xalign(0.0);
    summary.set_hexpand(true);
    foot.append(&summary);
    for icon in ["view-list-ordered-symbolic", "media-playlist-shuffle-symbolic"] {
        let b = gtk::Button::from_icon_name(icon);
        foot.append(&b);
    }

    panel.append(&tabs);
    panel.append(&stack);
    panel.append(&foot);
    (panel, stack, queue_list, chapter_list, details_list, summary)
}

// ── Transport ───────────────────────────────────────────────────────────

struct TransportWidgets {
    container: gtk::Box,
    scrubber: gtk::Scale,
    elapsed: gtk::Label,
    remaining: gtk::Label,
    play: gtk::Button,
    play_icon: gtk::Image,
    volume: gtk::Scale,
    volume_icon: gtk::Button,
    back10: gtk::Button,
    forward10: gtk::Button,
    previous: gtk::Button,
    next: gtk::Button,
    fullscreen: gtk::Button,
    more: gtk::Button,
    open: gtk::Button,
    browse: gtk::Button,
}

impl TransportWidgets {
    fn build() -> TransportWidgets {
        let container = gtk::Box::new(gtk::Orientation::Vertical, 10);
        container.add_css_class("owl-transport");
        container.set_valign(gtk::Align::End);

        let seek_row = gtk::Box::new(gtk::Orientation::Horizontal, 14);
        let elapsed = gtk::Label::new(Some("0:00"));
        elapsed.add_css_class("owl-time");
        let remaining = gtk::Label::new(Some("0:00"));
        remaining.add_css_class("owl-time");

        let scrubber = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.001);
        scrubber.add_css_class("owl-scrubber");
        scrubber.set_draw_value(false);
        scrubber.set_hexpand(true);

        seek_row.append(&elapsed);
        seek_row.append(&scrubber);
        seek_row.append(&remaining);

        let button_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);

        let left = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        left.set_halign(gtk::Align::Start);
        left.set_hexpand(true);
        // Opening a file is the single most common thing anyone does with
        // a player, so it gets a button of its own rather than a line in a
        // menu behind "…".
        // The queue toggle lives in the sidebar now, beside Now Playing,
        // which is where it belongs: it is a view, not a transport control.
        let open = flat_button("folder-open-symbolic", "Open files (O)");
        let browse = flat_button("view-grid-symbolic", "Browse your media (B)");
        left.append(&open);
        left.append(&browse);

        let centre = gtk::Box::new(gtk::Orientation::Horizontal, 14);
        centre.set_halign(gtk::Align::Center);
        let back10 = flat_button("media-seek-backward-symbolic", "Back 10 seconds");
        back10.add_css_class("skip");
        let previous = flat_button("media-skip-backward-symbolic", "Previous");
        let play = gtk::Button::new();
        let play_icon = gtk::Image::from_icon_name("media-playback-start-symbolic");
        play.set_child(Some(&play_icon));
        play.add_css_class("owl-play");
        play.set_tooltip_text(Some("Play or pause"));
        let next = flat_button("media-skip-forward-symbolic", "Next");
        let forward10 = flat_button("media-seek-forward-symbolic", "Forward 10 seconds");
        forward10.add_css_class("skip");
        for w in [&back10, &previous] {
            centre.append(w);
        }
        centre.append(&play);
        for w in [&next, &forward10] {
            centre.append(w);
        }

        let right = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        right.set_halign(gtk::Align::End);
        right.set_hexpand(true);
        let volume_icon = flat_button("audio-volume-high-symbolic", "Mute");
        let volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.01);
        volume.add_css_class("owl-volume");
        volume.set_draw_value(false);
        volume.set_value(1.0);
        volume.set_valign(gtk::Align::Center);
        let fullscreen = flat_button("view-fullscreen-symbolic", "Fullscreen");
        let more = flat_button("view-more-symbolic", "More");
        right.append(&volume_icon);
        right.append(&volume);
        right.append(&fullscreen);
        right.append(&more);

        button_row.append(&left);
        button_row.append(&centre);
        button_row.append(&right);

        container.append(&seek_row);
        container.append(&button_row);

        TransportWidgets {
            container,
            scrubber,
            elapsed,
            remaining,
            play,
            play_icon,
            volume,
            volume_icon,
            back10,
            forward10,
            previous,
            next,
            fullscreen,
            more,
            open,
            browse,
        }
    }
}

fn flat_button(icon: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.add_css_class("owl-tbtn");
    b.set_tooltip_text(Some(tooltip));
    b
}

/// Height reserved for the transport bar, so a caption is never drawn
/// underneath the controls.
const TRANSPORT_CLEARANCE: i32 = 150;

/// How long the pointer has to sit still before it is hidden.
const CHROME_IDLE: std::time::Duration = std::time::Duration::from_millis(2600);

/// How close to an edge the pointer has to be for that edge's chrome to
/// appear. Generous on purpose — a reveal zone you have to aim for is a
/// reveal zone people think is broken.
const LEFT_ZONE: f64 = 130.0;
const RIGHT_ZONE: f64 = 150.0;
const TOP_ZONE: f64 = 110.0;
const BOTTOM_ZONE: f64 = 210.0;

/// Fade a piece of chrome out and stop it taking input. The two go
/// together: a widget at zero opacity is invisible but still swallows
/// every click that lands on it.
fn set_away(widget: &impl IsA<gtk::Widget>, away: bool) {
    if away {
        widget.add_css_class("owl-away");
    } else {
        widget.remove_css_class("owl-away");
    }
    widget.set_can_target(!away);
}

fn menu_heading(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("owl-menu-heading");
    label.set_xalign(0.0);
    label
}

pub fn format_time(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "0:00".into();
    }
    let total = seconds as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

// ── Wiring ──────────────────────────────────────────────────────────────

fn wire(ui: &Rc<Ui>, t: &TransportWidgets) {
    macro_rules! on {
        ($widget:expr, $body:expr) => {{
            let ui = Rc::clone(ui);
            $widget.connect_clicked(move |_| {
                $body(&ui);
            });
        }};
    }

    on!(t.play, |ui: &Rc<Ui>| {
        ui.player.borrow_mut().toggle();
        ui.refresh_transport();
        ui.stage.refresh();
    });
    on!(t.back10, |ui: &Rc<Ui>| ui.seek_by(-10.0));
    on!(t.forward10, |ui: &Rc<Ui>| ui.seek_by(10.0));
    on!(t.previous, |ui: &Rc<Ui>| ui.step_queue(-1));
    on!(t.next, |ui: &Rc<Ui>| ui.step_queue(1));
    on!(t.fullscreen, |ui: &Rc<Ui>| ui.toggle_fullscreen());
    on!(t.more, |ui: &Rc<Ui>| ui.show_more_menu());
    on!(t.open, |ui: &Rc<Ui>| ui.open_dialog());
    on!(t.browse, |ui: &Rc<Ui>| ui.show_browse("home"));
    on!(t.volume_icon, |ui: &Rc<Ui>| {
        let muted = {
            let mut player = ui.player.borrow_mut();
            let muted = !player.is_muted();
            player.set_muted(muted);
            muted
        };
        ui.volume.set_sensitive(!muted);
        ui.volume_icon.set_icon_name(if muted {
            "audio-volume-muted-symbolic"
        } else {
            "audio-volume-high-symbolic"
        });
    });

    // Seeking. `change_value` fires for drags, clicks and keyboard steps
    // alike, and gives the value the widget is *about* to take, which is
    // what should be seeked to.
    t.scrubber.connect_change_value({
        let ui = Rc::clone(ui);
        move |_, _, value| {
            ui.player.borrow_mut().seek(value);
            ui.elapsed.set_text(&format_time(value));
            ui.stage.refresh();
            glib::Propagation::Proceed
        }
    });
    // While the handle is held, stop the per-frame update writing to it.
    let press = gtk::GestureClick::new();
    press.connect_pressed({
        let ui = Rc::clone(ui);
        move |_, _, _, _| ui.scrubbing.set(true)
    });
    press.connect_released({
        let ui = Rc::clone(ui);
        move |_, _, _, _| ui.scrubbing.set(false)
    });
    t.scrubber.add_controller(press);

    t.volume.connect_value_changed({
        let ui = Rc::clone(ui);
        move |scale| {
            let v = scale.value();
            ui.player.borrow_mut().set_volume(v);
            ui.config.borrow_mut().volume = v;
            ui.volume_icon.set_icon_name(match v {
                v if v <= 0.001 => "audio-volume-muted-symbolic",
                v if v < 0.34 => "audio-volume-low-symbolic",
                v if v < 0.67 => "audio-volume-medium-symbolic",
                _ => "audio-volume-high-symbolic",
            });
        }
    });

    // Activating a queue row plays it.
    ui.queue_list.connect_row_activated({
        let ui = Rc::clone(ui);
        move |_, row| {
            let index = row.index() as usize;
            let path = ui.queue.borrow().items.get(index).map(|i| i.path.clone());
            if let Some(path) = path {
                ui.open(&path);
            }
        }
    });

    // Activating a chapter seeks to it.
    ui.chapter_list.connect_row_activated({
        let ui = Rc::clone(ui);
        move |_, row| {
            let index = row.index() as usize;
            let start = ui.player.borrow().info().and_then(|i| i.chapters.get(index).map(|c| c.start));
            if let Some(start) = start {
                ui.player.borrow_mut().seek(start);
                ui.stage.refresh();
            }
        }
    });

    // Keyboard. These are the bindings every player has, and getting them
    // wrong is more annoying than not having them.
    // The sidebar actually navigates. Rows that are not wired to anything
    // yet are insensitive, so nothing in it looks live and does nothing.
    for list in &ui.nav_lists {
        list.connect_row_activated({
            let ui = Rc::clone(ui);
            move |list, row| {
                let action = row.widget_name().to_string();
                if action.is_empty() {
                    return;
                }
                // Selection is per-list, so clear the other one or two
                // rows end up looking selected at once.
                for other in &ui.nav_lists {
                    if other != list {
                        other.unselect_all();
                    }
                }
                match action.as_str() {
                    "player" => ui.show_player(),
                    // A toggle, not a destination: it flips the panel and
                    // leaves the selection where it was.
                    "queue" => {
                        ui.queue_wanted.set(!ui.queue_wanted.get());
                        ui.apply_chrome();
                        list.unselect_row(row);
                    }
                    place => ui.show_browse(place),
                }
            }
        });
    }

    ui.browser.connect_play({
        let ui = Rc::clone(ui);
        move |files, at| ui.play_from(files, at)
    });

    let motion = gtk::EventControllerMotion::new();
    motion.connect_motion({
        let ui = Rc::clone(ui);
        move |_, x, y| ui.pointer_moved(x, y)
    });
    motion.connect_leave({
        let ui = Rc::clone(ui);
        move |_| {
            ui.pointer_inside.set(false);
            ui.apply_chrome();
        }
    });
    ui.window.add_controller(motion);

    let clicks = gtk::GestureClick::new();
    clicks.set_button(0);
    clicks.connect_pressed({
        let ui = Rc::clone(ui);
        move |_, _, _, _| ui.note_activity()
    });
    ui.window.add_controller(clicks);

    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed({
        let ui = Rc::clone(ui);
        move |_, key, _, _| {
            use gtk::gdk::Key;
            ui.note_activity();
            match key {
                Key::space | Key::k => {
                    ui.player.borrow_mut().toggle();
                    ui.refresh_transport();
                    ui.stage.refresh();
                }
                Key::Left => ui.seek_by(-10.0),
                Key::Right => ui.seek_by(10.0),
                Key::Down => ui.seek_by(-60.0),
                Key::Up => ui.seek_by(60.0),
                Key::f | Key::F11 => ui.toggle_fullscreen(),
                Key::Escape => {
                    if ui.fullscreen.get() {
                        ui.toggle_fullscreen();
                    }
                }
                Key::m => {
                    let muted = {
                        let mut p = ui.player.borrow_mut();
                        let m = !p.is_muted();
                        p.set_muted(m);
                        m
                    };
                    ui.volume.set_sensitive(!muted);
                }
                Key::o | Key::O => ui.open_dialog(),
                Key::b | Key::B => ui.show_browse("home"),
                Key::n => {
                    ui.step_queue(1);
                }
                Key::p => {
                    ui.step_queue(-1);
                }
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        }
    });
    ui.window.add_controller(keys);

    // Dropping files onto the window queues them.
    let drop = gtk::DropTarget::new(gtk::gdk::FileList::static_type(), gtk::gdk::DragAction::COPY);
    drop.connect_drop({
        let ui = Rc::clone(ui);
        move |_, value, _, _| {
            let Ok(files) = value.get::<gtk::gdk::FileList>() else { return false };
            let paths: Vec<PathBuf> = files.files().iter().filter_map(|f| f.path()).collect();
            if paths.is_empty() {
                return false;
            }
            ui.open_all(&paths);
            true
        }
    });
    ui.window.add_controller(drop);

    // One tick per display refresh, updating the things that move.
    ui.window.add_tick_callback({
        let ui = Rc::downgrade(ui);
        move |_, _| {
            if let Some(ui) = ui.upgrade() {
                ui.tick();
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        }
    });

    // Remember where each file was left.
    ui.window.connect_close_request({
        let ui = Rc::clone(ui);
        move |_| {
            ui.remember_position();
            let _ = ui.config.borrow().save();
            glib::Propagation::Proceed
        }
    });

    ui.volume.set_value(ui.config.borrow().volume);
}

impl Ui {
    /// Called every frame. Deliberately cheap: reading the clock is an
    /// atomic load, and the labels are only touched when their text would
    /// actually change.
    fn tick(&self) {
        for event in self.player.borrow_mut().poll_events() {
            match event {
                owl_media::Event::EndOfFile => {
                    glib::idle_add_local_once({
                        let ui = self.self_rc();
                        move || {
                            if let Some(ui) = ui.upgrade() {
                                if !ui.step_queue(1) {
                                    ui.refresh_transport();
                                }
                            }
                        }
                    });
                }
                owl_media::Event::Error(message) => log::warn!("{message}"),
                _ => {}
            }
        }

        let (position, duration, state) = {
            let player = self.player.borrow();
            (player.position(), player.duration(), player.state())
        };
        if duration > 0.0 && !self.scrubbing.get() {
            if (self.scrubber.value() - position).abs() > 0.05 {
                self.scrubber.set_value(position);
            }
        }
        let elapsed = format_time(position);
        if self.elapsed.text() != elapsed {
            self.elapsed.set_text(&elapsed);
        }
        let _ = state;

        let revision = self.stage.text_revision.get();
        if revision != self.subtitle_revision.get() {
            self.subtitle_revision.set(revision);
            self.show_subtitle();
        }
        // Placement is refreshed every frame rather than with the cue: the
        // picture moves whenever the window is resized or the video's
        // aspect changes, and the caption has to follow it.
        if self.subtitle.is_visible() {
            self.place_subtitle();
        }
        self.tick_chrome();
    }

    /// Someone is still at the keyboard or the mouse.
    fn note_activity(&self) {
        self.last_activity.set(std::time::Instant::now());
        if !self.cursor_shown.get() {
            self.cursor_shown.set(true);
            self.apply_chrome();
        }
    }

    fn pointer_moved(&self, x: f64, y: f64) {
        self.pointer.set((x, y));
        self.pointer_inside.set(true);
        self.note_activity();
        self.apply_chrome();
    }

    /// The pointer is the only thing still on a timer: it has no edge to
    /// belong to, so it goes away when it stops moving over a playing film.
    fn tick_chrome(&self) {
        let playing = self.player.borrow().state() == State::Playing;
        let idle = self.last_activity.get().elapsed() >= CHROME_IDLE;
        let shown = !(playing && idle);
        if shown != self.cursor_shown.get() {
            self.cursor_shown.set(shown);
            self.apply_chrome();
        }
    }

    fn apply_chrome(&self) {
        let playing = self.player.borrow().state() == State::Playing;
        let fullscreen = self.fullscreen.get();
        let on_player = self.stage_stack.visible_child_name().as_deref() == Some("player");

        // Nothing is playing, so nothing is being interrupted: show it all.
        // Immersion is for when there is something to be immersed in.
        let (x, y) = self.pointer.get();
        let inside = self.pointer_inside.get();
        let width = self.window.width() as f64;
        let height = self.window.height() as f64;

        let near_left = inside && x <= LEFT_ZONE;
        let near_bottom = inside && y >= height - BOTTOM_ZONE;
        let near_top = inside && y <= TOP_ZONE;
        let near_right = inside && x >= width - RIGHT_ZONE;

        let show_sidebar = !playing || near_left;
        let show_transport = !playing || near_bottom;
        let show_title = !playing || near_top;
        // The queue is the viewer's own choice first; once chosen it
        // follows the sidebar it is paired with, or its own edge.
        let show_queue =
            self.queue_wanted.get() && on_player && (!playing || near_right || near_left);

        // Fullscreen is for the picture. Browsing chrome stays away
        // whatever the pointer is doing; the transport still answers.
        self.sidebar_slot.set_reveal_child(show_sidebar && !fullscreen);
        self.sidebar_slot.set_can_target(show_sidebar && !fullscreen);
        set_away(&self.queue_panel, !(show_queue && !fullscreen));
        set_away(&self.transport, !show_transport);
        set_away(&self.watermark, !show_transport);
        set_away(&self.top_bar, !show_title);
        self.title_box.set_visible(on_player);

        let cursor = self.cursor_shown.get() || !playing;
        self.window.set_cursor_from_name(Some(if cursor { "default" } else { "none" }));
    }

    fn show_subtitle(&self) {
        let cue = self.stage.text.borrow().clone();
        let Some((markup, alignment)) = cue else {
            self.subtitle.set_visible(false);
            return;
        };
        // Pango refuses a whole string it cannot parse, so a cue with
        // markup we got wrong would vanish rather than merely lose its
        // styling. Check first and fall back to plain text.
        match gtk::pango::parse_markup(&markup, '\u{0}') {
            Ok(_) => self.subtitle.set_markup(&markup),
            Err(e) => {
                log::debug!("subtitle markup rejected ({e}); showing it plain");
                self.subtitle.set_text(&markup);
            }
        }
        self.subtitle.set_valign(match alignment {
            Alignment::Top => gtk::Align::Start,
            // ASS "middle" is a band across the centre; putting a caption
            // over the middle of the picture is worse than putting it low,
            // so it is treated as bottom unless a file asks for the top.
            Alignment::Middle | Alignment::Bottom => gtk::Align::End,
        });
        self.subtitle.set_visible(true);
        self.place_subtitle();
    }

    /// Keep the caption inside the picture, not the widget.
    ///
    /// For a 2.39:1 film in a 16:9 window those are different rectangles,
    /// and a subtitle placed against the widget sits in the black bar
    /// below the image — which is where a lot of players put it.
    fn place_subtitle(&self) {
        let (x, y, w, h) = self.stage.video_rect.get();
        if w <= 1.0 || h <= 1.0 {
            return;
        }
        let side = (x + w * 0.05).max(0.0) as i32;
        self.subtitle.set_margin_start(side);
        self.subtitle.set_margin_end(side);

        if self.subtitle.valign() == gtk::Align::Start {
            self.subtitle.set_margin_top((y + h * 0.05) as i32);
        } else {
            let widget_height = self.stage.widget.height() as f32;
            let below_picture = (widget_height - (y + h)).max(0.0);
            let inset = (below_picture + h * 0.06) as i32;
            // Never underneath the transport bar, even while it is hidden:
            // the bar reveals on hover and must not land on the caption.
            self.subtitle.set_margin_bottom(inset.max(TRANSPORT_CLEARANCE));
        }
    }

    /// Switch to the player view.
    pub fn show_player(&self) {
        self.stage_stack.set_visible_child_name("player");
        self.apply_chrome();
    }

    /// Switch to the browser, at one of the sidebar's destinations.
    pub fn show_browse(&self, place: &str) {
        let (label, dir) = match place {
            "videos" => ("Movies & Shows", glib::user_special_dir(glib::UserDirectory::Videos)),
            "music" => ("Music", glib::user_special_dir(glib::UserDirectory::Music)),
            _ => ("Local Files", Some(glib::home_dir())),
        };
        // An XDG folder the user has never created is not an error; fall
        // back to home rather than showing an empty view for a missing path.
        let dir = dir.filter(|d| d.is_dir()).unwrap_or_else(glib::home_dir);
        self.browser.show(&dir, label);
        self.stage_stack.set_visible_child_name("browse");
        self.apply_chrome();
    }

    /// Play `files[at]`, with the rest of the folder queued behind it.
    fn play_from(&self, files: Vec<PathBuf>, at: usize) {
        if files.is_empty() {
            return;
        }
        {
            let mut queue = self.queue.borrow_mut();
            queue.items.clear();
            queue.current = None;
            for file in &files {
                queue.add(file);
            }
        }
        let start = files.get(at).cloned().unwrap_or_else(|| files[0].clone());
        self.show_player();
        self.open(&start);
    }

    /// Audio and subtitle track choice, plus opening files. The design
    /// puts these behind the transport's "…" rather than giving each a
    /// button, so the control row stays the shape of the mockup.
    fn show_more_menu(&self) {
        let popover = gtk::Popover::new();
        popover.set_autohide(true);
        popover.set_position(gtk::PositionType::Top);

        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.add_css_class("owl-menu");
        column.set_size_request(240, -1);

        let open = gtk::Button::with_label("Open Files…");
        open.add_css_class("owl-menu-item");
        open.set_halign(gtk::Align::Fill);
        if let Some(label) = open.child().and_then(|c| c.downcast::<gtk::Label>().ok()) {
            label.set_xalign(0.0);
        }
        open.connect_clicked({
            let ui = self.self_rc();
            let popover = popover.clone();
            move |_| {
                popover.popdown();
                if let Some(ui) = ui.upgrade() {
                    ui.open_dialog();
                }
            }
        });
        column.append(&open);

        let info = self.player.borrow().info().cloned();
        if let Some(info) = info {
            let selection = self.player.borrow().selection();
            let audio_tracks: Vec<_> = info.tracks_of(TrackKind::Audio).cloned().collect();
            if audio_tracks.len() > 1 {
                column.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
                column.append(&menu_heading("AUDIO"));
                let mut group: Option<gtk::CheckButton> = None;
                for track in &audio_tracks {
                    let active = selection.and_then(|s| s.audio) == Some(track.index);
                    let button = self.track_choice(&track.label(), active, &mut group, &popover, TrackKind::Audio, Some(track.index));
                    column.append(&button);
                }
            }

            let subtitle_tracks: Vec<_> = info.tracks_of(TrackKind::Subtitle).cloned().collect();
            if !subtitle_tracks.is_empty() {
                column.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
                column.append(&menu_heading("SUBTITLES"));
                let current = self.player.borrow().subtitle_track();
                let mut group: Option<gtk::CheckButton> = None;
                let off = self.track_choice("Off", current.is_none(), &mut group, &popover, TrackKind::Subtitle, None);
                column.append(&off);
                for track in &subtitle_tracks {
                    let active = current == Some(track.index);
                    let button = self.track_choice(&track.label(), active, &mut group, &popover, TrackKind::Subtitle, Some(track.index));
                    column.append(&button);
                }
            }
        }

        popover.set_child(Some(&column));
        popover.set_parent(&self.more_button);
        // A popover parented to a widget has to be unparented again or it
        // leaks and warns on window close.
        popover.connect_closed(|p| p.unparent());
        popover.popup();
    }

    fn track_choice(
        &self,
        label: &str,
        active: bool,
        group: &mut Option<gtk::CheckButton>,
        popover: &gtk::Popover,
        kind: TrackKind,
        index: Option<usize>,
    ) -> gtk::CheckButton {
        let button = gtk::CheckButton::with_label(label);
        match group {
            None => *group = Some(button.clone()),
            Some(first) => button.set_group(Some(first)),
        }
        button.set_active(active);
        button.connect_toggled({
            let ui = self.self_rc();
            let popover = popover.clone();
            move |b| {
                if !b.is_active() {
                    return;
                }
                popover.popdown();
                if let Some(ui) = ui.upgrade() {
                    ui.choose_track(kind, index);
                }
            }
        });
        button
    }

    /// Switch a track and put the window back in step with the reopened
    /// file. `set_track` restarts playback at the current position, so the
    /// title, chips and chapter list all have to be redrawn.
    fn choose_track(&self, kind: TrackKind, index: Option<usize>) {
        if self.player.borrow().selection().map(|s| match kind {
            TrackKind::Audio => s.audio,
            TrackKind::Subtitle => s.subtitle,
            TrackKind::Video => s.video,
        }) == Some(index)
        {
            return;
        }
        if let Err(e) = self.player.borrow_mut().set_track(kind, index) {
            log::error!("cannot switch track: {e}");
            return;
        }
        if index.is_none() {
            self.subtitle.set_visible(false);
        }
        let info = self.player.borrow().info().cloned();
        if let Some(info) = info {
            self.show_metadata(&info);
        }
        self.refresh_transport();
        self.stage.refresh();
    }

    fn self_rc(&self) -> std::rc::Weak<Ui> {
        // The tick callback holds the only weak handle we need; this is a
        // convenience for the idle closure above.
        SELF.with(|s| s.borrow().clone()).unwrap_or_default()
    }

    pub fn open_all(&self, paths: &[PathBuf]) {
        let mut first = None;
        for path in paths {
            let index = self.queue.borrow_mut().add(path);
            if first.is_none() {
                first = Some(index);
            }
        }
        self.rebuild_queue_list();
        if let Some(index) = first {
            let path = self.queue.borrow().items[index].path.clone();
            self.open(&path);
        }
    }

    pub fn open(&self, path: &Path) {
        self.remember_position();

        let index = self.queue.borrow_mut().add(path);
        self.queue.borrow_mut().current = Some(index);

        let prefer = TrackPreference::default();
        let info = match self.player.borrow_mut().open(path, prefer) {
            Ok(info) => info,
            Err(e) => {
                log::error!("cannot open {}: {e}", path.display());
                self.title.set_text(&format!("Cannot open {}", path.display()));
                return;
            }
        };

        if let Some(resume) = self.config.borrow().resume_for(path) {
            self.player.borrow_mut().seek(resume);
        }
        self.player.borrow_mut().play();

        self.show_metadata(&info);
        self.rebuild_queue_list();
        self.refresh_transport();
        self.show_player();
        self.stage.refresh();
    }

    fn show_metadata(&self, info: &MediaInfo) {
        self.title.set_text(&info.display_title());
        self.window.set_title(Some(&format!("{} — Owl Player", info.display_title())));

        while let Some(child) = self.chips.first_child() {
            self.chips.remove(&child);
        }
        // The chips are derived from the file every time rather than
        // cached, so they can never disagree with what is playing.
        let chips = info.chips();
        for (i, text) in chips.iter().enumerate() {
            let label = gtk::Label::new(Some(text));
            label.add_css_class("owl-chip");
            if i == 0 {
                label.add_css_class("hdr");
            }
            self.chips.append(&label);
        }

        self.scrubber.set_range(0.0, info.duration.max(0.001));
        self.remaining.set_text(&format_time(info.duration));
        self.rebuild_chapters(info);
        self.rebuild_details(info);
    }

    fn rebuild_queue_list(&self) {
        while let Some(child) = self.queue_list.first_child() {
            self.queue_list.remove(&child);
        }
        let queue = self.queue.borrow();
        for (i, item) in queue.items.iter().enumerate() {
            let row = gtk::ListBoxRow::new();
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 11);

            let thumb = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            thumb.add_css_class("owl-thumb");
            thumb.set_size_request(58, 38);
            thumb.set_valign(gtk::Align::Center);
            let glyph = gtk::Image::from_icon_name("video-x-generic-symbolic");
            glyph.set_hexpand(true);
            thumb.append(&glyph);
            line.append(&thumb);

            let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
            text.set_valign(gtk::Align::Center);
            text.set_hexpand(true);
            let title = gtk::Label::new(Some(&item.title));
            title.add_css_class("owl-item-title");
            title.set_xalign(0.0);
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            let time = gtk::Label::new(Some(&format_time(item.duration)));
            time.add_css_class("owl-item-time");
            time.set_xalign(0.0);
            text.append(&title);
            text.append(&time);
            line.append(&text);

            if queue.current == Some(i) {
                let mark = gtk::Image::from_icon_name("media-playback-start-symbolic");
                mark.add_css_class("owl-now-mark");
                mark.set_valign(gtk::Align::Center);
                line.append(&mark);
            }

            row.set_child(Some(&line));
            self.queue_list.append(&row);
            if queue.current == Some(i) {
                self.queue_list.select_row(Some(&row));
            }
        }
        self.panel_foot.set_text(&queue.summary());
    }

    fn rebuild_chapters(&self, info: &MediaInfo) {
        while let Some(child) = self.chapter_list.first_child() {
            self.chapter_list.remove(&child);
        }
        if info.chapters.is_empty() {
            let row = gtk::ListBoxRow::new();
            row.set_selectable(false);
            let label = gtk::Label::new(Some("This file has no chapters."));
            label.add_css_class("owl-item-time");
            label.set_margin_top(18);
            label.set_margin_bottom(18);
            row.set_child(Some(&label));
            self.chapter_list.append(&row);
            return;
        }
        for chapter in &info.chapters {
            let row = gtk::ListBoxRow::new();
            let line = gtk::Box::new(gtk::Orientation::Vertical, 2);
            let title = gtk::Label::new(Some(&chapter.title));
            title.add_css_class("owl-item-title");
            title.set_xalign(0.0);
            let time = gtk::Label::new(Some(&format_time(chapter.start)));
            time.add_css_class("owl-item-time");
            time.set_xalign(0.0);
            line.append(&title);
            line.append(&time);
            row.set_child(Some(&line));
            self.chapter_list.append(&row);
        }
    }

    fn rebuild_details(&self, info: &MediaInfo) {
        while let Some(child) = self.details_list.first_child() {
            self.details_list.remove(&child);
        }
        let add = |key: &str, value: String| {
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            let k = gtk::Label::new(Some(key));
            k.add_css_class("owl-item-time");
            k.set_xalign(0.0);
            k.set_size_request(92, -1);
            let v = gtk::Label::new(Some(&value));
            v.add_css_class("owl-item-title");
            v.set_xalign(0.0);
            v.set_hexpand(true);
            v.set_wrap(true);
            line.append(&k);
            line.append(&v);
            self.details_list.append(&line);
        };

        add("Container", info.container.clone());
        add("Duration", format_time(info.duration));
        if info.bit_rate > 0 {
            add("Bit rate", format!("{} kb/s", info.bit_rate / 1000));
        }
        for track in &info.tracks {
            let key = match track.kind {
                TrackKind::Video => "Video",
                TrackKind::Audio => "Audio",
                TrackKind::Subtitle => "Subtitle",
            };
            add(key, track.label());
        }
    }

    fn refresh_transport(&self) {
        let state = self.player.borrow().state();
        self.play_icon.set_icon_name(Some(if state == State::Playing {
            "media-playback-pause-symbolic"
        } else {
            "media-playback-start-symbolic"
        }));
        self.play_button.set_tooltip_text(Some(if state == State::Playing { "Pause" } else { "Play" }));
    }

    fn seek_by(&self, delta: f64) {
        self.player.borrow_mut().seek_by(delta);
        self.stage.refresh();
    }

    /// Move `step` places through the queue. Returns false when there is
    /// nowhere to go, which is how the end of the last file becomes a
    /// stop rather than a wrap-around.
    fn step_queue(&self, step: i32) -> bool {
        let target = {
            let queue = self.queue.borrow();
            if step > 0 { queue.next_index() } else { queue.previous_index() }
        };
        let Some(index) = target else { return false };
        let path = self.queue.borrow().items[index].path.clone();
        self.open(&path);
        true
    }

    fn toggle_fullscreen(&self) {
        let next = !self.fullscreen.get();
        self.fullscreen.set(next);
        if next {
            self.window.fullscreen();
        } else {
            self.window.unfullscreen();
        }
        self.note_activity();
        self.apply_chrome();
    }

    fn remember_position(&self) {
        let player = self.player.borrow();
        if let Some(path) = player.path() {
            let (position, duration) = (player.position(), player.duration());
            drop(player);
            self.config.borrow_mut().remember(&path, position, duration);
        }
    }

    pub fn open_dialog(&self) {
        let dialog = gtk::FileDialog::new();
        dialog.set_title("Open media");

        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Media files"));
        for mime in crate::MIME_TYPES {
            filter.add_mime_type(mime);
        }
        let all = gtk::FileFilter::new();
        all.set_name(Some("All files"));
        all.add_pattern("*");
        let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        filters.append(&all);
        dialog.set_filters(Some(&filters));
        dialog.set_default_filter(Some(&filter));

        let ui = self.self_rc();
        dialog.open_multiple(Some(&self.window), gtk::gio::Cancellable::NONE, move |result| {
            let Ok(files) = result else { return };
            let paths: Vec<PathBuf> =
                (0..files.n_items()).filter_map(|i| files.item(i)).filter_map(|o| o.downcast::<gtk::gio::File>().ok()).filter_map(|f| f.path()).collect();
            if let Some(ui) = ui.upgrade() {
                ui.open_all(&paths);
            }
        });
    }
}

thread_local! {
    static SELF: RefCell<Option<std::rc::Weak<Ui>>> = const { RefCell::new(None) };
}

/// Publish the window so the few callbacks that need to re-enter it can.
pub fn register(ui: &Rc<Ui>) {
    SELF.with(|s| *s.borrow_mut() = Some(Rc::downgrade(ui)));
}

/// The live window, if there is one.
pub fn current() -> Option<Rc<Ui>> {
    SELF.with(|s| s.borrow().as_ref().and_then(|w| w.upgrade()))
}
