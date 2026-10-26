//! Live talk-down (ADR-091 §5): the operator's microphone, pulled from the
//! SFU by a receive-only `webrtcbin` and forwarded to the speaker's call frame
//! by frame as it arrives. The network paces it; nothing here does.

use std::time::Duration;

use nexus_cloud_protocol::v1::IceServer;
use nexus_pipeline::{IceServerCfg, TalkdownReceiver};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::sip::{SipCall, TalkdownError};

/// How long the edge has to answer the SFU's offer, ICE gathering included.
const NEGOTIATE_TIMEOUT: Duration = Duration::from_secs(5);
/// No audio for this long ends the session: the browser went away, or the
/// connection never came up.
#[cfg(not(test))]
const IDLE_LIMIT: Duration = Duration::from_secs(15);
/// Tests see the inactivity ending without waiting 15 s.
#[cfg(test)]
const IDLE_LIMIT: Duration = Duration::from_secs(2);
/// The longest a live session runs, whatever the operator does.
const MAX_LIVE: Duration = Duration::from_secs(10 * 60);
/// A 172-byte datagram that has not left in this long is held by the OS, and
/// waiting on it would outlive the stop and the limits above.
const SEND_TIMEOUT: Duration = Duration::from_secs(1);

/// A negotiated receive leg and the 160-sample frames it decodes.
pub struct Live {
    _leg: TalkdownReceiver,
    frames: mpsc::Receiver<Vec<i16>>,
}

impl Live {
    /// Answer the SFU's offer. The error never contains the SDP, and nothing
    /// here logs it or the TURN credentials.
    pub async fn answer(offer: &str, ice_servers: &[IceServer]) -> Result<(Self, String), String> {
        let ice: Vec<IceServerCfg> = ice_servers
            .iter()
            .map(|s| IceServerCfg {
                urls: s.urls.clone(),
                username: s.username.clone(),
                credential: s.credential.clone(),
            })
            .collect();
        let (leg, answer, frames) = TalkdownReceiver::answer(offer, &ice, NEGOTIATE_TIMEOUT)
            .await
            .map_err(|e| e.to_string())?;
        Ok((Self { _leg: leg, frames }, answer))
    }

    /// Forward frames to `call` until `cancel`, [`IDLE_LIMIT`] without a
    /// frame, or [`MAX_LIVE`]; a stalled send fails the call instead.
    /// Returning drops the leg, which tears it down.
    pub async fn forward(
        mut self,
        call: &mut SipCall,
        cancel: &CancellationToken,
    ) -> Result<(), TalkdownError> {
        let max = tokio::time::sleep(MAX_LIVE);
        tokio::pin!(max);
        let ended = loop {
            let frame = tokio::select! {
                _ = cancel.cancelled() => break "stopped",
                _ = &mut max => break "max_duration",
                frame = tokio::time::timeout(IDLE_LIMIT, self.frames.recv()) => frame,
            };
            match frame {
                Ok(Some(frame)) => tokio::time::timeout(SEND_TIMEOUT, call.send_frame(&frame))
                    .await
                    .unwrap_or(Err(TalkdownError::Unreachable))?,
                Ok(None) => break "receiver_closed",
                Err(_) => break "no_audio",
            }
        };
        tracing::info!(ended, "live talk-down audio ended");
        Ok(())
    }
}
