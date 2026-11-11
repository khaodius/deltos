//! SSH tunnels for Deltos connections, on russh: no OpenSSH client needed.
//!
//! - [`config`] reads `~/.ssh/config`, so a `Host` alias works as the SSH host.
//! - [`known_hosts`] checks host keys against `~/.ssh/known_hosts` and adds new ones.
//! - [`Tunnel`] signs in with a key file, the ssh-agent or a password, listens on a port
//!   of 127.0.0.1 the system picks, and forwards each connection to the database address
//!   as seen from the SSH server. It sends keepalives and reconnects when the session
//!   drops.

pub mod config;
pub mod known_hosts;
mod tunnel;

pub use tunnel::{
    AskHostKey, ErrorKind, HostKeyFiles, Resolved, Tunnel, TunnelError, TunnelRequest,
    TunnelStatus, UnknownHost, resolve,
};
