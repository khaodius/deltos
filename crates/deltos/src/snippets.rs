//! Saved queries: named statements for one connection or for every connection, kept in
//! `snippets.json` in the config folder. The store is read in the background at start and
//! written in the background after each change.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use deltos_settings::{PanelKind, Paths};
use gpui::{App, AppContext, Context, Entity, Global, Task, WeakEntity};
use serde::{Deserialize, Serialize};

use crate::state::AppState;
use crate::ui::editor::Editor;

/// One saved query.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Snippet {
    /// Stable while the snippet exists, unique in the file.
    pub id: String,
    pub name: String,
    pub text: String,
    /// The connection it belongs to, or `None` for every connection.
    pub connection: Option<String>,
    /// When it was saved or renamed last, in milliseconds since 1970.
    pub updated: u64,
}

impl Snippet {
    /// Whether it shows for `connection`: a snippet for every connection always does.
    pub fn applies_to(&self, connection: Option<&str>) -> bool {
        match (&self.connection, connection) {
            (None, _) => true,
            (Some(own), Some(c)) => own == c,
            (Some(_), None) => false,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct SnippetFile {
    snippets: Vec<Snippet>,
}

/// The saved queries of this install.
pub struct SnippetStore {
    items: Vec<Snippet>,
    path: Option<PathBuf>,
    /// Whether the file has been read; until then nothing is written over it.
    loaded: bool,
    /// The last write, which the next one waits for so they land in order.
    write: Option<Task<()>>,
    /// The query editor used last, for "insert at cursor".
    last_editor: Option<WeakEntity<Editor>>,
}

struct Snippets(Entity<SnippetStore>);

impl Global for Snippets {}

/// Creates the store and reads the file in the background.
pub fn start(paths: Option<&Paths>, cx: &mut App) {
    let path = paths.map(Paths::snippets_file);
    let store = cx.new(|_| SnippetStore {
        items: Vec::new(),
        path: path.clone(),
        loaded: path.is_none(),
        write: None,
        last_editor: None,
    });
    cx.set_global(Snippets(store.clone()));
    let Some(path) = path else {
        return;
    };
    let read = cx.background_spawn(async move { read_file(&path) });
    cx.spawn(async move |cx| {
        // A file that cannot be read is not written over: what is saved now stays in
        // this session only.
        let Some(found) = read.await else {
            return;
        };
        store
            .update(cx, |s, cx| {
                // Anything saved while the file was read is kept, after what was there.
                let added = std::mem::replace(&mut s.items, found);
                s.loaded = true;
                if !added.is_empty() {
                    for snippet in added {
                        s.put(snippet);
                    }
                    s.persist(cx);
                }
                cx.notify();
            })
            .ok();
    })
    .detach();
}

/// The saved queries in the file, or `None` when it cannot be read. A file that is not
/// valid JSON is moved aside to `snippets.json.broken` first, so the next save does not
/// destroy what someone may still fix by hand.
fn read_file(path: &std::path::Path) -> Option<Vec<Snippet>> {
    match std::fs::read(path) {
        Ok(bytes) => match parse(&bytes) {
            Ok(items) => Some(items),
            Err(e) => {
                let mut aside = path.as_os_str().to_owned();
                aside.push(".broken");
                let aside = PathBuf::from(aside);
                tracing::warn!(
                    "could not read {}: {e}; moving it to {}",
                    path.display(),
                    aside.display()
                );
                match std::fs::rename(path, &aside) {
                    Ok(()) => Some(Vec::new()),
                    Err(e) => {
                        tracing::warn!("could not move {}: {e}", path.display());
                        None
                    }
                }
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(Vec::new()),
        Err(e) => {
            tracing::warn!("could not read {}: {e}", path.display());
            None
        }
    }
}

/// The snippets of a file, without empty or repeated ones.
fn parse(bytes: &[u8]) -> Result<Vec<Snippet>, serde_json::Error> {
    let file: SnippetFile = serde_json::from_slice(bytes)?;
    let mut out: Vec<Snippet> = Vec::with_capacity(file.snippets.len());
    for mut s in file.snippets {
        s.name = s.name.trim().to_owned();
        if s.name.is_empty() || s.text.trim().is_empty() {
            continue;
        }
        if s.id.is_empty() || out.iter().any(|o| o.id == s.id) {
            s.id = new_id(&out);
        }
        out.push(s);
    }
    Ok(out)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// An id no snippet in `items` has.
fn new_id(items: &[Snippet]) -> String {
    let mut n = now_ms();
    loop {
        let id = format!("{n:x}");
        if !items.iter().any(|s| s.id == id) {
            return id;
        }
        n += 1;
    }
}

/// The snippet named `name` (in any case) in the same scope, other than `except`.
fn clash<'a>(
    items: &'a [Snippet],
    name: &str,
    connection: Option<&str>,
    except: Option<&str>,
) -> Option<&'a Snippet> {
    let name = name.trim().to_lowercase();
    items.iter().find(|s| {
        s.connection.as_deref() == connection
            && s.name.to_lowercase() == name
            && Some(s.id.as_str()) != except
    })
}

/// `items` sorted for a list: by name, those of one connection before those of all.
fn sorted(mut items: Vec<Snippet>) -> Vec<Snippet> {
    items.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(b.connection.is_some().cmp(&a.connection.is_some()))
    });
    items
}

impl SnippetStore {
    pub fn items(&self) -> &[Snippet] {
        &self.items
    }

    /// The saved query named `name` in that scope, which saving under that name replaces.
    pub fn clash(&self, name: &str, connection: Option<&str>) -> Option<&Snippet> {
        clash(&self.items, name, connection, None)
    }

    /// Saves `text` as `name`, replacing a snippet of that name in the same scope.
    pub fn save(
        &mut self,
        name: &str,
        text: &str,
        connection: Option<String>,
        cx: &mut Context<Self>,
    ) -> String {
        let snippet = Snippet {
            id: String::new(),
            name: name.trim().to_owned(),
            text: text.to_owned(),
            connection,
            updated: now_ms(),
        };
        let id = self.put(snippet);
        self.persist(cx);
        cx.notify();
        id
    }

    /// Adds `snippet`, or replaces the one of its name and scope. Returns its id.
    fn put(&mut self, mut snippet: Snippet) -> String {
        let existing = clash(
            &self.items,
            &snippet.name,
            snippet.connection.as_deref(),
            None,
        )
        .map(|s| s.id.clone());
        match existing.and_then(|id| self.items.iter_mut().find(|s| s.id == id)) {
            Some(old) => {
                snippet.id = old.id.clone();
                *old = snippet;
                old.id.clone()
            }
            None => {
                if snippet.id.is_empty() || self.items.iter().any(|s| s.id == snippet.id) {
                    snippet.id = new_id(&self.items);
                }
                let id = snippet.id.clone();
                self.items.push(snippet);
                id
            }
        }
    }

    /// Gives a snippet a new name. Fails when another one of its scope has that name.
    pub fn rename(&mut self, id: &str, name: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("A saved query needs a name.".into());
        }
        let Some(connection) = self
            .items
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.connection.clone())
        else {
            return Ok(());
        };
        if clash(&self.items, name, connection.as_deref(), Some(id)).is_some() {
            return Err(format!("Another saved query is called “{name}”."));
        }
        if let Some(s) = self.items.iter_mut().find(|s| s.id == id) {
            s.name = name.to_owned();
            s.updated = now_ms();
        }
        self.persist(cx);
        cx.notify();
        Ok(())
    }

