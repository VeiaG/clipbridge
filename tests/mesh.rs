//! In-process multi-node tests over loopback TCP with mock clipboards.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::OnceLock;
use std::time::Duration;

use clipbridge::clipboard::mock::MockClipboard;
use clipbridge::clipboard::{ClipboardHandle, RawClip};
use clipbridge::config::Config;
use clipbridge::crypto::Cipher;
use clipbridge::net::Timing;
use clipbridge::node::{self, NodeHandle, NodeParams};
use tokio::sync::watch;

fn cipher(pass: &str) -> Cipher {
    static A: OnceLock<Cipher> = OnceLock::new();
    static B: OnceLock<Cipher> = OnceLock::new();
    let cell = if pass == "right" { &A } else { &B };
    cell.get_or_init(|| Cipher::from_passphrase(pass).unwrap())
        .clone()
}

fn fast_timing() -> Timing {
    Timing {
        ping_interval: Duration::from_millis(500),
        peer_timeout: Duration::from_secs(3),
        backoff_min: Duration::from_millis(100),
        backoff_max: Duration::from_secs(1),
        connect_timeout: Duration::from_secs(2),
        handshake_timeout: Duration::from_secs(2),
        debounce: Duration::from_millis(40),
    }
}

struct TestNode {
    mock: MockClipboard,
    clipboard: ClipboardHandle,
    changes: watch::Receiver<u64>,
    config: Config,
    pass: &'static str,
    paused: watch::Sender<bool>,
    handle: Option<NodeHandle>,
}

impl TestNode {
    async fn start(name: &str, port: u16, peers: &[SocketAddr], pass: &'static str) -> Self {
        let (mock, changes) = MockClipboard::new();
        let clipboard = ClipboardHandle::spawn(Box::new(mock.clone())).unwrap();
        let config = Config {
            device_name: name.into(),
            passphrase: pass.into(),
            port,
            manual_peers: peers.iter().map(|a| a.to_string()).collect(),
            discovery: false,
            ..Config::default()
        };
        let (paused, _) = watch::channel(false);
        let mut node = Self {
            mock,
            clipboard,
            changes,
            config,
            pass,
            paused,
            handle: None,
        };
        node.boot().await;
        node
    }

    async fn boot(&mut self) {
        let params = NodeParams {
            timing: fast_timing(),
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            ..NodeParams::new(
                self.config.clone(),
                cipher(self.pass),
                self.paused.subscribe(),
            )
        };
        let handle = node::start(params, self.clipboard.clone(), self.changes.clone())
            .await
            .unwrap();
        // Pin the port so a restart comes back on the same address.
        self.config.port = handle.local_addr.port();
        self.handle = Some(handle);
    }

    async fn stop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.shutdown().await;
        }
    }

    fn addr(&self) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.config.port)
    }

    fn peer_count(&self) -> usize {
        self.handle
            .as_ref()
            .map(|h| h.net_status.borrow().peers.len())
            .unwrap_or(0)
    }
}

async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..100 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for: {what}");
}

fn text(s: &str) -> Option<RawClip> {
    Some(RawClip::Text(s.into()))
}

#[tokio::test]
async fn text_propagates_without_echo() {
    let a = TestNode::start("a", 0, &[], "right").await;
    let b = TestNode::start("b", 0, &[a.addr()], "right").await;
    let c = TestNode::start("c", 0, &[a.addr(), b.addr()], "right").await;
    eventually("full mesh", || {
        a.peer_count() == 2 && b.peer_count() == 2 && c.peer_count() == 2
    })
    .await;

    a.mock.copy(RawClip::Text("hello from a".into()));
    eventually("b and c receive", || {
        b.mock.content() == text("hello from a") && c.mock.content() == text("hello from a")
    })
    .await;

    // Give any echo time to bounce around.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        a.mock.writes().len(),
        0,
        "origin must not get its own clip back"
    );
    assert_eq!(b.mock.writes().len(), 1);
    assert_eq!(c.mock.writes().len(), 1);

    b.mock.copy(RawClip::Text("now from b".into()));
    eventually("a and c receive", || {
        a.mock.content() == text("now from b") && c.mock.content() == text("now from b")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(a.mock.writes().len(), 1);
    assert_eq!(b.mock.writes().len(), 1);
    assert_eq!(c.mock.writes().len(), 2);
}

#[tokio::test]
async fn image_propagates_as_png() {
    let a = TestNode::start("a", 0, &[], "right").await;
    let b = TestNode::start("b", 0, &[a.addr()], "right").await;
    eventually("connected", || a.peer_count() == 1 && b.peer_count() == 1).await;

    let (w, h) = (120, 80);
    let rgba: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 256) as u8).collect();
    let img = RawClip::Image {
        width: w,
        height: h,
        rgba,
    };
    a.mock.copy(img.clone());
    eventually("b receives image", || b.mock.content() == Some(img.clone())).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(a.mock.writes().len(), 0);
    assert_eq!(b.mock.writes().len(), 1);
}

