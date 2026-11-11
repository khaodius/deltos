//! A local port forward over one SSH session, with reconnects.
//!
//! [`Tunnel::open`] signs in to the SSH server, checks that the server can reach the
//! database address, and listens on `127.0.0.1` on a port the system picks. Each
//! connection to that port becomes a `direct-tcpip` channel to the database. A
//! supervisor watches the session: when it drops it connects again with the same key
//! and port, so the driver can open new connections to the same address.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use deltos_data::{SshAuth, SshTunnel};
use futures::future::BoxFuture;
use russh::client::{self, AuthResult, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey};
use russh::{ChannelOpenFailure, Disconnect};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;

use crate::config::{self, HostConfig};
use crate::known_hosts::{self, HostKeyStatus};

/// How long to wait for the SSH server's TCP port, and then for each further step.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const STEP_TIMEOUT: Duration = Duration::from_secs(20);
/// Keepalives every 15 seconds; three missed in a row end the session.
const KEEPALIVE: Duration = Duration::from_secs(15);
/// Reconnect attempts before the tunnel gives up, with waits of 1, 2, 4 ... 30 seconds.
const RECONNECT_ATTEMPTS: u32 = 8;

/// A host key the person has not trusted yet, for the trust-on-first-use question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownHost {
    pub host: String,
    pub port: u16,
    /// For example `ED25519` or `RSA`.
    pub algorithm: String,
    /// `SHA256:...`, as `ssh-keygen -l` prints it.
    pub fingerprint: String,
}

/// Asks the person whether to trust a host key. Resolves to `true` to trust it.
pub type AskHostKey = Arc<dyn Fn(UnknownHost) -> BoxFuture<'static, bool> + Send + Sync>;

/// Which step failed, for messages and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Config,
    Unreachable,
    HostKey,
    Auth,
    Forward,
    Closed,
}

/// Why the tunnel could not be opened. It displays as `SSH: ...`, so a message about
/// the database can never be mistaken for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelError {
    pub kind: ErrorKind,
    pub message: String,
}

impl TunnelError {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for TunnelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SSH: {}", self.message)
    }
}

impl std::error::Error for TunnelError {}

/// Where the tunnel stands, for the connection's status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelStatus {
    Up,
    /// The session dropped and the tunnel is connecting again.
    Reconnecting {
        attempt: u32,
        error: String,
    },
    /// The session dropped and could not be opened again.
    Failed(String),
    /// Closed on purpose.
    Closed,
}

/// What to open: the SSH server and the database address as seen from it.
#[derive(Debug, Clone)]
pub struct TunnelRequest {
    pub ssh: SshTunnel,
    pub target_host: String,
    pub target_port: u16,
    pub host_keys: HostKeyFiles,
}

/// Where host keys are checked, and where a newly trusted one is written.
#[derive(Debug, Clone)]
pub struct HostKeyFiles {
    pub check: Vec<PathBuf>,
    pub learn: Option<PathBuf>,
}

impl Default for HostKeyFiles {
    /// `~/.ssh/known_hosts` and the system-wide file, learning into the first.
    fn default() -> Self {
        Self {
            check: known_hosts::default_files(),
            learn: known_hosts::user_file(),
        }
    }
}

/// The SSH server after `~/.ssh/config` is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// What the person typed, for messages.
    pub alias: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub identity_files: Vec<PathBuf>,
}

/// Applies `~/.ssh/config` to the form's fields. What the form gives wins over the file.
pub fn resolve(ssh: &SshTunnel) -> Result<Resolved, TunnelError> {
    resolve_with(ssh, &config::lookup(ssh.host.trim()))
}

