//! Finding something to play.
//!
//! A player whose only way in is a file dialog buried behind a "…" button
//! is a player you have to already know. This is the other half: a list of
//! the places media actually lives, and a plain view of what is in them.
//!
//! It is deliberately not a file manager. It shows folders and the files
//! this player can open, and nothing else — no sizes, no permissions, no
//! sorting options. Anything more belongs in Raven Files.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4 as gtk;
use gtk4::prelude::*;

/// Extensions worth showing. Wider than the desktop MIME list on purpose:
/// that one says what OwlPlayer wants to be the default for, this one says
/// what it will happily open if you point it at one.
const MEDIA: &[&str] = &[
    "mp4", "mkv", "webm", "mov", "avi", "m4v", "mpg", "mpeg", "mp2", "ts", "m2ts", "mts", "wmv",
    "flv", "ogv", "ogm", "3gp", "3g2", "divx", "vob", "rmvb", "asf", "f4v", "mxf", "y4m",
    "mp3", "flac", "wav", "ogg", "oga", "opus", "m4a", "aac", "wma", "aiff", "aif", "ape", "wv",
    "mpc", "tta", "dsf", "spx", "caf", "au", "mka",
];

pub fn is_media(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| MEDIA.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

enum Row {
    Parent(PathBuf),
    Folder(PathBuf),
    Media(PathBuf),
}

pub struct Browser {
    pub widget: gtk::Box,
    list: gtk::ListBox,
    place_label: gtk::Label,
    path_label: gtk::Label,
    empty: gtk::Label,
    rows: RefCell<Vec<Row>>,
    current: RefCell<PathBuf>,
    /// Called with every playable file in the folder and which one was
    /// picked, so activating one file queues the rest of the album or
    /// series alongside it.
    on_play: RefCell<Option<Box<dyn Fn(Vec<PathBuf>, usize)>>>,
}

impl Browser {
    pub fn new() -> Rc<Browser> {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.add_css_class("owl-browse");

        let header = gtk::Box::new(gtk::Orientation::Vertical, 2);
        header.add_css_class("owl-browse-head");
        let place_label = gtk::Label::new(Some("Local Files"));
        place_label.add_css_class("owl-browse-title");
        place_label.set_xalign(0.0);
        let path_label = gtk::Label::new(None);
        path_label.add_css_class("owl-browse-path");
        path_label.set_xalign(0.0);
        path_label.set_ellipsize(gtk::pango::EllipsizeMode::Start);
        header.append(&place_label);
        header.append(&path_label);

        let places = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        places.add_css_class("owl-places");

        let list = gtk::ListBox::new();
        list.add_css_class("owl-browse-list");
        list.set_selection_mode(gtk::SelectionMode::Single);

        let empty = gtk::Label::new(Some("Nothing playable in this folder."));
        empty.add_css_class("owl-browse-empty");
        empty.set_visible(false);

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_vexpand(true);
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&list);
        body.append(&empty);
        scroll.set_child(Some(&body));

        widget.append(&header);
        widget.append(&places);
        widget.append(&scroll);

        let browser = Rc::new(Browser {
            widget,
            list: list.clone(),
            place_label,
            path_label,
            empty,
            rows: RefCell::new(Vec::new()),
            current: RefCell::new(home()),
            on_play: RefCell::new(None),
        });

        for (label, dir) in default_places() {
            let button = gtk::Button::with_label(&label);
            button.add_css_class("owl-place");
            let browser_ref = Rc::downgrade(&browser);
            button.connect_clicked(move |_| {
                if let Some(b) = browser_ref.upgrade() {
                    b.show(&dir, &label);
                }
            });
            places.append(&button);
        }

        list.connect_row_activated({
            let browser_ref = Rc::downgrade(&browser);
            move |_, row| {
                let Some(browser) = browser_ref.upgrade() else { return };
                browser.activate(row.index() as usize);
            }
        });

        browser
    }

    pub fn connect_play(&self, f: impl Fn(Vec<PathBuf>, usize) + 'static) {
        *self.on_play.borrow_mut() = Some(Box::new(f));
    }

    /// Show `dir`, labelled as `place`.
    pub fn show(&self, dir: &Path, place: &str) {
        self.place_label.set_text(place);
        self.navigate(dir);
    }

    pub fn navigate(&self, dir: &Path) {
        *self.current.borrow_mut() = dir.to_path_buf();
        self.path_label.set_text(&pretty_path(dir));

        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let mut rows = Vec::new();

        if let Some(parent) = dir.parent() {
            self.list.append(&entry_row("go-up-symbolic", "..", None));
            rows.push(Row::Parent(parent.to_path_buf()));
        }

        let mut folders: Vec<PathBuf> = Vec::new();
        let mut files: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                // Dotfiles are noise in a media folder.
                if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.')) {
                    continue;
                }
                if path.is_dir() {
                    folders.push(path);
                } else if is_media(&path) {
                    files.push(path);
                }
            }
        }
        folders.sort_by_key(|p| natural_key(p));
        files.sort_by_key(|p| natural_key(p));

        for folder in folders {
            self.list.append(&entry_row("folder-symbolic", &name_of(&folder), None));
            rows.push(Row::Folder(folder));
        }
        for file in &files {
            let icon = if is_audio(file) { "audio-x-generic-symbolic" } else { "video-x-generic-symbolic" };
            self.list.append(&entry_row(icon, &stem_of(file), Some(&extension_of(file))));
            rows.push(Row::Media(file.clone()));
        }

        // Only when there is genuinely nothing here. A folder of folders is
        // not empty — saying so while four of them are listed above the
        // message is just wrong.
        let has_folders = rows.iter().any(|r| matches!(r, Row::Folder(_)));
        let has_media = rows.iter().any(|r| matches!(r, Row::Media(_)));
        self.empty.set_visible(!has_folders && !has_media);
        *self.rows.borrow_mut() = rows;
    }

    fn activate(&self, index: usize) {
        let (target, play) = {
            let rows = self.rows.borrow();
            match rows.get(index) {
                Some(Row::Parent(p)) | Some(Row::Folder(p)) => (Some(p.clone()), None),
                Some(Row::Media(p)) => {
                    // Everything playable in this folder becomes the queue,
                    // in the order shown, starting at what was clicked.
                    let files: Vec<PathBuf> = rows
                        .iter()
                        .filter_map(|r| match r {
                            Row::Media(f) => Some(f.clone()),
                            _ => None,
                        })
                        .collect();
                    let at = files.iter().position(|f| f == p).unwrap_or(0);
                    (None, Some((files, at)))
                }
                None => (None, None),
            }
        };
        if let Some(dir) = target {
            self.navigate(&dir);
        }
        if let Some((files, at)) = play {
            if let Some(f) = self.on_play.borrow().as_ref() {
                f(files, at);
            }
        }
    }
}

