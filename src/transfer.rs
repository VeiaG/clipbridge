//! File and folder transfers. Each transfer runs on its own TCP connection, apart from
//! the clipboard mesh, so a big file never holds up clipboard frames or pings.
//!
//! The sender dials the receiver's listen port, reads its `Hello`, and answers with
//! `TransferHello` + `Offer`; the receiver's connection task then hands the stream to
//! [`receive`] instead of joining the mesh. Data flows as sealed `Chunk` frames written
//! straight to disk, so memory stays flat whatever the size. Received items land in a
//! hidden partial folder and are moved into place only once everything arrived.
//!
//! [`Hub`] holds the state the windows show (progress, pending questions) and outlives
//! node restarts.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures_util::{FutureExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_util::codec::Framed;
use tokio_util::sync::CancellationToken;

use crate::crypto::Cipher;
use crate::net::{self, Transport};
use crate::protocol::{self, Entry, Message, PROTOCOL_VERSION};

/// Plaintext bytes per `Chunk` frame.
pub const CHUNK: usize = 256 * 1024;
/// How long an offer waits for the receiving user.
pub const ASK_TIMEOUT: Duration = Duration::from_secs(120);
/// Silence on a running transfer this long means the other side is gone.
const IO_TIMEOUT: Duration = Duration::from_secs(60);
const OFFER_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ENTRIES: usize = 100_000;
const MAX_PATH_BYTES: usize = 1024;
const MAX_NAME_BYTES: usize = 255;
/// Finished transfers kept for the windows to show.
const KEEP_FINISHED: usize = 20;
/// A window that polled this recently counts as open.
const WATCH_WINDOW: Duration = Duration::from_secs(3);
/// OS clutter not worth sending.
const JUNK: &[&str] = &[".DS_Store", "Thumbs.db", "desktop.ini"];
const PARTIAL_PREFIX: &str = ".clipbridge-partial-";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    Connecting,
    /// Sender: the offer is out, waiting for the other side to accept.
    Waiting,
    /// Receiver: waiting for this user to accept or decline.
    Asking,
    Running,
    Done,
    /// With who/why, e.g. "MacBook declined".
    Declined(String),
    Failed(String),
    Cancelled,
}