fn resolve_with(ssh: &SshTunnel, file: &HostConfig) -> Result<Resolved, TunnelError> {
    let alias = ssh.host.trim().to_string();
    if alias.is_empty() {
        return Err(TunnelError::new(
            ErrorKind::Config,
            "give the SSH host, or a Host alias from ~/.ssh/config",
        ));
    }
    let host = file
        .hostname
        .as_deref()
        .map(|h| h.replace("%h", &alias))
        .unwrap_or_else(|| alias.clone());
    let port = ssh.port.or(file.port).unwrap_or(22);
    let user = match ssh.user.trim() {
        "" => file.user.clone().unwrap_or_else(config::local_user),
        u => u.to_string(),
    };
    if user.is_empty() {
        return Err(TunnelError::new(ErrorKind::Config, "give the SSH user"));
    }
    let identity_files = file
        .identity_files
        .iter()
        .map(|f| config::expand_identity(f, &alias, &host, &user))
        .collect();
    Ok(Resolved {
        alias,
        host,
        port,
        user,
        identity_files,
    })
}

/// An open tunnel. Dropping it closes the SSH session and stops listening.
pub struct Tunnel {
    local_port: u16,
    resolved: Resolved,
    status: watch::Receiver<TunnelStatus>,
    shared: Arc<Shared>,
    tasks: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for Tunnel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tunnel")
            .field("local_port", &self.local_port)
            .field("server", &self.resolved.host)
            .finish_non_exhaustive()
    }
}

struct Shared {
    request: TunnelRequest,
    resolved: Resolved,
    ask: AskHostKey,
    /// The key accepted on the first connect. Reconnects must see the same one.
    pinned: std::sync::Mutex<Option<PublicKey>>,
    session: Mutex<Option<Arc<Handle<Client>>>>,
    status: watch::Sender<TunnelStatus>,
}

impl Tunnel {
    /// Signs in, checks the database address is reachable from the server, and starts
    /// listening. Must run on a Tokio runtime.
    pub async fn open(request: TunnelRequest, ask: AskHostKey) -> Result<Self, TunnelError> {
        let resolved = resolve(&request.ssh)?;
        if resolved.port == 0 {
            return Err(TunnelError::new(
                ErrorKind::Config,
                "the SSH port cannot be 0",
            ));
        }
        let (status_tx, status_rx) = watch::channel(TunnelStatus::Up);
        let shared = Arc::new(Shared {
            request,
            resolved: resolved.clone(),
            ask,
            pinned: std::sync::Mutex::new(None),
            session: Mutex::new(None),
            status: status_tx,
        });
        let handle = Arc::new(connect(&shared).await?);
        probe(&shared, &handle).await?;
        // Port 0: the system picks a free port, so a local database is never in the way.
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| {
            TunnelError::new(
                ErrorKind::Forward,
                format!("could not listen on a local port: {e}"),
            )
        })?;
        let local_port = listener
            .local_addr()
            .map_err(|e| TunnelError::new(ErrorKind::Forward, e.to_string()))?
            .port();
        *shared.session.lock().await = Some(handle);
        let tasks = vec![
            tokio::spawn(accept_loop(listener, shared.clone())),
            tokio::spawn(supervise(shared.clone())),
        ];
        tracing::info!(
            "SSH tunnel to {}:{} via {}@{}:{} listens on 127.0.0.1:{local_port}",
            shared.request.target_host,
            shared.request.target_port,
            resolved.user,
            resolved.host,
            resolved.port
        );
        Ok(Self {
            local_port,
            resolved,
            status: status_rx,
            shared,
            tasks,
        })
    }

    /// The port on 127.0.0.1 that leads to the database.
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// The SSH server, after `~/.ssh/config`.
    pub fn server(&self) -> &Resolved {
        &self.resolved
    }

    /// Changes whenever the session drops, reconnects or gives up.
    pub fn status(&self) -> watch::Receiver<TunnelStatus> {
        self.status.clone()
    }

    /// Closes the session and stops listening.
    pub async fn close(mut self) {
        self.shutdown();
        let session = self.shared.session.lock().await.take();
        if let Some(handle) = session {
            let _ = handle
                .disconnect(Disconnect::ByApplication, "closed", "en")
                .await;
        }
    }

    fn shutdown(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
        self.shared.status.send_replace(TunnelStatus::Closed);
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.shutdown();
        // Dropping the last handle ends the session; ask the server politely first when
        // a runtime is still around to do it.
        let shared = self.shared.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                if let Some(handle) = shared.session.lock().await.take() {
                    let _ = handle
                        .disconnect(Disconnect::ByApplication, "closed", "en")
                        .await;
                }
            });
        }
    }
}

