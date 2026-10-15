//! mDNS: advertise `_clipbridge._tcp.local.` and report other ClipBridge devices.

use std::net::{IpAddr, SocketAddr};

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::net::DiscoveryEvent;
use crate::protocol::PROTOCOL_VERSION;

pub const SERVICE_TYPE: &str = "_clipbridge._tcp.local.";

pub struct DiscoveryTask {
    pub device_id: String,
    pub device_name: String,
    pub port: u16,
    pub events: mpsc::Sender<DiscoveryEvent>,
    pub cancel: CancellationToken,
}

/// Instance names are the device_id, so a removal (which only carries the fullname)
/// can be mapped back to a device.
fn device_id_from_fullname(fullname: &str) -> Option<&str> {
    fullname.strip_suffix(SERVICE_TYPE)?.strip_suffix('.')
}

/// Starts the daemon synchronously (so failures surface) and returns the event loop.
pub fn start(task: DiscoveryTask) -> Result<impl Future<Output = ()>> {
    let daemon = ServiceDaemon::new().context("start mDNS daemon")?;
    let host = format!(
        "clipbridge-{}.local.",
        &task.device_id[..8.min(task.device_id.len())]
    );
    let version = PROTOCOL_VERSION.to_string();
    let props = [
        ("id", task.device_id.as_str()),
        ("name", task.device_name.as_str()),
        ("v", version.as_str()),
    ];
    let info = ServiceInfo::new(
        SERVICE_TYPE,
        &task.device_id,
        &host,
        "",
        task.port,
        &props[..],
    )
    .context("build mDNS service info")?
    .enable_addr_auto();
    let fullname = info.get_fullname().to_string();
    daemon.register(info).context("register mDNS service")?;
    let browse = daemon.browse(SERVICE_TYPE).context("browse mDNS")?;
    tracing::info!("mDNS: advertising {SERVICE_TYPE} on port {}", task.port);

    Ok(async move {
        loop {
            let ev = tokio::select! {
                _ = task.cancel.cancelled() => break,
                ev = browse.recv_async() => match ev {
                    Ok(ev) => ev,
                    Err(_) => break,
                },
            };
            let out = match ev {
                ServiceEvent::ServiceResolved(svc) => {
                    let Some(id) = svc.get_property_val_str("id").map(str::to_string) else {
                        continue;
                    };
                    if id == task.device_id {
                        continue;
                    }
                    let name = svc.get_property_val_str("name").unwrap_or(&id).to_string();
                    let port = svc.get_port();
                    let mut ips: Vec<IpAddr> = svc
                        .get_addresses()
                        .iter()
                        .map(|ip| ip.to_ip_addr())
                        .collect();
                    // IPv4 first; skip IPv6 link-local (needs a scope id to connect).
                    ips.retain(|ip| match ip {
                        IpAddr::V4(_) => true,
                        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) != 0xfe80,
                    });
                    ips.sort_by_key(|ip| ip.is_ipv6());
                    let addrs = ips
                        .into_iter()
                        .map(|ip| SocketAddr::new(ip, port))
                        .collect();
                    DiscoveryEvent::Found {
                        device_id: id,
                        name,
                        addrs,
                    }
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    let Some(id) = device_id_from_fullname(&fullname) else {
                        continue;
                    };
                    DiscoveryEvent::Lost {
                        device_id: id.to_string(),
                    }
                }
                _ => continue,
            };
            if task.events.send(out).await.is_err() {
                break;
            }
        }
        let _ = daemon.unregister(&fullname);
        let _ = daemon.shutdown();
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fullname_to_id() {
        let full = format!("abc-123.{SERVICE_TYPE}");
        assert_eq!(device_id_from_fullname(&full), Some("abc-123"));
        assert_eq!(device_id_from_fullname("x._other._tcp.local."), None);
    }
}
