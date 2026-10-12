//! WHIP (WebRTC-HTTP ingestion, RFC 9725) output.
//!
//! The offer is POSTed to the endpoint (stream key as a Bearer token), the
//! answer starts an ICE/DTLS/SRTP session driven by str0m on one UDP
//! socket, and the shared H.264 encode plus Opus audio are sent as RTP.
//! The session resource is DELETEd when the stream stops.

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::format::Codec;
use str0m::media::{Direction, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output as RtcOutput, Rtc};

use super::audio::AudioCodec;
use super::rtmp::sleep_unless_stopped;
use super::{Formats, Output, OutputStatus, Packet};

/// How long ICE + DTLS may take before the attempt counts as failed.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// A connected server that sends nothing back for this long is gone.
const SILENCE_TIMEOUT: Duration = Duration::from_secs(5);
/// Media older than this (behind the newest) is dropped instead of sent.
const MAX_QUEUE_SECS: f64 = 2.0;

enum Media {
    Video { pts: f64, hevc: bool, annexb: Vec<u8> },
    Audio { pts: f64, data: Arc<Vec<u8>> },
}

impl Media {
    fn pts(&self) -> f64 {
        match self {
            Media::Video { pts, .. } | Media::Audio { pts, .. } => *pts,
        }
    }
}

#[derive(Default)]
struct Shared {
    queue: VecDeque<Media>,
    state: String,
    live: bool,
    error: Option<String>,
    live_since: Option<Instant>,
    dropped: u64,
    reconnects: u64,
    /// (time, total bytes sent) samples for the bitrate.
    samples: VecDeque<(Instant, u64)>,
    want_key: bool,
    skip_to_key: bool,
    /// The server negotiated H.265 (None until it answers).
    hevc: Option<bool>,
}

pub struct WhipOutput {
    name: String,
    shared: Arc<(Mutex<Shared>, Condvar)>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WhipOutput {
    pub fn start(url: &str, key: &str) -> Result<WhipOutput> {
        let parsed = parse_http_url(url)?;
        let name = format!("whip://{}", parsed.host);
        let shared = Arc::new((
            Mutex::new(Shared { state: "connecting".into(), want_key: true, skip_to_key: true, ..Default::default() }),
            Condvar::new(),
        ));
        let stop = Arc::new(AtomicBool::new(false));
        let (sh, st, url, key) = (shared.clone(), stop.clone(), url.to_string(), key.to_string());
        let thread = std::thread::Builder::new().name("whip".into()).spawn(move || run(url, key, sh, st))?;
        Ok(WhipOutput { name, shared, stop, thread: Some(thread) })
    }
}

impl Output for WhipOutput {
    fn packet(&mut self, p: &Packet, f: &Formats) {
        let (lock, cv) = &*self.shared;
        let mut s = lock.lock().unwrap();
        match p {
            Packet::Video(v) => {
                if !s.live {
                    s.skip_to_key = true;
                    return;
                }
                if s.skip_to_key {
                    if !v.key {
                        s.dropped += 1;
                        return;
                    }
                    s.skip_to_key = false;
                }
                // Congestion: drop queued video and restart from a keyframe.
                if !v.key && s.queue.front().is_some_and(|m| v.pts - m.pts() > MAX_QUEUE_SECS) {
                    let before = s.queue.len();
                    s.queue.retain(|m| matches!(m, Media::Audio { .. }));
                    s.dropped += (before - s.queue.len()) as u64 + 1;
                    s.skip_to_key = true;
                    s.want_key = true;
                    return;
                }
                let mut annexb = Vec::with_capacity(v.data.len() + 64);
                if v.hevc && s.hevc == Some(false) {
                    // Not negotiated: the encoder is switching to H.264.
                    return;
                }
                if v.key && f.hevc == v.hevc {
                    for ps in &f.param_sets {
                        annexb.extend_from_slice(&[0, 0, 0, 1]);
                        annexb.extend_from_slice(ps);
                    }
                }
                annexb.extend_from_slice(&super::video::avcc_to_annexb(&v.data));
                s.queue.push_back(Media::Video { pts: v.pts, hevc: v.hevc, annexb });
            }
            Packet::Audio(a) => {
                if a.codec != AudioCodec::Opus || !s.live || s.queue.len() > 4000 {
                    return;
                }
                s.queue.push_back(Media::Audio { pts: a.pts, data: a.data.clone() });
            }
        }
        cv.notify_one();
    }

