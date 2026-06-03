//! mDNS advertising so the phone can reach the bridge by a stable
//! `.local` name instead of chasing the host's LAN IP (Phase 13.4b).
//!
//! Registers `moon-bridge.local` (an A-record → the host's LAN IP)
//! plus an `_https._tcp` service instance, via `mdns-sd` — pure Rust,
//! no Avahi/Bonjour dependency. A phone on the same network resolves
//! `moon-bridge.local` natively on iOS and on recent Android.
//!
//! mDNS is best-effort: multicast is blocked on some corporate / VPN
//! networks, so the pairing payload still carries the raw IP as a
//! fallback (see `main::run_serve`). If registration fails we log and
//! carry on — the IP URL always works.

use std::net::Ipv4Addr;

use mdns_sd::{ServiceDaemon, ServiceInfo};

/// The `.local` hostname the bridge advertises. The phone can open
/// `https://moon-bridge.local:<port>/` once this is registered.
pub const MDNS_HOSTNAME: &str = "moon-bridge.local.";

/// Holds the daemon alive for the process's lifetime. Dropping it
/// unregisters (the daemon thread shuts down).
pub struct MdnsAdvert {
	_daemon: ServiceDaemon,
}

/// Advertise `moon-bridge.local` → `ip` on `port`. Returns the live
/// advert (keep it alive) or an error to log-and-ignore.
pub fn advertise(ip: Ipv4Addr, port: u16) -> anyhow::Result<MdnsAdvert> {
	let daemon = ServiceDaemon::new()?;
	// `_https._tcp` is the conventional type for a TLS web service;
	// the instance name is cosmetic (shows up in service browsers).
	let service = ServiceInfo::new(
		"_https._tcp.local.",
		"moon-ide companion",
		MDNS_HOSTNAME,
		ip.to_string().as_str(),
		port,
		&[("path", "/")][..],
	)?;
	daemon.register(service)?;
	tracing::info!(hostname = MDNS_HOSTNAME, %ip, port, "mDNS advertising started");
	Ok(MdnsAdvert { _daemon: daemon })
}
