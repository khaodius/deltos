//! The last session. The workspace keeps the tabs and panel windows in the saved layout;
//! this module remembers which connections were open and which was active, opens them
//! again at start, and keeps query text saved while the app runs, so a crash loses at
//! most a few seconds of typing.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use deltos_data::DriverKind;
use deltos_settings::Settings;
use gpui::{App, Entity, Global};

use crate::state::{AppState, ConnectState, DEMO_ID, StateEvent};
use crate::ui::workspace::Workspace;

/// How often query text is checked and saved.
const SAVE_EVERY: Duration = Duration::from_secs(5);

/// Set once quitting starts: closing every connection then is not the person closing
/// them, so the list of open connections is left as it was.
struct Quitting;

impl Global for Quitting {}

/// Marks the app as quitting, before its connections close.
pub fn begin_quit(cx: &mut App) {
    cx.set_global(Quitting);
}

/// The connections of the last session that a start opens again: those still saved
/// that need nothing typed (a kept password, or a file database), and the demo.
pub fn reconnectable(settings: &Settings) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in &settings.session.connections {
        if out.contains(id) {
            continue;
        }
        let ok = id == DEMO_ID
            || settings
                .connection(id)
                .is_some_and(|c| c.save_password || c.kind == DriverKind::Sqlite);
        if ok {
            out.push(id.clone());
        }
    }
    out
}

/// The connections to remember as open: those open, then those still connecting or that
/// failed to (so a server that was down is tried again next time), each once. Only
/// those `saved` says are kept in the settings (and the demo) count: a connection opened
/// without being saved is gone after a restart and is never opened again by itself.
pub fn open_ids<'a>(
    open: impl IntoIterator<Item = &'a str>,
    connecting: &HashMap<String, ConnectState>,
    saved: impl Fn(&str) -> bool,
) -> Vec<String> {
    let keep = |id: &str| id == DEMO_ID || saved(id);
    let mut out: Vec<String> = Vec::new();
    for id in open {
        if keep(id) && !out.iter().any(|o| o == id) {
            out.push(id.to_owned());
        }
    }
    let mut pending: Vec<&String> = connecting
        .keys()
        .filter(|id| keep(id) && !out.contains(id))
        .collect();
    pending.sort();
    out.extend(pending.into_iter().cloned());
    out
}

/// Opens the last session's connections when the settings say so, then follows which
/// are open from here on. Does nothing in the demo. `linked` is a connection Deltos was
/// started with a link to, already opening: it stays the active one.
pub fn init(
    state: &Entity<AppState>,
    workspace: &Entity<Workspace>,
    linked: Option<String>,
    cx: &mut App,
) {
    if state.read(cx).demo {
        return;
    }
    let settings = &state.read(cx).settings;
    if settings.session.restore {
        let mut ids = reconnectable(settings);
        let last = settings.session.active.clone().filter(|a| ids.contains(a));
        if let Some(l) = &linked {
            ids.retain(|id| id != l);
        }
        let active = linked.or(last);
        for id in &ids {
            state.update(cx, |s, cx| s.connect(id, cx));
        }
        if let Some(active) = active {
            keep_active(state, ids, active, cx);
        }
    }
    cx.subscribe(state, |state, event, cx| {
        if !matches!(event, StateEvent::Sessions | StateEvent::ActiveSession)
            || cx.has_global::<Quitting>()
        {
            return;
        }
        let s = state.read(cx);
        let open = open_ids(
            s.sessions.iter().map(|x| x.id.as_str()),
            &s.connecting,
            |id| s.settings.connection(id).is_some(),
        );
        let active = s.active.clone().filter(|a| open.contains(a));
        let last = &s.settings.session;
        if last.connections != open || last.active != active {
            state.update(cx, |s, cx| {
                s.update_settings_quietly(cx, |st| {
                    st.session.connections = open;
                    st.session.active = active;
                })
            });
        }
    })
    .detach();
    let workspace = workspace.downgrade();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(SAVE_EVERY).await;
            let alive = workspace.update(cx, |w, cx| {
                if !cx.has_global::<Quitting>() && w.sync_query_text(cx) {
                    w.save_layout(cx);
                }
            });
            if alive.is_err() {
                break;
            }
        }
    })
    .detach();
}

/// Each restored connection becomes active as it opens; once `active` is open it is
/// made active again whenever another restored one opens after it, until none is left
/// connecting.
fn keep_active(state: &Entity<AppState>, ids: Vec<String>, active: String, cx: &mut App) {
    let done = Rc::new(Cell::new(false));
    cx.subscribe(state, move |state, event, cx| {
        if done.get() || !matches!(event, StateEvent::Sessions) {
            return;
        }
        let s = state.read(cx);
        if s.session(&active).is_some() && s.active.as_deref() != Some(active.as_str()) {
            state.update(cx, |s, cx| s.set_active(&active, cx));
        }
        let s = state.read(cx);
        let pending = ids
            .iter()
            .any(|id| s.connecting.get(id) == Some(&ConnectState::Connecting));
        if !pending {
            done.set(true);
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use deltos_settings::SavedConnection;

    fn saved(id: &str, kind: DriverKind, save_password: bool) -> SavedConnection {
        SavedConnection {
            id: id.into(),
            name: id.into(),
            kind,
            save_password,
            ..Default::default()
        }
    }

    #[test]
    fn only_connections_that_need_nothing_typed_reopen() {
        let mut s = Settings {
            connections: vec![
                saved("kept", DriverKind::Postgres, true),
                saved("asks", DriverKind::Postgres, false),
                saved("file", DriverKind::Sqlite, false),
            ],
            ..Default::default()
        };
        s.session.connections = ["asks", "kept", "gone", "file", "kept", DEMO_ID]
            .map(String::from)
            .to_vec();
        assert_eq!(reconnectable(&s), ["kept", "file", DEMO_ID]);
    }

    #[test]
    fn remembers_open_and_pending_connections_once() {
        let mut connecting = HashMap::new();
        connecting.insert("b".to_owned(), ConnectState::Connecting);
        connecting.insert("a".to_owned(), ConnectState::Failed("down".into()));
        connecting.insert("x".to_owned(), ConnectState::Notice("tunnel".into()));
        let all = |_: &str| true;
        assert_eq!(
            open_ids(["x", "y", "x"], &connecting, all),
            ["x", "y", "a", "b"]
        );
        assert!(open_ids([], &HashMap::new(), all).is_empty());
        // Connections that are not saved are not remembered; the demo is.
        let saved = |id: &str| id != "y" && id != "b";
        assert_eq!(
            open_ids(["x", "y", DEMO_ID], &connecting, saved),
            ["x", DEMO_ID, "a"]
        );
    }
}
