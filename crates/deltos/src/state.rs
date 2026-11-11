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
    /// The SSH tunnel the connection runs through, if it has one.
    pub tunnel: Option<Arc<deltos_ssh::Tunnel>>,
    /// The config the driver connected with (pointing at the tunnel when there is one),
    /// to open the connection again after the tunnel reconnects.
    pub config: ConnectionConfig,
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
    /// Open, with something to say, such as an SSH tunnel reconnecting.
    Notice(String),
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
        let ssh_secret = conn.config.ssh.secret.clone();
        let id = conn.id.clone();
        let keep = conn.save_password;
        self.update_settings(cx, |s| {
            match s.connections.iter_mut().find(|c| c.id == conn.id) {
                Some(c) => *c = conn,
                None => s.connections.push(conn),
            }
        });
        let job = cx.global::<Net>().run_blocking(move || {
            use deltos_settings::secrets;
            let db = match (keep, password) {
                (true, Some(p)) if !p.is_empty() => secrets::set_password(&id, &p),
                (false, _) => secrets::delete_password(&id),
                _ => Ok(()),
            };
            let ssh = match (keep, ssh_secret) {
                (true, Some(p)) if !p.is_empty() => secrets::set_ssh_secret(&id, &p),
                (false, _) => secrets::delete_ssh_secret(&id),
                _ => Ok(()),
            };
            db.and(ssh)
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
        let job = cx.global::<Net>().run_blocking(move || {
            deltos_settings::secrets::delete_password(&id)
                .and(deltos_settings::secrets::delete_ssh_secret(&id))
        });
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
        let filled = fill_secrets(saved.clone(), cx);
        cx.spawn(async move |this, cx| {
            let config = filled.await;
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
                Ok(opened) => {
                    this.connecting.remove(&id);
                    if let Some(tunnel) = &opened.tunnel {
                        this.watch_tunnel(&id, tunnel.clone(), cx);
                    }
                    this.add_session(
                        Session {
                            id: id.clone(),
                            name: saved.name.clone(),
                            kind: saved.kind,
                            tag: saved.tag,
                            version: opened.conn.server_version(),
                            conn: opened.conn,
                            tunnel: opened.tunnel,
                            config: opened.config,
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
                tunnel: None,
                config: ConnectionConfig::default(),
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
        self.connecting.remove(id);
        let (conn, tunnel) = (session.conn.clone(), session.tunnel.clone());
        drop(session);
        cx.global::<Net>().spawn(async move {
            conn.close().await;
            drop(conn);
            // The tunnel goes once the connection through it is closed.
            if let Some(tunnel) = tunnel.and_then(Arc::into_inner) {
                tunnel.close().await;
            }
        });
        if self.active.as_deref() == Some(id) {
            self.active = self.sessions.first().map(|s| s.id.clone());
            cx.emit(StateEvent::ActiveSession);
        }
        cx.emit(StateEvent::Sessions);
        cx.notify();
    }

    /// Closes every connection and its tunnel, for quitting.
    pub fn close_all(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<String> = self.sessions.iter().map(|s| s.id.clone()).collect();
        for id in ids {
            self.disconnect(&id, cx);
        }
    }

    /// Follows a connection's SSH tunnel: says when it drops, opens the database
    /// connection again once the tunnel is back, and gives up with the reason when it
    /// cannot come back.
    fn watch_tunnel(&mut self, id: &str, tunnel: Arc<deltos_ssh::Tunnel>, cx: &mut Context<Self>) {
        let mut status = tunnel.status();
        let id = id.to_owned();
        let weak = Arc::downgrade(&tunnel);
        drop(tunnel);
        cx.spawn(async move |this, cx| {
            // tokio's watch channel needs no runtime to be awaited.
            while status.changed().await.is_ok() {
                let now = status.borrow_and_update().clone();
                let Some(tunnel) = weak.upgrade() else {
                    break;
                };
                let alive = this
                    .update(cx, |this, cx| {
                        let ours = this
                            .session(&id)
                            .and_then(|s| s.tunnel.as_ref())
                            .is_some_and(|t| Arc::ptr_eq(t, &tunnel));
                        if ours {
                            this.tunnel_changed(&id, now, cx);
                        }
                        ours
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    fn tunnel_changed(
        &mut self,
        id: &str,
        status: deltos_ssh::TunnelStatus,
        cx: &mut Context<Self>,
    ) {
        use deltos_ssh::TunnelStatus;
        match status {
            TunnelStatus::Up => {
                let Some((kind, config)) = self.session(id).map(|s| (s.kind, s.config.clone()))
                else {
                    return;
                };
                // The database connection ran through the old session and is gone with
                // it; open it again through the same local port.
                self.connecting.insert(
                    id.to_owned(),
                    ConnectState::Notice("SSH is back, reconnecting the database…".into()),
                );
                let attempt = connect_driver(kind, config, cx);
                let id = id.to_owned();
                cx.spawn(async move |this, cx| {
                    let result = attempt.await;
                    this.update(cx, |this, cx| {
                        match result {
                            Ok(conn) => {
                                this.connecting.remove(&id);
                                if let Some(s) = this.session_mut(&id) {
                                    let old = std::mem::replace(&mut s.conn, conn);
                                    s.version = s.conn.server_version();
                                    cx.global::<Net>().spawn(async move { old.close().await });
                                }
                            }
                            Err(e) => {
                                this.connecting.insert(
                                    id.clone(),
                                    ConnectState::Notice(format!("Tunnel OK, database: {e}")),
                                );
                            }
                        }
                        cx.emit(StateEvent::Sessions);
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
            }
            TunnelStatus::Reconnecting { attempt, error } => {
                self.connecting.insert(
                    id.to_owned(),
                    ConnectState::Notice(format!(
                        "SSH: {}; reconnecting (try {attempt})…",
                        error.trim_start_matches("SSH: ")
                    )),
                );
            }
            TunnelStatus::Failed(error) => {
                self.disconnect(id, cx);
                self.connecting.insert(
                    id.to_owned(),
                    ConnectState::Failed(format!("{error}. Double-click to connect again.")),
                );
            }
            TunnelStatus::Closed => {}
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

/// How long a connection attempt may take before it is given up.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// A connection that opened, with the tunnel it runs through.
pub struct Opened {
    pub conn: Arc<dyn Connection>,
    pub tunnel: Option<Arc<deltos_ssh::Tunnel>>,
    /// What the driver connected with: the local end of the tunnel when there is one.
    pub config: ConnectionConfig,
}

/// Opens the SSH tunnel when the config has one, then the connection through it, on the
/// runtime. Errors name the step that failed: `SSH: ...` for the tunnel, and
/// `Tunnel OK, database: ...` for the database behind a working tunnel.
pub fn open_connection(
    kind: DriverKind,
    config: ConnectionConfig,
    cx: &App,
) -> impl std::future::Future<Output = Result<Opened, String>> + use<> {
    let ask = cx.global::<HostKeys>().asker();
    let job = cx.global::<Net>().run(async move {
        let tunneled = config.ssh.enabled && kind != DriverKind::Sqlite;
        if !tunneled {
            let conn = connect_on_runtime(kind, &config).await?;
            return Ok(Opened {
                conn,
                tunnel: None,
                config,
            });
        }
        let reason = |e: deltos_data::Error| match e {
            deltos_data::Error::Connect(m) => m,
            other => other.to_string(),
        };
        let (target_host, target_port) = deltos_drivers::tunnel_target(kind, &config)
            .map_err(|e| format!("SSH: {}", reason(e)))?;
        let request = deltos_ssh::TunnelRequest {
            ssh: config.ssh.clone(),
            target_host,
            target_port,
            host_keys: deltos_ssh::HostKeyFiles::default(),
        };
        let tunnel = deltos_ssh::Tunnel::open(request, ask)
            .await
            .map_err(|e| e.to_string())?;
        let through = deltos_drivers::through_tunnel(kind, &config, tunnel.local_port())
            .map_err(|e| format!("SSH: {}", reason(e)))?;
        let conn = connect_on_runtime(kind, &through)
            .await
            .map_err(|e| format!("Tunnel OK, database: {e}"))?;
        Ok(Opened {
            conn,
            tunnel: Some(Arc::new(tunnel)),
            config: through,
        })
    });
    async move {
        job.await
            .unwrap_or_else(|| Err("the connection attempt stopped".into()))
    }
}

/// Opens a driver connection with `config` as it is, with no tunnel.
fn connect_driver(
    kind: DriverKind,
    config: ConnectionConfig,
    cx: &App,
) -> impl std::future::Future<Output = Result<Arc<dyn Connection>, String>> + use<> {
    let job = cx
        .global::<Net>()
        .run(async move { connect_on_runtime(kind, &config).await });
    async move {
        job.await
            .unwrap_or_else(|| Err("the connection attempt stopped".into()))
    }
}

async fn connect_on_runtime(
    kind: DriverKind,
    config: &ConnectionConfig,
) -> Result<Arc<dyn Connection>, String> {
    let Some(driver) = deltos_drivers::driver(kind) else {
        return Err(format!(
            "{} is not supported in this build yet.",
            kind.label()
        ));
    };
    // Not every driver bounds its own connect: a firewall that drops packets would
    // otherwise keep the attempt open for minutes.
    match tokio::time::timeout(CONNECT_TIMEOUT, driver.connect(config)).await {
        Ok(result) => result.map_err(|e| e.to_string()),
        Err(_) => Err(format!(
            "the server did not answer within {} seconds",
            CONNECT_TIMEOUT.as_secs()
        )),
    }
}

/// Fills in the passwords the form left empty from the keychain, when the person keeps
/// them there.
pub fn fill_secrets(
    saved: SavedConnection,
    cx: &App,
) -> impl std::future::Future<Output = ConnectionConfig> + use<> {
    let net = cx.global::<Net>();
    let key = saved.id.clone();
    let keep = saved.save_password;
    let need_ssh = saved.config.ssh.enabled
        && saved.config.ssh.auth != deltos_data::SshAuth::Agent
        && saved.config.ssh.secret.is_none();
    let need_db = saved.config.password.is_none();
    let read = net.run_blocking(move || {
        use deltos_settings::secrets;
        if !keep {
            return (Ok(None), Ok(None));
        }
        let db = if need_db {
            secrets::password(&key)
        } else {
            Ok(None)
        };
        let ssh = if need_ssh {
            secrets::ssh_secret(&key)
        } else {
            Ok(None)
        };
        (db, ssh)
    });
    async move {
        let mut config = saved.config;
        let (db, ssh) = read.await.unwrap_or((Ok(None), Ok(None)));
        for e in [db.as_ref().err(), ssh.as_ref().err()]
            .into_iter()
            .flatten()
        {
            tracing::warn!("could not read the keychain: {e}");
        }
        if config.password.is_none() {
            config.password = db.ok().flatten();
        }
        if config.ssh.secret.is_none() {
            config.ssh.secret = ssh.ok().flatten();
        }
        config
    }
}

/// The question a new SSH host key raises, with where to send the answer.
type HostKeyQuestion = (
    deltos_ssh::UnknownHost,
    futures::channel::oneshot::Sender<bool>,
);

/// Carries trust-on-first-use questions from the SSH tunnels on the runtime to a prompt
/// in a window.
pub struct HostKeys(async_channel::Sender<HostKeyQuestion>);

impl gpui::Global for HostKeys {}

impl HostKeys {
    fn asker(&self) -> deltos_ssh::AskHostKey {
        let tx = self.0.clone();
        Arc::new(move |question| {
            let tx = tx.clone();
            Box::pin(async move {
                let (answer, reply) = futures::channel::oneshot::channel();
                if tx.send((question, answer)).await.is_err() {
                    return false;
                }
                reply.await.unwrap_or(false)
            })
        })
    }
}

/// Answers host key questions one at a time with a prompt in the active window.
fn serve_host_keys(cx: &mut App) {
    let (tx, rx) = async_channel::unbounded::<HostKeyQuestion>();
    cx.set_global(HostKeys(tx));
    cx.spawn(async move |cx| {
        while let Ok((q, answer)) = rx.recv().await {
            let message = format!("Trust the SSH host {}?", q.host);
            let port = if q.port == 22 {
                String::new()
            } else {
                format!(" on port {}", q.port)
            };
            let detail = format!(
                "Deltos has not connected to {}{port} before. Its {} key fingerprint is\n\n{}\n\nCompare it with the one the server's administrator gives you (ssh-keygen -lf /etc/ssh/ssh_host_*_key.pub on the server). Trusting adds it to ~/.ssh/known_hosts.",
                q.host, q.algorithm, q.fingerprint
            );
            let prompt = cx
                .update(|cx| {
                    let window = cx
                        .active_window()
                        .or_else(|| cx.windows().into_iter().next())?;
                    window
                        .update(cx, |_, window, cx| {
                            window.prompt(
                                gpui::PromptLevel::Warning,
                                &message,
                                Some(&detail),
                                &["Trust and connect", "Cancel"],
                                cx,
                            )
                        })
                        .ok()
                })
                .ok()
                .flatten();
            let trusted = match prompt {
                Some(choice) => choice.await.ok() == Some(0),
                None => false,
            };
            let _ = answer.send(trusted);
        }
    })
    .detach();
}

pub fn init(
    settings: Settings,
    paths: Option<Paths>,
    demo: bool,
    cx: &mut App,
) -> Entity<AppState> {
    let themes = paths.as_ref().map(Paths::scan_themes).unwrap_or_default();
    serve_host_keys(cx);
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
