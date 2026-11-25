//! Chromecast: the Frame's live stream, repackaged as HLS and served on the LAN.
//!
//! A Cast receiver can't consume what the app itself uses: the mirror is an fMP4 stream over
//! the loopback WebSocket, fed into MSE. Receivers pull media over HTTP instead, so a second
//! viewer joins the same stream through the proxy, its keyframes become segment boundaries and
//! the result is a sliding HLS playlist. The H.264 is passed through untouched; nothing decodes
//! or re-encodes it here.
//!
//! The receiver fetches over plain HTTP: a self-signed HTTPS server would be rejected, and the
//! default media receiver has no way to install a certificate. The URL carries a random path
//! token that only lives as long as the session, which is all the access control that leg can
//! have. It is documented as such.

use std::collections::VecDeque;
use std::net::{IpAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

/// The agent's `fmp4::TIMESCALE`, repeated here because the app crate can't depend on it.
const TIMESCALE: u64 = 90_000;
/// ~12 s of video kept for a receiver that falls behind; the live edge is where it starts.
const SEGMENTS: usize = 6;
/// A segment ends at the first keyframe at least this long after it began.
const MIN_SEGMENT: f64 = 1.0;
const MAX_HEADER: usize = 8 * 1024;
/// The first segment has to arrive before the receiver can be given a URL.
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// No receiver has asked for the playlist for this long: the TV was turned off or the app
/// died. Stop, so the Frame stops capturing for nobody.
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const RECONNECT: Duration = Duration::from_millis(500);

#[derive(Clone, Serialize)]
pub struct CastInfo {
    /// What the receiver loads; includes the random path token.
    pub url: String,
    /// The phone's address as the TV has to reach it, for showing and diagnosing.
    pub address: String,
}

struct Session {
    info: CastInfo,
    shared: Arc<Shared>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// The cast pipeline. At most one runs at a time; picking another TV reuses the same stream.
#[derive(Default)]
pub struct Cast {
    inner: Mutex<Option<Session>>,
}

impl Cast {
    pub fn new() -> Self {
        Self::default()
    }

    fn guard(&self) -> MutexGuard<'_, Option<Session>> {
        // A panicked task must not take the whole cast feature down with it.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Starts serving if nothing runs yet and returns the playlist URL. Waits for the first
    /// segment, so a receiver never loads an empty playlist.
    pub async fn start(
        &self,
        proxy_port: u16,
        secret: String,
        token: String,
        peer: Option<String>,
    ) -> Result<CastInfo, String> {
        if let Some(info) = self.info() {
            return Ok(info);
        }
        // A session that stopped itself (idle watchdog) is replaced, and its tasks are aborted.
        self.guard().take();

        let listener = TcpListener::bind(("0.0.0.0", 0))
            .await
            .map_err(|e| format!("can't listen on the local network: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let address = lan_ip(peer.as_deref())?;
        let path_token = random_token()?;

        let shared = Arc::new(Shared {
            token: path_token.clone(),
            playlist: Mutex::new(Playlist::default()),
            stop: AtomicBool::new(false),
            last_fetch: Mutex::new(Instant::now()),
        });

        let tasks = vec![
            tokio::spawn(serve(listener, shared.clone())),
            tokio::spawn(produce(shared.clone(), proxy_port, secret, token)),
            tokio::spawn(watchdog(shared.clone())),
        ];

        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if shared.stop.load(Ordering::Relaxed) {
                break;
            }
            if shared.guard_playlist().ready {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        if !shared.guard_playlist().ready {
            shared.stop.store(true, Ordering::Relaxed);
            for task in &tasks {
                task.abort();
            }
            return Err(
                "the headset isn't sending video yet — check that Mirroring plays on this phone, then try again"
                    .to_owned(),
            );
        }

        let info = CastInfo {
            url: format!("http://{address}:{port}/s/{path_token}/stream.m3u8"),
            address,
        };
        *self.guard() = Some(Session { info: info.clone(), shared, tasks });
        Ok(info)
    }

    pub fn stop(&self) {
        if let Some(session) = self.guard().take() {
            session.shared.stop.store(true, Ordering::Relaxed);
            // Dropping the session aborts the server and the producer.
        }
    }

    pub fn info(&self) -> Option<CastInfo> {
        let guard = self.guard();
        guard
            .as_ref()
            .filter(|session| !session.shared.stop.load(Ordering::Relaxed))
            .map(|session| session.info.clone())
    }
}

/// One HLS playlist, built in memory. There are never more than a handful of segments.
#[derive(Default)]
struct Playlist {
    init: Option<Init>,
    /// A receiver may still be fetching the previous init after a stream change.
    inits: VecDeque<(u32, Arc<[u8]>)>,
    segments: VecDeque<Segment>,
    next_index: u64,
    target: u64,
    /// A new init means the next segment has to be announced as a discontinuity.
    pending_discontinuity: bool,
    ready: bool,
}

struct Init {
    version: u32,
    data: Arc<[u8]>,
}

struct Segment {
    index: u64,
    data: Arc<[u8]>,
    duration: f64,
    init_version: u32,
    discontinuity: bool,
}

impl Playlist {
    fn set_init(&mut self, data: Vec<u8>) {
        if let Some(init) = &self.init {
            if &*init.data == data.as_slice() {
                return;
            }
        }
        if self.init.is_some() && !self.segments.is_empty() {
            self.pending_discontinuity = true;
        }
        let version = self.init.as_ref().map_or(0, |init| init.version + 1);
        let data: Arc<[u8]> = data.into();
        self.inits.push_back((version, data.clone()));
        while self.inits.len() > 2 {
            self.inits.pop_front();
        }
        self.init = Some(Init { version, data });
    }

    fn init_for(&self, version: u32) -> Option<Arc<[u8]>> {
        self.inits
            .iter()
            .rev()
            .find(|(candidate, _)| *candidate == version)
            .map(|(_, data)| data.clone())
    }

    fn push_segment(&mut self, data: Vec<u8>, duration: f64) {
        let Some(init) = &self.init else { return };
        let segment = Segment {
            index: self.next_index,
            data: data.into(),
            duration,
            init_version: init.version,
            discontinuity: std::mem::take(&mut self.pending_discontinuity),
        };
        self.next_index += 1;
        self.segments.push_back(segment);
        while self.segments.len() > SEGMENTS {
            self.segments.pop_front();
        }
        self.target = self.target.max(duration.ceil() as u64).max(2);
        self.ready = true;
    }

    fn segment_for(&self, index: u64) -> Option<Arc<[u8]>> {
        self.segments
            .iter()
            .find(|segment| segment.index == index)
            .map(|segment| segment.data.clone())
    }

    fn m3u8(&self) -> String {
        let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:7\n");
        out.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", self.target.max(2)));
        out.push_str(&format!(
            "#EXT-X-MEDIA-SEQUENCE:{}\n",
            self.segments.front().map_or(0, |segment| segment.index)
        ));
        out.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");
        let mut mapped = None;
        for segment in &self.segments {
            if segment.discontinuity && mapped.is_some() {
                out.push_str("#EXT-X-DISCONTINUITY\n");
            }
            if mapped != Some(segment.init_version) {
                out.push_str(&format!("#EXT-X-MAP:URI=\"init-{}.mp4\"\n", segment.init_version));
                mapped = Some(segment.init_version);
            }
            out.push_str(&format!("#EXTINF:{:.3},\n{}.m4s\n", segment.duration, segment.index));
        }
        out
    }
}

struct Shared {
    token: String,
    playlist: Mutex<Playlist>,
    stop: AtomicBool,
    last_fetch: Mutex<Instant>,
}

impl Shared {
    fn guard_playlist(&self) -> MutexGuard<'_, Playlist> {
        self.playlist.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn touch(&self) {
        *self.last_fetch.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }
}

/// A partial segment: fragments since the last keyframe boundary.
#[derive(Default)]
struct Part {
    data: Vec<u8>,
    duration: f64,
    started: bool,
}

/// Keeps a second viewer connected through the proxy, repackaging what it receives. Retries
/// like the app's own player: the Frame sleeps, Wi-Fi drops, the socket dies without closing.
async fn produce(shared: Arc<Shared>, proxy_port: u16, secret: String, token: String) {
    let url = format!("ws://127.0.0.1:{proxy_port}/api/stream/ws?token={token}&s={secret}");
    // The agent restarts decode times at 0 for every connection. Keep one timeline across
    // reconnects, otherwise the receiver sees the timestamps jump backwards.
    let mut offset = 0u64;
    let mut last_end = 0u64;
    let mut init: Option<Vec<u8>> = None;
    let mut part = Part::default();

    'connect: while !shared.stop.load(Ordering::Relaxed) {
        let Ok(Ok((mut socket, _))) =
            tokio::time::timeout(Duration::from_secs(5), tokio_tungstenite::connect_async(url.as_str()))
                .await
        else {
            tokio::time::sleep(RECONNECT).await;
            continue;
        };

        let mut initialized = false;
        loop {
            if shared.stop.load(Ordering::Relaxed) {
                break 'connect;
            }
            let message = match tokio::time::timeout(Duration::from_secs(5), socket.next()).await {
                Ok(Some(Ok(message))) => message,
                Ok(Some(Err(_))) | Ok(None) => break,
                // No frames while the headset sleeps; keep waiting, the watchdog decides.
                Err(_) => continue,
            };
            match message {
                Message::Binary(data) if !initialized => {
                    initialized = true;
                    let data: &[u8] = data.as_ref();
                    if init.as_deref() != Some(data) {
                        init = Some(data.to_vec());
                        shared.guard_playlist().set_init(data.to_vec());
                    }
                }
                Message::Binary(data) => {
                    let data: &[u8] = data.as_ref();
                    fragment(&shared, &mut part, &mut offset, &mut last_end, data)
                }
                Message::Close(_) => break,
                // The JSON codec header; the receiver reads the codec from the init segment.
                _ => {}
            }
        }

        // Whatever was buffered still ends at a frame boundary; publish it if it is long enough.
        if part.duration >= 0.5 {
            flush(&shared, &mut part);
        }
        part = Part::default();
        tokio::time::sleep(RECONNECT).await;
    }
}

fn fragment(shared: &Shared, part: &mut Part, offset: &mut u64, last_end: &mut u64, data: &[u8]) {
    let Some(fragment) = parse_fragment(data) else { return };

    // A new connection restarts at 0; line it up with the end of what was already sent.
    if fragment.decode_time + *offset < *last_end {
        *offset = *last_end - fragment.decode_time;
    }
    let corrected = fragment.decode_time + *offset;

    let mut patched = None;
    if corrected != fragment.decode_time {
        let mut copy = data.to_vec();
        if patch_tfdt(&mut copy, corrected) {
            patched = Some(copy);
        }
    }
    let bytes = patched.as_deref().unwrap_or(data);

    if !part.started {
        // A segment has to begin with a keyframe or the receiver can't decode it.
        if !fragment.keyframe {
            return;
        }
        part.started = true;
    } else if part.duration >= MIN_SEGMENT && fragment.keyframe {
        flush(shared, part);
        part.started = true;
    }

    part.data.extend_from_slice(bytes);
    part.duration += fragment.duration as f64 / TIMESCALE as f64;
    *last_end = corrected + u64::from(fragment.duration);
}

fn flush(shared: &Shared, part: &mut Part) {
    if part.data.is_empty() {
        return;
    }
    let data = std::mem::take(&mut part.data);
    let duration = std::mem::take(&mut part.duration);
    part.started = false;
    if duration > 0.0 {
        shared.guard_playlist().push_segment(data, duration);
    }
}

struct Fragment {
    keyframe: bool,
    decode_time: u64,
    duration: u32,
}

/// Reads the one sample a segment carries (see the agent's `fmp4::media_segment`).
fn parse_fragment(data: &[u8]) -> Option<Fragment> {
    let moof_end = moof_size(data)?;
    let (traf, traf_end) = find_child(data, 8, moof_end, b"traf")?;
    let (tfdt, tfdt_end) = find_child(data, traf, traf_end, b"tfdt")?;
    let decode_time = if data[tfdt] == 1 {
        u64::from_be_bytes(read_bytes(data, tfdt + 4, tfdt_end, 8)?.try_into().ok()?)
    } else {
        u64::from(u32::from_be_bytes(read_bytes(data, tfdt + 4, tfdt_end, 4)?.try_into().ok()?))
    };

    let (trun, trun_end) = find_child(data, traf, traf_end, b"trun")?;
    let flags = u32::from_be_bytes([0, data[trun + 1], data[trun + 2], data[trun + 3]]);
    let mut at = trun + 8;
    if flags & 0x000001 != 0 {
        at += 4; // data offset
    }
    let first_flags = if flags & 0x000004 != 0 {
        let value = read_u32(data, at, trun_end)?;
        at += 4;
        Some(value)
    } else {
        None
    };
    let duration = if flags & 0x000100 != 0 {
        let value = read_u32(data, at, trun_end)?;
        at += 4;
        value
    } else {
        0
    };
    if flags & 0x000200 != 0 {
        at += 4; // sample size
    }
    let sample_flags = first_flags.or_else(|| {
        if flags & 0x000400 != 0 {
            read_u32(data, at, trun_end)
        } else {
            None
        }
    });
    Some(Fragment {
        // Bit 16 marks a non-sync sample.
        keyframe: sample_flags.is_some_and(|value| value & 0x0001_0000 == 0),
        decode_time,
        duration,
    })
}

fn patch_tfdt(data: &mut [u8], value: u64) -> bool {
    let Some(moof_end) = moof_size(data) else { return false };
    let Some((traf, traf_end)) = find_child(data, 8, moof_end, b"traf") else { return false };
    let Some((tfdt, tfdt_end)) = find_child(data, traf, traf_end, b"tfdt") else { return false };
    if data[tfdt] == 1 {
        if tfdt + 12 > tfdt_end {
            return false;
        }
        data[tfdt + 4..tfdt + 12].copy_from_slice(&value.to_be_bytes());
    } else {
        if tfdt + 8 > tfdt_end {
            return false;
        }
        data[tfdt + 4..tfdt + 8].copy_from_slice(&(value as u32).to_be_bytes());
    }
    true
}

fn moof_size(data: &[u8]) -> Option<usize> {
    if data.len() < 8 || &data[4..8] != b"moof" {
        return None;
    }
    let size = u32::from_be_bytes(data[0..4].try_into().ok()?) as usize;
    (size >= 8 && size <= data.len()).then_some(size)
}

/// Payload range of the first `kind` box inside `[at, end)`.
fn find_child(data: &[u8], mut at: usize, end: usize, kind: &[u8; 4]) -> Option<(usize, usize)> {
    while at + 8 <= end {
        let size = u32::from_be_bytes(data[at..at + 4].try_into().ok()?) as usize;
        if size < 8 || at + size > end {
            return None;
        }
        if &data[at + 4..at + 8] == kind {
            return Some((at + 8, at + size));
        }
        at += size;
    }
    None
}

fn read_bytes(data: &[u8], at: usize, end: usize, len: usize) -> Option<&[u8]> {
    (at + len <= end && at + len <= data.len()).then(|| &data[at..at + len])
}

fn read_u32(data: &[u8], at: usize, end: usize) -> Option<u32> {
    Some(u32::from_be_bytes(read_bytes(data, at, end, 4)?.try_into().ok()?))
}

/// The phone's address on the network the Frame is on. A UDP `connect` sends nothing; it only
/// asks the routing table which local address would be used.
fn lan_ip(peer: Option<&str>) -> Result<String, String> {
    let mut targets: Vec<String> = peer.into_iter().map(str::to_owned).collect();
    // Works even when the pairing code carried no address; the default route is the LAN.
    targets.push("8.8.8.8".to_owned());
    for target in targets {
        let Ok(address) = target.parse::<IpAddr>() else { continue };
        let Ok(socket) = UdpSocket::bind(("0.0.0.0", 0)) else { continue };
        if socket.connect((address, 9)).is_err() {
            continue;
        }
        let Ok(local) = socket.local_addr() else { continue };
        if local.ip().is_ipv4() && !local.ip().is_loopback() {
            return Ok(local.ip().to_string());
        }
    }
    Err("can't work out this phone's network address — check the Wi-Fi connection".to_owned())
}

fn random_token() -> Result<String, String> {
    use std::fmt::Write;

    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 12];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "no randomness available".to_owned())?;
    let mut out = String::with_capacity(24);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

async fn watchdog(shared: Arc<Shared>) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        let idle = shared
            .last_fetch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .elapsed();
        if idle > IDLE_TIMEOUT {
            shared.stop.store(true, Ordering::Relaxed);
            return;
        }
    }
}

async fn serve(listener: TcpListener, shared: Arc<Shared>) {
    while !shared.stop.load(Ordering::Relaxed) {
        let accepted = tokio::time::timeout(Duration::from_secs(1), listener.accept()).await;
        match accepted {
            Ok(Ok((stream, _))) => {
                let shared = shared.clone();
                tokio::spawn(async move { handle(stream, shared).await });
            }
            Ok(Err(e)) => eprintln!("framemate: cast accept failed: {e}"),
            Err(_) => continue,
        }
    }
}

async fn handle(mut stream: TcpStream, shared: Arc<Shared>) {
    let mut head = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    loop {
        match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return,
            Ok(Ok(_)) => head.push(byte[0]),
        }
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
        if head.len() > MAX_HEADER {
            return;
        }
    }

    let text = String::from_utf8_lossy(&head);
    let mut request = text.lines().next().unwrap_or_default().split_whitespace();
    let method = request.next().unwrap_or_default();
    let target = request.next().unwrap_or_default();
    let path = target.split('?').next().unwrap_or_default();
    let head_only = method == "HEAD";

    if method == "OPTIONS" {
        respond(&mut stream, "204 No Content", "text/plain", &[], head_only).await;
        return;
    }
    if method != "GET" && !head_only {
        respond(&mut stream, "405 Method Not Allowed", "text/plain", &[], false).await;
        return;
    }

    let Some(rest) = path.strip_prefix("/s/") else {
        respond(&mut stream, "404 Not Found", "text/plain", &[], head_only).await;
        return;
    };
    let Some((token, file)) = rest.split_once('/') else {
        respond(&mut stream, "404 Not Found", "text/plain", &[], head_only).await;
        return;
    };
    if token != shared.token {
        respond(&mut stream, "404 Not Found", "text/plain", &[], head_only).await;
        return;
    }
    shared.touch();

    if file == "stream.m3u8" {
        let playlist = shared.guard_playlist().m3u8();
        respond(
            &mut stream,
            "200 OK",
            "application/vnd.apple.mpegurl",
            playlist.as_bytes(),
            head_only,
        )
        .await;
        return;
    }

    if let Some(body) = segment_body(&shared, file) {
        respond(&mut stream, "200 OK", "video/mp4", &body, head_only).await;
        return;
    }

    respond(&mut stream, "404 Not Found", "text/plain", &[], head_only).await;
}