impl TransferState {
    pub fn finished(&self) -> bool {
        matches!(
            self,
            Self::Done | Self::Declined(_) | Self::Failed(_) | Self::Cancelled
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferInfo {
    pub id: u64,
    pub outgoing: bool,
    /// Name of the other device.
    pub peer: String,
    /// "report.pdf", or a short list of the top-level names.
    pub label: String,
    pub files: u64,
    pub total_bytes: u64,
    pub done_bytes: u64,
    pub state: TransferState,
    /// Receiver: the saved item (or the folder, for several items).
    pub saved_to: Option<String>,
    /// Sender: symlinks inside folders that were left out.
    pub skipped: u32,
}

/// Which window is polling, so we know whether it is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Watcher {
    Send,
    Incoming,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiveSettings {
    pub enabled: bool,
    pub ask: bool,
    pub dir: PathBuf,
}

impl ReceiveSettings {
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self {
            enabled: cfg.receive_files,
            ask: cfg.ask_before_receive,
            dir: cfg.receive_dir(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum HubEvent {
    /// An offer is waiting for the user.
    Offer,
    Finished(TransferInfo),
}

struct Slot {
    info: TransferInfo,
    cancel: CancellationToken,
    decision: Option<oneshot::Sender<bool>>,
}

struct HubState {
    next_id: u64,
    slots: Vec<Slot>,
    settings: ReceiveSettings,
    watched: [Option<Instant>; 2],
}

pub struct Hub {
    state: Mutex<HubState>,
    events: mpsc::UnboundedSender<HubEvent>,
}

impl Hub {
    pub fn new(settings: ReceiveSettings) -> (Arc<Self>, mpsc::UnboundedReceiver<HubEvent>) {
        let (events, rx) = mpsc::unbounded_channel();
        let hub = Self {
            state: Mutex::new(HubState {
                next_id: 0,
                slots: Vec::new(),
                settings,
                watched: [None; 2],
            }),
            events,
        };
        (Arc::new(hub), rx)
    }

    fn lock(&self) -> MutexGuard<'_, HubState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn settings(&self) -> ReceiveSettings {
        self.lock().settings.clone()
    }

    pub fn set_settings(&self, settings: ReceiveSettings) {
        self.lock().settings = settings;
    }

    fn add(&self, mut info: TransferInfo, cancel: CancellationToken) -> u64 {
        let mut s = self.lock();
        s.next_id += 1;
        info.id = s.next_id;
        s.slots.push(Slot {
            info,
            cancel,
            decision: None,
        });
        // Keep every running transfer and the most recent finished ones.
        let finished = s.slots.iter().filter(|x| x.info.state.finished()).count();
        let mut drop = finished.saturating_sub(KEEP_FINISHED);
        s.slots.retain(|x| {
            if drop > 0 && x.info.state.finished() {
                drop -= 1;
                false
            } else {
                true
            }
        });
        s.next_id
    }

    fn update(&self, id: u64, f: impl FnOnce(&mut TransferInfo)) {
        if let Some(slot) = self.lock().slots.iter_mut().find(|x| x.info.id == id) {
            f(&mut slot.info);
        }
    }

    fn finish(&self, id: u64, state: TransferState) {
        let info = {
            let mut s = self.lock();
            let Some(slot) = s.slots.iter_mut().find(|x| x.info.id == id) else {
                return;
            };
            slot.info.state = state;
            slot.decision = None;
            slot.info.clone()
        };
        let _ = self.events.send(HubEvent::Finished(info));
    }

    pub fn list(&self) -> Vec<TransferInfo> {
        self.lock().slots.iter().map(|x| x.info.clone()).collect()
    }

    /// Any transfer not finished yet.
    pub fn active(&self) -> bool {
        self.lock().slots.iter().any(|x| !x.info.state.finished())
    }

    pub fn cancel(&self, id: u64) {
        if let Some(slot) = self.lock().slots.iter().find(|x| x.info.id == id) {
            slot.cancel.cancel();
        }
    }

    /// The user's answer to an offer.
    pub fn decide(&self, id: u64, accept: bool) {
        if let Some(slot) = self.lock().slots.iter_mut().find(|x| x.info.id == id)
            && let Some(tx) = slot.decision.take()
        {
            let _ = tx.send(accept);
        }
    }

    pub fn clear_finished(&self, outgoing: bool) {
        self.lock()
            .slots
            .retain(|x| x.info.outgoing != outgoing || !x.info.state.finished());
    }

    pub fn watched(&self, w: Watcher) {
        self.lock().watched[w as usize] = Some(Instant::now());
    }

    pub fn is_watched(&self, w: Watcher) -> bool {
        self.lock().watched[w as usize].is_some_and(|t| t.elapsed() < WATCH_WINDOW)
    }
}

/// Why a transfer ended without success.
enum End {
    Declined(String),
    Cancelled,
    Failed(String),
}

impl End {
    fn state(self) -> TransferState {
        match self {
            End::Declined(r) => TransferState::Declined(r),
            End::Cancelled => TransferState::Cancelled,
            End::Failed(r) => TransferState::Failed(r),
        }
    }
}

fn fail(e: impl std::fmt::Display) -> End {
    End::Failed(e.to_string())
}

async fn send(framed: &mut Transport, cipher: &Cipher, msg: &Message) -> Result<(), End> {
    match tokio::time::timeout(IO_TIMEOUT, net::send_msg(framed, cipher, msg)).await {
        Ok(r) => r.map_err(|e| End::Failed(format!("connection lost ({e:#})"))),
        Err(_) => Err(fail("the other device stopped responding")),
    }
}

async fn recv(
    framed: &mut Transport,
    cipher: &Cipher,
    addr: SocketAddr,
    wait: Duration,
    cancel: &CancellationToken,
) -> Result<Message, End> {
    let next = tokio::select! {
        _ = cancel.cancelled() => return Err(End::Cancelled),
        r = tokio::time::timeout(wait, framed.next()) => r,
    };
    match next {
        Err(_) => Err(fail("the other device stopped responding")),
        Ok(None) => Err(fail("connection closed")),
        Ok(Some(Err(e))) => Err(End::Failed(format!("connection lost ({e})"))),
        Ok(Some(Ok(frame))) => {
            net::open_msg(cipher, &frame, addr).map_err(|e| fail(format!("{e:#}")))
        }
    }
}

/// Tells the other side we stopped, without waiting long.
async fn say_cancel(framed: &mut Transport, cipher: &Cipher, reason: String) {
    let msg = Message::Cancel(reason);
    let _ = tokio::time::timeout(Duration::from_secs(2), net::send_msg(framed, cipher, &msg)).await;
}

/// "report.pdf", "a.txt, photos", or "a.txt, b.txt and 4 more".
fn label_for(top: &[String]) -> String {
    match top {
        [] => String::new(),
        few if few.len() <= 3 => few.join(", "),
        many => format!("{}, {} and {} more", many[0], many[1], many.len() - 2),
    }
}

/// `name` if free in `dir`, else `name (1)`, `name (2)`… (before the extension, for
/// files).
fn unique_name(name: &str, is_dir: bool, taken: impl Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && !is_dir => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|c| !taken(c))
        .expect("unbounded")
}

// ---------------------------------------------------------------------------------
// Sending

pub struct Identity {
    pub device_id: String,
    pub device_name: String,
    pub cipher: Cipher,
    pub connect_timeout: Duration,
    pub handshake_timeout: Duration,
}

pub struct SendTarget {
    pub device_id: String,
    pub name: String,
    pub addr: SocketAddr,
}

/// What the sender walks: entries in wire order and, for files, where to read them.
struct Collected {
    entries: Vec<Entry>,
    sources: Vec<Option<PathBuf>>,
    top: Vec<String>,
    skipped: u32,
}

fn collect(paths: &[PathBuf]) -> std::io::Result<Collected> {
    fn walk(dir: &Path, rel: &str, c: &mut Collected) -> std::io::Result<()> {
        let mut items: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
        items.sort_by_key(|e| e.file_name());
        for item in items {
            let name = item.file_name().to_string_lossy().into_owned();
            if JUNK.contains(&name.as_str()) {
                continue;
            }
            let meta = std::fs::symlink_metadata(item.path())?;
            let path = format!("{rel}/{name}");
            if meta.file_type().is_symlink() {
                c.skipped += 1;
            } else if meta.is_dir() {
                c.entries.push(Entry {
                    path: path.clone(),
                    size: 0,
                    dir: true,
                });
                c.sources.push(None);
                walk(&item.path(), &path, c)?;
            } else {
                c.entries.push(Entry {
                    path,
                    size: meta.len(),
                    dir: false,
                });
                c.sources.push(Some(item.path()));
            }
            if c.entries.len() > MAX_ENTRIES {
                return Err(std::io::Error::other("too many files (100 000 at most)"));
            }
        }
        Ok(())
    }

    let mut c = Collected {
        entries: Vec::new(),
        sources: Vec::new(),
        top: Vec::new(),
        skipped: 0,
    };
    let mut taken = HashSet::new();
    for p in paths {
        let Some(name) = p.file_name() else {
            return Err(std::io::Error::other(format!(
                "can't send {} (pick files or folders, not a whole drive)",
                p.display()
            )));
        };
        // Followed at the top level: the user picked it on purpose.
        let meta = std::fs::metadata(p)
            .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", p.display())))?;
        // Two dropped items with the same name: rename the second one.
        let name = unique_name(&name.to_string_lossy(), meta.is_dir(), |n| {
            taken.contains(&n.to_lowercase())
        });
        taken.insert(name.to_lowercase());
        c.top.push(name.clone());
        if meta.is_dir() {
            c.entries.push(Entry {
                path: name.clone(),
                size: 0,
                dir: true,
            });
            c.sources.push(None);
            walk(p, &name, &mut c)
                .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", p.display())))?;
        } else {
            c.entries.push(Entry {
                path: name,
                size: meta.len(),
                dir: false,
            });
            c.sources.push(Some(p.clone()));
        }
    }
    Ok(c)
}

/// Starts sending `paths` to one device; returns the transfer id at once.
pub fn start_send(
    hub: &Arc<Hub>,
    me: Arc<Identity>,
    target: SendTarget,
    paths: Vec<PathBuf>,
) -> u64 {
    let names: Vec<String> = paths
        .iter()
        .map(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string())
        })
        .collect();
    let cancel = CancellationToken::new();
    let id = hub.add(
        TransferInfo {
            id: 0,
            outgoing: true,
            peer: target.name.clone(),
            label: label_for(&names),
            files: 0,
            total_bytes: 0,
            done_bytes: 0,
            state: TransferState::Connecting,
            saved_to: None,
            skipped: 0,
        },
        cancel.clone(),
    );
    let hub = hub.clone();
    tokio::spawn(async move {
        let state = match send_inner(&hub, id, &me, &target, paths, &cancel).await {
            Ok(()) => TransferState::Done,
            Err(end) => end.state(),
        };
        match &state {
            TransferState::Done => tracing::info!("transfer {id} delivered to {}", target.name),
            other => tracing::info!("transfer {id} to {} ended: {other:?}", target.name),
        }
        hub.finish(id, state);
    });
    id
}

async fn send_inner(
    hub: &Hub,
    id: u64,
    me: &Identity,
    target: &SendTarget,
    paths: Vec<PathBuf>,
    cancel: &CancellationToken,
) -> Result<(), End> {
    let c = tokio::task::spawn_blocking(move || collect(&paths))
        .await
        .map_err(fail)?
        .map_err(fail)?;
    let total: u64 = c.entries.iter().map(|e| e.size).sum();
    let files = c.entries.iter().filter(|e| !e.dir).count() as u64;
    hub.update(id, |i| {
        i.total_bytes = total;
        i.files = files;
        i.skipped = c.skipped;
        i.label = label_for(&c.top);
    });
    tracing::info!(
        "sending {} ({files} files, {total} bytes) to {} at {}",
        label_for(&c.top),
        target.name,
        target.addr
    );

    let connect = tokio::time::timeout(me.connect_timeout, TcpStream::connect(target.addr));
    let stream = tokio::select! {
        _ = cancel.cancelled() => return Err(End::Cancelled),
        r = connect => match r {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return Err(End::Failed(format!("can't connect ({e})"))),
            Err(_) => return Err(fail("can't connect (timed out)")),
        },
    };
    let mut framed = Framed::new(stream, protocol::codec());
    framed
        .codec_mut()
        .set_max_frame_length(net::HANDSHAKE_MAX_FRAME);
    let cipher = &me.cipher;
    let addr = target.addr;

    match recv(&mut framed, cipher, addr, me.handshake_timeout, cancel).await? {
        Message::Hello {
            device_id,
            protocol_version,
            ..
        } => {
            if protocol_version != PROTOCOL_VERSION {
                return Err(fail(format!(
                    "{} runs a different ClipBridge version; update both",
                    target.name
                )));
            }
            if device_id != target.device_id {
                return Err(fail("a different device answered at that address"));
            }
        }
        _ => return Err(fail("unexpected reply")),
    }
    framed.codec_mut().set_max_frame_length(protocol::MAX_FRAME);
    let hello = Message::TransferHello {
        device_id: me.device_id.clone(),
        device_name: me.device_name.clone(),
        protocol_version: PROTOCOL_VERSION,
    };
    send(&mut framed, cipher, &hello).await?;
    send(&mut framed, cipher, &Message::Offer(c.entries.clone())).await?;
    hub.update(id, |i| i.state = TransferState::Waiting);

    // The receiver gives up after ASK_TIMEOUT and tells us; allow some slack.
    let answer = recv(&mut framed, cipher, addr, ASK_TIMEOUT + IO_TIMEOUT, cancel).await;
    match answer {
        Ok(Message::Accept) => {}
        Ok(Message::Decline(reason)) => return Err(End::Declined(reason)),
        Ok(Message::Cancel(reason)) => return Err(End::Failed(reason)),
        Ok(_) => return Err(fail("unexpected reply")),
        Err(End::Cancelled) => {
            say_cancel(
                &mut framed,
                cipher,
                format!("cancelled on {}", me.device_name),
            )
            .await;
            return Err(End::Cancelled);
        }
        Err(e) => return Err(e),
    }
    hub.update(id, |i| i.state = TransferState::Running);

    let result = stream_files(hub, id, &mut framed, cipher, addr, &c, cancel).await;
    if let Err(End::Cancelled) = result {
        say_cancel(
            &mut framed,
            cipher,
            format!("cancelled on {}", me.device_name),
        )
        .await;
    }
    result
}

async fn stream_files(
    hub: &Hub,
    id: u64,
    framed: &mut Transport,
    cipher: &Cipher,
    addr: SocketAddr,
    c: &Collected,
    cancel: &CancellationToken,
) -> Result<(), End> {
    let mut buf = vec![0u8; CHUNK];
    for (entry, source) in c.entries.iter().zip(&c.sources) {
        let Some(source) = source else { continue };
        if entry.size == 0 {
            continue;
        }
        let mut file = tokio::fs::File::open(source)
            .await
            .map_err(|e| End::Failed(format!("{}: {e}", source.display())))?;
        let mut left = entry.size;
        while left > 0 {
            if cancel.is_cancelled() {
                return Err(End::Cancelled);
            }
            // The receiver only talks mid-stream to cancel; check without waiting.
            if let Some(Some(frame)) = framed.next().now_or_never() {
                let msg = frame
                    .map_err(fail)
                    .and_then(|f| net::open_msg(cipher, &f, addr).map_err(fail))?;
                return Err(match msg {
                    Message::Cancel(reason) => End::Failed(reason),
                    _ => fail("unexpected message"),
                });
            }
            let want = left.min(CHUNK as u64) as usize;
            let n = file
                .read(&mut buf[..want])
                .await
                .map_err(|e| End::Failed(format!("{}: {e}", source.display())))?;
            if n == 0 {
                return Err(End::Failed(format!(
                    "{} got shorter while sending",
                    source.display()
                )));
            }
            send(framed, cipher, &Message::Chunk(buf[..n].to_vec())).await?;
            left -= n as u64;
            hub.update(id, |i| i.done_bytes += n as u64);
        }
    }
    send(framed, cipher, &Message::Done).await?;
    match recv(framed, cipher, addr, IO_TIMEOUT, cancel).await? {
        Message::Received => Ok(()),
        Message::Cancel(reason) => Err(End::Failed(reason)),
        _ => Err(fail("unexpected reply")),
    }
}

// ---------------------------------------------------------------------------------
// Receiving

fn invalid_char(c: char) -> bool {
    // Not allowed in Windows names. Backslash too: it would be a separator there.
    c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\')
}

fn reserved_on_windows(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit())
}

/// Makes one name from a peer safe to create on Windows and macOS.
fn sanitize_component(c: &str) -> Result<String, String> {
    if c.is_empty() || c == "." || c == ".." {
        return Err(format!("bad name {c:?}"));
    }
    let mut s: String = c
        .chars()
        .map(|ch| if invalid_char(ch) { '_' } else { ch })
        .collect();
    // Windows drops trailing dots and spaces, which would merge names.
    while s.ends_with(['.', ' ']) {
        s.pop();
    }
    if s.is_empty() {
        s.push('_');
    }
    if reserved_on_windows(&s) {
        s.insert(0, '_');
    }
    if s.len() > MAX_NAME_BYTES {
        return Err(format!("name too long: {s:.40}…"));
    }
    Ok(s)
}

/// Turns a peer-supplied `/`-separated path into a relative path that can't leave the
/// folder it is joined to.
pub fn sanitize_path(path: &str) -> Result<PathBuf, String> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return Err("bad path length".into());
    }
    let mut out = PathBuf::new();
    for comp in path.split('/') {
        out.push(sanitize_component(comp)?);
    }
    Ok(out)
}

