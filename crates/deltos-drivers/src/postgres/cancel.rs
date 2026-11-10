//! Cancel requests that have taken effect by the time they return.
//!
//! PostgreSQL cancels a statement through a separate connection to the server: the client
//! sends the session's key, and the server signals the session's process and closes the
//! connection. `tokio_postgres::CancelToken::cancel_query` returns as soon as the request is
//! written, before the server has acted on it. A request still in flight then lands on
//! whatever the session runs next, so the statement after a cancelled one could itself be
//! cancelled at random.
//!
//! [`cancel`] waits until the server has closed the cancel connection. The server signals
//! the session before it closes, so once [`cancel`] returns the signal has been delivered:
//! the old statement ends, or, if it had already finished, the session ignores the signal
//! because it is idle waiting for a command. Only then does the caller release the session
//! for its next statement.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use deltos_data::Error;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_postgres::CancelToken;
use tokio_postgres::config::Host;
use tokio_postgres::tls::MakeTlsConnect;
use tokio_postgres_rustls::MakeRustlsConnect;

use super::pg_error;

/// How long to wait for the server to act on a cancel request before carrying on.
const WAIT: Duration = Duration::from_secs(5);

/// Where a session's cancel requests go: the one TCP host it connected to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Target {
    host: String,
    port: u16,
}

impl Target {
    /// The target for a session opened with `config`. `None` for Unix sockets and for
    /// configs listing several hosts, where the host the session reached is not known;
    /// those fall back to a request that does not wait.
    pub(super) fn of(config: &tokio_postgres::Config) -> Option<Target> {
        let [Host::Tcp(host)] = config.get_hosts() else {
            return None;
        };
        let port = match config.get_ports() {
            [] => 5432,
            [port] => *port,
            _ => return None,
        };
        Some(Target {
            host: host.clone(),
            port,
        })
    }
}

/// Cancels what the session behind `token` runs, and returns once the server has acted on
/// the request (see the module documentation).
pub(super) async fn cancel(
    token: &CancelToken,
    target: Option<&Target>,
    tls: &MakeRustlsConnect,
) -> Result<(), Error> {
    let Some(target) = target else {
        return token.cancel_query(tls.clone()).await.map_err(pg_error);
    };
    let request = async {
        let socket = TcpStream::connect((target.host.as_str(), target.port))
            .await
            .map_err(|e| Error::Other(format!("could not reach the server to cancel: {e}")))?;
        let connect = MakeTlsConnect::<UntilClosed<TcpStream>>::make_tls_connect(
            &mut tls.clone(),
            &target.host,
        )
        .map_err(|e| Error::Other(format!("could not set up TLS to cancel: {e}")))?;
        token
            .cancel_query_raw(UntilClosed(socket), connect)
            .await
            .map_err(pg_error)
    };
    match tokio::time::timeout(WAIT, request).await {
        Ok(result) => result,
        Err(_) => {
            tracing::debug!("PostgreSQL did not close the cancel connection in time");
            Ok(())
        }
    }
}

/// A socket whose shutdown also waits for the server to close its side.
struct UntilClosed<S>(S);

impl<S: AsyncRead + Unpin> AsyncRead for UntilClosed<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for UntilClosed<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(Pin::new(&mut self.0).poll_shutdown(cx))?;
        // The server sends nothing back; anything that does arrive is discarded. A reset
        // counts as closed too.
        let mut scratch = [0u8; 64];
        loop {
            let mut buf = ReadBuf::new(&mut scratch);
            match ready!(Pin::new(&mut self.0).poll_read(cx, &mut buf)) {
                Ok(()) if buf.filled().is_empty() => return Poll::Ready(Ok(())),
                Ok(()) => continue,
                Err(_) => return Poll::Ready(Ok(())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_one_tcp_host() {
        let config: tokio_postgres::Config = "host=db.local port=6000".parse().expect("config");
        assert_eq!(
            Target::of(&config),
            Some(Target {
                host: "db.local".into(),
                port: 6000
            })
        );
        let config: tokio_postgres::Config = "host=db.local".parse().expect("config");
        assert_eq!(Target::of(&config).map(|t| t.port), Some(5432));
        let config: tokio_postgres::Config = "host=a,b".parse().expect("config");
        assert_eq!(Target::of(&config), None);
    }

    #[tokio::test]
    async fn shutdown_waits_for_the_peer_to_close() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (client, mut server) = tokio::io::duplex(64);
        let mut client = UntilClosed(client);
        let closer = tokio::spawn(async move {
            let mut buf = Vec::new();
            server.read_to_end(&mut buf).await.expect("read");
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(server);
            buf
        });
        client.write_all(b"cancel").await.expect("write");
        let started = std::time::Instant::now();
        client.shutdown().await.expect("shutdown");
        assert!(started.elapsed() >= Duration::from_millis(40));
        assert_eq!(closer.await.expect("join"), b"cancel");
    }
}
