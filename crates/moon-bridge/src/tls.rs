//! Self-signed TLS for the bridge's LAN listener (Phase 13.2).
//!
//! The bridge serves WSS on the LAN with a self-signed cert. Mobile
//! browsers reject self-signed certs at the chain level, so the trust
//! model is **TOFU on the fingerprint**: the desktop shows the cert's
//! SHA-256 fingerprint in the pairing QR, the phone pins it on first
//! connect, and the user installs the cert once to silence the
//! browser interstitial (see `specs/companion.md`).
//!
//! This module owns generate-or-load: on first run it mints a cert +
//! key under `<data_local_dir>/moon-ide/bridge/` and reuses them
//! after, so the fingerprint is stable across restarts (a phone that
//! pinned it once keeps trusting it). It also computes the
//! fingerprint the QR encodes.

use std::sync::Arc;

use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest, Sha256};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::ServerConfig;

const CERT_FILE: &str = "bridge-cert.der";
const KEY_FILE: &str = "bridge-key.der";
/// Marker recording which SAN set the persisted cert was generated
/// for, so a changed LAN IP triggers a regenerate (and only then).
const SANS_FILE: &str = "bridge-cert-sans.txt";

/// The bridge's TLS identity: a rustls server config ready to wrap
/// accepted sockets, plus the cert fingerprint for the QR payload.
pub struct TlsIdentity {
	pub server_config: Arc<ServerConfig>,
	/// SHA-256 of the DER cert, lowercase hex with colon separators
	/// (`a1:b2:…`), the conventional fingerprint display the phone
	/// pins against.
	pub fingerprint: String,
}

/// Resolve `<data_local_dir>/moon-ide/bridge/`, creating it if
/// needed. Sibling of the `workspaces/` dir the discovery module
/// reads.
pub fn resolve_bridge_dir() -> anyhow::Result<Utf8PathBuf> {
	let raw = dirs::data_local_dir().context("could not resolve local data dir")?;
	let utf8 =
		Utf8PathBuf::from_path_buf(raw).map_err(|p| anyhow::anyhow!("non-utf8 local data dir: {}", p.display()))?;
	Ok(utf8.join("moon-ide").join("bridge"))
}

/// Load the persisted cert + key, or mint and persist a fresh
/// self-signed pair on first run. The cert covers loopback plus the
/// conventional LAN names. Since the phone pins the fingerprint
/// rather than validating the chain or SANs, the exact SAN set only
/// matters for browsers that check it after the user-installed trust
/// profile is in place.
pub fn load_or_generate(bridge_dir: &Utf8Path, lan_ip: Option<std::net::Ipv4Addr>) -> anyhow::Result<TlsIdentity> {
	std::fs::create_dir_all(bridge_dir).with_context(|| format!("creating {bridge_dir}"))?;
	let cert_path = bridge_dir.join(CERT_FILE);
	let key_path = bridge_dir.join(KEY_FILE);
	let sans_path = bridge_dir.join(SANS_FILE);

	// The SAN set we want this cert to cover: the stable names plus
	// the host's current LAN IP (so a browser hitting `https://<ip>`
	// doesn't reject on a name mismatch even after trusting the cert).
	let desired_sans = desired_sans(lan_ip);

	// Reuse the persisted cert only if it exists AND was generated for
	// the same SAN set. Keeping the cert stable matters: the phone
	// pinned its fingerprint at pair time, so regenerating forces a
	// re-pair. With a fixed LAN IP this regenerates exactly once (the
	// first run that adds the IP), then never again; a network change
	// regenerates once more — a deliberate, logged re-pair, not churn.
	let reuse =
		cert_path.exists() && key_path.exists() && stored_sans(&sans_path).as_deref() == Some(desired_sans.as_slice());

	let (cert_der, key_der) = if reuse {
		let cert = std::fs::read(cert_path.as_std_path()).with_context(|| format!("reading {cert_path}"))?;
		let key = std::fs::read(key_path.as_std_path()).with_context(|| format!("reading {key_path}"))?;
		(cert, key)
	} else {
		if cert_path.exists() {
			tracing::warn!(
				?desired_sans,
				"bridge cert SANs changed (LAN IP?); regenerating — paired devices must re-pair"
			);
		}
		let (cert, key) = generate_self_signed(&desired_sans)?;
		std::fs::write(cert_path.as_std_path(), &cert).with_context(|| format!("writing {cert_path}"))?;
		// Key material: owner-only perms on unix. Best-effort.
		std::fs::write(key_path.as_std_path(), &key).with_context(|| format!("writing {key_path}"))?;
		restrict_key_perms(&key_path);
		// Record the SAN set so the next launch knows whether to reuse.
		let _ = std::fs::write(sans_path.as_std_path(), desired_sans.join("\n"));
		(cert, key)
	};

	let fingerprint = fingerprint_hex(&cert_der);

	let cert = CertificateDer::from(cert_der);
	let key = PrivateKeyDer::try_from(key_der).map_err(|e| anyhow::anyhow!("bad private key: {e}"))?;
	let server_config = ServerConfig::builder()
		.with_no_client_auth()
		.with_single_cert(vec![cert], key)
		.context("building rustls server config")?;

	Ok(TlsIdentity {
		server_config: Arc::new(server_config),
		fingerprint,
	})
}