/// Validates an offer; returns the safe relative path of every entry and the
/// top-level names in order.
fn plan(entries: &[Entry]) -> Result<(Vec<PathBuf>, Vec<String>), String> {
    if entries.is_empty() {
        return Err("empty offer".into());
    }
    if entries.len() > MAX_ENTRIES {
        return Err("too many files".into());
    }
    let mut seen = HashSet::new();
    let mut top = Vec::new();
    let mut paths = Vec::with_capacity(entries.len());
    for e in entries {
        let p = sanitize_path(&e.path)?;
        // Default file systems on both platforms ignore case.
        if !seen.insert(p.to_string_lossy().to_lowercase()) {
            return Err(format!("duplicate path {:?}", e.path));
        }
        let first = p
            .components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_default();
        if !top.contains(&first) {
            top.push(first);
        }
        paths.push(p);
    }
    Ok((paths, top))
}

/// Removes partial folders left behind by a crash.
pub fn clean_partials(dir: &Path) {
    let Ok(items) = std::fs::read_dir(dir) else {
        return;
    };
    for item in items.flatten() {
        if item
            .file_name()
            .to_string_lossy()
            .starts_with(PARTIAL_PREFIX)
        {
            let _ = std::fs::remove_dir_all(item.path());
        }
    }
}

