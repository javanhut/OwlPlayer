//! The play queue. Files the viewer has lined up, with enough metadata to
//! draw a row without opening any of them for playback.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct QueueItem {
    pub path: PathBuf,
    pub title: String,
    pub duration: f64,
}

#[derive(Debug, Default)]
pub struct Queue {
    pub items: Vec<QueueItem>,
    pub current: Option<usize>,
}

impl Queue {
    /// Add a file, probing it for a title and duration. Probing opens the
    /// container and reads its header only — no decoding — so a queue of
    /// several files costs milliseconds.
    pub fn add(&mut self, path: &Path) -> usize {
        if let Some(i) = self.items.iter().position(|i| i.path == path) {
            return i;
        }
        let (title, duration) = match owl_media::probe(path) {
            Ok(info) => (info.display_title(), info.duration),
            Err(e) => {
                log::warn!("cannot read {}: {e}", path.display());
                (
                    path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "Unknown".into()),
                    0.0,
                )
            }
        };
        self.items.push(QueueItem { path: path.to_path_buf(), title, duration });
        self.items.len() - 1
    }

    pub fn total_duration(&self) -> f64 {
        self.items.iter().map(|i| i.duration).sum()
    }

    pub fn next_index(&self) -> Option<usize> {
        let current = self.current?;
        (current + 1 < self.items.len()).then_some(current + 1)
    }

    pub fn previous_index(&self) -> Option<usize> {
        let current = self.current?;
        (current > 0).then(|| current - 1)
    }

    pub fn summary(&self) -> String {
        if self.items.is_empty() {
            return "Nothing queued".into();
        }
        let minutes = (self.total_duration() / 60.0).round() as u64;
        let items = self.items.len();
        let noun = if items == 1 { "item" } else { "items" };
        if minutes > 0 { format!("{items} {noun} · {minutes} min") } else { format!("{items} {noun}") }
    }
}