/// The SAN set the cert should cover: stable names first, then the
/// host's LAN IP when known. Order is fixed so the stored-vs-desired
/// comparison is stable.
fn desired_sans(lan_ip: Option<std::net::Ipv4Addr>) -> Vec<String> {
	let mut sans = vec![
		"localhost".to_string(),
		"moon-bridge.local".to_string(),
		"127.0.0.1".to_string(),
	];
	if let Some(ip) = lan_ip {
		sans.push(ip.to_string());
	}
	sans
}

/// Read the SAN set the persisted cert was generated for (one per
/// line), or `None` if the marker is missing.
fn stored_sans(path: &Utf8Path) -> Option<Vec<String>> {
	let text = std::fs::read_to_string(path.as_std_path()).ok()?;
	Some(text.lines().map(str::to_owned).collect())
}

/// Mint a fresh self-signed cert + PKCS#8 key over `sans`, returned as
/// DER bytes. `rcgen` parses IP-shaped SANs as IP entries and the rest
/// as DNS names, which is exactly what a browser checks against the
/// host in the URL.
fn generate_self_signed(sans: &[String]) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
	let cert = rcgen::generate_simple_self_signed(sans.to_vec()).context("generating self-signed cert")?;
	let cert_der = cert.cert.der().to_vec();
	let key_der = cert.signing_key.serialize_der();
	Ok((cert_der, key_der))
}

/// SHA-256 of the DER cert, lowercase hex, colon-separated.
fn fingerprint_hex(cert_der: &[u8]) -> String {
	let digest = Sha256::digest(cert_der);
	let mut out = String::with_capacity(digest.len() * 3);
	for (i, byte) in digest.iter().enumerate() {
		if i > 0 {
			out.push(':');
		}
		out.push_str(&format!("{byte:02x}"));
	}
	out
}

#[cfg(unix)]
fn restrict_key_perms(path: &Utf8Path) {
	use std::os::unix::fs::PermissionsExt;
	if let Err(err) = std::fs::set_permissions(path.as_std_path(), std::fs::Permissions::from_mode(0o600)) {
		tracing::warn!(error = %err, path = %path, "could not restrict key file permissions");
	}
}

#[cfg(not(unix))]
fn restrict_key_perms(_path: &Utf8Path) {}

#[cfg(test)]
mod tests {
	use super::*;

	/// `load_or_generate` builds a `ServerConfig`, which resolves the
	/// process-global rustls CryptoProvider. `main` installs ring at
	/// startup, but tests don't run `main` — and under
	/// `cargo test --workspace` feature unification enables both
	/// `ring` and `aws-lc-rs` on rustls (reqwest pulls in the
	/// latter), so auto-detection bails instead of picking one.
	/// Mirror `main`'s install here; `let _ =` because a second
	/// test installing after the first is expected to fail.
	fn install_crypto_provider() {
		let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
	}

	#[test]
	fn generate_then_load_is_stable() {
		install_crypto_provider();
		let dir = std::env::temp_dir().join(format!("moon-bridge-tls-{}", uuid::Uuid::new_v4().simple()));
		let dir = Utf8PathBuf::from_path_buf(dir).unwrap();

		let ip = Some(std::net::Ipv4Addr::new(192, 168, 1, 50));
		let first = load_or_generate(&dir, ip).unwrap();
		let second = load_or_generate(&dir, ip).unwrap();
		// Same files reused -> same fingerprint across "restarts".
		assert_eq!(first.fingerprint, second.fingerprint);
		// Fingerprint shape: 32 colon-separated hex bytes.
		assert_eq!(first.fingerprint.split(':').count(), 32);
		assert!(first
			.fingerprint
			.split(':')
			.all(|b| b.len() == 2 && u8::from_str_radix(b, 16).is_ok()));

		let _ = std::fs::remove_dir_all(dir.as_std_path());
	}

	#[test]
	fn changing_lan_ip_regenerates_cert() {
		install_crypto_provider();
		let dir = std::env::temp_dir().join(format!("moon-bridge-tls-ip-{}", uuid::Uuid::new_v4().simple()));
		let dir = Utf8PathBuf::from_path_buf(dir).unwrap();

		let a = load_or_generate(&dir, Some(std::net::Ipv4Addr::new(192, 168, 1, 50))).unwrap();
		// Same IP -> stable (no surprise re-pair).
		let a2 = load_or_generate(&dir, Some(std::net::Ipv4Addr::new(192, 168, 1, 50))).unwrap();
		assert_eq!(a.fingerprint, a2.fingerprint);
		// Different IP -> new cert (the documented re-pair case).
		let b = load_or_generate(&dir, Some(std::net::Ipv4Addr::new(10, 0, 0, 9))).unwrap();
		assert_ne!(a.fingerprint, b.fingerprint);

		let _ = std::fs::remove_dir_all(dir.as_std_path());
	}
}