#[tokio::test]
async fn wrong_passphrase_is_rejected() {
    let a = TestNode::start("a", 0, &[], "right").await;
    let b = TestNode::start("b", 0, &[a.addr()], "wrong").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(a.peer_count(), 0);
    assert_eq!(b.peer_count(), 0);

    b.mock.copy(RawClip::Text("secret".into()));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(a.mock.content(), None);
}

#[tokio::test]
async fn reconnects_after_restart() {
    let mut a = TestNode::start("a", 0, &[], "right").await;
    let b = TestNode::start("b", 0, &[a.addr()], "right").await;
    eventually("connected", || a.peer_count() == 1 && b.peer_count() == 1).await;

    a.stop().await;
    eventually("b notices", || b.peer_count() == 0).await;
    a.boot().await;
    eventually("reconnected", || a.peer_count() == 1 && b.peer_count() == 1).await;

    a.mock.copy(RawClip::Text("after restart".into()));
    eventually("b receives", || b.mock.content() == text("after restart")).await;
}

#[tokio::test]
async fn paused_node_neither_sends_nor_applies() {
    let a = TestNode::start("a", 0, &[], "right").await;
    let b = TestNode::start("b", 0, &[a.addr()], "right").await;
    eventually("connected", || a.peer_count() == 1 && b.peer_count() == 1).await;

    b.paused.send_replace(true);
    a.mock.copy(RawClip::Text("while paused".into()));
    b.mock.copy(RawClip::Text("b local".into()));
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(b.mock.writes().len(), 0);
    assert_eq!(a.mock.content(), text("while paused"));

    b.paused.send_replace(false);
    a.mock.copy(RawClip::Text("resumed".into()));
    eventually("b receives after resume", || {
        b.mock.content() == text("resumed")
    })
    .await;
}

#[tokio::test]
async fn oversized_text_is_not_sent() {
    let a = TestNode::start("a", 0, &[], "right").await;
    let b = TestNode::start("b", 0, &[a.addr()], "right").await;
    eventually("connected", || a.peer_count() == 1 && b.peer_count() == 1).await;

    a.mock.copy(RawClip::Text(
        "x".repeat(clipbridge::protocol::MAX_TEXT_BYTES + 1),
    ));
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(b.mock.writes().len(), 0);

    a.mock.copy(RawClip::Text("small".into()));
    eventually("small text arrives", || b.mock.content() == text("small")).await;
}

/// Needs working multicast on the host, so it is not run in CI.
/// Run with: cargo test --test mesh -- --ignored
#[tokio::test]
#[ignore]
async fn mdns_discovery_connects() {
    async fn start(name: &str) -> (MockClipboard, NodeHandle, watch::Sender<bool>) {
        let (mock, changes) = MockClipboard::new();
        let clipboard = ClipboardHandle::spawn(Box::new(mock.clone())).unwrap();
        let config = Config {
            device_name: name.into(),
            passphrase: "right".into(),
            port: 0,
            ..Config::default()
        };
        let (paused_tx, paused) = watch::channel(false);
        let params = NodeParams {
            timing: fast_timing(),
            ..NodeParams::new(config, cipher("right"), paused)
        };
        let handle = node::start(params, clipboard, changes).await.unwrap();
        (mock, handle, paused_tx)
    }
    let (a_mock, a, _pa) = start("mdns-a").await;
    let (b_mock, b, _pb) = start("mdns-b").await;
    let connected =
        || a.net_status.borrow().peers.len() == 1 && b.net_status.borrow().peers.len() == 1;
    for _ in 0..200 {
        if connected() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(connected(), "peers did not find each other via mDNS");
    a_mock.copy(RawClip::Text("via mdns".into()));
    eventually("b receives", || b_mock.content() == text("via mdns")).await;
}
