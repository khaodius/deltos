//! End to end: a russh SSH server in this process, a TCP service behind it standing in
//! for the database, and the tunnel between them. Nothing leaves the loopback address.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use deltos_data::{SshAuth, SshTunnel};
use deltos_ssh::{
    AskHostKey, ErrorKind, HostKeyFiles, Tunnel, TunnelRequest, TunnelStatus, UnknownHost,
};
use russh::Channel;
use russh::keys::ssh_key::LineEnding;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::{PrivateKey, PublicKey};
use russh::server::{self, Auth, ChannelOpenHandle, Msg, Session};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

fn key(seed: u8) -> PrivateKey {
    PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]))
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("deltos-ssh-e2e-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

/// A service that answers each line with `echo: ` and the line.
async fn echo_service() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                while let Ok(n) = socket.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let mut reply = b"echo: ".to_vec();
                    reply.extend_from_slice(&buf[..n]);
                    if socket.write_all(&reply).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

#[derive(Clone)]
struct ServerHandler {
    client_key: PublicKey,
}

impl server::Handler for ServerHandler {
    type Error = russh::Error;

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        if user == "tester" && key == &self.client_key {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        if user == "tester" && password == "letmein" {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host: &str,
        port: u32,
        _originator: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let target = format!("{host}:{port}");
        match TcpStream::connect(&target).await {
            Ok(mut socket) => {
                reply.accept().await;
                tokio::spawn(async move {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
                });
            }
            Err(_) => reply.reject(russh::ChannelOpenFailure::ConnectFailed).await,
        }
        Ok(())
    }
}

/// An SSH server on a free port. Aborting the returned sessions drops every client.
struct TestServer {
    port: u16,
    sessions: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl TestServer {
    async fn start(host_key: PrivateKey, client_key: PublicKey) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let config = Arc::new(server::Config {
            keys: vec![host_key],
            auth_rejection_time: Duration::from_millis(1),
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Default::default()
        });
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let config = config.clone();
                let handler = ServerHandler {
                    client_key: client_key.clone(),
                };
                tokio::spawn(async move {
                    if let Ok(session) = server::run_stream(config, socket, handler).await {
                        let _ = session.await;
                    }
                });
            }
        });
        // Clients reach the server through a relay, so a test can cut the network.
        let front = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let inner = port;
        let port = front.local_addr().expect("addr").port();
        let sessions = Arc::new(Mutex::new(Vec::new()));
        let kept = sessions.clone();
        tokio::spawn(async move {
            while let Ok((mut client, _)) = front.accept().await {
                let task = tokio::spawn(async move {
                    if let Ok(mut server) = TcpStream::connect(("127.0.0.1", inner)).await {
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                    }
                });
                kept.lock().expect("lock").push(task);
            }
        });
        Self { port, sessions }
    }

    fn drop_clients(&self) {
        for task in self.sessions.lock().expect("lock").drain(..) {
            task.abort();
        }
    }
}

fn asker(answer: bool, asked: Arc<Mutex<Vec<UnknownHost>>>) -> AskHostKey {
    Arc::new(move |q| {
        asked.lock().expect("lock").push(q);
        Box::pin(async move { answer })
    })
}

fn request(port: u16, dir: &std::path::Path, target_port: u16, auth: SshAuth) -> TunnelRequest {
    let known = dir.join("known_hosts");
    TunnelRequest {
        ssh: SshTunnel {
            enabled: true,
            host: "127.0.0.1".into(),
            port: Some(port),
            user: "tester".into(),
            auth,
            key_path: dir.join("id_ed25519").to_string_lossy().into_owned(),
            secret: (auth == SshAuth::Password).then(|| "letmein".to_string()),
        },
        target_host: "127.0.0.1".into(),
        target_port,
        host_keys: HostKeyFiles {
            check: vec![known.clone()],
            learn: Some(known),
        },
    }
}