/// Handles an incoming transfer connection after its `TransferHello`.
pub async fn receive(
    mut framed: Transport,
    cipher: &Cipher,
    hub: &Arc<Hub>,
    me: &str,
    peer: &str,
    addr: SocketAddr,
    stop: &CancellationToken,
) -> anyhow::Result<()> {
    let entries = match recv(&mut framed, cipher, addr, OFFER_TIMEOUT, stop).await {
        Ok(Message::Offer(e)) => e,
        Ok(_) => anyhow::bail!("expected an offer"),
        Err(End::Failed(e)) => anyhow::bail!("{e}"),
        Err(_) => return Ok(()),
    };
    let settings = hub.settings();
    if !settings.enabled {
        tracing::info!("declined files from {peer}: receiving is turned off");
        let msg = Message::Decline(format!("receiving files is turned off on {me}"));
        let _ = send(&mut framed, cipher, &msg).await;
        return Ok(());
    }
    let (paths, top) = match plan(&entries) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("rejected an offer from {peer}: {e}");
            let msg = Message::Decline(format!("{me} can't accept these files ({e})"));
            let _ = send(&mut framed, cipher, &msg).await;
            return Ok(());
        }
    };

    let cancel = stop.child_token();
    let id = hub.add(
        TransferInfo {
            id: 0,
            outgoing: false,
            peer: peer.to_string(),
            label: label_for(&top),
            files: entries.iter().filter(|e| !e.dir).count() as u64,
            total_bytes: entries.iter().map(|e| e.size).sum(),
            done_bytes: 0,
            state: if settings.ask {
                TransferState::Asking
            } else {
                TransferState::Running
            },
            saved_to: None,
            skipped: 0,
        },
        cancel.clone(),
    );
    tracing::info!("incoming transfer {id} from {peer}: {}", label_for(&top));

    let partial = settings
        .dir
        .join(format!("{PARTIAL_PREFIX}{}", uuid::Uuid::new_v4().simple()));
    let job = Job {
        hub,
        id,
        me,
        peer,
        cipher,
        addr,
        cancel: &cancel,
    };
    let state = match job
        .run(&mut framed, &settings, &entries, &paths, &top, &partial)
        .await
    {
        Ok(()) => TransferState::Done,
        Err(end) => {
            let _ = tokio::fs::remove_dir_all(&partial).await;
            if let End::Cancelled = end {
                say_cancel(&mut framed, cipher, format!("cancelled on {me}")).await;
            }
            end.state()
        }
    };
    tracing::info!("incoming transfer {id} from {peer} ended: {state:?}");
    hub.finish(id, state);
    Ok(())
}

