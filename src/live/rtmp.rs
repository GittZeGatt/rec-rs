//! RTMP / RTMPS publishing (YouTube, Twitch, any RTMP server): handshake,
//! chunk stream, AMF0 commands and FLV tags for H.264 + AAC, with a bounded
//! send queue, GOP dropping under congestion and reconnects with backoff.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use super::audio::{AAC_ASC, AudioCodec};
use super::video::avcc_record;
use super::{Formats, Output, OutputStatus, Packet};

// ---------------------------------------------------------------- AMF0

#[derive(Clone, Debug, PartialEq)]
pub enum Amf {
    Number(f64),
    Bool(bool),
    Str(String),
    Object(Vec<(String, Amf)>),
    Null,
    Undefined,
    Ecma(Vec<(String, Amf)>),
}

impl Amf {
    pub fn encode(&self, out: &mut Vec<u8>) {
        let props = |out: &mut Vec<u8>, kv: &[(String, Amf)]| {
            for (k, v) in kv {
                out.extend_from_slice(&(k.len() as u16).to_be_bytes());
                out.extend_from_slice(k.as_bytes());
                v.encode(out);
            }
            out.extend_from_slice(&[0, 0, 9]);
        };
        match self {
            Amf::Number(n) => {
                out.push(0);
                out.extend_from_slice(&n.to_be_bytes());
            }
            Amf::Bool(b) => out.extend_from_slice(&[1, *b as u8]),
            Amf::Str(s) => {
                out.push(2);
                out.extend_from_slice(&(s.len() as u16).to_be_bytes());
                out.extend_from_slice(s.as_bytes());
            }
            Amf::Object(kv) => {
                out.push(3);
                props(out, kv);
            }
            Amf::Null => out.push(5),
            Amf::Undefined => out.push(6),
            Amf::Ecma(kv) => {
                out.push(8);
                out.extend_from_slice(&(kv.len() as u32).to_be_bytes());
                props(out, kv);
            }
        }
    }