    pub fn delete(&mut self, id: &str, cx: &mut Context<Self>) {
        let before = self.items.len();
        self.items.retain(|s| s.id != id);
        if self.items.len() != before {
            self.persist(cx);
            cx.notify();
        }
    }

    /// Writes the file in the background, after the write before it.
    fn persist(&mut self, cx: &mut Context<Self>) {
        let (Some(path), true) = (self.path.clone(), self.loaded) else {
            return;
        };
        let file = SnippetFile {
            snippets: self.items.clone(),
        };
        let bytes = match serde_json::to_vec_pretty(&file) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("could not save the saved queries: {e}");
                return;
            }
        };
        let before = self.write.take();
        self.write = Some(cx.background_spawn(async move {
            if let Some(before) = before {
                before.await;
            }
            if let Err(e) = deltos_settings::write_atomic(&path, &bytes) {
                tracing::warn!("could not save the saved queries: {e}");
            }
        }));
    }
}

pub fn store(cx: &App) -> Option<Entity<SnippetStore>> {
    cx.try_global::<Snippets>().map(|s| s.0.clone())
}

/// Every saved query, sorted by name: for lists such as a command palette.
pub fn all_snippets(cx: &App) -> Vec<Snippet> {
    store(cx).map_or_else(Vec::new, |s| sorted(s.read(cx).items.clone()))
}

/// The saved queries that show for `connection`: its own and those for every
/// connection, sorted by name.
pub fn snippets_for(connection: Option<&str>, cx: &App) -> Vec<Snippet> {
    store(cx).map_or_else(Vec::new, |s| {
        sorted(
            s.read(cx)
                .items
                .iter()
                .filter(|s| s.applies_to(connection))
                .cloned()
                .collect(),
        )
    })
}

/// Opens `snippet` in a new query tab, on its connection or else the active one.
pub fn open(state: &Entity<AppState>, snippet: &Snippet, cx: &mut App) {
    let connection = snippet
        .connection
        .clone()
        .or_else(|| state.read(cx).active.clone());
    let kind = PanelKind::Query {
        connection,
        text: snippet.text.clone(),
    };
    state.update(cx, |s, cx| s.open(kind, true, cx));
}

