//! Minimal SIP UAC over UDP for direct-IP talk-down calls (ADR-090): INVITE a
//! speaker, stream G.711 RTP at it, BYE. No REGISTER, TCP/TLS, re-INVITE or
//! inbound calls. Bench recipe: `docs/edge-core/TALKDOWN_SPEAKERS.md`.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::time::{timeout_at, Instant};
use tokio_util::sync::CancellationToken;

use super::g711::Codec;

/// Samples in one 20 ms frame at 8 kHz.
pub const FRAME_SAMPLES: usize = 160;

#[derive(Debug, Clone)]
pub struct SipTarget {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum TalkdownError {
    #[error("speaker did not answer")]
    Unreachable,
    #[error("speaker answered SIP {0}")]
    Rejected(u16),
    #[error("speaker refused the credentials, or needs some")]
    AuthFailed,
    #[error("speaker answered with neither PCMU nor PCMA")]
    NoCommonCodec,
    #[error("talk-down socket error: {0}")]
    Io(#[from] std::io::Error),
}

/// RFC 3261 T1: the first INVITE retransmit interval, doubled each time (timer A).
const T1: Duration = Duration::from_millis(500);
/// How long [`SipCall::hangup`] waits for the BYE's 200.
const BYE_WAIT: Duration = Duration::from_secs(2);

/// An answered call. Dropping it without [`SipCall::hangup`] still sends one
/// best-effort BYE (without waiting for the 200).
#[derive(Debug)]
pub struct SipCall {
    leg: Leg,
    /// The 2xx's `To` (it carries the speaker's tag); in-dialog requests reuse it.
    to: String,
    invite_cseq: u32,
    codec: Codec,
    rtp: UdpSocket,
    rtp_dest: SocketAddr,
    seq: u16,
    timestamp: u32,
    ssrc: u32,
    hung_up: bool,
}

/// Send SIP `OPTIONS`; on any final response return its `User-Agent` (or
/// `Server`) header, or an empty string when it has neither.
pub async fn options_ping(host: &str, port: u16, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    let leg = timeout_at(deadline, Leg::open(host, port))
        .await
        .ok()?
        .ok()?;
    let to = format!("<{}>", leg.uri);
    let resp = leg
        .transact("OPTIONS", 1, &branch(), &to, "", "", deadline)
        .await
        .ok()??;
    let ua = resp.header("user-agent").or_else(|| resp.header("server"));
    Some(ua.unwrap_or_default().to_string())
}

/// INVITE `target` directly by IP (no REGISTER) offering PCMU and PCMA, and
/// wait up to `timeout` for a final answer. A 401/407 is retried once with
/// Digest credentials when the target has them.
pub async fn place_call(target: &SipTarget, timeout: Duration) -> Result<SipCall, TalkdownError> {
    let deadline = Instant::now() + timeout;
    let leg = timeout_at(deadline, Leg::open(&target.host, target.port))
        .await
        .map_err(|_| TalkdownError::Unreachable)??;
    let rtp = UdpSocket::bind((leg.local.ip(), 0)).await?;
    let sdp = offer(leg.local.ip(), rtp.local_addr()?.port());
    let invite_to = format!("<{}>", leg.uri);
    let mut cseq = 1;
    let mut auth = String::new();
    let answer = loop {
        let branch = branch();
        let Some(resp) = leg
            .transact("INVITE", cseq, &branch, &invite_to, &auth, &sdp, deadline)
            .await?
        else {
            return Err(TalkdownError::Unreachable);
        };
        if resp.status >= 300 {
            // A non-2xx is ACKed inside the INVITE transaction (same branch).
            let to = resp.header("to").unwrap_or(&invite_to);
            leg.send(&leg.request("ACK", cseq, &branch, to, "", ""))
                .await?;
        }
        match resp.status {
            200..=299 => break resp,
            401 | 407 if auth.is_empty() => {
                let (Some(user), Some(pass)) = (&target.username, &target.password) else {
                    return Err(TalkdownError::AuthFailed);
                };
                let (challenge, header) = if resp.status == 401 {
                    ("www-authenticate", "Authorization")
                } else {
                    ("proxy-authenticate", "Proxy-Authorization")
                };
                let value = resp
                    .headers(challenge)
                    .find_map(|c| digest_authorization(c, user, pass, "INVITE", &leg.uri, &token()))
                    .ok_or(TalkdownError::AuthFailed)?;
                auth = format!("{header}: {value}\r\n");
                cseq += 1;
            }
            401 | 407 => return Err(TalkdownError::AuthFailed),
            status => return Err(TalkdownError::Rejected(status)),
        }
    };

    let to = answer.header("to").unwrap_or_default().to_string();
    // The 2xx ACK is its own transaction (new branch).
    leg.send(&leg.request("ACK", cseq, &branch(), &to, "", ""))
        .await?;
    let (rtp_dest, codec) = match parse_answer(&answer.body, leg.peer.ip()) {
        Ok(media) => media,
        Err(e) => {
            let _ = leg.bye(cseq + 1, &to).await;
            return Err(e);
        }
    };
    let r = getrandom::u64().unwrap_or_default();
    Ok(SipCall {
        leg,
        to,
        invite_cseq: cseq,
        codec,
        rtp,
        rtp_dest,
        seq: r as u16,
        timestamp: (r >> 16) as u32,
        ssrc: getrandom::u32().unwrap_or_default(),
        hung_up: false,
    })
}

impl SipCall {
    /// Send one 20 ms frame (at most 160 samples, padded with silence) as one
    /// RTP packet. The caller paces.
    pub async fn send_frame(&mut self, pcm: &[i16]) -> Result<(), TalkdownError> {
        if pcm.len() > FRAME_SAMPLES {
            let msg = "send_frame takes at most one 20 ms frame";
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, msg).into());
        }
        let mut packet = Vec::with_capacity(12 + FRAME_SAMPLES);
        packet.extend_from_slice(&[0x80, self.codec.payload_type()]);
        packet.extend_from_slice(&self.seq.to_be_bytes());
        packet.extend_from_slice(&self.timestamp.to_be_bytes());
        packet.extend_from_slice(&self.ssrc.to_be_bytes());
        let samples = pcm.iter().copied().chain(std::iter::repeat(0));
        packet.extend(samples.take(FRAME_SAMPLES).map(|s| self.codec.encode(s)));
        self.rtp.send_to(&packet, self.rtp_dest).await?;
        self.seq = self.seq.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(FRAME_SAMPLES as u32);
        Ok(())
    }