    /// Decode one value; returns it and the bytes used.
    pub fn decode(b: &[u8]) -> Option<(Amf, usize)> {
        let props = |b: &[u8]| -> Option<(Vec<(String, Amf)>, usize)> {
            let mut i = 0;
            let mut kv = vec![];
            loop {
                let n = u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]) as usize;
                i += 2;
                if n == 0 && *b.get(i)? == 9 {
                    return Some((kv, i + 1));
                }
                let k = String::from_utf8_lossy(b.get(i..i + n)?).into_owned();
                i += n;
                let (v, used) = Amf::decode(&b[i..])?;
                i += used;
                kv.push((k, v));
            }
        };
        match *b.first()? {
            0 => Some((Amf::Number(f64::from_be_bytes(b.get(1..9)?.try_into().ok()?)), 9)),
            1 => Some((Amf::Bool(*b.get(1)? != 0), 2)),
            2 => {
                let n = u16::from_be_bytes([*b.get(1)?, *b.get(2)?]) as usize;
                Some((Amf::Str(String::from_utf8_lossy(b.get(3..3 + n)?).into_owned()), 3 + n))
            }
            3 => props(&b[1..]).map(|(kv, n)| (Amf::Object(kv), 1 + n)),
            5 => Some((Amf::Null, 1)),
            6 => Some((Amf::Undefined, 1)),
            8 => props(&b[5..]).map(|(kv, n)| (Amf::Ecma(kv), 5 + n)),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Amf> {
        match self {
            Amf::Object(kv) | Amf::Ecma(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn str(&self) -> Option<&str> {
        if let Amf::Str(s) = self { Some(s) } else { None }
    }
}

fn amf_all(b: &[u8]) -> Vec<Amf> {
    let mut v = vec![];
    let mut i = 0;
    while i < b.len() {
        let Some((a, n)) = Amf::decode(&b[i..]) else { break };
        v.push(a);
        i += n;
    }
    v
}

fn obj(kv: &[(&str, Amf)]) -> Amf {
    Amf::Object(kv.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
}

fn command(values: &[Amf]) -> Vec<u8> {
    let mut out = vec![];
    for v in values {
        v.encode(&mut out);
    }
    out
}

// ---------------------------------------------------------------- URLs

#[derive(Clone, Debug, PartialEq)]
pub struct RtmpUrl {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub app: String,
    pub tc_url: String,
}

pub fn parse_url(url: &str) -> Result<RtmpUrl> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("rtmps://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("rtmp://") {
        (false, r)
    } else {
        bail!("not an rtmp:// or rtmps:// URL");
    };
    let (hostport, app) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().context("bad port")?),
        None => (hostport.to_string(), if tls { 443 } else { 1935 }),
    };
    if host.is_empty() {
        bail!("missing host");
    }
    let app = app.trim_end_matches('/').to_string();
    let scheme = if tls { "rtmps" } else { "rtmp" };
    Ok(RtmpUrl { tls, tc_url: format!("{scheme}://{hostport}/{app}"), host, port, app })
}

// ---------------------------------------------------------------- chunks

const C_CONTROL: u8 = 2;
const C_COMMAND: u8 = 3;
const C_AUDIO: u8 = 4;
const C_VIDEO: u8 = 6;
const OUT_CHUNK: usize = 4096;

/// One RTMP message split into chunks (type-0 header, then type-3).
pub fn chunk_message(csid: u8, msg_type: u8, stream_id: u32, ts: u32, payload: &[u8], chunk_size: usize) -> Vec<u8> {
    let ext = ts >= 0xFF_FFFF;
    let mut out = Vec::with_capacity(payload.len() + 16 + payload.len() / chunk_size * 5);
    out.push(csid & 0x3f);
    let t = if ext { 0xFF_FFFF } else { ts };
    out.extend_from_slice(&t.to_be_bytes()[1..]);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
    out.push(msg_type);
    out.extend_from_slice(&stream_id.to_le_bytes());
    if ext {
        out.extend_from_slice(&ts.to_be_bytes());
    }
    for (i, part) in payload.chunks(chunk_size).enumerate() {
        if i > 0 {
            out.push(0xC0 | (csid & 0x3f));
            if ext {
                out.extend_from_slice(&ts.to_be_bytes());
            }
        }
        out.extend_from_slice(part);
    }
    if payload.is_empty() {
        return out;
    }
    out
}

#[derive(Default, Clone)]
struct InState {
    ts: u32,
    len: usize,
    typ: u8,
    sid: u32,
    buf: Vec<u8>,
}

/// Incoming chunk stream parser.
pub struct ChunkReader {
    chunk_size: usize,
    states: HashMap<u32, InState>,
    data: Vec<u8>,
}

/// A complete incoming message.
#[derive(Debug, PartialEq)]
pub struct Message {
    pub typ: u8,
    pub sid: u32,
    pub payload: Vec<u8>,
}

impl Default for ChunkReader {
    fn default() -> Self {
        ChunkReader { chunk_size: 128, states: HashMap::new(), data: vec![] }
    }
}

impl ChunkReader {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Message> {
        self.data.extend_from_slice(bytes);
        let mut out = vec![];
        while let Some((used, msg)) = self.one() {
            self.data.drain(..used);
            if let Some(m) = msg {
                if m.typ == 1 && m.payload.len() >= 4 {
                    self.chunk_size = (u32::from_be_bytes(m.payload[..4].try_into().unwrap()) & 0x7fff_ffff) as usize;
                }
                out.push(m);
            }
        }
        out
    }

    /// Parse one chunk from the buffer: (bytes used, finished message).
    fn one(&mut self) -> Option<(usize, Option<Message>)> {
        let d = &self.data;
        let b0 = *d.first()?;
        let fmt = b0 >> 6;
        let (csid, mut i) = match b0 & 0x3f {
            0 => (*d.get(1)? as u32 + 64, 2),
            1 => (*d.get(1)? as u32 + 64 + *d.get(2)? as u32 * 256, 3),
            c => (c as u32, 1),
        };
        let mut st = self.states.get(&csid).cloned().unwrap_or_default();
        let hdr = [11, 7, 3, 0][fmt as usize];
        let h = d.get(i..i + hdr)?;
        let be3 = |b: &[u8]| u32::from_be_bytes([0, b[0], b[1], b[2]]);
        let mut ts_field = None;
        if fmt <= 2 {
            ts_field = Some(be3(&h[0..3]));
        }
        if fmt <= 1 {
            st.len = be3(&h[3..6]) as usize;
            st.typ = h[6];
        }
        if fmt == 0 {
            st.sid = u32::from_le_bytes(h[7..11].try_into().unwrap());
        }
        i += hdr;
        if ts_field == Some(0xFF_FFFF) {
            ts_field = Some(u32::from_be_bytes(d.get(i..i + 4)?.try_into().unwrap()));
            i += 4;
        }
        if let Some(t) = ts_field {
            st.ts = if fmt == 0 { t } else { st.ts.wrapping_add(t) };
        }
        let want = (st.len - st.buf.len()).min(self.chunk_size);
        let part = d.get(i..i + want)?;
        st.buf.extend_from_slice(part);
        i += want;
        let msg = (st.buf.len() >= st.len).then(|| Message { typ: st.typ, sid: st.sid, payload: std::mem::take(&mut st.buf) });
        self.states.insert(csid, st);
        Some((i, msg))
    }
}

// ---------------------------------------------------------------- FLV

pub fn flv_video_header(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut v = vec![0x17, 0, 0, 0, 0];
    v.extend(avcc_record(sps, pps));
    v
}

pub fn flv_video(key: bool, avcc: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(avcc.len() + 5);
    v.extend_from_slice(&[if key { 0x17 } else { 0x27 }, 1, 0, 0, 0]);
    v.extend_from_slice(avcc);
    v
}

pub fn flv_audio_header() -> Vec<u8> {
    let mut v = vec![0xAF, 0];
    v.extend_from_slice(&AAC_ASC);
    v
}

pub fn flv_audio(raw: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(raw.len() + 2);
    v.extend_from_slice(&[0xAF, 1]);
    v.extend_from_slice(raw);
    v
}

// ---------------------------------------------------------------- connection

trait Stream: Read + Write + Send {
    fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()>;
}

impl Stream for TcpStream {
    fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        TcpStream::set_read_timeout(self, d)
    }
}