/// Remembers the query editor used last, for [`insert_at_cursor`].
pub fn set_last_editor(editor: &Entity<Editor>, cx: &mut App) {
    if let Some(store) = store(cx) {
        let same = store
            .read(cx)
            .last_editor
            .as_ref()
            .is_some_and(|e| e.entity_id() == editor.entity_id());
        if !same {
            let weak = editor.downgrade();
            store.update(cx, |s, _| s.last_editor = Some(weak));
        }
    }
}

/// Types `text` at the cursor of the query editor used last. Returns false when there is
/// none open.
pub fn insert_at_cursor(text: &str, cx: &mut App) -> bool {
    let Some(editor) = store(cx)
        .and_then(|s| s.read(cx).last_editor.clone())
        .and_then(|e| e.upgrade())
    else {
        return false;
    };
    editor.update(cx, |e, cx| e.insert_at_cursor(text, cx));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snippet(id: &str, name: &str, connection: Option<&str>) -> Snippet {
        Snippet {
            id: id.into(),
            name: name.into(),
            text: format!("select '{name}'"),
            connection: connection.map(Into::into),
            updated: 1,
        }
    }

    #[test]
    fn the_file_is_read_forgivingly() {
        let text = br#"{"snippets": [
            {"id": "a", "name": " Daily ", "text": "select 1", "connection": "pg", "extra": 1},
            {"id": "a", "name": "Again", "text": "select 2"},
            {"name": "", "text": "select 3"},
            {"name": "Blank", "text": "   "},
            {"name": "No id", "text": "select 4"}
        ], "version": 2}"#;
        let items = parse(text).unwrap();
        let names: Vec<&str> = items.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Daily", "Again", "No id"]);
        assert_eq!(items[0].connection.as_deref(), Some("pg"));
        assert_eq!(items[1].connection, None);
        assert_ne!(items[1].id, "a");
        assert_ne!(items[2].id, items[1].id);
        assert!(parse(b"{}").unwrap().is_empty());
        assert!(parse(b"not json").is_err());
    }

    #[test]
    fn a_broken_file_is_moved_aside_not_lost() {
        let dir = std::env::temp_dir().join(format!(
            "deltos-snippets-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("snippets.json");
        assert_eq!(read_file(&path), Some(Vec::new()));
        std::fs::write(&path, b"{\"snippets\": [ {\"name\": \"a\", }").unwrap();
        assert_eq!(read_file(&path), Some(Vec::new()));
        assert!(!path.exists());
        assert_eq!(
            std::fs::read(dir.join("snippets.json.broken")).unwrap(),
            b"{\"snippets\": [ {\"name\": \"a\", }"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_clash_within_a_scope_only() {
        let items = vec![
            snippet("1", "Daily", Some("pg")),
            snippet("2", "Daily", None),
            snippet("3", "Größe", None),
        ];
        assert_eq!(clash(&items, "daily ", Some("pg"), None).unwrap().id, "1");
        assert_eq!(clash(&items, "DAILY", None, None).unwrap().id, "2");
        assert!(clash(&items, "Daily", Some("mysql"), None).is_none());
        assert!(clash(&items, "Daily", Some("pg"), Some("1")).is_none());
        assert_eq!(clash(&items, "GRÖSSE", None, None), None);
        assert_eq!(clash(&items, "größe", None, None).unwrap().id, "3");
    }

    #[test]
    fn a_connection_sees_its_own_and_the_shared_ones() {
        let items = [
            snippet("1", "a", Some("pg")),
            snippet("2", "b", None),
            snippet("3", "c", Some("mysql")),
        ];
        let seen = |c: Option<&str>| -> Vec<&str> {
            items
                .iter()
                .filter(|s| s.applies_to(c))
                .map(|s| s.id.as_str())
                .collect()
        };
        assert_eq!(seen(Some("pg")), ["1", "2"]);
        assert_eq!(seen(None), ["2"]);
    }

    #[test]
    fn lists_are_by_name_with_a_connection_first() {
        let items = sorted(vec![
            snippet("1", "beta", None),
            snippet("2", "Alpha", None),
            snippet("3", "beta", Some("pg")),
        ]);
        let ids: Vec<&str> = items.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["2", "3", "1"]);
    }

    #[test]
    fn saving_a_known_name_replaces_it() {
        let mut store = SnippetStore {
            items: vec![snippet("1", "Daily", None)],
            path: None,
            loaded: true,
            write: None,
            last_editor: None,
        };
        let mut again = snippet("", "daily", None);
        again.text = "select 2".into();
        assert_eq!(store.put(again), "1");
        assert_eq!(store.items.len(), 1);
        assert_eq!(store.items[0].text, "select 2");
        let id = store.put(snippet("", "Daily", Some("pg")));
        assert_ne!(id, "1");
        assert_eq!(store.items.len(), 2);
    }
}