    fn status(&self) -> OutputStatus {
        let s = self.shared.0.lock().unwrap();
        let bitrate = match (s.samples.front(), s.samples.back()) {
            (Some(a), Some(b)) if s.live && b.0 > a.0 => b.1.saturating_sub(a.1) as f64 * 8.0 / (b.0 - a.0).as_secs_f64(),
            _ => 0.0,
        };
        OutputStatus {
            name: self.name.clone(),
            state: s.state.clone(),
            live: s.live,
            bitrate,
            dropped_frames: s.dropped,
            reconnects: s.reconnects,
            duration: s.live_since.map_or(0.0, |t| t.elapsed().as_secs_f64()),
            error: s.error.clone(),
        }
    }

    fn wants_opus(&self) -> bool {
        true
    }

    fn accepts_hevc(&self) -> bool {
        self.shared.0.lock().unwrap().hevc != Some(false)
    }

    fn take_keyframe_request(&mut self) -> bool {
        std::mem::take(&mut self.shared.0.lock().unwrap().want_key)
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.shared.1.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for WhipOutput {
    fn drop(&mut self) {
        self.stop();
    }
}

struct HttpUrl {
    scheme: String,
    host: String,
    port: u16,
}

fn parse_http_url(url: &str) -> Result<HttpUrl> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| anyhow!("WHIP URL needs http:// or https://"))?;
    let default_port = match scheme {
        "http" => 80,
        "https" => 443,
        _ => bail!("WHIP URL needs http:// or https://"),
    };
    let authority = rest.split(['/', '?']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (h, tail) = rest.split_once(']').ok_or_else(|| anyhow!("bad IPv6 host in WHIP URL"))?;
        (h, tail.strip_prefix(':'))
    } else {
        match authority.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    let port = port.map_or(Ok(default_port), |p| p.parse().context("WHIP URL port"))?;
    if host.is_empty() {
        bail!("WHIP URL has no host");
    }
    Ok(HttpUrl { scheme: scheme.into(), host: host.into(), port })
}

/// The session resource URL from a `Location` header (may be relative).
fn resource_url(endpoint: &HttpUrl, location: &str) -> String {
    if location.contains("://") {
        location.to_string()
    } else {
        let host = if endpoint.host.contains(':') { format!("[{}]", endpoint.host) } else { endpoint.host.clone() };
        let path = if location.starts_with('/') { location.to_string() } else { format!("/{location}") };
        format!("{}://{}:{}{}", endpoint.scheme, host, endpoint.port, path)
    }
}

fn agent() -> ureq::Agent {
    let insecure = std::env::var_os("REC_INSECURE_TLS").is_some();
    ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .disable_verification(insecure)
                .build(),
        )
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into()
}

/// POST the offer; returns the answer and the session resource URL.
fn post_offer(url: &str, key: &str, offer: &str) -> Result<(String, Option<String>)> {
    let endpoint = parse_http_url(url)?;
    let mut req = agent().post(url).header("Content-Type", "application/sdp");
    if !key.is_empty() {
        req = req.header("Authorization", &format!("Bearer {key}"));
    }
    let mut resp = req.send(offer).map_err(|e| anyhow!("WHIP request: {e}"))?;
    let status = resp.status().as_u16();
    let location = resp.headers().get("location").and_then(|v| v.to_str().ok()).map(|l| resource_url(&endpoint, l));
    let body = resp.body_mut().read_to_string().unwrap_or_default();
    match status {
        200 | 201 => Ok((body, location)),
        401 | 403 => bail!("WHIP server refused the stream key (HTTP {status})"),
        _ => bail!("WHIP server answered HTTP {status}: {}", body.lines().next().unwrap_or("").trim()),
    }
}

fn delete_resource(url: &str, key: &str) {
    let mut req = agent().delete(url);
    if !key.is_empty() {
        req = req.header("Authorization", &format!("Bearer {key}"));
    }
    let _ = req.call();
}

/// A UDP socket on the local address that routes to the WHIP server.
fn bind_socket(url: &HttpUrl) -> Result<UdpSocket> {
    let server: SocketAddr = (url.host.as_str(), url.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("cannot resolve {}", url.host))?;
    let probe = UdpSocket::bind(if server.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })?;
    probe.connect(server)?;
    let local = probe.local_addr()?.ip();
    let socket = UdpSocket::bind(SocketAddr::new(local, 0))?;
    // Keyframes go out as bursts of a few hundred packets.
    let size: libc::c_int = 4 << 20;
    unsafe {
        use std::os::fd::AsRawFd;
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &size as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
    Ok(socket)
}

struct Session {
    rtc: Rtc,
    socket: UdpSocket,
    /// Video mid with the H.264 and (if negotiated) H.265 payload types.
    video: (Mid, Pt, Option<Pt>),
    audio: (Mid, Pt),
    resource: Option<String>,
    sent: u64,
    send_errors: u64,
    written: u64,
    /// Last datagram from the server (RTCP reports, ICE consent checks).
    last_rx: Instant,
}

impl Session {
    fn open(url: &str, key: &str) -> Result<Session> {
        let parsed = parse_http_url(url)?;
        let socket = bind_socket(&parsed)?;
        let mut rtc = Rtc::builder()
            .clear_codecs()
            .enable_h264(true)
            .enable_h265(true)
            .enable_opus(true, false)
            .build(Instant::now());
        rtc.add_local_candidate(Candidate::host(socket.local_addr()?, "udp")?);
        let mut change = rtc.sdp_api();
        let vmid = change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        let amid = change.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None);
        let (offer, pending) = change.apply().ok_or_else(|| anyhow!("no SDP offer"))?;
        let offer: SdpOffer = offer;
        let (answer, resource) = post_offer(url, key, &offer.to_sdp_string())?;
        let answer = SdpAnswer::from_sdp_string(&answer).map_err(|e| anyhow!("WHIP answer: {e}"))?;
        rtc.sdp_api().accept_answer(pending, answer).map_err(|e| anyhow!("WHIP answer: {e}"))?;
        let video = (vmid, pick_pt(&mut rtc, vmid, Codec::H264)?, pick_pt(&mut rtc, vmid, Codec::H265).ok());
        let audio = (amid, pick_pt(&mut rtc, amid, Codec::Opus)?);
        Ok(Session { rtc, socket, video, audio, resource, sent: 0, send_errors: 0, written: 0, last_rx: Instant::now() })
    }