    /// Stream `pcm` as 20 ms RTP frames until it ends or `cancel` fires.
    // shortcut: inbound SIP is not read while playing, so a 2xx retransmitted
    // after a lost ACK is not re-ACKed and the speaker may drop the call ~32 s
    // in; read the SIP socket here if long plays on lossy links matter.
    pub async fn play_pcm(
        &mut self,
        pcm: &[i16],
        cancel: &CancellationToken,
    ) -> Result<(), TalkdownError> {
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        for frame in pcm.chunks(FRAME_SAMPLES) {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = tick.tick() => self.send_frame(frame).await?,
            }
        }
        Ok(())
    }

    /// Send BYE and wait briefly for the speaker's 200.
    pub async fn hangup(mut self) -> Result<(), TalkdownError> {
        self.hung_up = true;
        self.leg.bye(self.invite_cseq + 1, &self.to).await
    }
}

impl Drop for SipCall {
    fn drop(&mut self) {
        if !self.hung_up {
            let cseq = self.invite_cseq + 1;
            let bye = self.leg.request("BYE", cseq, &branch(), &self.to, "", "");
            let _ = self.leg.sock.try_send_to(bye.as_bytes(), self.leg.peer);
        }
    }
}

/// One UAC endpoint toward one peer: the SIP socket and the dialog identifiers.
#[derive(Debug)]
struct Leg {
    sock: UdpSocket,
    peer: SocketAddr,
    local: SocketAddr,
    uri: String,
    call_id: String,
    tag: String,
}

impl Leg {
    async fn open(host: &str, port: u16) -> std::io::Result<Self> {
        let peer = tokio::net::lookup_host((host, port))
            .await?
            .next()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no address"))?;
        // Connecting a throwaway socket makes the OS pick the local address
        // that routes to the peer; the SDP and Via must carry that address.
        let any = if peer.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let probe = UdpSocket::bind(any).await?;
        probe.connect(peer).await?;
        let sock = UdpSocket::bind((probe.local_addr()?.ip(), 0)).await?;
        let local = sock.local_addr()?;
        let uri = match (peer.ip(), peer.port()) {
            (IpAddr::V4(ip), 5060) => format!("sip:{ip}"),
            (IpAddr::V6(ip), 5060) => format!("sip:[{ip}]"),
            _ => format!("sip:{peer}"),
        };
        Ok(Self {
            sock,
            peer,
            local,
            uri,
            call_id: format!("{}@nexus", token()),
            tag: token(),
        })
    }