fn entry_row(icon: &str, title: &str, badge: Option<&str>) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 11);
    let image = gtk::Image::from_icon_name(icon);
    image.add_css_class("owl-browse-icon");
    let label = gtk::Label::new(Some(title));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    label.add_css_class("owl-item-title");
    line.append(&image);
    line.append(&label);
    if let Some(badge) = badge {
        let b = gtk::Label::new(Some(badge));
        b.add_css_class("owl-browse-ext");
        line.append(&b);
    }
    row.set_child(Some(&line));
    row
}

fn home() -> PathBuf {
    glib::home_dir()
}

/// Home, plus whichever XDG media folders actually exist.
fn default_places() -> Vec<(String, PathBuf)> {
    let mut places = vec![("Home".to_string(), home())];
    for (label, dir) in [
        ("Videos", glib::user_special_dir(glib::UserDirectory::Videos)),
        ("Music", glib::user_special_dir(glib::UserDirectory::Music)),
        ("Downloads", glib::user_special_dir(glib::UserDirectory::Downloads)),
    ] {
        if let Some(dir) = dir {
            if dir.is_dir() && dir != home() {
                places.push((label.to_string(), dir));
            }
        }
    }
    places
}

fn pretty_path(path: &Path) -> String {
    let home = home();
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

fn name_of(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

fn stem_of(path: &Path) -> String {
    path.file_stem().map(|n| n.to_string_lossy().replace(['.', '_'], " ")).unwrap_or_default()
}

fn extension_of(path: &Path) -> String {
    path.extension().map(|e| e.to_string_lossy().to_uppercase()).unwrap_or_default()
}

fn is_audio(path: &Path) -> bool {
    const AUDIO: &[&str] = &[
        "mp3", "flac", "wav", "ogg", "oga", "opus", "m4a", "aac", "wma", "aiff", "aif", "ape",
        "wv", "mpc", "tta", "dsf", "spx", "caf", "au", "mka",
    ];
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Sort "Episode 2" before "Episode 10", which plain string order does not.
fn natural_key(path: &Path) -> Vec<(u64, String)> {
    let name = name_of(path).to_lowercase();
    let mut key = Vec::new();
    let mut text = String::new();
    let mut digits = String::new();
    for c in name.chars() {
        if c.is_ascii_digit() {
            if !text.is_empty() {
                key.push((0, std::mem::take(&mut text)));
            }
            digits.push(c);
        } else {
            if !digits.is_empty() {
                key.push((digits.parse().unwrap_or(0), String::new()));
                digits.clear();
            }
            text.push(c);
        }
    }
    if !digits.is_empty() {
        key.push((digits.parse().unwrap_or(0), String::new()));
    }
    if !text.is_empty() {
        key.push((0, text));
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn episodes_sort_numerically() {
        let mut names: Vec<PathBuf> =
            ["ep10.mkv", "ep2.mkv", "ep1.mkv"].iter().map(PathBuf::from).collect();
        names.sort_by_key(|p| natural_key(p));
        let order: Vec<String> = names.iter().map(|p| name_of(p)).collect();
        assert_eq!(order, vec!["ep1.mkv", "ep2.mkv", "ep10.mkv"]);
    }

    #[test]
    fn media_is_recognised_case_insensitively() {
        assert!(is_media(Path::new("Film.MKV")));
        assert!(is_media(Path::new("song.flac")));
        assert!(!is_media(Path::new("notes.txt")));
        assert!(!is_media(Path::new("no-extension")));
    }

    #[test]
    fn audio_and_video_are_distinguished() {
        assert!(is_audio(Path::new("a.flac")));
        assert!(!is_audio(Path::new("a.mkv")));
    }
}
