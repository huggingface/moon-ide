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
pub fn load_or_generate(bridge_dir: &Utf8Path) -> anyhow::Result<TlsIdentity> {
	std::fs::create_dir_all(bridge_dir).with_context(|| format!("creating {bridge_dir}"))?;
	let cert_path = bridge_dir.join(CERT_FILE);
	let key_path = bridge_dir.join(KEY_FILE);

	let (cert_der, key_der) = if cert_path.exists() && key_path.exists() {
		let cert = std::fs::read(cert_path.as_std_path()).with_context(|| format!("reading {cert_path}"))?;
		let key = std::fs::read(key_path.as_std_path()).with_context(|| format!("reading {key_path}"))?;
		(cert, key)
	} else {
		let (cert, key) = generate_self_signed()?;
		std::fs::write(cert_path.as_std_path(), &cert).with_context(|| format!("writing {cert_path}"))?;
		// Key material: owner-only perms on unix. Best-effort.
		std::fs::write(key_path.as_std_path(), &key).with_context(|| format!("writing {key_path}"))?;
		restrict_key_perms(&key_path);
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

/// Mint a fresh self-signed cert + PKCS#8 key, returned as DER bytes.
fn generate_self_signed() -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
	// SANs cover loopback + a wildcard for the `*.local` mDNS names
	// a phone on the LAN is likely to hit. The IP the QR advertises
	// is the host's LAN address; we don't know it at cert-gen time
	// and don't need it in a SAN because the phone pins the
	// fingerprint, not the name.
	let subject_alt_names = vec![
		"localhost".to_string(),
		"moon-bridge.local".to_string(),
		"127.0.0.1".to_string(),
	];
	let cert = rcgen::generate_simple_self_signed(subject_alt_names).context("generating self-signed cert")?;
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

	#[test]
	fn generate_then_load_is_stable() {
		let dir = std::env::temp_dir().join(format!("moon-bridge-tls-{}", uuid::Uuid::new_v4().simple()));
		let dir = Utf8PathBuf::from_path_buf(dir).unwrap();

		let first = load_or_generate(&dir).unwrap();
		let second = load_or_generate(&dir).unwrap();
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
}
