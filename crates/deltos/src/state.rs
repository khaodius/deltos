//! The app's shared state: settings, open connections and what the panels share (the
//! row shown in the inspector, the statement history). Views read it and ask it to
//! change; it emits a [`StateEvent`] for each kind of change so views redraw only for
//! what concerns them.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use deltos_data::{
    Column, Connection, ConnectionConfig, DriverKind, Namespace, ObjectEntry, Value,
};
use deltos_settings::{ConnectionTag, PanelKind, Paths, SavedConnection, Settings, ThemeFile};
use gpui::{App, AppContext, Context, Entity, EventEmitter, Task};

use crate::net::Net;
use crate::theme::{Theme, system_is_dark};

/// The id of the built-in demo connection.
pub const DEMO_ID: &str = "demo";

#[derive(Debug, Clone)]
pub enum StateEvent {
    Settings,
    /// A connection opened, closed, or its object tree changed.
    Sessions,
    /// The active connection changed.
    ActiveSession,
    /// The inspected row changed.
    Inspect,
    History,
    /// The updater has something new to show.
    Update,
    /// A view asks the workspace to open or show a panel.
    Open {
        kind: PanelKind,
        pin: bool,
    },
}

/// One namespace in a connection's object tree, with its objects once they are listed.
#[derive(Debug, Clone)]
pub struct NamespaceNode {
    pub namespace: Namespace,
    pub objects: Option<Vec<ObjectEntry>>,
    pub loading: bool,
    pub error: Option<String>,
    pub expanded: bool,
}

/// An open connection.
pub struct Session {
    /// The saved connection's id, or [`DEMO_ID`].
    pub id: String,
    pub name: String,
    pub kind: DriverKind,
    pub tag: ConnectionTag,
    pub conn: Arc<dyn Connection>,
    pub version: String,
    pub namespaces: Vec<NamespaceNode>,
    pub loading: bool,
    pub error: Option<String>,
}

/// The row the inspector shows, from whichever grid selected it last.
#[derive(Debug, Clone)]
pub struct Inspected {
    pub title: String,
    pub row: u64,
    pub columns: Arc<[Column]>,
    pub values: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub at: SystemTime,
    pub connection: String,
    pub statement: String,
    pub duration: Duration,
    pub rows: Option<u64>,
    pub error: Option<String>,
}

const HISTORY_LIMIT: usize = 500;

/// Where a connection attempt stands, for the connection list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectState {
    Connecting,
    Failed(String),
}

pub struct AppState {
    pub settings: Settings,
    pub paths: Option<Paths>,
    pub sessions: Vec<Session>,
    pub active: Option<String>,
    pub connecting: HashMap<String, ConnectState>,
    pub inspected: Option<Inspected>,
    pub history: VecDeque<HistoryEntry>,
    /// Theme files in the themes folder, by file stem.
    pub themes: Vec<(String, ThemeFile)>,
    pub demo: bool,
    save_task: Option<Task<()>>,
}

impl EventEmitter<StateEvent> for AppState {}

impl AppState {
    pub fn session(&self, id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    fn session_mut(&mut self, id: &str) -> Option<&mut Session> {
        self.sessions.iter_mut().find(|s| s.id == id)
    }

    pub fn active_session(&self) -> Option<&Session> {
        self.active.as_deref().and_then(|id| self.session(id))
    }

    /// Asks the workspace to open `kind`, or show it if it is open.
    pub fn open(&mut self, kind: PanelKind, pin: bool, cx: &mut Context<Self>) {
        cx.emit(StateEvent::Open { kind, pin });
    }

    pub fn set_active(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.active.as_deref() != Some(id) && self.session(id).is_some() {
            self.active = Some(id.to_owned());
            cx.emit(StateEvent::ActiveSession);
            cx.notify();
        }
    }

    // ---- settings ---------------------------------------------------------------------

    pub fn update_settings(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Settings)) {
        f(&mut self.settings);
        self.settings.sanitize();
        self.rebuild_theme(cx);
        cx.emit(StateEvent::Settings);
        cx.notify();
        cx.refresh_windows();
        self.save_settings(cx);
    }