/// Accepts local connections and joins each one to a channel to the database.
async fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        let Ok((mut socket, peer)) = listener.accept().await else {
            continue;
        };
        socket.set_nodelay(true).ok();
        let shared = shared.clone();
        tokio::spawn(async move {
            let Some(handle) = current_session(&shared).await else {
                tracing::warn!("SSH tunnel: no session for a new connection");
                return;
            };
            let request = &shared.request;
            let channel = match handle
                .channel_open_direct_tcpip(
                    request.target_host.clone(),
                    u32::from(request.target_port),
                    peer.ip().to_string(),
                    u32::from(peer.port()),
                )
                .await
            {
                Ok(channel) => channel,
                Err(e) => {
                    tracing::warn!("SSH tunnel: could not open a channel: {e}");
                    return;
                }
            };
            let mut stream = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
        });
    }
}

/// The open session, waiting a little for one while the tunnel reconnects.
async fn current_session(shared: &Shared) -> Option<Arc<Handle<Client>>> {
    let mut status = shared.status.subscribe();
    let wait = async {
        loop {
            if let Some(handle) = shared.session.lock().await.clone()
                && !handle.is_closed()
            {
                return Some(handle);
            }
            if matches!(
                *status.borrow_and_update(),
                TunnelStatus::Failed(_) | TunnelStatus::Closed
            ) {
                return None;
            }
            if status.changed().await.is_err() {
                return None;
            }
        }
    };
    tokio::time::timeout(STEP_TIMEOUT, wait)
        .await
        .ok()
        .flatten()
}

/// Watches the session and opens it again when it drops.
async fn supervise(shared: Arc<Shared>) {
    loop {
        // russh closes the handle when keepalives go unanswered or the socket ends.
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let closed = shared
                .session
                .lock()
                .await
                .as_ref()
                .is_none_or(|h| h.is_closed());
            if closed {
                break;
            }
        }
        shared.session.lock().await.take();
        let mut error = "the SSH connection dropped".to_string();
        tracing::warn!("SSH tunnel to {}: {error}", shared.resolved.host);
        let mut reopened = false;
        for attempt in 1..=RECONNECT_ATTEMPTS {
            shared.status.send_replace(TunnelStatus::Reconnecting {
                attempt,
                error: error.clone(),
            });
            let wait = Duration::from_secs((1u64 << (attempt - 1)).min(30));
            tokio::time::sleep(wait).await;
            match connect(&shared).await {
                Ok(handle) => {
                    *shared.session.lock().await = Some(Arc::new(handle));
                    shared.status.send_replace(TunnelStatus::Up);
                    tracing::info!("SSH tunnel to {} is back", shared.resolved.host);
                    reopened = true;
                    break;
                }
                // A host key that changed will not change back; stop at once.
                Err(e) if e.kind == ErrorKind::HostKey => {
                    error = e.to_string();
                    break;
                }
                Err(e) => error = e.to_string(),
            }
        }
        if !reopened {
            shared.status.send_replace(TunnelStatus::Failed(error));
            return;
        }
    }
}