async fn round_trip(port: u16, text: &str) -> String {
    let mut socket = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    socket.write_all(text.as_bytes()).await.expect("write");
    let mut buf = vec![0u8; 256];
    let n = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buf))
        .await
        .expect("answer in time")
        .expect("read");
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwards_trusts_on_first_use_and_reconnects() {
    let dir = temp_dir("main");
    let client = key(2);
    std::fs::write(
        dir.join("id_ed25519"),
        client
            .to_openssh(LineEnding::LF)
            .expect("encode")
            .as_bytes(),
    )
    .expect("key file");
    let server = TestServer::start(key(1), client.public_key().clone()).await;
    let service = echo_service().await;

    // First use: the key is unknown, the person is asked and trusts it.
    let asked = Arc::new(Mutex::new(Vec::new()));
    let tunnel = Tunnel::open(
        request(server.port, &dir, service, SshAuth::Key),
        asker(true, asked.clone()),
    )
    .await
    .expect("tunnel opens");
    {
        let asked = asked.lock().expect("lock");
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].port, server.port);
        assert!(asked[0].fingerprint.starts_with("SHA256:"));
    }
    assert_ne!(tunnel.local_port(), 0);
    assert_ne!(tunnel.local_port(), service);
    assert_eq!(round_trip(tunnel.local_port(), "ping").await, "echo: ping");

    // The session drops; the tunnel comes back on the same port.
    let mut status = tunnel.status();
    server.drop_clients();
    let back = tokio::time::timeout(Duration::from_secs(15), async {
        let mut saw_reconnecting = false;
        loop {
            status.changed().await.expect("status");
            match status.borrow_and_update().clone() {
                TunnelStatus::Reconnecting { .. } => saw_reconnecting = true,
                TunnelStatus::Up => return saw_reconnecting,
                other => panic!("unexpected {other:?}"),
            }
        }
    })
    .await
    .expect("reconnects in time");
    assert!(back);
    assert_eq!(
        round_trip(tunnel.local_port(), "again").await,
        "echo: again"
    );
    tunnel.close().await;

    // Second use: the key is in known_hosts now, so nobody is asked.
    let asked = Arc::new(Mutex::new(Vec::new()));
    let tunnel = Tunnel::open(
        request(server.port, &dir, service, SshAuth::Password),
        asker(false, asked.clone()),
    )
    .await
    .expect("known host, password");
    assert!(asked.lock().expect("lock").is_empty());
    assert_eq!(round_trip(tunnel.local_port(), "pw").await, "echo: pw");
    drop(tunnel);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn says_which_step_failed() {
    let dir = temp_dir("errors");
    let client = key(4);
    std::fs::write(
        dir.join("id_ed25519"),
        client
            .to_openssh(LineEnding::LF)
            .expect("encode")
            .as_bytes(),
    )
    .expect("key file");
    let server = TestServer::start(key(3), client.public_key().clone()).await;
    let service = echo_service().await;
    let trust = || asker(true, Arc::new(Mutex::new(Vec::new())));

    // Refusing the unknown key stops before signing in.
    let e = Tunnel::open(
        request(server.port, &dir, service, SshAuth::Key),
        asker(false, Arc::new(Mutex::new(Vec::new()))),
    )
    .await
    .expect_err("not trusted");
    assert_eq!(e.kind, ErrorKind::HostKey);

    // A wrong password is an auth failure.
    let mut wrong = request(server.port, &dir, service, SshAuth::Password);
    wrong.ssh.secret = Some("nope".into());
    let e = Tunnel::open(wrong, trust())
        .await
        .expect_err("bad password");
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.to_string().starts_with("SSH: auth failed"), "{e}");

    // Signed in, but nothing listens where the database should be.
    let closed = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let nothing = closed.local_addr().expect("addr").port();
    drop(closed);
    let e = Tunnel::open(request(server.port, &dir, nothing, SshAuth::Key), trust())
        .await
        .expect_err("no database");
    assert_eq!(e.kind, ErrorKind::Forward);
    assert!(e.to_string().contains("could not reach"), "{e}");

    // The server's key changes: refused, without asking.
    let other = TestServer::start(key(9), client.public_key().clone()).await;
    let known = dir.join("known_hosts");
    let text = std::fs::read_to_string(&known).expect("learned");
    std::fs::write(
        &known,
        text.replace(
            &format!("]:{} ", server.port),
            &format!("]:{} ", other.port),
        ),
    )
    .expect("rewrite");
    let asked = Arc::new(Mutex::new(Vec::new()));
    let e = Tunnel::open(
        request(other.port, &dir, service, SshAuth::Key),
        asker(true, asked.clone()),
    )
    .await
    .expect_err("changed key");
    assert_eq!(e.kind, ErrorKind::HostKey);
    assert!(e.to_string().contains("has changed"), "{e}");
    assert!(asked.lock().expect("lock").is_empty());

    std::fs::remove_dir_all(&dir).ok();
}
