//! A running sync node: sync actor + network actor + (optional) mDNS discovery.
//!
//! The clipboard handle and change feed outlive nodes; the app restarts the node
//! whenever the config changes. Integration tests run several nodes in one process.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::clipboard::ClipboardHandle;
use crate::config::Config;
use crate::crypto::Cipher;
use crate::discovery;
use crate::net::{self, NetStatus, Timing};
use crate::sync::{self, Limits, SyncCore, SyncStats};
use crate::transfer::{self, Hub, Identity, ReceiveSettings, SendTarget};

pub struct NodeParams {
    pub config: Config,
    pub cipher: Cipher,
    pub timing: Timing,
    /// Interface to listen on; tests use loopback to avoid firewall prompts.
    pub bind_ip: IpAddr,
    pub paused: watch::Receiver<bool>,
    /// Transfer state; the app passes one that outlives node restarts.
    pub hub: Arc<Hub>,
}

impl NodeParams {
    pub fn new(config: Config, cipher: Cipher, paused: watch::Receiver<bool>) -> Self {
        let (hub, _) = Hub::new(ReceiveSettings::from_config(&config));
        Self {
            hub,
            config,
            cipher,
            timing: Timing::default(),
            bind_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            paused,
        }
    }
}

pub struct NodeHandle {
    pub local_addr: SocketAddr,
    pub net_status: watch::Receiver<NetStatus>,
    pub stats: watch::Receiver<SyncStats>,
    pub hub: Arc<Hub>,
    identity: Arc<Identity>,
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl NodeHandle {
    /// Starts one transfer per target device. Returns the transfer ids.
    pub fn send_files(&self, paths: Vec<PathBuf>, peers: &[String]) -> Result<Vec<u64>, String> {
        if paths.is_empty() {
            return Err("nothing to send".into());
        }
        let status = self.net_status.borrow().clone();
        let mut ids = Vec::new();
        for id in peers {
            let Some(p) = status.peers.iter().find(|p| &p.device_id == id) else {
                return Err("that device is no longer connected".into());
            };
            let ip = p
                .addr
                .parse::<SocketAddr>()
                .map_err(|e| format!("bad peer address: {e}"))?
                .ip();
            let target = SendTarget {
                device_id: p.device_id.clone(),
                name: p.name.clone(),
                addr: SocketAddr::new(ip, p.listen_port),
            };
            ids.push(transfer::start_send(
                &self.hub,
                self.identity.clone(),
                target,
                paths.clone(),
            ));
        }
        Ok(ids)
    }

    pub async fn shutdown(self) {
        self.cancel.cancel();
        for t in self.tasks {
            let _ = t.await;
        }
    }
}

pub async fn start(
    params: NodeParams,
    clipboard: ClipboardHandle,
    changes: watch::Receiver<u64>,
) -> Result<NodeHandle> {
    let NodeParams {
        config,
        cipher,
        timing,
        bind_ip,
        paused,
        hub,
    } = params;
    hub.set_settings(ReceiveSettings::from_config(&config));
    if !hub.active() {
        transfer::clean_partials(&config.receive_dir());
    }
    let identity = Arc::new(Identity {
        device_id: config.device_id.clone(),
        device_name: config.device_name.clone(),
        cipher: cipher.clone(),
        connect_timeout: timing.connect_timeout,
        handshake_timeout: timing.handshake_timeout,
    });
    let listener = TcpListener::bind(SocketAddr::new(bind_ip, config.port))
        .await
        .with_context(|| format!("listen on port {} (already in use?)", config.port))?;
    let local_addr = listener.local_addr()?;

    let cancel = CancellationToken::new();
    let (out_tx, out_rx) = mpsc::channel(16);
    let (in_tx, in_rx) = mpsc::channel(16);
    let (disc_tx, disc_rx) = mpsc::channel(16);
    let (status_tx, net_status) = watch::channel(NetStatus::default());
    let (stats_tx, stats) = watch::channel(SyncStats::default());
    let mut tasks = Vec::new();

    if config.discovery {
        let task = discovery::DiscoveryTask {
            device_id: config.device_id.clone(),
            device_name: config.device_name.clone(),
            port: local_addr.port(),
            events: disc_tx,
            cancel: cancel.clone(),
        };
        match discovery::start(task) {
            Ok(fut) => tasks.push(tokio::spawn(fut)),
            // Manual peers still work without multicast.
            Err(e) => tracing::warn!("mDNS discovery unavailable: {e:#}"),
        }
    } else {
        drop(disc_tx);
    }

    tasks.push(tokio::spawn(net::run(net::NetTask {
        device_id: config.device_id.clone(),
        device_name: config.device_name.clone(),
        cipher,
        timing,
        manual_peers: config.manual_peers.clone(),
        listener,
        outgoing: out_rx,
        incoming: in_tx,
        discovery: disc_rx,
        status: status_tx,
        hub: hub.clone(),
        cancel: cancel.clone(),
    })));

    tasks.push(tokio::spawn(sync::run(sync::SyncTask {
        core: SyncCore::new(config.device_id.clone(), Limits::from_config(&config)),
        clipboard,
        changes,
        incoming: in_rx,
        outgoing: out_tx,
        paused,
        stats: stats_tx,
        debounce: timing.debounce,
        cancel: cancel.clone(),
    })));

    Ok(NodeHandle {
        local_addr,
        net_status,
        stats,
        hub,
        identity,
        cancel,
        tasks,
    })
}