/// Opens and signs in to the SSH session.
async fn connect(shared: &Arc<Shared>) -> Result<Handle<Client>, TunnelError> {
    let r = &shared.resolved;
    let ssh = &shared.request.ssh;
    let addresses = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio::net::lookup_host((r.host.as_str(), r.port)),
    )
    .await
    {
        Ok(Ok(addresses)) => addresses.collect::<Vec<_>>(),
        Ok(Err(e)) => {
            return Err(TunnelError::new(
                ErrorKind::Unreachable,
                format!("host unreachable: could not find {} ({e})", r.host),
            ));
        }
        Err(_) => {
            return Err(TunnelError::new(
                ErrorKind::Unreachable,
                format!("host unreachable: looking up {} took too long", r.host),
            ));
        }
    };
    let socket =
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&addresses[..])).await {
            Ok(Ok(socket)) => socket,
            Ok(Err(e)) => {
                return Err(TunnelError::new(
                    ErrorKind::Unreachable,
                    format!("host unreachable: {}:{} ({e})", r.host, r.port),
                ));
            }
            Err(_) => {
                return Err(TunnelError::new(
                    ErrorKind::Unreachable,
                    format!(
                        "host unreachable: {}:{} did not answer within {} seconds",
                        r.host,
                        r.port,
                        CONNECT_TIMEOUT.as_secs()
                    ),
                ));
            }
        };
    socket.set_nodelay(true).ok();
    let config = Arc::new(client::Config {
        keepalive_interval: Some(KEEPALIVE),
        keepalive_max: 3,
        inactivity_timeout: None,
        nodelay: true,
        ..Default::default()
    });
    let asking = Arc::new(AtomicBool::new(false));
    let rejected = Arc::new(std::sync::Mutex::new(None));
    let handler = Client {
        shared: shared.clone(),
        asking: asking.clone(),
        rejected: rejected.clone(),
    };
    let handshake = client::connect_stream(config, socket, handler);
    let mut handle = match patient(handshake, &asking).await {
        Some(Ok(handle)) => handle,
        Some(Err(e)) => {
            if let Some(reason) = rejected.lock().ok().and_then(|mut r| r.take()) {
                return Err(reason);
            }
            return Err(TunnelError::new(
                ErrorKind::Unreachable,
                format!("could not start an SSH session with {}: {e}", r.host),
            ));
        }
        None => {
            return Err(TunnelError::new(
                ErrorKind::Unreachable,
                format!("{} did not finish the SSH handshake in time", r.host),
            ));
        }
    };
    let step = authenticate(&mut handle, &r.user, ssh, &r.identity_files);
    match tokio::time::timeout(STEP_TIMEOUT, step).await {
        Ok(Ok(())) => Ok(handle),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(TunnelError::new(
            ErrorKind::Auth,
            "auth failed: the server did not answer in time",
        )),
    }
}

/// Waits for `fut`, allowing [`STEP_TIMEOUT`] of silence, but not counting the time the
/// person spends on the host key question.
async fn patient<F: std::future::Future>(fut: F, asking: &AtomicBool) -> Option<F::Output> {
    tokio::pin!(fut);
    loop {
        tokio::select! {
            out = &mut fut => return Some(out),
            _ = tokio::time::sleep(STEP_TIMEOUT) => {
                if !asking.load(Ordering::SeqCst) {
                    return None;
                }
            }
        }
    }
}

/// Opens one channel to the database address and closes it, so a wrong host or port,
/// or a server that forbids forwarding, shows up before the driver tries.
async fn probe(shared: &Shared, handle: &Handle<Client>) -> Result<(), TunnelError> {
    let req = &shared.request;
    let target = format!("{}:{}", req.target_host, req.target_port);
    let open = handle.channel_open_direct_tcpip(
        req.target_host.clone(),
        u32::from(req.target_port),
        "127.0.0.1",
        0,
    );
    match tokio::time::timeout(STEP_TIMEOUT, open).await {
        Ok(Ok(channel)) => {
            let _ = channel.close().await;
            Ok(())
        }
        Ok(Err(russh::Error::ChannelOpenFailure(reason))) => Err(TunnelError::new(
            ErrorKind::Forward,
            match reason {
                ChannelOpenFailure::AdministrativelyProhibited => format!(
                    "the server does not allow port forwarding (AllowTcpForwarding in its sshd_config), so it cannot reach {target}"
                ),
                ChannelOpenFailure::ConnectFailed => format!(
                    "signed in, but the server could not reach {target}. Check Host and Port: they are the database address as seen from the SSH server"
                ),
                other => format!("the server would not open a channel to {target}: {other:?}"),
            },
        )),
        Ok(Err(e)) => Err(TunnelError::new(
            ErrorKind::Forward,
            format!("could not open a channel to {target}: {e}"),
        )),
        Err(_) => Err(TunnelError::new(
            ErrorKind::Forward,
            format!("the server did not answer when asked to reach {target}"),
        )),
    }
}