    /// Drain the engine to its next timeout, sending datagrams and
    /// collecting events.
    fn drain(&mut self, events: &mut Vec<Event>) -> Result<Instant> {
        loop {
            match self.rtc.poll_output()? {
                RtcOutput::Timeout(t) => return Ok(t),
                RtcOutput::Transmit(t) => {
                    // A full send buffer drops the datagram; RTP recovers.
                    match self.socket.send_to(&t.contents, t.destination) {
                        Ok(_) => self.sent += t.contents.len() as u64,
                        Err(e) => {
                            self.send_errors += 1;
                            if self.send_errors < 5 {
                                eprintln!("whip send: {e}");
                            }
                        }
                    }
                }
                RtcOutput::Event(e) => events.push(e),
            }
        }
    }

    /// Feed one received datagram or the timeout. A deadline already due
    /// is fed at once: the unpaced sender releases one packet per timeout.
    fn input(&mut self, buf: &mut [u8], deadline: Instant) -> Result<()> {
        let now = Instant::now();
        if deadline <= now {
            self.rtc.handle_input(Input::Timeout(now))?;
            return Ok(());
        }
        let wait = (deadline - now).min(Duration::from_millis(2));
        self.socket.set_read_timeout(Some(wait.max(Duration::from_millis(1))))?;
        let input = match self.socket.recv_from(buf) {
            Ok((n, source)) => {
                self.last_rx = Instant::now();
                Input::Receive(
                Instant::now(),
                Receive {
                    proto: Protocol::Udp,
                    source,
                    destination: self.socket.local_addr()?,
                    contents: buf[..n].try_into()?,
                },
            )
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => Input::Timeout(Instant::now()),
            Err(e) => return Err(e.into()),
        };
        self.rtc.handle_input(input)?;
        Ok(())
    }
}

/// Pick the negotiated payload type for `codec` (H.264 prefers
/// packetization mode 1, needed for frames larger than one packet).
fn pick_pt(rtc: &mut Rtc, mid: Mid, codec: Codec) -> Result<Pt> {
    let w = rtc.writer(mid).ok_or_else(|| anyhow!("WHIP server rejected the {codec:?} track"))?;
    let mut params: Vec<_> = w.payload_params().filter(|p| p.spec().codec == codec).collect();
    params.sort_by_key(|p| p.spec().format.packetization_mode != Some(1));
    params.first().map(|p| p.pt()).ok_or_else(|| anyhow!("WHIP server does not accept {codec:?}"))
}

fn run(url: String, key: String, shared: Arc<(Mutex<Shared>, Condvar)>, stop: Arc<AtomicBool>) {
    let (lock, _) = &*shared;
    let mut backoff = 1u64;
    let redact = |e: &anyhow::Error| super::redact(&format!("{e:#}"), &key);
    let host = parse_http_url(&url).map(|u| u.host).unwrap_or_default();
    while !stop.load(Ordering::SeqCst) {
        lock.lock().unwrap().state = "connecting".into();
        let result = Session::open(&url, &key).and_then(|mut sess| {
            let r = stream(&mut sess, &shared, &stop, &mut backoff);
            if let Some(res) = &sess.resource {
                delete_resource(res, &key);
            }
            r
        });
        let mut s = lock.lock().unwrap();
        s.live = false;
        s.samples.clear();
        s.queue.clear();
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let e = result.err().unwrap_or_else(|| anyhow!("connection closed"));
        s.error = Some(redact(&e));
        s.state = format!("retrying in {backoff}s");
        s.reconnects += 1;
        drop(s);
        eprintln!("rec: stream {host}: {} (reconnecting)", redact(&e));
        sleep_unless_stopped(&stop, Duration::from_secs(backoff));
        backoff = (backoff * 2).min(30);
    }
    let mut s = lock.lock().unwrap();
    s.live = false;
    s.state = "stopped".into();
}

/// Extra delay on top of the first keyframe's arrival, absorbing jitter in
/// when packets reach the hub.
const SEND_MARGIN: Duration = Duration::from_millis(40);

/// Opus frame length; the first audio packet is picked within one of the
/// first keyframe.
const OPUS_FRAME: f64 = 0.02;

#[derive(Clone, Copy)]
struct Anchor {
    /// Capture time sent at `at`.
    t0: f64,
    at: Instant,
    /// RTP time (seconds) for the first keyframe, equal to the first audio
    /// packet's.
    first_video_rtp: Option<f64>,
}

/// Start the session timeline once a keyframe is queued. Some servers (e.g.
/// MediaMTX) treat the first packet of each track as simultaneous, so the
/// first audio packet is the one nearest the keyframe (earlier ones are
/// dropped) and the keyframe takes its RTP time: only that one frame moves,
/// by less than an audio frame. Returns None while waiting for audio.
fn start_anchor(q: &mut VecDeque<Media>, now: Instant) -> Option<Anchor> {
    let Some(k) = q.iter().position(|m| matches!(m, Media::Video { .. })) else {
        // No video yet: keep only recent audio.
        if let Some(newest) = q.back().map(Media::pts) {
            q.retain(|m| m.pts() > newest - 1.0);
        }
        return None;
    };
    let tk = q[k].pts();
    let candidates = || q.iter().filter(|m| matches!(m, Media::Audio { .. })).map(Media::pts);
    let best = candidates()
        .filter(|&p| p > tk - OPUS_FRAME && p < tk + OPUS_FRAME)
        .min_by(|a, b| (a - tk).abs().total_cmp(&(b - tk).abs()));
    let newest_audio = candidates().fold(f64::MIN, f64::max);
    let newest_video = q.iter().rev().find(|m| matches!(m, Media::Video { .. })).map_or(tk, Media::pts);
    // Wait for the audio around the keyframe (it may still be in the mixer),
    // but not for silence that never comes.
    if newest_audio < tk + OPUS_FRAME && newest_video - tk < 0.5 {
        return None;
    }
    let first = best.filter(|&p| (p - tk).abs() < OPUS_FRAME);
    let cut = first.unwrap_or(tk);
    q.retain(|m| match m {
        Media::Audio { .. } => m.pts() >= cut - 1e-6,
        Media::Video { .. } => m.pts() >= tk,
    });
    Some(Anchor { t0: cut.min(tk), at: now + SEND_MARGIN, first_video_rtp: first })
}

/// Take the earliest queued item once it is due. Packets reach the hub
/// with different latencies (video later than audio), so every packet is
/// sent at `anchor.at + (pts - t0)` to keep arrival in step with capture
/// time; that time doubles as the RTCP wallclock. Returns the item with its
/// wallclock and RTP time (seconds), or when the next one is due.
fn take_due(q: &mut VecDeque<Media>, anchor: &mut Option<Anchor>, now: Instant) -> (Option<(Media, Instant, f64)>, Option<Instant>) {
    let a = match anchor {
        Some(a) => a,
        None => match start_anchor(q, now) {
            Some(a) => anchor.insert(a),
            None => return (None, Some(now + Duration::from_millis(5))),
        },
    };
    loop {
        let Some(i) = (0..q.len().min(64)).min_by(|&x, &y| q[x].pts().total_cmp(&q[y].pts())) else {
            return (None, None);
        };
        let pts = q[i].pts();
        if pts < a.t0 - 1e-6 {
            q.remove(i);
            continue;
        }
        let due = a.at + Duration::from_secs_f64((pts - a.t0).max(0.0));
        if due > now {
            return (None, Some(due));
        }
        let m = q.remove(i).unwrap();
        let rtp = match (&m, a.first_video_rtp.take()) {
            (Media::Video { .. }, Some(r)) => r,
            (Media::Video { .. }, None) => pts,
            (Media::Audio { .. }, keep) => {
                a.first_video_rtp = keep;
                pts
            }
        };
        return (Some((m, due, rtp)), None);
    }
}

/// Run one connected session until it fails or the output stops.
fn stream(sess: &mut Session, shared: &Arc<(Mutex<Shared>, Condvar)>, stop: &AtomicBool, backoff: &mut u64) -> Result<()> {
    let (lock, _) = &**shared;
    lock.lock().unwrap().hevc = Some(sess.video.2.is_some());
    let mut buf = vec![0u8; 2000];
    let mut events = Vec::new();
    let opened = Instant::now();
    let mut connected = false;
    let mut anchor: Option<Anchor> = None;
    let mut first_sent: [Option<f64>; 2] = [None; 2];
    let mut next_sample = Instant::now();
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        let timeout = sess.drain(&mut events)?;
        for e in events.drain(..) {
            match e {
                Event::Connected => {
                    connected = true;
                    *backoff = 1;
                    let mut s = lock.lock().unwrap();
                    s.live = true;
                    s.state = "live".into();
                    s.error = None;
                    s.live_since.get_or_insert_with(Instant::now);
                    s.skip_to_key = true;
                    s.want_key = true;
                }
                Event::IceConnectionStateChange(IceConnectionState::Disconnected) => bail!("connection lost"),
                Event::KeyframeRequest(_) => lock.lock().unwrap().want_key = true,
                _ => {}
            }
        }
        if connected && sess.last_rx.elapsed() > SILENCE_TIMEOUT {
            bail!("connection lost (no reply from the server for {} s)", SILENCE_TIMEOUT.as_secs());
        }
        if !connected && opened.elapsed() > CONNECT_TIMEOUT {
            bail!("WebRTC connection timed out (ICE/DTLS)");
        }
        // Send one due item per pass (each write is one mutation that must
        // be drained before the next).
        let now = Instant::now();
        let (item, next_due) = if connected { take_due(&mut lock.lock().unwrap().queue, &mut anchor, now) } else { (None, None) };
        if let Some((m, wallclock, rtp_secs)) = item {
            let pts = m.pts();
            if std::env::var_os("REC_DEBUG").is_some() && first_sent[matches!(m, Media::Audio { .. }) as usize].is_none() {
                first_sent[matches!(m, Media::Audio { .. }) as usize] = Some(pts);
                eprintln!("whip: first {} pts {pts:.4} late {:.1} ms", if matches!(m, Media::Audio { .. }) { "audio" } else { "video" }, (now - wallclock).as_secs_f64() * 1000.0);
            }
            match m {
                Media::Video { annexb, hevc, .. } => {
                    let rtp = MediaTime::new((rtp_secs * 90_000.0).round() as u64, str0m::media::Frequency::NINETY_KHZ);
                    sess.written += annexb.len() as u64;
                    let pt = if hevc { sess.video.2 } else { Some(sess.video.1) };
                    if let (Some(pt), Some(w)) = (pt, sess.rtc.writer(sess.video.0)) {
                        w.write(pt, wallclock, rtp, annexb)?;
                    }
                }
                Media::Audio { data, .. } => {
                    let rtp = MediaTime::new((rtp_secs * 48_000.0).round() as u64, str0m::media::Frequency::FORTY_EIGHT_KHZ);
                    if let Some(w) = sess.rtc.writer(sess.audio.0) {
                        w.write(sess.audio.1, wallclock, rtp, data.as_slice())?;
                    }
                }
            }
            continue;
        }
        if Instant::now() >= next_sample {
            next_sample = Instant::now() + Duration::from_millis(250);
            let mut s = lock.lock().unwrap();
            let now = Instant::now();
            s.samples.push_back((now, sess.sent));
            if std::env::var_os("REC_DEBUG").is_some() {
                eprintln!("whip: written {} sent {} send errors {}", sess.written, sess.sent, sess.send_errors);
            }
            while s.samples.front().is_some_and(|(t, _)| now - *t > Duration::from_secs(3)) {
                s.samples.pop_front();
            }
        }
        // Wait for a datagram, the engine's deadline or (briefly) new media.
        sess.input(&mut buf, next_due.map_or(timeout, |d| d.min(timeout)))?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let u = parse_http_url("http://localhost:8889/live/whip").unwrap();
        assert_eq!((u.scheme.as_str(), u.host.as_str(), u.port), ("http", "localhost", 8889));
        let u = parse_http_url("https://example.com/whip?x=1").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("example.com", 443));
        let u = parse_http_url("http://[::1]:8889/a").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("::1", 8889));
        assert!(parse_http_url("rtmp://x/y").is_err());
        assert_eq!(resource_url(&parse_http_url("http://h:1/live/whip").unwrap(), "/live/whip/abc"), "http://h:1/live/whip/abc");
        assert_eq!(resource_url(&parse_http_url("http://h:1/w").unwrap(), "https://o/r"), "https://o/r");
    }

    fn video(pts: f64) -> Media {
        Media::Video { pts, hevc: false, annexb: vec![0, 0, 0, 1, 0x65] }
    }

    fn audio(pts: f64) -> Media {
        Media::Audio { pts, data: Arc::new(vec![0xfc]) }
    }

    /// Audio reaches the queue ahead of video (encode latency). The first
    /// audio packet nearest the keyframe is kept, earlier ones dropped, the
    /// keyframe takes its RTP time and later packets keep their own.
    #[test]
    fn session_starts_aligned_and_paced() {
        let mut q: VecDeque<Media> = (0..10).map(|i| audio(9.9 + i as f64 * 0.02)).collect();
        let t = Instant::now();
        let mut anchor = None;
        // Video not there yet: nothing is sent.
        assert!(take_due(&mut q, &mut anchor, t).0.is_none());
        assert!(anchor.is_none());
        for f in 0..4 {
            q.push_back(video(10.005 + f as f64 / 60.0));
        }
        let mut sent = vec![];
        let mut now = t;
        while sent.len() < 8 {
            match take_due(&mut q, &mut anchor, now) {
                (Some((m, wall, rtp)), _) => sent.push((matches!(m, Media::Audio { .. }), m.pts(), rtp, wall)),
                (None, Some(due)) => now = due,
                (None, None) => break,
            }
        }
        let a = anchor.unwrap();
        // Audio at 9.9, 9.92, ...: 10.0 is nearest to the keyframe at 10.005.
        assert!((a.t0 - 10.0).abs() < 1e-9, "t0 {}", a.t0);
        assert!(sent.iter().all(|s| s.1 >= 10.0 - 1e-9), "earlier audio dropped");
        let first_video = sent.iter().find(|s| !s.0).unwrap();
        assert!((first_video.2 - 10.0).abs() < 1e-9, "keyframe takes the first audio packet's RTP time");
        let second_video = sent.iter().filter(|s| !s.0).nth(1).unwrap();
        assert!((second_video.2 - second_video.1).abs() < 1e-9, "later frames keep their own time");
        // Sent in time order, each at anchor + (pts - t0).
        for w in sent.windows(2) {
            assert!(w[0].1 <= w[1].1);
        }
        for s in &sent {
            let off = s.3.duration_since(a.at).as_secs_f64();
            assert!((off - (s.1 - a.t0)).abs() < 1e-6);
        }
    }

    /// Without any audio the session still starts once video has queued up.
    #[test]
    fn starts_without_audio() {
        let mut q: VecDeque<Media> = (0..40).map(|f| video(5.0 + f as f64 / 60.0)).collect();
        let mut anchor = None;
        let (item, due) = take_due(&mut q, &mut anchor, Instant::now());
        assert!(item.is_none() && anchor.is_some(), "anchored, first frame due after the margin");
        let (m, _, rtp) = take_due(&mut q, &mut anchor, due.unwrap()).0.expect("starts");
        assert!(matches!(m, Media::Video { .. }) && (rtp - 5.0).abs() < 1e-9);
    }
}