    /// Changes settings that no view draws from, such as the saved layout, without a
    /// redraw of everything.
    pub fn update_settings_quietly(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Settings),
    ) {
        f(&mut self.settings);
        self.save_settings(cx);
    }

    pub fn rebuild_theme(&mut self, cx: &mut App) {
        let a = &self.settings.appearance;
        let custom = a
            .custom_theme
            .as_ref()
            .and_then(|name| self.themes.iter().find(|(n, _)| n == name))
            .map(|(_, t)| t);
        let mut theme = Theme::new(a, &self.settings.editor, system_is_dark(cx), custom);
        theme.settle_fonts(a.grid_font.is_some(), cx);
        cx.set_global(theme);
    }

    pub fn rescan_themes(&mut self, cx: &mut Context<Self>) {
        self.themes = self
            .paths
            .as_ref()
            .map(Paths::scan_themes)
            .unwrap_or_default();
        self.rebuild_theme(cx);
        cx.emit(StateEvent::Settings);
        cx.refresh_windows();
    }

    /// Saves after a short pause, so a slider drag writes the file once.
    pub fn save_settings(&mut self, cx: &mut Context<Self>) {
        if self.demo {
            return;
        }
        let Some(path) = self.paths.as_ref().map(Paths::settings_file) else {
            return;
        };
        let settings = self.settings.clone();
        self.save_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(400))
                .await;
            let result = cx
                .background_executor()
                .spawn(async move { settings.save(&path) })
                .await;
            if let Err(e) = result {
                tracing::error!("could not save settings: {e}");
            }
        }));
    }

    /// Writes the settings now, for quitting.
    pub fn save_now(&mut self) {
        self.save_task = None;
        if self.demo {
            return;
        }
        if let Some(path) = self.paths.as_ref().map(Paths::settings_file)
            && let Err(e) = self.settings.save(&path)
        {
            tracing::error!("could not save settings: {e}");
        }
    }

    // ---- connections ------------------------------------------------------------------

    /// Saves a new or changed connection. The password goes to the keychain when the
    /// person chose to keep it.
    pub fn save_connection(&mut self, conn: SavedConnection, cx: &mut Context<Self>) {
        let password = conn.config.password.clone();
        let id = conn.id.clone();
        let keep = conn.save_password;
        self.update_settings(cx, |s| {
            match s.connections.iter_mut().find(|c| c.id == conn.id) {
                Some(c) => *c = conn,
                None => s.connections.push(conn),
            }
        });
        let job = cx
            .global::<Net>()
            .run_blocking(move || match (keep, password) {
                (true, Some(p)) if !p.is_empty() => deltos_settings::secrets::set_password(&id, &p),
                (false, _) => deltos_settings::secrets::delete_password(&id),
                _ => Ok(()),
            });
        cx.spawn(async move |_, _| {
            if let Some(Err(e)) = job.await {
                tracing::warn!("could not keep the password in the keychain: {e}");
            }
        })
        .detach();
    }

    pub fn delete_connection(&mut self, id: &str, cx: &mut Context<Self>) {
        let id = id.to_owned();
        self.update_settings(cx, |s| s.connections.retain(|c| c.id != id));
        let job = cx
            .global::<Net>()
            .run_blocking(move || deltos_settings::secrets::delete_password(&id));
        cx.spawn(async move |_, _| {
            if let Some(Err(e)) = job.await {
                tracing::warn!("could not remove the password from the keychain: {e}");
            }
        })
        .detach();
    }

    /// Opens saved connection `id`, reading its password from the keychain first.
    pub fn connect(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.session(id).is_some() {
            self.set_active(id, cx);
            return;
        }
        if id == DEMO_ID {
            self.open_demo(cx);
            return;
        }
        let Some(saved) = self.settings.connection(id).cloned() else {
            return;
        };
        self.connecting
            .insert(saved.id.clone(), ConnectState::Connecting);
        cx.emit(StateEvent::Sessions);
        cx.notify();
        let net = cx.global::<Net>();
        let key = saved.id.clone();
        let keep = saved.save_password;
        let password = net.run_blocking(move || {
            if keep {
                deltos_settings::secrets::password(&key)
            } else {
                Ok(None)
            }
        });
        cx.spawn(async move |this, cx| {
            let password = match password.await {
                Some(Ok(p)) => p,
                Some(Err(e)) => {
                    tracing::warn!("could not read the keychain: {e}");
                    None
                }
                None => None,
            };
            let mut config = saved.config.clone();
            if config.password.is_none() {
                config.password = password;
            }
            this.update(cx, |this, cx| this.connect_with(saved, config, cx))
                .ok();
        })
        .detach();
    }

    /// Opens a connection with a config that already has its password.
    pub fn connect_with(
        &mut self,
        saved: SavedConnection,
        config: ConnectionConfig,
        cx: &mut Context<Self>,
    ) {
        let id = saved.id.clone();
        self.connecting.insert(id.clone(), ConnectState::Connecting);
        cx.emit(StateEvent::Sessions);
        let attempt = open_connection(saved.kind, config, cx);
        cx.spawn(async move |this, cx| {
            let result = attempt.await;
            this.update(cx, |this, cx| match result {
                Ok(conn) => {
                    this.connecting.remove(&id);
                    this.add_session(
                        Session {
                            id: id.clone(),
                            name: saved.name.clone(),
                            kind: saved.kind,
                            tag: saved.tag,
                            version: conn.server_version(),
                            conn,
                            namespaces: Vec::new(),
                            loading: false,
                            error: None,
                        },
                        cx,
                    );
                }
                Err(e) => {
                    this.connecting.insert(id.clone(), ConnectState::Failed(e));
                    cx.emit(StateEvent::Sessions);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub fn open_demo(&mut self, cx: &mut Context<Self>) {
        if self.session(DEMO_ID).is_some() {
            self.set_active(DEMO_ID, cx);
            return;
        }
        let conn: Arc<dyn Connection> = Arc::new(crate::demo::DemoConnection::new());
        self.add_session(
            Session {
                id: DEMO_ID.into(),
                name: "Demo".into(),
                kind: conn.kind(),
                tag: ConnectionTag::Local,
                version: conn.server_version(),
                conn,
                namespaces: Vec::new(),
                loading: false,
                error: None,
            },
            cx,
        );
    }

    fn add_session(&mut self, session: Session, cx: &mut Context<Self>) {
        let id = session.id.clone();
        self.sessions.retain(|s| s.id != id);
        self.sessions.push(session);
        self.active = Some(id.clone());
        self.load_namespaces(&id, cx);
        cx.emit(StateEvent::Sessions);
        cx.emit(StateEvent::ActiveSession);
        cx.notify();
    }

    pub fn disconnect(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(ix) = self.sessions.iter().position(|s| s.id == id) else {
            return;
        };
        let session = self.sessions.remove(ix);
        let conn = session.conn.clone();
        cx.global::<Net>().spawn(async move { conn.close().await });
        if self.active.as_deref() == Some(id) {
            self.active = self.sessions.first().map(|s| s.id.clone());
            cx.emit(StateEvent::ActiveSession);
        }
        cx.emit(StateEvent::Sessions);
        cx.notify();
    }

    /// Lists the namespaces of a connection, then the objects of each one that starts
    /// expanded (all of them, when there are few).
    pub fn load_namespaces(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(session) = self.session_mut(id) else {
            return;
        };
        session.loading = true;
        session.error = None;
        let conn = session.conn.clone();
        let id = id.to_owned();
        let job = cx
            .global::<Net>()
            .run(async move { conn.namespaces().await.map_err(|e| e.to_string()) });
        cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|| Err("cancelled".into()));
            this.update(cx, |this, cx| {
                let Some(session) = this.session_mut(&id) else {
                    return;
                };
                session.loading = false;
                match result {
                    Ok(list) => {
                        let expand = list.len() <= 4;
                        session.namespaces = list
                            .into_iter()
                            .map(|namespace| NamespaceNode {
                                namespace,
                                objects: None,
                                loading: false,
                                error: None,
                                expanded: expand,
                            })
                            .collect();
                        let paths: Vec<Vec<String>> = session
                            .namespaces
                            .iter()
                            .filter(|n| n.expanded)
                            .map(|n| n.namespace.path.clone())
                            .collect();
                        for path in paths {
                            this.load_objects(&id, &path, cx);
                        }
                    }
                    Err(e) => session.error = Some(e),
                }
                cx.emit(StateEvent::Sessions);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn load_objects(&mut self, id: &str, path: &[String], cx: &mut Context<Self>) {
        let Some(session) = self.session_mut(id) else {
            return;
        };
        let Some(node) = session
            .namespaces
            .iter_mut()
            .find(|n| n.namespace.path == path)
        else {
            return;
        };
        node.loading = true;
        node.error = None;
        let conn = session.conn.clone();
        let (id, path) = (id.to_owned(), path.to_vec());
        let ns = path.clone();
        let job = cx
            .global::<Net>()
            .run(async move { conn.objects(&ns).await.map_err(|e| e.to_string()) });
        cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|| Err("cancelled".into()));
            this.update(cx, |this, cx| {
                if let Some(node) = this
                    .session_mut(&id)
                    .and_then(|s| s.namespaces.iter_mut().find(|n| n.namespace.path == path))
                {
                    node.loading = false;
                    match result {
                        Ok(objects) => node.objects = Some(objects),
                        Err(e) => node.error = Some(e),
                    }
                }
                cx.emit(StateEvent::Sessions);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn toggle_namespace(&mut self, id: &str, path: &[String], cx: &mut Context<Self>) {
        let Some(node) = self
            .session_mut(id)
            .and_then(|s| s.namespaces.iter_mut().find(|n| n.namespace.path == path))
        else {
            return;
        };
        node.expanded = !node.expanded;
        let load = node.expanded && node.objects.is_none() && !node.loading;
        if load {
            self.load_objects(id, path, cx);
        }
        cx.emit(StateEvent::Sessions);
        cx.notify();
    }

    // ---- shared panel state -----------------------------------------------------------

    pub fn inspect(&mut self, inspected: Option<Inspected>, cx: &mut Context<Self>) {
        self.inspected = inspected;
        cx.emit(StateEvent::Inspect);
    }

    pub fn record(&mut self, entry: HistoryEntry, cx: &mut Context<Self>) {
        if self.history.len() >= HISTORY_LIMIT {
            self.history.pop_front();
        }
        self.history.push_back(entry);
        cx.emit(StateEvent::History);
    }
}

/// Connects on the runtime and returns the connection or an error message.
/// How long a connection attempt may take before it is given up.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

pub fn open_connection(
    kind: DriverKind,
    config: ConnectionConfig,
    cx: &App,
) -> impl std::future::Future<Output = Result<Arc<dyn Connection>, String>> + use<> {
    let job = cx.global::<Net>().run(async move {
        let Some(driver) = deltos_drivers::driver(kind) else {
            return Err(format!(
                "{} is not supported in this build yet.",
                kind.label()
            ));
        };
        // Not every driver bounds its own connect: a firewall that drops packets would
        // otherwise keep the attempt open for minutes.
        match tokio::time::timeout(CONNECT_TIMEOUT, driver.connect(&config)).await {
            Ok(result) => result.map_err(|e| e.to_string()),
            Err(_) => Err(format!(
                "the server did not answer within {} seconds",
                CONNECT_TIMEOUT.as_secs()
            )),
        }
    });
    async move {
        job.await
            .unwrap_or_else(|| Err("the connection attempt stopped".into()))
    }
}

pub fn init(
    settings: Settings,
    paths: Option<Paths>,
    demo: bool,
    cx: &mut App,
) -> Entity<AppState> {
    let themes = paths.as_ref().map(Paths::scan_themes).unwrap_or_default();
    cx.new(|cx| {
        let mut state = AppState {
            settings,
            paths,
            sessions: Vec::new(),
            active: None,
            connecting: HashMap::new(),
            inspected: None,
            history: VecDeque::new(),
            themes,
            demo,
            save_task: None,
        };
        state.rebuild_theme(cx);
        if demo {
            state.open_demo(cx);
        }
        state
    })
}