async fn authenticate(
    handle: &mut Handle<Client>,
    user: &str,
    ssh: &SshTunnel,
    identity_files: &[PathBuf],
) -> Result<(), TunnelError> {
    let failed = |detail: String| {
        TunnelError::new(ErrorKind::Auth, format!("auth failed for {user}: {detail}"))
    };
    let ssh_error = |e: russh::Error| failed(e.to_string());
    match ssh.auth {
        SshAuth::Password => {
            let password = ssh.secret.clone().unwrap_or_default();
            if password.is_empty() {
                return Err(failed("give the SSH password".into()));
            }
            let result = handle
                .authenticate_password(user, password.clone())
                .await
                .map_err(ssh_error)?;
            if result.success() {
                return Ok(());
            }
            // Servers with PasswordAuthentication off often still ask for the password
            // through keyboard-interactive.
            if keyboard_interactive(handle, user, &password).await? {
                return Ok(());
            }
            Err(failed("the server refused the password".into()))
        }
        SshAuth::Key => {
            let files = key_files(ssh, identity_files);
            if files.is_empty() {
                return Err(failed(
                    "no private key found. Choose a key file, or add IdentityFile to ~/.ssh/config"
                        .into(),
                ));
            }
            let passphrase = ssh.secret.as_deref().filter(|p| !p.is_empty());
            let mut problems = Vec::new();
            for path in files {
                let key = match russh::keys::load_secret_key(&path, passphrase) {
                    Ok(key) => key,
                    Err(russh::keys::Error::KeyIsEncrypted) => {
                        problems.push(format!("{} needs its passphrase", path.display()));
                        continue;
                    }
                    Err(e) => {
                        let why = if passphrase.is_some() {
                            format!("wrong passphrase or unsupported key ({e})")
                        } else {
                            e.to_string()
                        };
                        problems.push(format!("could not read {}: {why}", path.display()));
                        continue;
                    }
                };
                let hash = rsa_hash(handle).await;
                let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
                let result = handle
                    .authenticate_publickey(user, key)
                    .await
                    .map_err(ssh_error)?;
                if result.success() {
                    return Ok(());
                }
                problems.push(format!("the server refused {}", path.display()));
            }
            Err(failed(problems.join("; ")))
        }
        SshAuth::Agent => agent_auth(handle, user).await.map_err(failed),
    }
}

/// The key files to try: the one in the form, or the alias's `IdentityFile`s, or the
/// usual names in `~/.ssh`, keeping those that exist.
fn key_files(ssh: &SshTunnel, identity_files: &[PathBuf]) -> Vec<PathBuf> {
    let given = ssh.key_path.trim();
    if !given.is_empty() {
        return vec![config::expand_tilde(given)];
    }
    let mut files: Vec<PathBuf> = identity_files.to_vec();
    if files.is_empty()
        && let Some(home) = config::home_dir()
    {
        for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
            files.push(home.join(".ssh").join(name));
        }
    }
    files.retain(|p| p.is_file());
    files
}

/// The SHA-2 variant to sign RSA keys with, from what the server says it accepts.
async fn rsa_hash(handle: &Handle<Client>) -> Option<HashAlg> {
    handle
        .best_supported_rsa_hash()
        .await
        .ok()
        .flatten()
        .flatten()
}

