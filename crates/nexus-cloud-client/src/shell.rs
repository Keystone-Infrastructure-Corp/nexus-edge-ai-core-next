//! Remote-shell byte pump.
//!
//! The transport half of a support session: a raw WSS side channel in
//! one hand, a local TCP socket in the other, and nothing in between
//! but bytes. It lives in this crate rather than in the engine because
//! the WebSocket machinery already does — the engine holds the policy
//! (who may open a session, where it may terminate, when it must stop)
//! and delegates the plumbing here.
//!
//! The pump deliberately knows nothing about SSH. It never inspects,
//! buffers for inspection, or logs payload bytes; it counts them and
//! moves them. Anything else would make the appliance a wiretap on its
//! owner's own maintenance session.

use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;

use crate::tunnel::SideChannel;

/// Read chunk for the local → cloud direction. Interactive traffic is
/// tiny; a bulk `scp` is what actually fills this.
const READ_CHUNK: usize = 16 * 1024;

/// Why a pumped session stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellStop {
    /// The far side (recipient's browser or SSH client) went away, or
    /// the local service closed the connection. The ordinary ending.
    PeerClosed,
    /// The caller's stop signal fired.
    Cancelled,
    /// The deadline passed.
    Expired,
    /// The byte ceiling was reached.
    ByteLimit,
    /// The local TCP socket errored mid-session.
    LocalIoError,
}

/// Byte counts and the reason a session ended.
#[derive(Debug, Clone, Copy)]
pub struct ShellTally {
    /// Bytes relayed recipient → local service.
    pub bytes_up: u64,
    /// Bytes relayed local service → recipient.
    pub bytes_down: u64,
    /// Why it stopped.
    pub stop: ShellStop,
}

/// Relay bytes between `ws` and `tcp` until something stops it.
///
/// Stops at the first of: either side closing, `cancel` firing,
/// `deadline` passing, or `max_bytes` being reached. Both sockets are
/// shut down cleanly before returning, so the recipient's terminal
/// reports a closed connection instead of hanging.
pub async fn pump(
    mut ws: SideChannel,
    mut tcp: TcpStream,
    deadline: tokio::time::Instant,
    max_bytes: Option<u64>,
    cancel: &mut tokio::sync::mpsc::Receiver<()>,
) -> ShellTally {
    let mut bytes_up: u64 = 0;
    let mut bytes_down: u64 = 0;
    let mut buf = vec![0_u8; READ_CHUNK];
    let mut stop = ShellStop::PeerClosed;

    loop {
        tokio::select! {
            _ = cancel.recv() => {
                stop = ShellStop::Cancelled;
                break;
            }
            () = tokio::time::sleep_until(deadline) => {
                stop = ShellStop::Expired;
                break;
            }
            frame = ws.next() => {
                match frame {
                    Some(Ok(Message::Binary(bytes))) => {
                        bytes_up += bytes.len() as u64;
                        if tcp.write_all(&bytes).await.is_err() {
                            stop = ShellStop::LocalIoError;
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        if ws.send(Message::Pong(p)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                    Some(Ok(_)) => {}
                }
            }
            read = tcp.read(&mut buf) => {
                match read {
                    Ok(0) => break,
                    Ok(n) => {
                        bytes_down += n as u64;
                        if ws.send(Message::Binary(buf[..n].to_vec())).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        stop = ShellStop::LocalIoError;
                        break;
                    }
                }
            }
        }

        if let Some(cap) = max_bytes {
            if bytes_up + bytes_down >= cap {
                stop = ShellStop::ByteLimit;
                break;
            }
        }
    }

    let _ = ws.send(Message::Close(None)).await;
    let _ = tcp.shutdown().await;

    ShellTally {
        bytes_up,
        bytes_down,
        stop,
    }
}
