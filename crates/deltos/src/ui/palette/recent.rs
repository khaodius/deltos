//! The palette's recently used items, newest first, kept in a small text file beside the
//! settings: one item id per line.

use std::path::PathBuf;

use gpui::{App, Global};

/// How many ids are remembered.
pub const LIMIT: usize = 50;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Recent {
    pub ids: Vec<String>,
    file: Option<PathBuf>,
}

impl Global for Recent {}

impl Recent {
    /// Reads the list from its file text; blank and repeated lines are skipped.
    pub fn parse(text: &str) -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        for line in text.lines().map(str::trim) {
            if !line.is_empty() && !ids.iter().any(|i| i == line) && ids.len() < LIMIT {
                ids.push(line.to_owned());
            }
        }
        ids
    }

    /// Moves `id` to the front.
    pub fn touch(ids: &mut Vec<String>, id: &str) {
        ids.retain(|i| i != id);
        ids.insert(0, id.to_owned());
        ids.truncate(LIMIT);
    }
}

/// Loads the list in the background. Without a file (the demo, or no config folder) the
/// list lives for this run only.
pub fn init(file: Option<PathBuf>, cx: &mut App) {
    cx.set_global(Recent {
        ids: Vec::new(),
        file: file.clone(),
    });
    let Some(path) = file else {
        return;
    };
    let read = cx
        .background_executor()
        .spawn(async move { std::fs::read_to_string(path).ok() });
    cx.spawn(async move |cx| {
        if let Some(text) = read.await {
            cx.update(|cx| {
                let ids = Recent::parse(&text);
                if let Some(r) = cx.try_global::<Recent>().cloned() {
                    // Anything used while the file was being read stays in front.
                    let mut merged = r.ids;
                    for id in ids {
                        if !merged.contains(&id) && merged.len() < LIMIT {
                            merged.push(id);
                        }
                    }
                    cx.set_global(Recent {
                        ids: merged,
                        file: r.file,
                    });
                }
            })
            .ok();
        }
    })
    .detach();
}

/// Records that `id` was used, and writes the list in the background.
pub fn record(id: &str, cx: &mut App) {
    let Some(mut recent) = cx.try_global::<Recent>().cloned() else {
        return;
    };
    Recent::touch(&mut recent.ids, id);
    let file = recent.file.clone();
    let text = recent.ids.join("\n") + "\n";
    cx.set_global(recent);
    if let Some(path) = file {
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = deltos_settings::write_atomic(&path, text.as_bytes()) {
                    tracing::warn!("could not save the recent palette items: {e}");
                }
            })
            .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_first_without_repeats() {
        let mut ids = Recent::parse("a\n\n b \na\nc\n");
        assert_eq!(ids, ["a", "b", "c"]);
        Recent::touch(&mut ids, "c");
        assert_eq!(ids, ["c", "a", "b"]);
        Recent::touch(&mut ids, "d");
        assert_eq!(ids, ["d", "c", "a", "b"]);
        for n in 0..100 {
            Recent::touch(&mut ids, &n.to_string());
        }
        assert_eq!(ids.len(), LIMIT);
        assert_eq!(ids[0], "99");
        assert!(!ids.iter().any(|i| i == "a"));
    }

    #[test]
    fn a_long_file_is_cut() {
        let text: String = (0..500).map(|n| format!("{n}\n")).collect();
        assert_eq!(Recent::parse(&text).len(), LIMIT);
    }
}
