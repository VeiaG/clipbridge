//! In-process multi-node tests over loopback TCP with mock clipboards.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use clipbridge::clipboard::mock::MockClipboard;
use clipbridge::clipboard::{ClipboardHandle, RawClip};
use clipbridge::config::Config;
use clipbridge::crypto::Cipher;
use clipbridge::net::Timing;
use clipbridge::node::{self, NodeHandle, NodeParams};
use clipbridge::transfer::{TransferInfo, TransferState};
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
            receive_dir: temp_dir("recv").display().to_string(),
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

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("clipbridge-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

impl TestNode {
    fn hub(&self) -> &clipbridge::transfer::Hub {
        &self.handle.as_ref().unwrap().hub
    }

    fn transfer(&self, outgoing: bool) -> Option<TransferInfo> {
        self.hub()
            .list()
            .into_iter()
            .rfind(|t| t.outgoing == outgoing)
    }

    fn send(&self, paths: &[&Path], to: &TestNode) -> u64 {
        let paths = paths.iter().map(|p| p.to_path_buf()).collect();
        self.handle
            .as_ref()
            .unwrap()
            .send_files(paths, std::slice::from_ref(&to.config.device_id))
            .unwrap()[0]
    }

    fn recv_dir(&self) -> PathBuf {
        PathBuf::from(&self.config.receive_dir)
    }
}

/// Every file under `dir`, relative, with its contents.
fn tree(dir: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            let rel = p
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if p.is_dir() {
                out.push((format!("{rel}/"), Vec::new()));
                walk(base, &p, out);
            } else {
                out.push((rel, std::fs::read(&p).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
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

#[tokio::test]
async fn rich_text_propagates_with_plain_alternative() {
    let a = TestNode::start("a", 0, &[], "right").await;
    let b = TestNode::start("b", 0, &[a.addr()], "right").await;
    eventually("connected", || a.peer_count() == 1 && b.peer_count() == 1).await;

    let rich = RawClip::Html {
        html: "<p>Hello <b>world</b></p>".into(),
        text: "Hello world".into(),
    };
    a.mock.copy(rich.clone());
    eventually("b receives rich text", || {
        b.mock.content() == Some(rich.clone())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(a.mock.writes().len(), 0);
    assert_eq!(b.mock.writes().len(), 1);
}

async fn pair(ask: bool) -> (TestNode, TestNode) {
    let a = TestNode::start("a", 0, &[], "right").await;
    let mut b = TestNode::start("b", 0, &[a.addr()], "right").await;
    if ask {
        b.config.ask_before_receive = true;
        b.stop().await;
        b.boot().await;
    }
    eventually("connected", || a.peer_count() == 1 && b.peer_count() == 1).await;
    (a, b)
}

#[tokio::test]
async fn files_and_folders_transfer() {
    let (a, b) = pair(false).await;
    let src = temp_dir("src");
    let big: Vec<u8> = (0..700_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(src.join("report.txt"), &big).unwrap();
    let proj = src.join("proj");
    std::fs::create_dir_all(proj.join("sub")).unwrap();
    std::fs::create_dir_all(proj.join("empty")).unwrap();
    std::fs::write(proj.join("sub").join("x.bin"), b"hello").unwrap();
    std::fs::write(proj.join("zero"), b"").unwrap();
    std::fs::write(proj.join(".DS_Store"), b"junk").unwrap();

    a.send(&[&src.join("report.txt"), &proj], &b);
    eventually("delivered", || {
        a.transfer(true)
            .is_some_and(|t| t.state == TransferState::Done)
    })
    .await;
    let got = b.transfer(false).unwrap();
    assert_eq!(got.state, TransferState::Done);
    assert_eq!(got.files, 3);
    assert_eq!(got.done_bytes, big.len() as u64 + 5);

    let names: Vec<String> = tree(&b.recv_dir()).into_iter().map(|(n, _)| n).collect();
    assert_eq!(
        names,
        [
            "proj/",
            "proj/empty/",
            "proj/sub/",
            "proj/sub/x.bin",
            "proj/zero",
            "report.txt"
        ]
    );
    assert_eq!(std::fs::read(b.recv_dir().join("report.txt")).unwrap(), big);

    // Sending the same thing again never overwrites.
    a.send(&[&src.join("report.txt")], &b);
    eventually("second copy", || {
        b.recv_dir().join("report (1).txt").exists()
    })
    .await;
    eventually("second delivered", || {
        a.transfer(true)
            .is_some_and(|t| t.state == TransferState::Done)
    })
    .await;
}

#[tokio::test]
async fn asked_receiver_can_decline() {
    let (a, b) = pair(true).await;
    let src = temp_dir("src");
    std::fs::write(src.join("f.txt"), b"data").unwrap();
    a.send(&[&src.join("f.txt")], &b);
    eventually("asked", || {
        b.transfer(false)
            .is_some_and(|t| t.state == TransferState::Asking)
    })
    .await;
    let id = b.transfer(false).unwrap().id;
    b.hub().decide(id, false);
    eventually("sender sees decline", || {
        a.transfer(true)
            .is_some_and(|t| matches!(t.state, TransferState::Declined(_)))
    })
    .await;
    assert!(
        tree(&b.recv_dir()).is_empty(),
        "nothing saved, no partial left"
    );
}

#[tokio::test]
async fn asked_receiver_can_accept() {
    let (a, b) = pair(true).await;
    let src = temp_dir("src");
    std::fs::write(src.join("f.txt"), b"data").unwrap();
    a.send(&[&src.join("f.txt")], &b);
    eventually("asked", || {
        b.transfer(false)
            .is_some_and(|t| t.state == TransferState::Asking)
    })
    .await;
    assert_eq!(a.transfer(true).unwrap().state, TransferState::Waiting);
    b.hub().decide(b.transfer(false).unwrap().id, true);
    eventually("delivered", || {
        a.transfer(true)
            .is_some_and(|t| t.state == TransferState::Done)
    })
    .await;
    assert_eq!(std::fs::read(b.recv_dir().join("f.txt")).unwrap(), b"data");
}

#[tokio::test]
async fn cancel_mid_transfer_cleans_up_and_clipboard_keeps_syncing() {
    let (a, b) = pair(false).await;
    let src = temp_dir("src");
    let big = vec![7u8; 64 * 1024 * 1024];
    std::fs::write(src.join("big.bin"), &big).unwrap();
    let id = a.send(&[&src.join("big.bin")], &b);
    eventually("running", || {
        b.transfer(false).is_some_and(|t| t.done_bytes > 0)
    })
    .await;

    a.mock.copy(RawClip::Text("during transfer".into()));
    eventually("clip arrives", || {
        b.mock.content() == text("during transfer")
    })
    .await;

    a.hub().cancel(id);
    eventually("receiver stops", || {
        b.transfer(false).is_some_and(|t| t.state.finished())
    })
    .await;
    assert_eq!(a.transfer(true).unwrap().state, TransferState::Cancelled);
    assert!(
        matches!(b.transfer(false).unwrap().state, TransferState::Failed(_)),
        "receiver: {:?}",
        b.transfer(false).unwrap().state
    );
    assert!(tree(&b.recv_dir()).is_empty(), "partial folder removed");
}

#[tokio::test]
async fn turned_off_receiver_declines() {
    let a = TestNode::start("a", 0, &[], "right").await;
    let mut b = TestNode::start("b", 0, &[a.addr()], "right").await;
    b.config.receive_files = false;
    b.stop().await;
    b.boot().await;
    eventually("connected", || a.peer_count() == 1 && b.peer_count() == 1).await;
    let src = temp_dir("src");
    std::fs::write(src.join("f.txt"), b"data").unwrap();
    a.send(&[&src.join("f.txt")], &b);
    eventually("declined", || {
        a.transfer(true)
            .is_some_and(|t| matches!(t.state, TransferState::Declined(_)))
    })
    .await;
    assert!(b.transfer(false).is_none());
}