    fn request(
        &self,
        method: &str,
        cseq: u32,
        branch: &str,
        to: &str,
        extra: &str,
        body: &str,
    ) -> String {
        let Self {
            local,
            uri,
            call_id,
            tag,
            ..
        } = self;
        let content_type = if body.is_empty() {
            ""
        } else {
            "Content-Type: application/sdp\r\n"
        };
        format!(
            "{method} {uri} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {local};branch={branch};rport\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:nexus@{local}>;tag={tag}\r\n\
             To: {to}\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq} {method}\r\n\
             Contact: <sip:nexus@{local}>\r\n\
             User-Agent: nexus-engine\r\n\
             {extra}{content_type}Content-Length: {len}\r\n\r\n{body}",
            len = body.len()
        )
    }

    async fn send(&self, msg: &str) -> std::io::Result<()> {
        self.sock.send_to(msg.as_bytes(), self.peer).await.map(drop)
    }

    /// Send a request, retransmitting on timer A/E (T1, doubling) until a
    /// response for it arrives, then wait for the final one. Returns `None` at
    /// `deadline`, CANCELling an INVITE that got a provisional.
    #[allow(clippy::too_many_arguments)]
    async fn transact(
        &self,
        method: &str,
        cseq: u32,
        branch: &str,
        to: &str,
        extra: &str,
        body: &str,
        deadline: Instant,
    ) -> std::io::Result<Option<Response>> {
        let msg = self.request(method, cseq, branch, to, extra, body);
        let mut buf = vec![0u8; 8192];
        let mut gap = T1;
        // `None` once a provisional arrived: stop retransmitting, wait for the final.
        let mut retransmit_at = Some(Instant::now() + gap);
        self.send(&msg).await?;
        loop {
            let wake = retransmit_at.map_or(deadline, |at| at.min(deadline));
            match timeout_at(wake, self.sock.recv_from(&mut buf)).await {
                Ok(received) => {
                    let (n, _) = received?;
                    let Some(resp) = Response::parse(&buf[..n]) else {
                        continue;
                    };
                    if !resp.answers(&self.call_id, cseq, method) {
                        continue;
                    }
                    if resp.status >= 200 {
                        return Ok(Some(resp));
                    }
                    retransmit_at = None;
                }
                Err(_) if Instant::now() >= deadline => {
                    if retransmit_at.is_none() && method == "INVITE" {
                        self.send(&self.request("CANCEL", cseq, branch, to, "", ""))
                            .await?;
                    }
                    return Ok(None);
                }
                Err(_) => {
                    self.send(&msg).await?;
                    gap *= 2;
                    retransmit_at = Some(Instant::now() + gap);
                }
            }
        }
    }

    async fn bye(&self, cseq: u32, to: &str) -> Result<(), TalkdownError> {
        let deadline = Instant::now() + BYE_WAIT;
        match self
            .transact("BYE", cseq, &branch(), to, "", "", deadline)
            .await?
        {
            Some(resp) if resp.status < 300 => Ok(()),
            Some(resp) => Err(TalkdownError::Rejected(resp.status)),
            None => Err(TalkdownError::Unreachable),
        }
    }
}

/// A parsed SIP response. Header names are lowercased.
#[derive(Debug)]
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Response {
    fn parse(datagram: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(datagram).ok()?;
        let (head, body) = text
            .split_once("\r\n\r\n")
            .or_else(|| text.split_once("\n\n"))
            .unwrap_or((text, ""));
        let mut lines = head.lines();
        let status_line = lines.next()?.strip_prefix("SIP/2.0 ")?;
        let status = status_line.split_whitespace().next()?.parse().ok()?;
        let headers = lines
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
            })
            .collect();
        Some(Self {
            status,
            headers,
            body: body.to_string(),
        })
    }

    /// Every value of header `name` (lowercase), including its compact form.
    fn headers<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        let compact = match name {
            "to" => "t",
            "call-id" => "i",
            _ => name,
        };
        self.headers
            .iter()
            .filter(move |(k, _)| k == name || k == compact)
            .map(|(_, v)| v.as_str())
    }

    fn header<'a>(&'a self, name: &'a str) -> Option<&'a str> {
        self.headers(name).next()
    }

    /// Whether this answers our request `cseq method` in dialog `call_id`.
    fn answers(&self, call_id: &str, cseq: u32, method: &str) -> bool {
        let cseq_matches = self.header("cseq").is_some_and(|value| {
            let mut parts = value.split_whitespace();
            parts.next().and_then(|n| n.parse().ok()) == Some(cseq) && parts.next() == Some(method)
        });
        cseq_matches && self.header("call-id") == Some(call_id)
    }
}