async fn keyboard_interactive(
    handle: &mut Handle<Client>,
    user: &str,
    password: &str,
) -> Result<bool, TunnelError> {
    let to_error =
        |e: russh::Error| TunnelError::new(ErrorKind::Auth, format!("auth failed for {user}: {e}"));
    let mut reply = handle
        .authenticate_keyboard_interactive_start(user, None)
        .await
        .map_err(to_error)?;
    for _ in 0..4 {
        match reply {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                let answers = prompts.iter().map(|_| password.to_string()).collect();
                reply = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                    .map_err(to_error)?;
            }
        }
    }
    Ok(false)
}

#[cfg(unix)]
async fn agent_auth(handle: &mut Handle<Client>, user: &str) -> Result<(), String> {
    let agent = russh::keys::agent::client::AgentClient::connect_env()
        .await
        .map_err(|e| format!("no ssh-agent is running or SSH_AUTH_SOCK is not set ({e})"))?;
    agent_sign_in(handle, user, agent).await
}

#[cfg(windows)]
async fn agent_auth(handle: &mut Handle<Client>, user: &str) -> Result<(), String> {
    use russh::keys::agent::client::AgentClient;
    match AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await {
        Ok(agent) => agent_sign_in(handle, user, agent).await,
        Err(pipe_error) => match AgentClient::connect_pageant().await {
            Ok(agent) => agent_sign_in(handle, user, agent).await,
            Err(_) => Err(format!(
                "no ssh-agent is running. Start the OpenSSH Authentication Agent service or Pageant ({pipe_error})"
            )),
        },
    }
}

#[cfg(not(any(unix, windows)))]
async fn agent_auth(_: &mut Handle<Client>, _: &str) -> Result<(), String> {
    Err("ssh-agent is not supported on this system".into())
}

#[cfg(any(unix, windows))]
async fn agent_sign_in<S>(
    handle: &mut Handle<Client>,
    user: &str,
    mut agent: russh::keys::agent::client::AgentClient<S>,
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use russh::keys::agent::AgentIdentity;
    let identities = agent
        .request_identities()
        .await
        .map_err(|e| format!("could not list the agent's keys ({e})"))?;
    let keys: Vec<PublicKey> = identities
        .into_iter()
        .filter_map(|identity| match identity {
            AgentIdentity::PublicKey { key, .. } => Some(key),
            AgentIdentity::Certificate { .. } => None,
        })
        .collect();
    if keys.is_empty() {
        return Err("the ssh-agent has no keys. Add one with ssh-add".into());
    }
    for key in keys {
        let hash = if key.algorithm().is_rsa() {
            rsa_hash(handle).await
        } else {
            None
        };
        match handle
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
        {
            Ok(AuthResult::Success) => return Ok(()),
            Ok(AuthResult::Failure { .. }) => {}
            Err(e) => return Err(format!("the agent could not sign ({e:?})")),
        }
    }
    Err("the server refused every key in the ssh-agent".into())
}

/// The russh callbacks: only the host key check needs anything from us.
struct Client {
    shared: Arc<Shared>,
    asking: Arc<AtomicBool>,
    /// Why the key was refused, since russh only reports that it was.
    rejected: Arc<std::sync::Mutex<Option<TunnelError>>>,
}