struct Job<'a> {
    hub: &'a Hub,
    id: u64,
    me: &'a str,
    peer: &'a str,
    cipher: &'a Cipher,
    addr: SocketAddr,
    cancel: &'a CancellationToken,
}

impl Job<'_> {
    async fn ask(&self, framed: &mut Transport) -> Result<(), End> {
        let (tx, rx) = oneshot::channel();
        {
            let mut s = self.hub.lock();
            if let Some(slot) = s.slots.iter_mut().find(|x| x.info.id == self.id) {
                slot.decision = Some(tx);
            }
        }
        let _ = self.hub.events.send(HubEvent::Offer);
        let decline = |r: String| Message::Decline(r);
        tokio::select! {
            _ = self.cancel.cancelled() => {
                let _ = send(framed, self.cipher, &decline(format!("{} declined", self.me))).await;
                Err(End::Declined("declined".into()))
            }
            // Anything from the sender now means it gave up (or the link died).
            _ = framed.next() => Err(fail(format!("{} cancelled", self.peer))),
            r = tokio::time::timeout(ASK_TIMEOUT, rx) => match r {
                Ok(Ok(true)) => Ok(()),
                Ok(_) => {
                    let _ = send(framed, self.cipher, &decline(format!("{} declined", self.me))).await;
                    Err(End::Declined("declined".into()))
                }
                Err(_) => {
                    let msg = decline(format!("no answer on {}", self.me));
                    let _ = send(framed, self.cipher, &msg).await;
                    Err(End::Declined("expired without an answer".into()))
                }
            },
        }
    }

    async fn run(
        &self,
        framed: &mut Transport,
        settings: &ReceiveSettings,
        entries: &[Entry],
        paths: &[PathBuf],
        top: &[String],
        partial: &Path,
    ) -> Result<(), End> {
        if settings.ask {
            self.ask(framed).await?;
            self.hub
                .update(self.id, |i| i.state = TransferState::Running);
        }
        let io = |e: std::io::Error| End::Failed(format!("can't save: {e}"));
        tokio::fs::create_dir_all(partial).await.map_err(io)?;
        send(framed, self.cipher, &Message::Accept).await?;

        for (entry, rel) in entries.iter().zip(paths) {
            let dest = partial.join(rel);
            if entry.dir {
                tokio::fs::create_dir_all(&dest).await.map_err(io)?;
                continue;
            }
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(io)?;
            }
            let mut file = tokio::fs::File::create(&dest).await.map_err(io)?;
            let mut left = entry.size;
            while left > 0 {
                let msg = recv(framed, self.cipher, self.addr, IO_TIMEOUT, self.cancel).await?;
                let data = match msg {
                    Message::Chunk(d) => d,
                    Message::Cancel(reason) => return Err(End::Failed(reason)),
                    _ => return Err(fail("unexpected message")),
                };
                if data.len() as u64 > left {
                    return Err(fail("received more data than offered"));
                }
                file.write_all(&data).await.map_err(io)?;
                left -= data.len() as u64;
                self.hub
                    .update(self.id, |i| i.done_bytes += data.len() as u64);
            }
            file.flush().await.map_err(io)?;
        }
        match recv(framed, self.cipher, self.addr, IO_TIMEOUT, self.cancel).await? {
            Message::Done => {}
            Message::Cancel(reason) => return Err(End::Failed(reason)),
            _ => return Err(fail("unexpected message")),
        }

        // Everything arrived: move it into place without overwriting anything.
        let mut saved = Vec::new();
        for name in top {
            let from = partial.join(name);
            let is_dir = tokio::fs::metadata(&from).await.map_err(io)?.is_dir();
            let final_name = unique_name(name, is_dir, |n| settings.dir.join(n).exists());
            let to = settings.dir.join(&final_name);
            tokio::fs::rename(&from, &to).await.map_err(io)?;
            saved.push(to);
        }
        let _ = tokio::fs::remove_dir_all(partial).await;
        let saved_to = match saved.as_slice() {
            [one] => one.clone(),
            _ => settings.dir.clone(),
        };
        self.hub.update(self.id, |i| {
            i.saved_to = Some(saved_to.display().to_string());
        });
        // The files are safe now; a lost confirmation only affects the sender's view.
        let _ = send(framed, self.cipher, &Message::Received).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_cannot_escape() {
        for bad in [
            "",
            "..",
            "a/../b",
            "/etc/passwd",
            "a//b",
            "a/",
            ".",
            "a/./b",
        ] {
            assert!(sanitize_path(bad).is_err(), "{bad:?} should be rejected");
        }
        // Separators and drive letters of the other OS become plain characters.
        let p = sanitize_path("C:\\Windows\\system32").unwrap();
        assert_eq!(p.components().count(), 1);
        let p = sanitize_path("..\\..\\x").unwrap();
        assert_eq!(p, PathBuf::from(".._.._x"));
        assert_eq!(
            sanitize_path("dir/sub/f.txt").unwrap(),
            Path::new("dir").join("sub").join("f.txt")
        );
    }

    #[test]
    fn names_are_made_windows_safe() {
        assert_eq!(sanitize_component("a:b?.txt").unwrap(), "a_b_.txt");
        assert_eq!(sanitize_component("CON").unwrap(), "_CON");
        assert_eq!(sanitize_component("nul.txt").unwrap(), "_nul.txt");
        assert_eq!(sanitize_component("com1").unwrap(), "_com1");
        assert_eq!(sanitize_component("console").unwrap(), "console");
        assert_eq!(sanitize_component("trail. ").unwrap(), "trail");
        assert_eq!(sanitize_component("...").unwrap(), "_");
        assert_eq!(sanitize_component("tab\there").unwrap(), "tab_here");
        assert_eq!(sanitize_component("привіт.txt").unwrap(), "привіт.txt");
        assert!(sanitize_component(&"x".repeat(300)).is_err());
    }

    #[test]
    fn plan_rejects_duplicates_and_lists_top_level() {
        let e = |p: &str, dir| Entry {
            path: p.into(),
            size: 0,
            dir,
        };
        let (paths, top) = plan(&[e("d", true), e("d/x", false), e("f.txt", false)]).unwrap();
        assert_eq!(paths.len(), 3);
        assert_eq!(top, ["d", "f.txt"]);
        assert!(plan(&[e("a", false), e("A", false)]).is_err());
        assert!(plan(&[e("a:b", false), e("a_b", false)]).is_err());
        assert!(plan(&[]).is_err());
    }

    #[test]
    fn unique_names_keep_extension() {
        let taken = ["a.txt", "a (1).txt", "dir"];
        let t = |n: &str| taken.contains(&n);
        assert_eq!(unique_name("b.txt", false, t), "b.txt");
        assert_eq!(unique_name("a.txt", false, t), "a (2).txt");
        assert_eq!(unique_name("dir", true, t), "dir (1)");
        assert_eq!(
            unique_name(".bashrc", false, |n| n == ".bashrc"),
            ".bashrc (1)"
        );
    }

    #[test]
    fn labels_name_what_is_sent() {
        let v = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(label_for(&v(&["a.pdf"])), "a.pdf");
        assert_eq!(label_for(&v(&["a", "b", "c"])), "a, b, c");
        assert_eq!(label_for(&v(&["a", "b", "c", "d"])), "a, b and 2 more");
    }

    #[test]
    fn hub_keeps_recent_finished_only() {
        let (hub, _rx) = Hub::new(ReceiveSettings {
            enabled: true,
            ask: false,
            dir: PathBuf::from("x"),
        });
        let info = |outgoing| TransferInfo {
            id: 0,
            outgoing,
            peer: "p".into(),
            label: "l".into(),
            files: 1,
            total_bytes: 1,
            done_bytes: 0,
            state: TransferState::Running,
            saved_to: None,
            skipped: 0,
        };
        let running = hub.add(info(true), CancellationToken::new());
        for _ in 0..KEEP_FINISHED + 5 {
            let id = hub.add(info(false), CancellationToken::new());
            hub.finish(id, TransferState::Done);
        }
        let list = hub.list();
        assert!(list.iter().any(|i| i.id == running));
        assert!(list.len() <= KEEP_FINISHED + 2);
        assert!(hub.active());
        hub.clear_finished(false);
        assert_eq!(hub.list().len(), 1);
    }
}