fn segment_body(shared: &Shared, file: &str) -> Option<Arc<[u8]>> {
    let playlist = shared.guard_playlist();
    if let Some(version) = file
        .strip_prefix("init-")
        .and_then(|name| name.strip_suffix(".mp4"))
        .and_then(|version| version.parse::<u32>().ok())
    {
        return playlist.init_for(version);
    }
    let index = file.strip_suffix(".m4s")?.parse::<u64>().ok()?;
    playlist.segment_for(index)
}

async fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
    head_only: bool,
) {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, HEAD, OPTIONS\r\n\
         Access-Control-Allow-Headers: *\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes()).await;
    if !head_only {
        let _ = stream.write_all(body).await;
    }
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The agent's `media_segment` layout, mirrored here so the parser is tested against real
    /// bytes instead of assumptions.
    fn segment(keyframe: bool, decode_time: u64, duration: u32, sample: &[u8]) -> Vec<u8> {
        let flags: u32 = if keyframe { 0x0200_0000 } else { 0x0101_0000 };
        let trun_body = [
            1u32,
            0,
            duration,
            sample.len() as u32,
            flags,
        ];
        let trun = boxed(b"trun", &[&[0u8, 0, 7, 1][..], &trun_body.iter().flat_map(|v| v.to_be_bytes()).collect::<Vec<_>>()].concat());
        let tfdt = boxed(b"tfdt", &[&[1u8, 0, 0, 0][..], &decode_time.to_be_bytes()].concat());
        let tfhd = boxed(b"tfhd", &[&[0u8, 2, 0, 0][..], &1u32.to_be_bytes()].concat());
        let traf = boxed(b"traf", &[tfhd, tfdt, trun].concat());
        let mfhd = boxed(b"mfhd", &[&[0u8, 0, 0, 0][..], &1u32.to_be_bytes()].concat());
        let moof = boxed(b"moof", &[mfhd, traf].concat());
        [moof, boxed(b"mdat", sample)].concat()
    }

    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        [&((body.len() + 8) as u32).to_be_bytes()[..], kind, body].concat()
    }

    #[test]
    fn reads_keyframes_and_timestamps() {
        let key = parse_fragment(&segment(true, 0, 3000, &[1, 2, 3])).unwrap();
        assert!(key.keyframe);
        assert_eq!((key.decode_time, key.duration), (0, 3000));

        let after = parse_fragment(&segment(false, 90_000, 3000, &[1, 2, 3])).unwrap();
        assert!(!after.keyframe);
        assert_eq!(after.decode_time, 90_000);
    }

    #[test]
    fn patches_tfdt_in_place() {
        let mut data = segment(true, 0, 3000, &[9]);
        assert!(patch_tfdt(&mut data, 123_456));
        assert_eq!(parse_fragment(&data).unwrap().decode_time, 123_456);
        // The sample is untouched.
        assert!(data.ends_with(&[9]));
    }

    #[test]
    fn playlist_uses_map_and_media_sequence() {
        let mut playlist = Playlist::default();
        playlist.set_init(vec![1, 2, 3]);
        playlist.push_segment(vec![4], 2.0);
        playlist.push_segment(vec![5], 2.0);
        let text = playlist.m3u8();
        assert!(text.contains("#EXT-X-TARGETDURATION:2"));
        assert!(text.contains("#EXT-X-MAP:URI=\"init-0.mp4\""));
        assert!(text.contains("#EXTINF:2.000,\n0.m4s"));
        assert!(text.contains("#EXTINF:2.000,\n1.m4s"));

        // Dropping the oldest segment moves the media sequence, not the segment names.
        for _ in 0..SEGMENTS {
            playlist.push_segment(vec![6], 2.0);
        }
        assert_eq!(playlist.segments.front().unwrap().index, 2);
        assert!(playlist.m3u8().contains("#EXT-X-MEDIA-SEQUENCE:2"));
    }
}
