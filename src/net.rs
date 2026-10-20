//! Network actor: TCP listener, outgoing dials with backoff, one task per connection.
//!
//! The manager task owns all peer state; connection tasks report to it over a channel.
//! Topology is a full mesh without forwarding. Duplicate connections between two peers
//! are resolved deterministically (see [`Manager::on_up`]).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio_util::codec::Framed;
use tokio_util::sync::CancellationToken;

use crate::crypto::Cipher;
use crate::protocol::{self, Clip, Message, PROTOCOL_VERSION};
use crate::transfer::Hub;

/// Frames before the handshake completes are small; don't let strangers make us
/// buffer 32 MB.
pub(crate) const HANDSHAKE_MAX_FRAME: usize = 64 * 1024;
/// Per-peer queue of outgoing clips. A peer this far behind is dropping clips.
const PEER_QUEUE: usize = 8;
const TICK: Duration = Duration::from_secs(1);
/// A tick gap this large means the machine slept (or the clock jumped).
const WAKE_GAP: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub ping_interval: Duration,
    pub peer_timeout: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub connect_timeout: Duration,
    pub handshake_timeout: Duration,
    pub debounce: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            ping_interval: Duration::from_secs(15),
            peer_timeout: Duration::from_secs(45),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(5),
            handshake_timeout: Duration::from_secs(10),
            debounce: Duration::from_millis(150),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerStatus {
    pub device_id: String,
    pub name: String,
    pub addr: String,
    /// The port it accepts connections on (file transfers dial it).
    #[serde(default)]
    pub listen_port: u16,
    pub inbound: bool,
    pub rtt_ms: Option<u32>,
    /// Unix seconds.
    pub connected_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingStatus {
    pub label: String,
    pub state: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetStatus {
    pub listen_port: u16,
    pub peers: Vec<PeerStatus>,
    pub pending: Vec<PendingStatus>,
}

#[derive(Debug, Clone)]
pub enum DiscoveryEvent {
    Found {
        device_id: String,
        name: String,
        addrs: Vec<SocketAddr>,
    },
    Lost {
        device_id: String,
    },
}

pub struct NetTask {
    pub device_id: String,
    pub device_name: String,
    pub cipher: Cipher,
    pub timing: Timing,
    pub manual_peers: Vec<String>,
    pub listener: TcpListener,
    pub outgoing: mpsc::Receiver<Clip>,
    pub incoming: mpsc::Sender<Clip>,
    pub discovery: mpsc::Receiver<DiscoveryEvent>,
    pub status: watch::Sender<NetStatus>,
    pub hub: Arc<Hub>,
    pub cancel: CancellationToken,
}

// ---------------------------------------------------------------------------------
// Connection task

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum TargetKey {
    Manual(String),
    Mdns(String),
}

enum ConnEvent {
    Up(Box<ConnUp>),
    Down {
        conn_id: u64,
        target: Option<TargetKey>,
        lived: Duration,
        error: String,
    },
    DialFailed {
        target: TargetKey,
        error: String,
    },
    Rtt {
        conn_id: u64,
        ms: u32,
    },
}

struct ConnUp {
    conn_id: u64,
    device_id: String,
    name: String,
    addr: SocketAddr,
    listen_port: u16,
    inbound: bool,
    /// device_id of the side that opened the TCP connection.
    initiator: String,
    target: Option<TargetKey>,
    tx: mpsc::Sender<Arc<Vec<u8>>>,
    cancel: CancellationToken,
}

/// Shared, immutable context for connection tasks.
struct ConnCtx {
    device_id: String,
    device_name: String,
    cipher: Cipher,
    timing: Timing,
    listen_port: u16,
    incoming: mpsc::Sender<Clip>,
    events: mpsc::Sender<ConnEvent>,
    hub: Arc<Hub>,
}

pub(crate) type Transport = Framed<TcpStream, tokio_util::codec::LengthDelimitedCodec>;

async fn send_plain(framed: &mut Transport, cipher: &Cipher, plain: &[u8]) -> Result<()> {
    let sealed = cipher.seal(plain)?;
    framed.send(Bytes::from(sealed)).await.context("send")
}

pub(crate) async fn send_msg(framed: &mut Transport, cipher: &Cipher, msg: &Message) -> Result<()> {
    send_plain(framed, cipher, &msg.encode()?).await
}

pub(crate) fn open_msg(cipher: &Cipher, frame: &[u8], addr: SocketAddr) -> Result<Message> {
    let plain = cipher.open(frame).map_err(|e| {
        tracing::warn!("{addr}: {e} — is the passphrase the same on both machines?");
        anyhow!("decryption failed (wrong passphrase?)")
    })?;
    Message::decode(&plain)
}

struct ConnSpec {
    stream: TcpStream,
    addr: SocketAddr,
    inbound: bool,
    target: Option<TargetKey>,
    conn_id: u64,
    cancel: CancellationToken,
}

async fn run_conn(ctx: Arc<ConnCtx>, spec: ConnSpec) {
    let started = Instant::now();
    let (conn_id, target, addr) = (spec.conn_id, spec.target.clone(), spec.addr);
    let error = match conn_inner(&ctx, spec).await {
        Ok(()) => "closed".to_string(),
        Err(e) => format!("{e:#}"),
    };
    tracing::debug!("connection {conn_id} to {addr} ended: {error}");
    let _ = ctx
        .events
        .send(ConnEvent::Down {
            conn_id,
            target,
            lived: started.elapsed(),
            error,
        })
        .await;
}

async fn conn_inner(ctx: &ConnCtx, spec: ConnSpec) -> Result<()> {
    let ConnSpec {
        stream,
        addr,
        inbound,
        target,
        conn_id,
        cancel,
    } = spec;
    let _ = stream.set_nodelay(true);
    let mut framed = Framed::new(stream, protocol::codec());
    framed.codec_mut().set_max_frame_length(HANDSHAKE_MAX_FRAME);

    let hello = Message::Hello {
        device_id: ctx.device_id.clone(),
        device_name: ctx.device_name.clone(),
        protocol_version: PROTOCOL_VERSION,
        listen_port: ctx.listen_port,
    };
    send_msg(&mut framed, &ctx.cipher, &hello).await?;
    let first = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        r = tokio::time::timeout(ctx.timing.handshake_timeout, framed.next()) => r,
    };
    let frame = first
        .map_err(|_| anyhow!("handshake timeout"))?
        .ok_or_else(|| anyhow!("closed during handshake"))?
        .context("read handshake")?;
    let (device_id, device_name, protocol_version, listen_port) =
        match open_msg(&ctx.cipher, &frame, addr)? {
            Message::Hello {
                device_id,
                device_name,
                protocol_version,
                listen_port,
            } => (device_id, device_name, protocol_version, listen_port),
            // A file transfer: its own connection, never a mesh peer.
            Message::TransferHello {
                device_id,
                device_name,
                protocol_version,
            } if inbound => {
                if protocol_version != PROTOCOL_VERSION || device_id == ctx.device_id {
                    bail!("transfer from {device_name} refused (protocol v{protocol_version})");
                }
                framed.codec_mut().set_max_frame_length(protocol::MAX_FRAME);
                return crate::transfer::receive(
                    framed,
                    &ctx.cipher,
                    &ctx.hub,
                    &ctx.device_name,
                    &device_name,
                    addr,
                    &cancel,
                )
                .await;
            }
            _ => bail!("expected Hello"),
        };
    if protocol_version != PROTOCOL_VERSION {
        tracing::warn!(
            "{addr} ({device_name}) speaks protocol v{protocol_version}, we speak v{PROTOCOL_VERSION}; update both"
        );
        bail!("protocol version mismatch");
    }
    if device_id == ctx.device_id {
        bail!("connected to ourselves");
    }
    framed.codec_mut().set_max_frame_length(protocol::MAX_FRAME);

    let (tx, mut rx) = mpsc::channel::<Arc<Vec<u8>>>(PEER_QUEUE);
    let initiator = if inbound {
        device_id.clone()
    } else {
        ctx.device_id.clone()
    };
    let up = ConnUp {
        conn_id,
        device_id: device_id.clone(),
        name: device_name.clone(),
        addr,
        listen_port,
        inbound,
        initiator,
        target,
        tx,
        cancel: cancel.clone(),
    };
    ctx.events
        .send(ConnEvent::Up(Box::new(up)))
        .await
        .map_err(|_| anyhow!("manager gone"))?;

    let mut last_rx = Instant::now();
    let mut ping_sent: Option<Instant> = None;
    let mut ping = tokio::time::interval(ctx.timing.ping_interval);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await; // first tick is immediate

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            frame = framed.next() => {
                let frame = frame.ok_or_else(|| anyhow!("peer closed"))?.context("read")?;
                last_rx = Instant::now();
                match open_msg(&ctx.cipher, &frame, addr)? {
                    Message::Clip(clip) => {
                        tracing::debug!("{device_name}: received {} clip", clip.kind.label());
                        if ctx.incoming.send(clip).await.is_err() {
                            return Ok(());
                        }
                    }
                    Message::Ping => send_msg(&mut framed, &ctx.cipher, &Message::Pong).await?,
                    Message::Pong => {
                        if let Some(sent) = ping_sent.take() {
                            let ms = sent.elapsed().as_millis().min(u32::MAX as u128) as u32;
                            let _ = ctx.events.try_send(ConnEvent::Rtt { conn_id, ms });
                        }
                    }
                    // Transfer messages only travel on transfer connections.
                    _ => {}
                }
            }
            Some(plain) = rx.recv() => send_plain(&mut framed, &ctx.cipher, &plain).await?,
            _ = ping.tick() => {
                if last_rx.elapsed() > ctx.timing.peer_timeout {
                    bail!("no traffic for {:?}", ctx.timing.peer_timeout);
                }
                ping_sent = Some(Instant::now());
                send_msg(&mut framed, &ctx.cipher, &Message::Ping).await?;
            }
        }
    }
}