impl Stream for native_tls::TlsStream<TcpStream> {
    fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        self.get_ref().set_read_timeout(d)
    }
}

struct Conn {
    s: Box<dyn Stream>,
    reader: ChunkReader,
    stream_id: u32,
    sent: u64,
}

impl Conn {
    fn open(u: &RtmpUrl, key: &str) -> Result<Conn> {
        let addr = format!("{}:{}", u.host, u.port);
        let tcp = std::net::ToSocketAddrs::to_socket_addrs(&addr)?
            .find_map(|a| TcpStream::connect_timeout(&a, Duration::from_secs(10)).ok())
            .ok_or_else(|| anyhow!("cannot connect to {addr}"))?;
        tcp.set_nodelay(true)?;
        tcp.set_write_timeout(Some(Duration::from_secs(10)))?;
        let s: Box<dyn Stream> = if u.tls {
            let mut b = native_tls::TlsConnector::builder();
            // Local test servers with self-signed certificates only.
            if std::env::var_os("REC_INSECURE_TLS").is_some() {
                b.danger_accept_invalid_certs(true).danger_accept_invalid_hostnames(true);
            }
            let tls = b.build()?;
            Box::new(tls.connect(&u.host, tcp).map_err(|e| anyhow!("TLS: {e}"))?)
        } else {
            Box::new(tcp)
        };
        let mut c = Conn { s, reader: ChunkReader::default(), stream_id: 0, sent: 0 };
        c.s.set_read_timeout(Some(Duration::from_secs(10)))?;
        c.handshake()?;
        c.publish(u, key)?;
        Ok(c)
    }

    fn handshake(&mut self) -> Result<()> {
        let mut c1 = vec![0u8; 1536];
        getrandom::fill(&mut c1[8..]).ok();
        c1[..8].fill(0);
        let mut c0c1 = vec![3u8];
        c0c1.extend_from_slice(&c1);
        self.s.write_all(&c0c1)?;
        let mut s0s1 = vec![0u8; 1537];
        self.s.read_exact(&mut s0s1).context("handshake (S0/S1)")?;
        if s0s1[0] != 3 {
            bail!("unexpected RTMP version {}", s0s1[0]);
        }
        self.s.write_all(&s0s1[1..])?; // C2 echoes S1
        let mut s2 = vec![0u8; 1536];
        self.s.read_exact(&mut s2).context("handshake (S2)")?;
        Ok(())
    }