impl Client {
    fn reject(&self, error: TunnelError) -> Result<bool, russh::Error> {
        if let Ok(mut slot) = self.rejected.lock() {
            *slot = Some(error);
        }
        Ok(false)
    }
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            return self.reject(TunnelError::new(
                ErrorKind::HostKey,
                "the server presented a host certificate, which is not supported yet",
            ));
        };
        let r = &self.shared.resolved;
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        // A reconnect must meet the key accepted the first time.
        let pinned = self.shared.pinned.lock().ok().and_then(|p| p.clone());
        if let Some(pinned) = pinned {
            if &pinned == key {
                return Ok(true);
            }
            return self.reject(TunnelError::new(
                ErrorKind::HostKey,
                format!(
                    "the host key of {} changed while connected (now {fingerprint}). Refusing to reconnect",
                    r.host
                ),
            ));
        }
        let files = &self.shared.request.host_keys;
        match known_hosts::check(&files.check, &r.host, r.port, key) {
            HostKeyStatus::Known => {}
            HostKeyStatus::Revoked => {
                return self.reject(TunnelError::new(
                    ErrorKind::HostKey,
                    format!(
                        "the host key of {} is marked @revoked in known_hosts",
                        r.host
                    ),
                ));
            }
            HostKeyStatus::Changed { path, line } => {
                return self.reject(TunnelError::new(
                    ErrorKind::HostKey,
                    format!(
                        "the host key of {} has changed (now {fingerprint}), which can mean someone is intercepting the connection. Refusing to connect. If the server was reinstalled, remove line {line} of {} or run: ssh-keygen -R {}",
                        r.host,
                        path.display(),
                        known_hosts::host_label(&r.host, r.port)
                    ),
                ));
            }
            HostKeyStatus::Unknown => {
                self.asking.store(true, Ordering::SeqCst);
                let question = UnknownHost {
                    host: r.host.clone(),
                    port: r.port,
                    algorithm: key.algorithm().to_string().to_uppercase(),
                    fingerprint,
                };
                let trusted = (self.shared.ask)(question).await;
                self.asking.store(false, Ordering::SeqCst);
                if !trusted {
                    return self.reject(TunnelError::new(
                        ErrorKind::HostKey,
                        format!("the host key of {} was not trusted", r.host),
                    ));
                }
                if let Some(path) = &files.learn
                    && let Err(e) = known_hosts::learn(path, &r.host, r.port, key)
                {
                    tracing::warn!("could not add {} to {}: {e}", r.host, path.display());
                }
            }
        }
        if let Ok(mut pinned) = self.shared.pinned.lock() {
            *pinned = Some(key.clone());
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_fields_win_over_the_config_file() {
        let file = HostConfig {
            hostname: Some("db1.example.com".into()),
            user: Some("deploy".into()),
            port: Some(2222),
            identity_files: vec!["/keys/%r_%n".into()],
            proxy_jump: None,
        };
        let mut ssh = SshTunnel {
            host: "prod".into(),
            ..Default::default()
        };
        let r = resolve_with(&ssh, &file).expect("resolves");
        assert_eq!(r.host, "db1.example.com");
        assert_eq!(r.port, 2222);
        assert_eq!(r.user, "deploy");
        assert_eq!(r.identity_files, vec![PathBuf::from("/keys/deploy_prod")]);

        ssh.port = Some(22);
        ssh.user = "me".into();
        let r = resolve_with(&ssh, &file).expect("resolves");
        assert_eq!((r.port, r.user.as_str()), (22, "me"));

        let r = resolve_with(&ssh, &HostConfig::default()).expect("resolves");
        assert_eq!(r.host, "prod");

        ssh.host = " ".into();
        assert_eq!(
            resolve_with(&ssh, &file).map_err(|e| e.kind),
            Err(ErrorKind::Config)
        );
    }

    #[test]
    fn errors_say_they_are_about_ssh() {
        let e = TunnelError::new(ErrorKind::Auth, "auth failed for me: no");
        assert_eq!(e.to_string(), "SSH: auth failed for me: no");
    }

    #[tokio::test]
    async fn unreachable_hosts_fail_as_unreachable() {
        // Nothing listens on port 1 of the loopback address.
        let request = TunnelRequest {
            ssh: SshTunnel {
                enabled: true,
                host: "127.0.0.1".into(),
                port: Some(1),
                user: "nobody".into(),
                ..Default::default()
            },
            target_host: "127.0.0.1".into(),
            target_port: 3306,
            host_keys: HostKeyFiles {
                check: Vec::new(),
                learn: None,
            },
        };
        let ask: AskHostKey = Arc::new(|_| Box::pin(async { false }));
        let error = Tunnel::open(request, ask).await.expect_err("fails");
        assert_eq!(error.kind, ErrorKind::Unreachable);
        assert!(error.to_string().starts_with("SSH: host unreachable"));
    }
}
