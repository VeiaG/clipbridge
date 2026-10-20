//! Local control channel between the background process and the settings window
//! (or a second launch of the app).
//!
//! The server listens on a random loopback port and writes `{port, token}` to
//! `control.json` next to the config file. Each request is one JSON line carrying the
//! token; the reply is one JSON line. The token keeps other local users out.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::app::AppStatus;
use crate::transfer::{TransferInfo, Watcher};

/// Room for a long list of dropped paths.
const MAX_REQUEST: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    Status,
    Reload,
    Pause,
    Resume,
    OpenSettings,
    CheckUpdates,
    InstallUpdate,
    /// Send files and folders to these device ids.
    SendFiles {
        paths: Vec<String>,
        peers: Vec<String>,
    },
    /// List transfers; also marks that window as open.
    Transfers {
        watcher: Watcher,
    },
    CancelTransfer {
        id: u64,
    },
    /// Answer to an incoming offer.
    Decide {
        id: u64,
        accept: bool,
    },
    ClearFinished {
        outgoing: bool,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct Request {
    token: String,
    cmd: Command,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default)]
    pub status: Option<Box<AppStatus>>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub transfers: Option<Vec<TransferInfo>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Endpoint {
    port: u16,
    token: String,
}

pub fn endpoint_file(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .map(|d| d.join("control.json"))
        .unwrap_or_else(|| PathBuf::from("control.json"))
}

/// A request forwarded to the app actor.
pub struct ControlRequest {
    pub cmd: Command,
    pub reply: oneshot::Sender<Response>,
}

/// Binds the server and publishes the endpoint file. Returns the accept loop.
pub async fn serve(
    config_path: &Path,
    requests: mpsc::Sender<ControlRequest>,
    cancel: CancellationToken,
) -> Result<impl Future<Output = ()> + use<>> {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .context("bind control port")?;
    let port = listener.local_addr()?.port();
    let token = uuid::Uuid::new_v4().simple().to_string();
    let file = endpoint_file(config_path);
    let body = serde_json::to_string(&Endpoint {
        port,
        token: token.clone(),
    })?;
    std::fs::write(&file, body).with_context(|| format!("write {}", file.display()))?;

    Ok(async move {
        loop {
            let stream = tokio::select! {
                _ = cancel.cancelled() => break,
                r = listener.accept() => match r {
                    Ok((s, _)) => s,
                    Err(_) => continue,
                },
            };
            let (requests, token) = (requests.clone(), token.clone());
            tokio::spawn(async move {
                let _ = handle(stream, &token, &requests).await;
            });
        }
        let _ = std::fs::remove_file(&file);
    })
}

async fn handle(
    stream: tokio::net::TcpStream,
    token: &str,
    requests: &mpsc::Sender<ControlRequest>,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    let mut reader = tokio::io::BufReader::new(read).take(MAX_REQUEST);
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line)).await??;
    let reply = match serde_json::from_str::<Request>(&line) {
        Ok(req) if req.token == token => {
            let (tx, rx) = oneshot::channel();
            requests
                .send(ControlRequest {
                    cmd: req.cmd,
                    reply: tx,
                })
                .await
                .map_err(|_| anyhow!("app gone"))?;
            rx.await.map_err(|_| anyhow!("app gone"))?
        }
        Ok(_) => error_response("bad token"),
        Err(e) => error_response(&format!("bad request: {e}")),
    };
    let mut out = serde_json::to_vec(&reply)?;
    out.push(b'\n');
    write.write_all(&out).await?;
    Ok(())
}

pub fn error_response(msg: &str) -> Response {
    Response {
        ok: false,
        status: None,
        error: Some(msg.to_string()),
        transfers: None,
    }
}

/// Blocking client used by the settings window and by a second app launch.
pub fn request(config_path: &Path, cmd: Command) -> Result<Response> {
    let file = endpoint_file(config_path);
    let ep: Endpoint =
        serde_json::from_str(&std::fs::read_to_string(&file).context("ClipBridge is not running")?)
            .context("bad control.json")?;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, ep.port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500))
        .context("ClipBridge is not running")?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let mut body = serde_json::to_vec(&Request {
        token: ep.token,
        cmd,
    })?;
    body.push(b'\n');
    stream.write_all(&body)?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let resp: Response = serde_json::from_str(&line).context("bad control response")?;
    if !resp.ok {
        bail!(
            "{}",
            resp.error
                .clone()
                .unwrap_or_else(|| "request failed".into())
        );
    }
    Ok(resp)
}