fn token() -> String {
    format!("{:016x}", getrandom::u64().unwrap_or_default())
}

fn branch() -> String {
    format!("z9hG4bK{}", token())
}

fn offer(ip: IpAddr, rtp_port: u16) -> String {
    let af = if ip.is_ipv4() { "IP4" } else { "IP6" };
    format!(
        "v=0\r\no=nexus 1 1 IN {af} {ip}\r\ns=talkdown\r\nc=IN {af} {ip}\r\nt=0 0\r\n\
         m=audio {rtp_port} RTP/AVP 0 8\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:8 PCMA/8000\r\n\
         a=ptime:20\r\na=sendonly\r\n"
    )
}

/// Where to send RTP and in which codec: the first audio stream's address
/// (media-level `c=` over session-level, else `fallback`) and its first
/// payload type that is PCMU or PCMA, in the answer's own order.
fn parse_answer(sdp: &str, fallback: IpAddr) -> Result<(SocketAddr, Codec), TalkdownError> {
    let mut ip = None;
    let mut audio: Option<&str> = None;
    // None = session level, Some(true) = our audio section, Some(false) = any other.
    let mut in_audio = None;
    for line in sdp.lines().map(str::trim) {
        if let Some(media) = line.strip_prefix("m=") {
            let first_audio = audio.is_none() && media.starts_with("audio ");
            if first_audio {
                audio = Some(media);
            }
            in_audio = Some(first_audio);
        } else if let Some(conn) = line.strip_prefix("c=") {
            // `IN IP4 192.0.2.1` (a multicast `/ttl` suffix is dropped).
            let addr = conn
                .split_whitespace()
                .nth(2)
                .and_then(|a| a.split('/').next());
            if in_audio != Some(false) {
                ip = addr.and_then(|a| a.parse().ok()).or(ip);
            }
        }
    }
    // Static payload types only: PT 0 and 8 are fixed by RFC 3551 and the
    // offer has no dynamic ones, so `a=rtpmap` cannot change the choice.
    let mut fields = audio
        .ok_or(TalkdownError::NoCommonCodec)?
        .split_whitespace();
    let port: u16 = fields.nth(1).and_then(|p| p.parse().ok()).unwrap_or(0);
    let codec = fields
        .skip(1)
        .find_map(|pt| Codec::from_payload_type(pt.parse().ok()?));
    match codec {
        Some(codec) if port != 0 => Ok((SocketAddr::new(ip.unwrap_or(fallback), port), codec)),
        _ => Err(TalkdownError::NoCommonCodec),
    }
}

fn md5_hex(s: &str) -> String {
    use md5::{Digest, Md5};
    format!("{:x}", Md5::digest(s.as_bytes()))
}