async fn dial(
    ctx: Arc<ConnCtx>,
    target: TargetKey,
    addrs: Vec<SocketAddr>,
    conn_id: u64,
    cancel: CancellationToken,
) {
    let fail = |error: String| ConnEvent::DialFailed {
        target: target.clone(),
        error,
    };
    let addrs = match &target {
        TargetKey::Manual(hostport) => match tokio::net::lookup_host(hostport.as_str()).await {
            Ok(a) => a.collect::<Vec<_>>(),
            Err(e) => {
                let _ = ctx.events.send(fail(format!("resolve: {e}"))).await;
                return;
            }
        },
        TargetKey::Mdns(_) => addrs,
    };
    let mut last_err = "no addresses".to_string();
    for addr in addrs {
        let attempt = tokio::time::timeout(ctx.timing.connect_timeout, TcpStream::connect(addr));
        let res = tokio::select! {
            _ = cancel.cancelled() => return,
            r = attempt => r,
        };
        match res {
            Ok(Ok(stream)) => {
                let spec = ConnSpec {
                    stream,
                    addr,
                    inbound: false,
                    target: Some(target.clone()),
                    conn_id,
                    cancel,
                };
                run_conn(ctx, spec).await;
                return;
            }
            Ok(Err(e)) => last_err = format!("{addr}: {e}"),
            Err(_) => last_err = format!("{addr}: connect timeout"),
        }
    }
    let _ = ctx.events.send(fail(last_err)).await;
}