    fn send(&mut self, csid: u8, typ: u8, sid: u32, ts: u32, payload: &[u8]) -> Result<()> {
        let bytes = chunk_message(csid, typ, sid, ts, payload, OUT_CHUNK);
        self.s.write_all(&bytes)?;
        self.sent += bytes.len() as u64;
        Ok(())
    }

    fn cmd(&mut self, sid: u32, values: &[Amf]) -> Result<()> {
        self.send(C_COMMAND, 20, sid, 0, &command(values))
    }

    /// Read messages until `pred` accepts a command (or an error arrives).
    fn wait_command(&mut self, what: &str, mut pred: impl FnMut(&[Amf]) -> bool) -> Result<Vec<Amf>> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut buf = [0u8; 8192];
        loop {
            if Instant::now() > deadline {
                bail!("timed out waiting for {what}");
            }
            let n = self.s.read(&mut buf).with_context(|| format!("waiting for {what}"))?;
            if n == 0 {
                bail!("server closed the connection while waiting for {what}");
            }
            for m in self.reader.feed(&buf[..n]) {
                if m.typ == 4 && m.payload.len() >= 6 && u16::from_be_bytes([m.payload[0], m.payload[1]]) == 6 {
                    // Ping request: answer with a pong.
                    let mut p = vec![0, 7];
                    p.extend_from_slice(&m.payload[2..6]);
                    self.send(C_CONTROL, 4, 0, 0, &p)?;
                }
                if m.typ == 20 {
                    let v = amf_all(&m.payload);
                    let name = v.first().and_then(Amf::str).unwrap_or("");
                    if name == "_error" || (name == "onStatus" && status_level(&v) == Some("error")) {
                        bail!("{what} refused: {}", status_text(&v));
                    }
                    if pred(&v) {
                        return Ok(v);
                    }
                }
            }
        }
    }

    fn publish(&mut self, u: &RtmpUrl, key: &str) -> Result<()> {
        // Larger outgoing chunks cut header overhead.
        self.send(C_CONTROL, 1, 0, 0, &(OUT_CHUNK as u32).to_be_bytes())?;
        self.cmd(
            0,
            &[
                Amf::Str("connect".into()),
                Amf::Number(1.0),
                obj(&[
                    ("app", Amf::Str(u.app.clone())),
                    ("type", Amf::Str("nonprivate".into())),
                    ("flashVer", Amf::Str("FMLE/3.0 (compatible; rec-rs)".into())),
                    ("tcUrl", Amf::Str(u.tc_url.clone())),
                ]),
            ],
        )?;
        self.wait_command("connect", |v| v.first().and_then(Amf::str) == Some("_result") && v.get(1) == Some(&Amf::Number(1.0)))?;
        self.cmd(0, &[Amf::Str("releaseStream".into()), Amf::Number(2.0), Amf::Null, Amf::Str(key.into())])?;
        self.cmd(0, &[Amf::Str("FCPublish".into()), Amf::Number(3.0), Amf::Null, Amf::Str(key.into())])?;
        self.cmd(0, &[Amf::Str("createStream".into()), Amf::Number(4.0), Amf::Null])?;
        let r = self.wait_command("createStream", |v| v.first().and_then(Amf::str) == Some("_result") && v.get(1) == Some(&Amf::Number(4.0)))?;
        self.stream_id = match r.get(3) {
            Some(Amf::Number(n)) => *n as u32,
            _ => 1,
        };
        let sid = self.stream_id;
        self.cmd(sid, &[Amf::Str("publish".into()), Amf::Number(5.0), Amf::Null, Amf::Str(key.into()), Amf::Str("live".into())])?;
        self.wait_command("publish", |v| {
            v.first().and_then(Amf::str) == Some("onStatus") && status_code(v).is_some_and(|c| c.contains("Publish.Start"))
        })?;
        Ok(())
    }

    /// Answer pings and notice a closed connection (non-blocking).
    fn poll(&mut self) -> Result<()> {
        self.s.set_read_timeout(Some(Duration::from_millis(1)))?;
        let mut buf = [0u8; 4096];
        let r = self.s.read(&mut buf);
        self.s.set_read_timeout(Some(Duration::from_secs(10)))?;
        match r {
            Ok(0) => bail!("server closed the connection"),
            Ok(n) => {
                for m in self.reader.feed(&buf[..n]) {
                    if m.typ == 4 && m.payload.len() >= 6 && u16::from_be_bytes([m.payload[0], m.payload[1]]) == 6 {
                        let mut p = vec![0, 7];
                        p.extend_from_slice(&m.payload[2..6]);
                        self.send(C_CONTROL, 4, 0, 0, &p)?;
                    }
                    if m.typ == 20 {
                        let v = amf_all(&m.payload);
                        if status_level(&v) == Some("error") {
                            bail!("server: {}", status_text(&v));
                        }
                    }
                }
                Ok(())
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

fn status_obj(v: &[Amf]) -> Option<&Amf> {
    v.iter().skip(2).find(|a| matches!(a, Amf::Object(_)))
}

fn status_level(v: &[Amf]) -> Option<&str> {
    status_obj(v)?.get("level")?.str()
}

fn status_code(v: &[Amf]) -> Option<&str> {
    status_obj(v)?.get("code")?.str()
}

fn status_text(v: &[Amf]) -> String {
    let o = status_obj(v);
    let code = o.and_then(|o| o.get("code")).and_then(Amf::str).unwrap_or("");
    let desc = o.and_then(|o| o.get("description")).and_then(Amf::str).unwrap_or("");
    format!("{code} {desc}").trim().to_string()
}

// ---------------------------------------------------------------- output

#[derive(Clone, Debug)]
struct Tag {
    typ: u8, // 8 audio, 9 video, 18 data
    ts: u32,
    body: Arc<Vec<u8>>,
}

#[derive(Default)]
struct Shared {
    queue: VecDeque<Tag>,
    formats: Option<(Vec<u8>, Vec<u8>)>,
    width_height: (usize, usize),
    state: String,
    error: Option<String>,
    live_since: Option<Instant>,
    started: Option<Instant>,
    dropped: u64,
    reconnects: u64,
    /// (time, total bytes sent) samples for the bitrate.
    samples: VecDeque<(Instant, u64)>,
    want_key: bool,
    /// Drop video until the next keyframe (after congestion or reconnect).
    skip_to_key: bool,
}

pub struct RtmpOutput {
    name: String,
    shared: Arc<(Mutex<Shared>, Condvar)>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    t0: Option<f64>,
}

/// Queued media older than this (by timestamp span) means the network is
/// not keeping up: drop video until the next keyframe.
const MAX_QUEUE_MS: u32 = 3000;

impl RtmpOutput {
    pub fn start(url: &str, key: &str) -> Result<RtmpOutput> {
        let u = parse_url(url)?;
        let name = format!("{}://{}", if u.tls { "rtmps" } else { "rtmp" }, u.host);
        let shared = Arc::new((Mutex::new(Shared { state: "connecting".into(), want_key: true, skip_to_key: true, ..Default::default() }), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (sh, st, key) = (shared.clone(), stop.clone(), key.to_string());
        let thread = std::thread::Builder::new().name("rtmp".into()).spawn(move || run(u, key, sh, st))?;
        Ok(RtmpOutput { name, shared, stop, thread: Some(thread), t0: None })
    }
}

fn run(u: RtmpUrl, key: String, shared: Arc<(Mutex<Shared>, Condvar)>, stop: Arc<AtomicBool>) {
    let (lock, cv) = &*shared;
    let mut backoff = 1u64;
    let redact = |e: &anyhow::Error| super::redact(&format!("{e:#}"), &key);
    while !stop.load(Ordering::SeqCst) {
        lock.lock().unwrap().state = "connecting".into();
        let mut conn = match Conn::open(&u, &key) {
            Ok(c) => c,
            Err(e) => {
                let mut s = lock.lock().unwrap();
                s.error = Some(redact(&e));
                s.state = format!("retrying in {backoff}s");
                drop(s);
                eprintln!("rec: stream {}: {}", u.host, redact(&e));
                sleep_unless_stopped(&stop, Duration::from_secs(backoff));
                backoff = (backoff * 2).min(30);
                lock.lock().unwrap().reconnects += 1;
                continue;
            }
        };
        {
            let mut s = lock.lock().unwrap();
            s.state = "live".into();
            s.error = None;
            s.live_since.get_or_insert_with(Instant::now);
            // Start the new connection from a keyframe.
            s.queue.retain(|t| t.typ == 8);
            s.skip_to_key = true;
            s.want_key = true;
        }
        let mut headers_sent = false;
        let result: Result<()> = (|| loop {
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            let (tags, formats, wh) = {
                let mut s = lock.lock().unwrap();
                if s.queue.is_empty() {
                    s = cv.wait_timeout(s, Duration::from_millis(50)).unwrap().0;
                }
                (s.queue.drain(..).collect::<Vec<_>>(), s.formats.clone(), s.width_height)
            };
            if !headers_sent
                && let Some((sps, pps)) = &formats
            {
                let meta = command(&[
                    Amf::Str("@setDataFrame".into()),
                    Amf::Str("onMetaData".into()),
                    Amf::Ecma(vec![
                        ("width".into(), Amf::Number(wh.0 as f64)),
                        ("height".into(), Amf::Number(wh.1 as f64)),
                        ("videocodecid".into(), Amf::Number(7.0)),
                        ("audiocodecid".into(), Amf::Number(10.0)),
                        ("audiosamplerate".into(), Amf::Number(48000.0)),
                        ("stereo".into(), Amf::Bool(true)),
                        ("encoder".into(), Amf::Str("rec-rs".into())),
                    ]),
                ]);
                let sid = conn.stream_id;
                conn.send(C_AUDIO, 18, sid, 0, &meta)?;
                conn.send(C_VIDEO, 9, sid, 0, &flv_video_header(sps, pps))?;
                conn.send(C_AUDIO, 8, sid, 0, &flv_audio_header())?;
                headers_sent = true;
            }
            if headers_sent {
                let sid = conn.stream_id;
                for t in tags {
                    let csid = if t.typ == 9 { C_VIDEO } else { C_AUDIO };
                    conn.send(csid, t.typ, sid, t.ts, &t.body)?;
                }
            }
            conn.poll()?;
            let mut s = lock.lock().unwrap();
            let now = Instant::now();
            s.samples.push_back((now, conn.sent));
            while s.samples.front().is_some_and(|(t, _)| now - *t > Duration::from_secs(3)) {
                s.samples.pop_front();
            }
            backoff = 1;
        })();
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if let Err(e) = result {
            let mut s = lock.lock().unwrap();
            s.error = Some(redact(&e));
            s.state = "reconnecting".into();
            s.reconnects += 1;
            s.samples.clear();
            drop(s);
            eprintln!("rec: stream {}: {} (reconnecting)", u.host, redact(&e));
            sleep_unless_stopped(&stop, Duration::from_secs(backoff));
            backoff = (backoff * 2).min(30);
        }
    }
    lock.lock().unwrap().state = "stopped".into();
}

pub(super) fn sleep_unless_stopped(stop: &AtomicBool, d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end && !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

impl Output for RtmpOutput {
    fn packet(&mut self, p: &Packet, f: &Formats) {
        let (lock, cv) = &*self.shared;
        let mut s = lock.lock().unwrap();
        if s.formats.is_none() && !f.sps.is_empty() {
            s.formats = Some((f.sps.clone(), f.pps.clone()));
        }
        match p {
            Packet::Video(v) => {
                if self.t0.is_none() {
                    if !v.key {
                        return;
                    }
                    self.t0 = Some(v.pts);
                    s.started = Some(Instant::now());
                }
                if s.width_height == (0, 0) {
                    s.width_height = video_size(&v.format.0);
                }
                if s.skip_to_key {
                    if !v.key {
                        s.dropped += 1;
                        return;
                    }
                    s.skip_to_key = false;
                }
                let ts = ((v.pts - self.t0.unwrap()) * 1000.0).round().max(0.0) as u32;
                // Congestion: the queue spans too much time.
                if let (Some(first), true) = (s.queue.front().map(|t| t.ts), !v.key)
                    && ts.saturating_sub(first) > MAX_QUEUE_MS
                {
                    s.queue.retain(|t| t.typ != 9);
                    s.skip_to_key = true;
                    s.dropped += 1;
                    return;
                }
                s.queue.push_back(Tag { typ: 9, ts, body: Arc::new(flv_video(v.key, &v.data)) });
            }
            Packet::Audio(a) => {
                let Some(t0) = self.t0 else { return };
                if a.codec != AudioCodec::Aac || a.pts < t0 {
                    return;
                }
                let ts = ((a.pts - t0) * 1000.0).round() as u32;
                if s.queue.len() > 2000 {
                    return;
                }
                s.queue.push_back(Tag { typ: 8, ts, body: Arc::new(flv_audio(&a.data)) });
            }
        }
        cv.notify_one();
    }

    fn take_keyframe_request(&mut self) -> bool {
        std::mem::take(&mut self.shared.0.lock().unwrap().want_key)
    }

    fn status(&self) -> OutputStatus {
        let s = self.shared.0.lock().unwrap();
        let bitrate = match (s.samples.front(), s.samples.back()) {
            (Some(a), Some(b)) if s.state == "live" && b.0 > a.0 => b.1.saturating_sub(a.1) as f64 * 8.0 / (b.0 - a.0).as_secs_f64(),
            _ => 0.0,
        };
        OutputStatus {
            name: self.name.clone(),
            state: s.state.clone(),
            live: s.state == "live",
            bitrate,
            dropped_frames: s.dropped,
            reconnects: s.reconnects,
            duration: s.live_since.map_or(0.0, |t| t.elapsed().as_secs_f64()),
            error: s.error.clone(),
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.shared.1.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for RtmpOutput {
    fn drop(&mut self) {
        Output::stop(self);
    }
}

fn video_size(fd: &objc2_core_media::CMFormatDescription) -> (usize, usize) {
    let d = unsafe { objc2_core_media::CMVideoFormatDescriptionGetDimensions(fd) };
    (d.width as usize, d.height as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amf_round_trip() {
        let v = vec![
            Amf::Str("connect".into()),
            Amf::Number(1.0),
            obj(&[("app", Amf::Str("live".into())), ("fpad", Amf::Bool(false))]),
            Amf::Null,
            Amf::Ecma(vec![("width".into(), Amf::Number(1920.0))]),
        ];
        let bytes = command(&v);
        assert_eq!(amf_all(&bytes), v);
        assert_eq!(&bytes[..3], &[2, 0, 7]);
    }

    #[test]
    fn urls() {
        let u = parse_url("rtmps://a.rtmps.youtube.com:443/live2").unwrap();
        assert_eq!((u.tls, u.host.as_str(), u.port, u.app.as_str()), (true, "a.rtmps.youtube.com", 443, "live2"));
        assert_eq!(u.tc_url, "rtmps://a.rtmps.youtube.com:443/live2");
        let u = parse_url("rtmp://localhost/live/").unwrap();
        assert_eq!((u.port, u.app.as_str(), u.tc_url.as_str()), (1935, "live", "rtmp://localhost/live"));
        assert!(parse_url("http://x/y").is_err());
    }

    #[test]
    fn chunking_round_trips_through_the_reader() {
        let payload: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
        let mut wire = vec![];
        // The reader starts at 128-byte chunks; announce 4096 first.
        wire.extend(chunk_message(2, 1, 0, 0, &4096u32.to_be_bytes(), 4096));
        wire.extend(chunk_message(6, 9, 1, 1234, &payload, 4096));
        wire.extend(chunk_message(6, 9, 1, 0x0100_0000, b"ext", 4096));
        let mut r = ChunkReader::default();
        // Feed in odd pieces like a socket would.
        let mut msgs = vec![];
        for part in wire.chunks(777) {
            msgs.extend(r.feed(part));
        }
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1], Message { typ: 9, sid: 1, payload });
        assert_eq!(msgs[2].payload, b"ext");
    }

    #[test]
    fn flv_bodies() {
        assert_eq!(flv_audio_header(), vec![0xAF, 0, 0x11, 0x90]);
        assert_eq!(&flv_video(true, &[1, 2])[..5], &[0x17, 1, 0, 0, 0]);
        assert_eq!(flv_video(false, &[9])[0], 0x27);
        let h = flv_video_header(&[0x67, 0x64, 0, 0x28], &[0x68]);
        assert_eq!(&h[..6], &[0x17, 0, 0, 0, 0, 1]);
    }
}
