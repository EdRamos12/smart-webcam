//! Wi-Fi path: browse mDNS for `_smartwebcam._tcp` phones.

use crate::protocol::SERVICE_TYPE;
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiPhone {
    pub id: String,
    pub name: String,
    pub addrs: Vec<SocketAddr>,
}

/// Live view of the phones currently announced on the LAN, keyed by mDNS fullname.
#[derive(Clone, Default)]
pub struct Phones(Arc<Mutex<HashMap<String, WifiPhone>>>);

impl Phones {
    pub fn list(&self) -> Vec<WifiPhone> {
        let mut v: Vec<_> = self.0.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        v.dedup_by(|a, b| a.id == b.id);
        v
    }

    pub fn find(&self, id: &str) -> Option<WifiPhone> {
        self.0.lock().unwrap().values().find(|p| p.id == id).cloned()
    }
}

/// Starts browsing in a background thread; `changed` is called whenever the set changes.
pub fn start(changed: impl Fn() + Send + 'static) -> anyhow::Result<(Phones, ServiceDaemon)> {
    let daemon = ServiceDaemon::new()?;
    let rx = daemon.browse(SERVICE_TYPE)?;
    let phones = Phones::default();
    let map = phones.clone();
    std::thread::Builder::new().name("mdns".into()).spawn(move || {
        while let Ok(ev) = rx.recv() {
            match ev {
                ServiceEvent::ServiceResolved(info) => {
                    let id = info.txt_properties.get_property_val_str("id").unwrap_or_default().to_string();
                    if id.is_empty() {
                        continue;
                    }
                    let name = info
                        .txt_properties
                        .get_property_val_str("name")
                        .map(str::to_string)
                        .unwrap_or_else(|| info.fullname.clone());
                    // Prefer IPv4: link-local IPv6 needs a scope id and often fails.
                    let mut ips: Vec<IpAddr> = info.addresses.iter().map(|a| a.to_ip_addr()).collect();
                    ips.sort_by_key(|ip| !ip.is_ipv4());
                    let addrs = ips.into_iter().map(|ip| SocketAddr::new(ip, info.port)).collect();
                    let phone = WifiPhone { id, name, addrs };
                    let mut m = map.0.lock().unwrap();
                    if m.get(&info.fullname) != Some(&phone) {
                        log::info!("found {} at {:?}", phone.name, phone.addrs);
                        m.insert(info.fullname.clone(), phone);
                        drop(m);
                        changed();
                    }
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    if map.0.lock().unwrap().remove(&fullname).is_some() {
                        changed();
                    }
                }
                _ => {}
            }
        }
    })?;
    Ok((phones, daemon))
}