/// The `Authorization` value answering a `Digest` challenge (RFC 2617, MD5,
/// `qop=auth` when offered). `None` for other schemes or algorithms.
fn digest_authorization(
    challenge: &str,
    user: &str,
    pass: &str,
    method: &str,
    uri: &str,
    cnonce: &str,
) -> Option<String> {
    let (scheme, params) = challenge.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("digest") {
        return None;
    }
    let (mut realm, mut nonce, mut opaque, mut qop_auth) = (None, None, None, false);
    for param in split_quoted_csv(params) {
        let (key, value) = param.split_once('=')?;
        let value = value.trim().trim_matches('"');
        match key.trim().to_ascii_lowercase().as_str() {
            "realm" => realm = Some(value),
            "nonce" => nonce = Some(value),
            "opaque" => opaque = Some(value),
            "qop" => {
                qop_auth = value
                    .split(',')
                    .any(|q| q.trim().eq_ignore_ascii_case("auth"))
            }
            "algorithm" if !value.eq_ignore_ascii_case("md5") => return None,
            _ => {}
        }
    }
    let (realm, nonce) = (realm?, nonce?);
    let ha1 = md5_hex(&format!("{user}:{realm}:{pass}"));
    let ha2 = md5_hex(&format!("{method}:{uri}"));
    let mut h = format!(
        r#"Digest username="{user}", realm="{realm}", nonce="{nonce}", uri="{uri}", algorithm=MD5"#
    );
    if qop_auth {
        let response = md5_hex(&format!("{ha1}:{nonce}:00000001:{cnonce}:auth:{ha2}"));
        h += &format!(r#", response="{response}", qop=auth, nc=00000001, cnonce="{cnonce}""#);
    } else {
        let response = md5_hex(&format!("{ha1}:{nonce}:{ha2}"));
        h += &format!(r#", response="{response}""#);
    }
    if let Some(opaque) = opaque {
        h += &format!(r#", opaque="{opaque}""#);
    }
    Some(h)
}

/// Comma-split challenge parameters, keeping quoted commas (`qop="auth,auth-int"`).
fn split_quoted_csv(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut quoted) = (0, false);
    for (i, ch) in s.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(s[start..].trim());
    out.retain(|p| !p.is_empty());
    out
}

/// Also the fake speaker of the talk-down endpoint tests in `crate::api`.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::talkdown::tone_pcm;
    use tokio::task::JoinHandle;
    use tokio::time::timeout;

    pub(crate) const SDP: &str = "Content-Type: application/sdp\r\n";

    /// A SIP UAS on loopback. `respond` maps each request to the responses
    /// sent back; the task returns every request seen, ending with the first
    /// one that starts with `until`.
    pub(crate) async fn fake_uas(
        until: &'static str,
        respond: impl Fn(&str) -> Vec<String> + Send + 'static,
    ) -> (u16, JoinHandle<Vec<String>>) {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = sock.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let mut seen = Vec::new();
            let mut buf = vec![0u8; 8192];
            loop {
                let (n, from) = timeout(Duration::from_secs(5), sock.recv_from(&mut buf))
                    .await
                    .expect("fake UAS waited 5 s for a request")
                    .unwrap();
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                for resp in respond(&req) {
                    sock.send_to(resp.as_bytes(), from).await.unwrap();
                }
                let done = req.starts_with(until);
                seen.push(req);
                if done {
                    return seen;
                }
            }
        });
        (port, task)
    }

    /// A response to `req` copying its Via/From/To/Call-ID/CSeq, with a To-tag.
    pub(crate) fn reply(req: &str, status: &str, extra: &str, body: &str) -> String {
        let mut out = format!("SIP/2.0 {status}\r\n");
        for line in req.lines().skip(1).take_while(|l| !l.is_empty()) {
            let name = line.split(':').next().unwrap().to_ascii_lowercase();
            if ["via", "from", "call-id", "cseq", "to"].contains(&name.as_str()) {
                out.push_str(line);
                if name == "to" && !line.contains("tag=") {
                    out.push_str(";tag=uas1");
                }
                out.push_str("\r\n");
            }
        }
        out + extra + &format!("Content-Length: {}\r\n\r\n{body}", body.len())
    }

    fn header<'a>(msg: &'a str, name: &str) -> Option<&'a str> {
        msg.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }

    fn methods(seen: &[String]) -> Vec<&str> {
        seen.iter().map(|r| r.split(' ').next().unwrap()).collect()
    }

    fn target(port: u16) -> SipTarget {
        SipTarget {
            host: "127.0.0.1".into(),
            port,
            username: None,
            password: None,
        }
    }

    pub(crate) fn answer(rtp_port: u16, pts: &str) -> String {
        format!(
            "v=0\r\no=uas 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
             m=audio {rtp_port} RTP/AVP {pts}\r\na=sendrecv\r\n"
        )
    }

    #[tokio::test]
    async fn options_ping_returns_user_agent() {
        let (port, uas) = fake_uas("OPTIONS", |req| {
            vec![reply(req, "200 OK", "User-Agent: TSIP\r\n", "")]
        })
        .await;
        let ua = options_ping("127.0.0.1", port, Duration::from_secs(2)).await;
        assert_eq!(ua.as_deref(), Some("TSIP"));
        uas.await.unwrap();
    }

    #[tokio::test]
    async fn call_streams_answered_codec_and_hangs_up() {
        let rtp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let rtp_port = rtp.local_addr().unwrap().port();
        let (port, uas) = fake_uas("BYE", move |req| {
            if req.starts_with("INVITE") {
                vec![
                    reply(req, "100 Trying", "", ""),
                    reply(req, "180 Ringing", "", ""),
                    reply(req, "200 OK", SDP, &answer(rtp_port, "8")),
                ]
            } else if req.starts_with("BYE") {
                vec![reply(req, "200 OK", "", "")]
            } else {
                vec![]
            }
        })
        .await;

        let mut call = place_call(&target(port), Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(call.codec, Codec::Pcma);
        // 70 ms = 560 samples: three full frames and one padded with silence.
        call.play_pcm(&tone_pcm(70), &CancellationToken::new())
            .await
            .unwrap();
        let mut buf = [0u8; 2048];
        let mut seqs = Vec::new();
        for _ in 0..4 {
            let n = timeout(Duration::from_secs(1), rtp.recv(&mut buf))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(n, 12 + FRAME_SAMPLES, "one 160-byte payload per packet");
            assert_eq!(buf[0], 0x80, "RTP v2, no padding/extension/CSRC");
            assert_eq!(buf[1] & 0x7f, 8, "payload type from the answer");
            seqs.push(u16::from_be_bytes([buf[2], buf[3]]));
        }
        assert!(
            buf[12 + 80..12 + FRAME_SAMPLES].iter().all(|&b| b == 0xd5),
            "A-law silence pad"
        );
        assert!(seqs.windows(2).all(|w| w[1] == w[0].wrapping_add(1)));
        call.hangup().await.unwrap();

        let seen = uas.await.unwrap();
        assert_eq!(methods(&seen), ["INVITE", "ACK", "BYE"]);
        assert!(seen[0].contains(" RTP/AVP 0 8\r\n") && seen[0].contains("a=sendonly"));
        for in_dialog in &seen[1..] {
            assert!(header(in_dialog, "To").unwrap().ends_with(";tag=uas1"));
        }
        assert_eq!(header(&seen[2], "CSeq"), Some("2 BYE"));
    }

    #[tokio::test]
    async fn digest_challenge_is_answered_once() {
        let (port, uas) = fake_uas("BYE", |req| {
            let challenge =
                "WWW-Authenticate: Digest realm=\"spk\", nonce=\"n0nce\", qop=\"auth\"\r\n";
            if req.starts_with("INVITE") && header(req, "Authorization").is_none() {
                vec![reply(req, "401 Unauthorized", challenge, "")]
            } else if req.starts_with("INVITE") {
                vec![reply(req, "200 OK", SDP, &answer(9, "0"))]
            } else if req.starts_with("BYE") {
                vec![reply(req, "200 OK", "", "")]
            } else {
                vec![]
            }
        })
        .await;
        let mut t = target(port);
        t.username = Some("admin".into());
        t.password = Some("s3cret".into());

        let call = place_call(&t, Duration::from_secs(2)).await.unwrap();
        assert_eq!(call.codec, Codec::Pcmu);
        call.hangup().await.unwrap();

        let seen = uas.await.unwrap();
        assert_eq!(methods(&seen), ["INVITE", "ACK", "INVITE", "ACK", "BYE"]);
        assert_eq!(header(&seen[2], "CSeq"), Some("2 INVITE"));
        assert_eq!(header(&seen[0], "Call-ID"), header(&seen[2], "Call-ID"));
        let auth = header(&seen[2], "Authorization").unwrap();
        let field = |k: &str| -> String {
            let rest = auth.strip_prefix("Digest ").unwrap();
            rest.split(", ")
                .find_map(|p| p.split_once('=').filter(|(name, _)| *name == k))
                .map(|(_, v)| v.trim_matches('"').to_string())
                .unwrap_or_else(|| panic!("no {k} in {auth}"))
        };
        // Verify as the UAS would (RFC 2617 §3.2.2, qop=auth).
        let ha1 = md5_hex("admin:spk:s3cret");
        let ha2 = md5_hex(&format!("INVITE:{}", field("uri")));
        let want = md5_hex(&format!(
            "{ha1}:n0nce:{}:{}:auth:{ha2}",
            field("nc"),
            field("cnonce")
        ));
        assert_eq!(field("username"), "admin");
        assert_eq!(field("response"), want);
    }

    #[test]
    fn digest_matches_rfc2617_example() {
        let challenge = "Digest realm=\"testrealm@host.com\", qop=\"auth,auth-int\", \
                         nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", \
                         opaque=\"5ccc069c403ebaf9f0171e9517f40e41\"";
        let h = digest_authorization(
            challenge,
            "Mufasa",
            "Circle Of Life",
            "GET",
            "/dir/index.html",
            "0a4f113b",
        )
        .unwrap();
        assert!(
            h.contains("response=\"6629fae49393a05397450978507c4ef1\""),
            "{h}"
        );
        assert!(
            h.contains("opaque=\"5ccc069c403ebaf9f0171e9517f40e41\""),
            "{h}"
        );
        assert!(digest_authorization("Basic realm=\"x\"", "u", "p", "GET", "/", "c").is_none());
    }

    #[tokio::test]
    async fn silent_peer_is_unreachable_after_timer_a_retransmits() {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = sock.local_addr().unwrap().port();
        let started = Instant::now();
        let err = place_call(&target(port), Duration::from_millis(1200))
            .await
            .unwrap_err();
        let took = started.elapsed();
        assert!(matches!(err, TalkdownError::Unreachable), "{err}");
        assert!(took >= Duration::from_millis(1200) && took < Duration::from_secs(2));
        // INVITE at 0 ms and 500 ms; the next (1500 ms) is past the deadline.
        let mut buf = vec![0u8; 8192];
        let mut invites = 0;
        while let Ok(Ok(n)) = timeout(Duration::from_millis(100), sock.recv(&mut buf)).await {
            invites += usize::from(buf[..n].starts_with(b"INVITE"));
        }
        assert_eq!(invites, 2);
    }

    #[tokio::test]
    async fn busy_is_rejected_and_acked_in_the_invite_transaction() {
        let (port, uas) = fake_uas("ACK", |req| {
            if req.starts_with("INVITE") {
                vec![reply(req, "486 Busy Here", "", "")]
            } else {
                vec![]
            }
        })
        .await;
        let err = place_call(&target(port), Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(matches!(err, TalkdownError::Rejected(486)), "{err}");
        let seen = uas.await.unwrap();
        assert_eq!(methods(&seen), ["INVITE", "ACK"]);
        assert_eq!(header(&seen[0], "Via"), header(&seen[1], "Via"));
    }

    #[tokio::test]
    async fn answer_without_g711_is_no_common_codec_and_hung_up() {
        let (port, uas) = fake_uas("BYE", |req| {
            if req.starts_with("INVITE") {
                vec![reply(req, "200 OK", SDP, &answer(9, "18 101"))]
            } else if req.starts_with("BYE") {
                vec![reply(req, "200 OK", "", "")]
            } else {
                vec![]
            }
        })
        .await;
        let err = place_call(&target(port), Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(matches!(err, TalkdownError::NoCommonCodec), "{err}");
        assert_eq!(methods(&uas.await.unwrap()), ["INVITE", "ACK", "BYE"]);
    }

    #[test]
    fn answer_codec_follows_the_answer_order_and_media_level_c_line() {
        let ip: IpAddr = "10.0.0.9".parse().unwrap();
        let pick = |pts: &str| parse_answer(&answer(4000, pts), ip).map(|(_, c)| c).ok();
        assert_eq!(pick("8 0"), Some(Codec::Pcma));
        assert_eq!(pick("0 8"), Some(Codec::Pcmu));
        assert_eq!(pick("101 8"), Some(Codec::Pcma));
        assert_eq!(pick("18"), None);

        let sdp = "v=0\r\nc=IN IP4 10.0.0.1\r\nm=video 5000 RTP/AVP 96\r\nc=IN IP4 10.0.0.2\r\n\
                   m=audio 35002 RTP/AVP 0\r\nc=IN IP4 10.0.0.3\r\n";
        let (dest, _) = parse_answer(sdp, ip).unwrap();
        assert_eq!(dest, "10.0.0.3:35002".parse().unwrap());
        let no_c = "v=0\r\nm=audio 35002 RTP/AVP 0\r\n";
        assert_eq!(
            parse_answer(no_c, ip).unwrap().0,
            "10.0.0.9:35002".parse().unwrap()
        );
        let rejected = "v=0\r\nc=IN IP4 10.0.0.1\r\nm=audio 0 RTP/AVP 0\r\n";
        assert!(matches!(
            parse_answer(rejected, ip),
            Err(TalkdownError::NoCommonCodec)
        ));
    }
}