// ---------------------------------------------------------------------------------
// Manager

struct Peer {
    conn_id: u64,
    name: String,
    addr: SocketAddr,
    listen_port: u16,
    inbound: bool,
    initiator: String,
    tx: mpsc::Sender<Arc<Vec<u8>>>,
    cancel: CancellationToken,
    rtt_ms: Option<u32>,
    connected_at: u64,
}

struct Target {
    label: String,
    addrs: Vec<SocketAddr>,
    known_id: Option<String>,
    active: bool,
    backoff: Duration,
    next_attempt: Instant,
    last_error: Option<String>,
}

struct Manager {
    ctx: Arc<ConnCtx>,
    timing: Timing,
    listen_port: u16,
    peers: HashMap<String, Peer>,
    targets: HashMap<TargetKey, Target>,
    tasks: JoinSet<()>,
    next_conn_id: u64,
    cancel: CancellationToken,
    status: watch::Sender<NetStatus>,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Manager {
    fn new_target(&self, label: String, addrs: Vec<SocketAddr>) -> Target {
        Target {
            label,
            addrs,
            known_id: None,
            active: false,
            backoff: self.timing.backoff_min,
            next_attempt: Instant::now(),
            last_error: None,
        }
    }

    fn conn_id(&mut self) -> u64 {
        self.next_conn_id += 1;
        self.next_conn_id
    }

    fn on_accept(&mut self, stream: TcpStream, addr: SocketAddr) {
        let spec = ConnSpec {
            stream,
            addr,
            inbound: true,
            target: None,
            conn_id: self.conn_id(),
            cancel: self.cancel.child_token(),
        };
        self.tasks.spawn(run_conn(self.ctx.clone(), spec));
    }

    /// Two peers may end up with two connections (both dialed, or manual + mDNS).
    /// Both sides keep the one opened by the peer with the smaller device_id; if both
    /// were opened by the same side, the newer one wins (the old one is likely dead).
    fn on_up(&mut self, up: ConnUp) {
        if let Some(key) = &up.target
            && let Some(t) = self.targets.get_mut(key)
        {
            t.known_id = Some(up.device_id.clone());
            t.last_error = None;
        }
        if let Some(existing) = self.peers.get(&up.device_id) {
            let preferred = self.ctx.device_id.as_str().min(up.device_id.as_str());
            let new_pref = up.initiator == preferred;
            let old_pref = existing.initiator == preferred;
            let keep_new = new_pref || !old_pref;
            if !keep_new {
                tracing::debug!("duplicate connection to {} dropped", up.name);
                up.cancel.cancel();
                return;
            }
            existing.cancel.cancel();
        }
        tracing::info!(
            "connected to {} ({}, {})",
            up.name,
            up.addr,
            if up.inbound { "inbound" } else { "outbound" }
        );
        self.peers.insert(
            up.device_id,
            Peer {
                conn_id: up.conn_id,
                name: up.name,
                addr: up.addr,
                listen_port: up.listen_port,
                inbound: up.inbound,
                initiator: up.initiator,
                tx: up.tx,
                cancel: up.cancel,
                rtt_ms: None,
                connected_at: unix_now(),
            },
        );
    }

    fn on_down(&mut self, conn_id: u64, target: Option<TargetKey>, lived: Duration, error: String) {
        let gone = self
            .peers
            .iter()
            .find(|(_, p)| p.conn_id == conn_id)
            .map(|(id, _)| id.clone());
        if let Some(id) = gone
            && let Some(p) = self.peers.remove(&id)
        {
            tracing::info!("disconnected from {} ({}): {error}", p.name, p.addr);
        }
        if let Some(key) = target {
            // A connection that stayed up a while resets the backoff.
            let reset = lived > Duration::from_secs(30);
            self.schedule_retry(&key, reset, error);
        }
    }

    fn schedule_retry(&mut self, key: &TargetKey, reset: bool, error: String) {
        let (min, max) = (self.timing.backoff_min, self.timing.backoff_max);
        if let Some(t) = self.targets.get_mut(key) {
            t.active = false;
            if reset {
                t.backoff = min;
            }
            t.next_attempt = Instant::now() + t.backoff;
            t.backoff = (t.backoff * 2).min(max);
            t.last_error = Some(error);
        }
    }

    fn on_discovery(&mut self, ev: DiscoveryEvent) {
        match ev {
            DiscoveryEvent::Found {
                device_id,
                name,
                addrs,
            } => {
                if device_id == self.ctx.device_id {
                    return;
                }
                // Initiator rule: only the smaller device_id dials, the other waits.
                if self.ctx.device_id >= device_id {
                    tracing::debug!("discovered {name}; waiting for it to connect to us");
                    return;
                }
                let key = TargetKey::Mdns(device_id.clone());
                if let Some(t) = self.targets.get_mut(&key) {
                    t.addrs = addrs;
                } else {
                    tracing::info!("discovered {name} at {addrs:?}");
                    let mut t = self.new_target(name, addrs);
                    t.known_id = Some(device_id);
                    self.targets.insert(key, t);
                }
            }
            DiscoveryEvent::Lost { device_id } => {
                self.targets.remove(&TargetKey::Mdns(device_id));
            }
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let mut to_dial = Vec::new();
        for (key, t) in &mut self.targets {
            let connected = t
                .known_id
                .as_ref()
                .is_some_and(|id| self.peers.contains_key(id));
            if !t.active && !connected && now >= t.next_attempt {
                t.active = true;
                to_dial.push((key.clone(), t.addrs.clone()));
            }
        }
        for (key, addrs) in to_dial {
            let id = self.conn_id();
            let cancel = self.cancel.child_token();
            self.tasks
                .spawn(dial(self.ctx.clone(), key, addrs, id, cancel));
        }
    }

    /// After sleep, TCP connections are usually dead but won't notice for a while:
    /// drop everything and redial immediately.
    fn on_wake(&mut self) {
        tracing::info!("wake from sleep detected; reconnecting to all peers");
        for p in self.peers.values() {
            p.cancel.cancel();
        }
        let min = self.timing.backoff_min;
        for t in self.targets.values_mut() {
            t.backoff = min;
            t.next_attempt = Instant::now();
        }
    }

    fn broadcast(&mut self, clip: Clip) {
        let kind = clip.kind.label();
        let plain = match Message::Clip(clip).encode() {
            Ok(p) => Arc::new(p),
            Err(e) => {
                tracing::warn!("encode clip: {e:#}");
                return;
            }
        };
        let mut sent = 0;
        for p in self.peers.values() {
            match p.tx.try_send(plain.clone()) {
                Ok(()) => sent += 1,
                Err(_) => tracing::warn!("{} is not keeping up; clip dropped for it", p.name),
            }
        }
        tracing::info!("sending {kind} to {sent} peer(s)");
    }

    fn publish(&self) {
        let mut peers: Vec<PeerStatus> = self
            .peers
            .iter()
            .map(|(id, p)| PeerStatus {
                device_id: id.clone(),
                name: p.name.clone(),
                addr: p.addr.to_string(),
                listen_port: p.listen_port,
                inbound: p.inbound,
                rtt_ms: p.rtt_ms,
                connected_at: p.connected_at,
            })
            .collect();
        peers.sort_by(|a, b| a.name.cmp(&b.name));
        let now = Instant::now();
        let mut pending: Vec<PendingStatus> = self
            .targets
            .values()
            .filter(|t| {
                !t.known_id
                    .as_ref()
                    .is_some_and(|id| self.peers.contains_key(id))
            })
            .map(|t| {
                let state = if t.active {
                    "connecting…".to_string()
                } else {
                    let secs = t.next_attempt.saturating_duration_since(now).as_secs();
                    match &t.last_error {
                        Some(e) => format!("retry in {secs}s ({e})"),
                        None => format!("retry in {secs}s"),
                    }
                };
                PendingStatus {
                    label: t.label.clone(),
                    state,
                }
            })
            .collect();
        pending.sort_by(|a, b| a.label.cmp(&b.label));
        let status = NetStatus {
            listen_port: self.listen_port,
            peers,
            pending,
        };
        self.status.send_if_modified(|s| {
            if *s == status {
                false
            } else {
                *s = status;
                true
            }
        });
    }
}

pub async fn run(task: NetTask) {
    let NetTask {
        device_id,
        device_name,
        cipher,
        timing,
        manual_peers,
        listener,
        mut outgoing,
        incoming,
        mut discovery,
        status,
        hub,
        cancel,
    } = task;
    let listen_port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let (events_tx, mut events) = mpsc::channel(64);
    let ctx = Arc::new(ConnCtx {
        device_id,
        device_name,
        cipher,
        timing,
        listen_port,
        incoming,
        events: events_tx,
        hub,
    });
    let mut m = Manager {
        ctx,
        timing,
        listen_port,
        peers: HashMap::new(),
        targets: HashMap::new(),
        tasks: JoinSet::new(),
        next_conn_id: 0,
        cancel: cancel.clone(),
        status,
    };
    for p in manual_peers {
        let p = p.trim().to_string();
        if !p.is_empty() {
            let t = m.new_target(p.clone(), Vec::new());
            m.targets.insert(TargetKey::Manual(p), t);
        }
    }
    tracing::info!("listening on port {listen_port}");

    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_wall = SystemTime::now();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, addr)) => m.on_accept(stream, addr),
                Err(e) => {
                    tracing::warn!("accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Some(ev) = events.recv() => match ev {
                ConnEvent::Up(up) => m.on_up(*up),
                ConnEvent::Down { conn_id, target, lived, error } => m.on_down(conn_id, target, lived, error),
                ConnEvent::DialFailed { target, error } => {
                    tracing::debug!("dial {target:?} failed: {error}");
                    m.schedule_retry(&target, false, error);
                }
                ConnEvent::Rtt { conn_id, ms } => {
                    if let Some(p) = m.peers.values_mut().find(|p| p.conn_id == conn_id) {
                        p.rtt_ms = Some(ms);
                    }
                }
            },
            Some(clip) = outgoing.recv() => m.broadcast(clip),
            Some(ev) = discovery.recv() => m.on_discovery(ev),
            _ = tick.tick() => {
                let wall = SystemTime::now();
                if wall.duration_since(last_wall).unwrap_or_default() > WAKE_GAP {
                    m.on_wake();
                }
                last_wall = wall;
                m.tick();
            }
            Some(_) = m.tasks.join_next(), if !m.tasks.is_empty() => {}
        }
        m.publish();
    }

    // Child tokens are cancelled with ours; wait for connections to close cleanly.
    while m.tasks.join_next().await.is_some() {}
    m.status.send_replace(NetStatus::default());
}
