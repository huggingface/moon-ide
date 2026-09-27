//! Companion end-to-end encryption (ADR 0087): the relay becomes a
//! blind pipe between phone and IDE.
//!
//! - The IDE host owns a static X25519 identity and the list of phone
//!   public keys allowed to drive it. Both live in the keyring and are
//!   shared by every IDE process on the host.
//! - Pairing is `Noise_IKpsk2` against the IDE key carried by the pairing
//!   link, with the link's single-use 256-bit secret as PSK.
//! - Every phone↔(IDE, workspace) session is a `Noise_IK` handshake whose
//!   initiator static must be an authorized phone. Transport uses the
//!   split keys directly with explicit nonces, because replies and events
//!   come back through the relay out of order.
//!
//! [`E2eRpc`] wraps the plain [`BridgeRpcHandler`]: on the relay path
//! (`strict`) it refuses every plaintext method except
//! `workspace_launch`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::ChaCha20Poly1305;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Digest as _;

use crate::rpc::BridgeRpcHandler;

pub const PAIR_PATTERN: &str = "Noise_IKpsk2_25519_ChaChaPoly_SHA256";
pub const SESSION_PATTERN: &str = "Noise_IK_25519_ChaChaPoly_SHA256";
pub const PAIR_PROLOGUE: &[u8] = b"moon-companion-pair-v1";
const SESSION_PROLOGUE_PREFIX: &[u8] = b"moon-companion-session-v1\0";

/// AEAD associated data per message kind. Keys are already
/// directional; the kind tag stops the relay passing a result off as an
/// event (or the reverse) on the IDE→phone direction.
pub const AD_CALL: &[u8] = b"moon-e2e-v1:c";
pub const AD_RESULT: &[u8] = b"moon-e2e-v1:r";
pub const AD_EVENT: &[u8] = b"moon-e2e-v1:e";

const KEYRING_SERVICE: &str = "moon-ide";
const ACCOUNT_IDENTITY: &str = "companion-e2e-identity";
const ACCOUNT_DEVICES: &str = "companion-e2e-devices";
const ACCOUNT_INVITES: &str = "companion-e2e-invites";

/// Copy-paste over ssh takes longer than a QR scan; 15 min keeps
/// that comfortable while the 256-bit secret makes the window moot for
/// guessing.
const INVITE_TTL_MS: i64 = 15 * 60 * 1000;
/// How long a cached device list is trusted before a revoke made by
/// another process (desktop panel, `moon-remote revoke`) is picked up.
const DEVICE_CACHE_TTL: Duration = Duration::from_secs(30);
const MAX_SESSIONS: usize = 256;
const SESSION_IDLE_TTL: Duration = Duration::from_secs(24 * 3600);
/// Replay window width in messages (WireGuard uses 2048 too).
const REPLAY_WINDOW: u64 = 2048;

/// Error a phone treats as "handshake again": the IDE process that held
/// the session restarted, evicted it, or the relay routed to a sibling.
pub const UNKNOWN_SESSION: &str = "e2e: unknown session";

/// Keyring-shaped storage, abstracted so tests don't touch the host
/// keyring.
pub trait SecretStore: Send + Sync {
	fn get(&self, account: &str) -> anyhow::Result<Option<String>>;
	fn set(&self, account: &str, value: &str) -> anyhow::Result<()>;
}

pub struct KeyringStore;

impl SecretStore for KeyringStore {
	fn get(&self, account: &str) -> anyhow::Result<Option<String>> {
		let entry = keyring::Entry::new(KEYRING_SERVICE, account)?;
		match entry.get_password() {
			Ok(v) => Ok(Some(v)),
			Err(keyring::Error::NoEntry) => Ok(None),
			Err(err) => Err(err.into()),
		}
	}

	fn set(&self, account: &str, value: &str) -> anyhow::Result<()> {
		keyring::Entry::new(KEYRING_SERVICE, account)?.set_password(value)?;
		Ok(())
	}
}

#[derive(Default)]
pub struct MemoryStore(Mutex<HashMap<String, String>>);

impl SecretStore for MemoryStore {
	fn get(&self, account: &str) -> anyhow::Result<Option<String>> {
		Ok(self.0.lock().expect("store lock").get(account).cloned())
	}

	fn set(&self, account: &str, value: &str) -> anyhow::Result<()> {
		self
			.0
			.lock()
			.expect("store lock")
			.insert(account.to_string(), value.to_string());
		Ok(())
	}
}

/// A phone allowed to open sessions against this IDE host.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthorizedDevice {
	pub id: String,
	pub label: String,
	/// X25519 public key, base64url.
	pub public_key: String,
	pub paired_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredIdentity {
	private_key: String,
	public_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredInvite {
	secret: String,
	expires_at_ms: i64,
}

/// A freshly minted pairing invite.
#[derive(Debug, Clone)]
pub struct Invite {
	/// 32 random bytes, base64url. Travels only in the pairing link.
	pub secret: String,
}

/// Public handle the phone sends so the IDE can pick the invite's PSK
/// without the secret ever crossing the relay.
pub fn invite_id(secret_b64: &str) -> String {
	let digest = sha2::Sha256::digest(secret_b64.as_bytes());
	B64.encode(&digest[..12])
}

/// Build the pairing link rendered as a QR and offered for copy-paste.
/// `relay_url` is the relay's advertised `wss://` URL (the PWA is served
/// from the same origin).
pub fn pair_link(
	relay_url: &str,
	relay_code: &str,
	ide_id: &str,
	workspace: &str,
	ide_key: &str,
	secret: &str,
) -> String {
	let origin = relay_url
		.trim_end_matches('/')
		.replacen("wss://", "https://", 1)
		.replacen("ws://", "http://", 1);
	format!(
		"{origin}/#pair={}&ide={}&ws={}&k={}&s={}",
		fragment_escape(relay_code),
		fragment_escape(ide_id),
		fragment_escape(workspace),
		fragment_escape(ide_key),
		fragment_escape(secret),
	)
}

fn fragment_escape(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	for b in s.bytes() {
		if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
			out.push(b as char);
		} else {
			out.push_str(&format!("%{b:02X}"));
		}
	}
	out
}

/// Human label the phone shows for this IDE host.
pub fn host_label() -> String {
	hostname::get()
		.ok()
		.and_then(|h| h.into_string().ok())
		.unwrap_or_else(|| "moon-ide".to_string())
}

fn now_ms() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_millis() as i64)
		.unwrap_or(0)
}

fn b64_decode(field: &str, s: &str) -> Result<Vec<u8>, String> {
	B64
		.decode(s.as_bytes())
		.map_err(|_| format!("e2e: `{field}` is not base64url"))
}

fn session_prologue(workspace: &str) -> Vec<u8> {
	let mut p = SESSION_PROLOGUE_PREFIX.to_vec();
	p.extend_from_slice(workspace.as_bytes());
	p
}

/// Noise ChaChaPoly nonce: 32 zero bits then the 64-bit counter,
/// little-endian.
pub fn noise_nonce(n: u64) -> [u8; 12] {
	let mut nonce = [0u8; 12];
	nonce[4..].copy_from_slice(&n.to_le_bytes());
	nonce
}

pub fn seal(key: &[u8; 32], n: u64, ad: &[u8], plaintext: &[u8]) -> Vec<u8> {
	let cipher = ChaCha20Poly1305::new(&(*key).into());
	cipher
		.encrypt(
			&noise_nonce(n).into(),
			Payload {
				msg: plaintext,
				aad: ad,
			},
		)
		.expect("chacha20poly1305 encrypt is infallible for in-memory buffers")
}

pub fn open(key: &[u8; 32], n: u64, ad: &[u8], ciphertext: &[u8]) -> Option<Vec<u8>> {
	let cipher = ChaCha20Poly1305::new(&(*key).into());
	cipher
		.decrypt(
			&noise_nonce(n).into(),
			Payload {
				msg: ciphertext,
				aad: ad,
			},
		)
		.ok()
}

/// Sliding-window replay filter over explicit nonces.
#[derive(Default)]
pub struct ReplayWindow {
	/// Highest nonce accepted so far, plus one (0 = nothing seen).
	top: u64,
	bits: [u64; (REPLAY_WINDOW / 64) as usize],
}

impl ReplayWindow {
	/// Cheap pre-check before spending a decrypt on the message.
	pub fn fresh(&self, n: u64) -> bool {
		if n >= self.top {
			return true;
		}
		if self.top - n > REPLAY_WINDOW {
			return false;
		}
		let bit = n % REPLAY_WINDOW;
		self.bits[(bit / 64) as usize] & (1 << (bit % 64)) == 0
	}

	/// Record `n` as seen. Only call after the message authenticated,
	/// so forged traffic can't burn window slots.
	pub fn mark(&mut self, n: u64) {
		if n >= self.top {
			let advance = n + 1 - self.top;
			if advance >= REPLAY_WINDOW {
				self.bits = [0; (REPLAY_WINDOW / 64) as usize];
			} else {
				for k in self.top..n + 1 {
					let bit = k % REPLAY_WINDOW;
					self.bits[(bit / 64) as usize] &= !(1 << (bit % 64));
				}
			}
			self.top = n + 1;
		}
		let bit = n % REPLAY_WINDOW;
		self.bits[(bit / 64) as usize] |= 1 << (bit % 64);
	}
}

struct Session {
	device_id: String,
	send_key: [u8; 32],
	recv_key: [u8; 32],
	send_n: AtomicU64,
	recv: Mutex<ReplayWindow>,
	last_used: Mutex<Instant>,
}

impl Session {
	fn seal_out(&self, ad: &[u8], plaintext: &[u8]) -> Value {
		let n = self.send_n.fetch_add(1, Ordering::Relaxed);
		json!({ "n": n, "c": B64.encode(seal(&self.send_key, n, ad, plaintext)) })
	}
}

#[derive(Deserialize)]
struct HandshakeParams {
	msg: String,
	#[serde(default)]
	invite: Option<String>,
}

#[derive(Deserialize)]
struct CallParams {
	sid: String,
	n: u64,
	c: String,
}

#[derive(Deserialize)]
struct InnerCall {
	id: u64,
	method: String,
	#[serde(default)]
	params: Value,
}

#[derive(Deserialize)]
struct PairPayload {
	#[serde(default)]
	label: String,
}

/// Per-process E2E terminator. Identity, devices and invites live in the
/// (host-shared) store; sessions live here, in memory.
pub struct E2eEndpoint {
	store: Arc<dyn SecretStore>,
	workspace: String,
	ide_label: String,
	identity: Mutex<Option<StoredIdentity>>,
	sessions: Mutex<HashMap<String, Arc<Session>>>,
	devices_cache: Mutex<Option<(Instant, Vec<AuthorizedDevice>)>>,
}

impl E2eEndpoint {
	pub fn new(store: Arc<dyn SecretStore>, workspace: impl Into<String>, ide_label: impl Into<String>) -> Self {
		Self {
			store,
			workspace: workspace.into(),
			ide_label: ide_label.into(),
			identity: Mutex::new(None),
			sessions: Mutex::new(HashMap::new()),
			devices_cache: Mutex::new(None),
		}
	}

	pub fn keyring(workspace: impl Into<String>, ide_label: impl Into<String>) -> Self {
		Self::new(Arc::new(KeyringStore), workspace, ide_label)
	}

	fn identity(&self) -> anyhow::Result<StoredIdentity> {
		let mut guard = self.identity.lock().expect("identity lock");
		if let Some(id) = guard.as_ref() {
			return Ok(id.clone());
		}
		let id = match self.store.get(ACCOUNT_IDENTITY)? {
			Some(json) => serde_json::from_str::<StoredIdentity>(&json)?,
			None => {
				let kp = snow::Builder::new(SESSION_PATTERN.parse()?).generate_keypair()?;
				let id = StoredIdentity {
					private_key: B64.encode(&kp.private),
					public_key: B64.encode(&kp.public),
				};
				self.store.set(ACCOUNT_IDENTITY, &serde_json::to_string(&id)?)?;
				id
			}
		};
		*guard = Some(id.clone());
		Ok(id)
	}

	/// This host's public key, base64url — what the pairing link pins.
	pub fn public_key(&self) -> anyhow::Result<String> {
		Ok(self.identity()?.public_key)
	}

	pub fn devices(&self) -> anyhow::Result<Vec<AuthorizedDevice>> {
		match self.store.get(ACCOUNT_DEVICES)? {
			Some(json) => Ok(serde_json::from_str(&json)?),
			None => Ok(Vec::new()),
		}
	}

	fn save_devices(&self, devices: &[AuthorizedDevice]) -> anyhow::Result<()> {
		self.store.set(ACCOUNT_DEVICES, &serde_json::to_string(devices)?)?;
		*self.devices_cache.lock().expect("cache lock") = Some((Instant::now(), devices.to_vec()));
		Ok(())
	}

	/// Remove a phone. Its live sessions in this process end now; other
	/// processes notice within [`DEVICE_CACHE_TTL`].
	pub fn revoke(&self, device_id: &str) -> anyhow::Result<bool> {
		let mut devices = self.devices()?;
		let before = devices.len();
		devices.retain(|d| d.id != device_id);
		let removed = devices.len() != before;
		self.save_devices(&devices)?;
		self
			.sessions
			.lock()
			.expect("sessions lock")
			.retain(|_, s| s.device_id != device_id);
		Ok(removed)
	}

	fn cached_devices(&self, force: bool) -> anyhow::Result<Vec<AuthorizedDevice>> {
		let mut cache = self.devices_cache.lock().expect("cache lock");
		if !force {
			if let Some((at, list)) = cache.as_ref() {
				if at.elapsed() < DEVICE_CACHE_TTL {
					return Ok(list.clone());
				}
			}
		}
		let list = self.devices()?;
		*cache = Some((Instant::now(), list.clone()));
		Ok(list)
	}

	fn device_authorized(&self, device_id: &str) -> bool {
		self
			.cached_devices(false)
			.map(|list| list.iter().any(|d| d.id == device_id))
			.unwrap_or(false)
	}

	fn load_invites(&self) -> anyhow::Result<Vec<StoredInvite>> {
		let now = now_ms();
		let mut invites: Vec<StoredInvite> = match self.store.get(ACCOUNT_INVITES)? {
			Some(json) => serde_json::from_str(&json).unwrap_or_default(),
			None => Vec::new(),
		};
		invites.retain(|i| i.expires_at_ms > now);
		Ok(invites)
	}

	pub fn mint_invite(&self) -> anyhow::Result<Invite> {
		let mut bytes = [0u8; 32];
		getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
		let secret = B64.encode(bytes);
		let mut invites = self.load_invites()?;
		invites.push(StoredInvite {
			secret: secret.clone(),
			expires_at_ms: now_ms() + INVITE_TTL_MS,
		});
		self.store.set(ACCOUNT_INVITES, &serde_json::to_string(&invites)?)?;
		Ok(Invite { secret })
	}

	/// Mint an invite and build the pairing link for this process's
	/// workspace. `relay_url` / `relay_code` come from the relay's
	/// `PairPayload` (routing only).
	pub fn pair_link_for(&self, relay_url: &str, relay_code: &str, ide_id: &str) -> anyhow::Result<String> {
		let invite = self.mint_invite()?;
		Ok(pair_link(
			relay_url,
			relay_code,
			ide_id,
			&self.workspace,
			&self.public_key()?,
			&invite.secret,
		))
	}

	/// Remove and return the live invite matching `id`, so a replayed
	/// pairing message can't reuse it.
	fn take_invite(&self, id: &str) -> anyhow::Result<Option<StoredInvite>> {
		let mut invites = self.load_invites()?;
		let pos = invites.iter().position(|i| invite_id(&i.secret) == id);
		let taken = pos.map(|p| invites.remove(p));
		self.store.set(ACCOUNT_INVITES, &serde_json::to_string(&invites)?)?;
		Ok(taken)
	}

	/// `e2e_pair`: `{ invite, msg }` → `{ msg }`. Authorizes the phone.
	pub fn handle_pair(&self, params: Value) -> Result<Value, String> {
		let p: HandshakeParams = serde_json::from_value(params).map_err(|e| format!("e2e_pair: {e}"))?;
		let invite_handle = p.invite.ok_or("e2e_pair: missing `invite`")?;
		let msg1 = b64_decode("msg", &p.msg)?;
		let identity = self.identity().map_err(|e| format!("e2e identity: {e}"))?;
		let private = b64_decode("identity", &identity.private_key)?;
		let invite = self
			.take_invite(&invite_handle)
			.map_err(|e| format!("e2e invites: {e}"))?
			.ok_or("e2e_pair: pairing link expired or already used — mint a new one")?;
		let psk: [u8; 32] = b64_decode("invite", &invite.secret)?
			.try_into()
			.map_err(|_| "e2e_pair: malformed invite secret")?;

		let mut hs = snow::Builder::new(PAIR_PATTERN.parse().expect("static pattern"))
			.local_private_key(&private)
			.and_then(|b| b.prologue(PAIR_PROLOGUE))
			.and_then(|b| b.psk(2, &psk))
			.and_then(|b| b.build_responder())
			.map_err(|e| format!("e2e_pair: {e}"))?;
		let mut buf = vec![0u8; 65535];
		let len = hs
			.read_message(&msg1, &mut buf)
			.map_err(|_| "e2e_pair: handshake rejected")?;
		let payload: PairPayload = serde_json::from_slice(&buf[..len]).unwrap_or(PairPayload { label: String::new() });
		let phone_key = B64.encode(hs.get_remote_static().ok_or("e2e_pair: no initiator static")?);

		let mut devices = self.devices().map_err(|e| format!("e2e devices: {e}"))?;
		let device = match devices.iter().find(|d| d.public_key == phone_key) {
			Some(existing) => existing.clone(),
			None => {
				let label = if payload.label.trim().is_empty() {
					"phone".to_string()
				} else {
					payload.label.trim().chars().take(80).collect()
				};
				let device = AuthorizedDevice {
					id: uuid::Uuid::new_v4().simple().to_string(),
					label,
					public_key: phone_key,
					paired_at_ms: now_ms(),
				};
				devices.push(device.clone());
				self.save_devices(&devices).map_err(|e| format!("e2e devices: {e}"))?;
				device
			}
		};

		let reply =
			serde_json::to_vec(&json!({ "device_id": device.id, "ide_label": self.ide_label })).expect("static json");
		let len = hs
			.write_message(&reply, &mut buf)
			.map_err(|e| format!("e2e_pair: {e}"))?;
		tracing::info!(device = %device.id, label = %device.label, "companion phone paired (e2e)");
		Ok(json!({ "msg": B64.encode(&buf[..len]) }))
	}

	/// `e2e_open`: `{ msg }` → `{ sid, msg }`.
	pub fn handle_open(&self, params: Value) -> Result<Value, String> {
		let p: HandshakeParams = serde_json::from_value(params).map_err(|e| format!("e2e_open: {e}"))?;
		let msg1 = b64_decode("msg", &p.msg)?;
		let identity = self.identity().map_err(|e| format!("e2e identity: {e}"))?;
		let private = b64_decode("identity", &identity.private_key)?;
		let prologue = session_prologue(&self.workspace);
		let mut hs = snow::Builder::new(SESSION_PATTERN.parse().expect("static pattern"))
			.local_private_key(&private)
			.and_then(|b| b.prologue(&prologue))
			.and_then(|b| b.build_responder())
			.map_err(|e| format!("e2e_open: {e}"))?;
		let mut buf = vec![0u8; 65535];
		hs.read_message(&msg1, &mut buf)
			.map_err(|_| "e2e_open: handshake rejected (wrong workspace or IDE key)")?;
		let phone_key = B64.encode(hs.get_remote_static().ok_or("e2e_open: no initiator static")?);
		let device = self
			.find_device(&phone_key)
			.ok_or("e2e_open: this phone is not paired with this IDE — scan its pairing QR")?;
		let len = hs.write_message(&[], &mut buf).map_err(|e| format!("e2e_open: {e}"))?;
		let (i2r, r2i) = hs.dangerously_get_raw_split();
		let sid = uuid::Uuid::new_v4().simple().to_string();
		let session = Arc::new(Session {
			device_id: device.id,
			send_key: r2i,
			recv_key: i2r,
			send_n: AtomicU64::new(0),
			recv: Mutex::new(ReplayWindow::default()),
			last_used: Mutex::new(Instant::now()),
		});
		self.insert_session(sid.clone(), session);
		Ok(json!({ "sid": sid, "msg": B64.encode(&buf[..len]) }))
	}

	fn find_device(&self, public_key: &str) -> Option<AuthorizedDevice> {
		let hit = |force| {
			self
				.cached_devices(force)
				.ok()
				.and_then(|list| list.into_iter().find(|d| d.public_key == public_key))
		};
		// A phone paired by a sibling process a moment ago isn't in our
		// cache yet — one forced reload before refusing it.
		hit(false).or_else(|| hit(true))
	}

	fn insert_session(&self, sid: String, session: Arc<Session>) {
		let mut sessions = self.sessions.lock().expect("sessions lock");
		sessions.retain(|_, s| s.last_used.lock().expect("session lock").elapsed() < SESSION_IDLE_TTL);
		if sessions.len() >= MAX_SESSIONS {
			let oldest = sessions
				.iter()
				.min_by_key(|(_, s)| *s.last_used.lock().expect("session lock"))
				.map(|(k, _)| k.clone());
			if let Some(k) = oldest {
				sessions.remove(&k);
			}
		}
		sessions.insert(sid, session);
	}

	fn session(&self, sid: &str) -> Result<Arc<Session>, String> {
		let session = self
			.sessions
			.lock()
			.expect("sessions lock")
			.get(sid)
			.cloned()
			.ok_or(UNKNOWN_SESSION)?;
		if !self.device_authorized(&session.device_id) {
			self.sessions.lock().expect("sessions lock").remove(sid);
			return Err("e2e: this phone was revoked on the IDE".into());
		}
		*session.last_used.lock().expect("session lock") = Instant::now();
		Ok(session)
	}

	fn decrypt_in(&self, session: &Session, n: u64, c: &str, ad: &[u8]) -> Result<Vec<u8>, String> {
		if !session.recv.lock().expect("window lock").fresh(n) {
			return Err("e2e: replayed or stale message".into());
		}
		let ct = b64_decode("c", c)?;
		let plain = open(&session.recv_key, n, ad, &ct).ok_or("e2e: message failed authentication")?;
		let mut window = session.recv.lock().expect("window lock");
		// Re-check under the lock: two copies of one message can race
		// past the pre-check.
		if !window.fresh(n) {
			return Err("e2e: replayed or stale message".into());
		}
		window.mark(n);
		Ok(plain)
	}

	/// `e2e_call`: `{ sid, n, c }` → `{ n, c }`. The inner call's result
	/// or error rides encrypted, tagged with the phone's request id.
	pub async fn handle_call(&self, params: Value, inner: &dyn BridgeRpcHandler) -> Result<Value, String> {
		let p: CallParams = serde_json::from_value(params).map_err(|e| format!("e2e_call: {e}"))?;
		let session = self.session(&p.sid)?;
		let plain = self.decrypt_in(&session, p.n, &p.c, AD_CALL)?;
		let call: InnerCall = serde_json::from_slice(&plain).map_err(|e| format!("e2e_call: inner frame: {e}"))?;
		let reply = if call.method.starts_with("e2e_") {
			json!({ "id": call.id, "err": "e2e methods can't be nested" })
		} else {
			match inner.dispatch(&call.method, call.params).await {
				Ok(ok) => json!({ "id": call.id, "ok": ok }),
				Err(err) => json!({ "id": call.id, "err": err }),
			}
		};
		let bytes = serde_json::to_vec(&reply).map_err(|e| format!("e2e_call: encode: {e}"))?;
		Ok(session.seal_out(AD_RESULT, &bytes))
	}

	/// Encrypted event stream: `params.sid` names the session; every
	/// inner event is sealed as `{ sid, n, c }`.
	pub async fn subscribe(
		&self,
		method: &str,
		params: &Value,
		inner: &dyn BridgeRpcHandler,
	) -> Result<tokio::sync::mpsc::Receiver<Value>, String> {
		let sid = params
			.get("sid")
			.and_then(Value::as_str)
			.ok_or("e2e: subscribe needs a `sid`")?
			.to_string();
		let session = self.session(&sid)?;
		let mut events = inner.subscribe(method, Value::Null).await?;
		let (tx, rx) = tokio::sync::mpsc::channel::<Value>(256);
		tokio::spawn(async move {
			while let Some(event) = events.recv().await {
				let Ok(bytes) = serde_json::to_vec(&event) else {
					continue;
				};
				let mut sealed = session.seal_out(AD_EVENT, &bytes);
				sealed["sid"] = Value::String(sid.clone());
				if tx.send(sealed).await.is_err() {
					return;
				}
			}
		});
		Ok(rx)
	}
}

/// [`BridgeRpcHandler`] that terminates E2E and gates plaintext.
pub struct E2eRpc {
	inner: Arc<dyn BridgeRpcHandler>,
	endpoint: Arc<E2eEndpoint>,
	/// Relay path: only E2E methods (plus `workspace_launch`) pass.
	strict: bool,
}

impl E2eRpc {
	pub fn new(inner: Arc<dyn BridgeRpcHandler>, endpoint: Arc<E2eEndpoint>, strict: bool) -> Self {
		Self {
			inner,
			endpoint,
			strict,
		}
	}
}

#[async_trait::async_trait]
impl BridgeRpcHandler for E2eRpc {
	async fn dispatch(&self, method: &str, params: Value) -> Result<Value, String> {
		match method {
			"e2e_pair" => self.endpoint.handle_pair(params),
			"e2e_open" => self.endpoint.handle_open(params),
			"e2e_call" => self.endpoint.handle_call(params, &*self.inner).await,
			// Routed by the relay to any live process of the IDE, so it
			// can't sit inside a per-process session. Starting an
			// existing workspace is the whole of what it grants.
			"workspace_launch" => self.inner.dispatch(method, params).await,
			other if self.strict => Err(format!(
				"refusing plaintext `{other}`: this IDE only accepts end-to-end encrypted calls over the relay — re-pair the phone"
			)),
			other => self.inner.dispatch(other, params).await,
		}
	}

	async fn subscribe(&self, method: &str, params: Value) -> Result<tokio::sync::mpsc::Receiver<Value>, String> {
		if params.get("sid").is_some() {
			return self.endpoint.subscribe(method, &params, &*self.inner).await;
		}
		if self.strict {
			return Err("refusing a plaintext event stream over the relay — re-pair the phone".into());
		}
		self.inner.subscribe(method, params).await
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	struct Echo;

	#[async_trait::async_trait]
	impl BridgeRpcHandler for Echo {
		async fn dispatch(&self, method: &str, params: Value) -> Result<Value, String> {
			if method == "fail" {
				return Err("boom".into());
			}
			Ok(json!({ "method": method, "params": params }))
		}

		async fn subscribe(&self, _method: &str, _params: Value) -> Result<tokio::sync::mpsc::Receiver<Value>, String> {
			let (tx, rx) = tokio::sync::mpsc::channel(4);
			tokio::spawn(async move {
				let _ = tx.send(json!({ "event": { "kind": "hello" } })).await;
			});
			Ok(rx)
		}
	}

	/// Phone side, written with snow so the IDE side is tested against
	/// a reference Noise implementation (the PWA's own is checked
	/// against the fixture in `vectors`).
	struct Phone {
		private: Vec<u8>,
	}

	struct PhoneSession {
		sid: String,
		send: [u8; 32],
		recv: [u8; 32],
		n: u64,
	}

	impl Phone {
		fn new() -> Self {
			let kp = snow::Builder::new(SESSION_PATTERN.parse().unwrap())
				.generate_keypair()
				.unwrap();
			Self { private: kp.private }
		}

		fn pair(&self, ep: &E2eEndpoint, ide_key: &str, secret: &str) -> Result<Value, String> {
			let rs = B64.decode(ide_key).unwrap();
			let psk: [u8; 32] = B64.decode(secret).unwrap().try_into().unwrap();
			let mut hs = snow::Builder::new(PAIR_PATTERN.parse().unwrap())
				.local_private_key(&self.private)
				.unwrap()
				.remote_public_key(&rs)
				.unwrap()
				.prologue(PAIR_PROLOGUE)
				.unwrap()
				.psk(2, &psk)
				.unwrap()
				.build_initiator()
				.unwrap();
			let mut buf = vec![0u8; 65535];
			let len = hs.write_message(br#"{"label":"test phone"}"#, &mut buf).unwrap();
			let reply = ep.handle_pair(json!({ "invite": invite_id(secret), "msg": B64.encode(&buf[..len]) }))?;
			let msg2 = B64.decode(reply["msg"].as_str().unwrap()).unwrap();
			let len = hs.read_message(&msg2, &mut buf).map_err(|e| e.to_string())?;
			Ok(serde_json::from_slice(&buf[..len]).unwrap())
		}

		fn open(&self, ep: &E2eEndpoint, ide_key: &str, workspace: &str) -> Result<PhoneSession, String> {
			let rs = B64.decode(ide_key).unwrap();
			let prologue = session_prologue(workspace);
			let mut hs = snow::Builder::new(SESSION_PATTERN.parse().unwrap())
				.local_private_key(&self.private)
				.unwrap()
				.remote_public_key(&rs)
				.unwrap()
				.prologue(&prologue)
				.unwrap()
				.build_initiator()
				.unwrap();
			let mut buf = vec![0u8; 65535];
			let len = hs.write_message(&[], &mut buf).unwrap();
			let reply = ep.handle_open(json!({ "msg": B64.encode(&buf[..len]) }))?;
			let msg2 = B64.decode(reply["msg"].as_str().unwrap()).unwrap();
			hs.read_message(&msg2, &mut buf).map_err(|e| e.to_string())?;
			let (send, recv) = hs.dangerously_get_raw_split();
			Ok(PhoneSession {
				sid: reply["sid"].as_str().unwrap().to_string(),
				send,
				recv,
				n: 0,
			})
		}
	}

	impl PhoneSession {
		fn call_frame(&mut self, id: u64, method: &str, params: Value) -> Value {
			let n = self.n;
			self.n += 1;
			let plain = serde_json::to_vec(&json!({ "id": id, "method": method, "params": params })).unwrap();
			json!({ "sid": self.sid, "n": n, "c": B64.encode(seal(&self.send, n, AD_CALL, &plain)) })
		}

		fn read(&self, frame: &Value, ad: &[u8]) -> Value {
			let n = frame["n"].as_u64().unwrap();
			let ct = B64.decode(frame["c"].as_str().unwrap()).unwrap();
			serde_json::from_slice(&open(&self.recv, n, ad, &ct).expect("decrypts")).unwrap()
		}
	}

	fn endpoint() -> Arc<E2eEndpoint> {
		Arc::new(E2eEndpoint::new(Arc::new(MemoryStore::default()), "ws1", "test-ide"))
	}

	fn paired() -> (Arc<E2eEndpoint>, Phone, String) {
		let ep = endpoint();
		let phone = Phone::new();
		let key = ep.public_key().unwrap();
		let invite = ep.mint_invite().unwrap();
		phone.pair(&ep, &key, &invite.secret).unwrap();
		(ep, phone, key)
	}

	#[test]
	fn pairing_authorizes_the_phone_and_burns_the_invite() {
		let ep = endpoint();
		let phone = Phone::new();
		let key = ep.public_key().unwrap();
		let invite = ep.mint_invite().unwrap();
		let reply = phone.pair(&ep, &key, &invite.secret).unwrap();
		assert_eq!(reply["ide_label"], "test-ide");
		let devices = ep.devices().unwrap();
		assert_eq!(devices.len(), 1);
		assert_eq!(devices[0].label, "test phone");
		assert_eq!(reply["device_id"], devices[0].id.as_str());

		let again = Phone::new().pair(&ep, &key, &invite.secret);
		assert!(again.unwrap_err().contains("expired or already used"));
	}

	#[test]
	fn pairing_with_a_wrong_secret_fails_on_the_phone() {
		let ep = endpoint();
		let key = ep.public_key().unwrap();
		let real = ep.mint_invite().unwrap();
		// Phone knows the invite id but holds a different PSK: the IDE
		// answers, the phone can't authenticate the reply.
		let phone = Phone::new();
		let rs = B64.decode(&key).unwrap();
		let mut hs = snow::Builder::new(PAIR_PATTERN.parse().unwrap())
			.local_private_key(&phone.private)
			.unwrap()
			.remote_public_key(&rs)
			.unwrap()
			.prologue(PAIR_PROLOGUE)
			.unwrap()
			.psk(2, &[7u8; 32])
			.unwrap()
			.build_initiator()
			.unwrap();
		let mut buf = vec![0u8; 65535];
		let len = hs.write_message(b"{}", &mut buf).unwrap();
		let reply = ep
			.handle_pair(json!({ "invite": invite_id(&real.secret), "msg": B64.encode(&buf[..len]) }))
			.unwrap();
		let msg2 = B64.decode(reply["msg"].as_str().unwrap()).unwrap();
		assert!(hs.read_message(&msg2, &mut buf).is_err());
	}

	#[test]
	fn unpaired_phone_cannot_open_a_session() {
		let (ep, _, key) = paired();
		let stranger = Phone::new();
		let err = stranger.open(&ep, &key, "ws1").err().unwrap();
		assert!(err.contains("not paired"), "{err}");
	}

	#[test]
	fn session_for_another_workspace_is_rejected() {
		let (ep, phone, key) = paired();
		assert!(phone.open(&ep, &key, "other-ws").is_err());
	}

	#[tokio::test]
	async fn encrypted_call_roundtrip_and_replay_rejection() {
		let (ep, phone, key) = paired();
		let rpc = E2eRpc::new(Arc::new(Echo), ep.clone(), true);
		let mut s = phone.open(&ep, &key, "ws1").unwrap();

		let frame = s.call_frame(41, "coder_status", json!({ "x": 1 }));
		let reply = rpc.dispatch("e2e_call", frame.clone()).await.unwrap();
		let plain = s.read(&reply, AD_RESULT);
		assert_eq!(plain["id"], 41);
		assert_eq!(plain["ok"]["method"], "coder_status");
		assert_eq!(plain["ok"]["params"]["x"], 1);

		let replay = rpc.dispatch("e2e_call", frame).await.unwrap_err();
		assert!(replay.contains("replayed"), "{replay}");

		let frame = s.call_frame(42, "fail", Value::Null);
		let plain = s.read(&rpc.dispatch("e2e_call", frame).await.unwrap(), AD_RESULT);
		assert_eq!(plain["err"], "boom");
	}

	#[tokio::test]
	async fn tampered_or_mistyped_frames_fail_authentication() {
		let (ep, phone, key) = paired();
		let rpc = E2eRpc::new(Arc::new(Echo), ep.clone(), true);
		let mut s = phone.open(&ep, &key, "ws1").unwrap();
		let n = s.n;
		s.n += 1;
		// Sealed as an event, presented as a call.
		let plain = br#"{"id":1,"method":"x"}"#;
		let frame = json!({ "sid": s.sid, "n": n, "c": B64.encode(seal(&s.send, n, AD_EVENT, plain)) });
		let err = rpc.dispatch("e2e_call", frame).await.unwrap_err();
		assert!(err.contains("authentication"), "{err}");
		// The failed frame didn't burn the slot.
		let frame = json!({ "sid": s.sid, "n": n, "c": B64.encode(seal(&s.send, n, AD_CALL, plain)) });
		assert!(rpc.dispatch("e2e_call", frame).await.is_ok());
	}

	#[tokio::test]
	async fn revoked_phone_loses_its_sessions() {
		let (ep, phone, key) = paired();
		let rpc = E2eRpc::new(Arc::new(Echo), ep.clone(), true);
		let mut s = phone.open(&ep, &key, "ws1").unwrap();
		let id = ep.devices().unwrap()[0].id.clone();
		assert!(ep.revoke(&id).unwrap());
		let frame = s.call_frame(1, "coder_status", Value::Null);
		assert_eq!(rpc.dispatch("e2e_call", frame).await.unwrap_err(), UNKNOWN_SESSION);
		assert!(phone.open(&ep, &key, "ws1").is_err());
	}

	#[tokio::test]
	async fn strict_gate_refuses_plaintext_but_lets_launch_through() {
		let ep = endpoint();
		let strict = E2eRpc::new(Arc::new(Echo), ep.clone(), true);
		assert!(strict
			.dispatch("coder_status", Value::Null)
			.await
			.unwrap_err()
			.contains("refusing plaintext"));
		assert!(strict.dispatch("workspace_launch", Value::Null).await.is_ok());
		assert!(strict.subscribe("coder_events", Value::Null).await.is_err());
		assert_eq!(
			strict
				.dispatch("e2e_call", json!({ "sid": "nope", "n": 0, "c": "" }))
				.await
				.unwrap_err(),
			UNKNOWN_SESSION
		);

		let lenient = E2eRpc::new(Arc::new(Echo), ep, false);
		assert!(lenient.dispatch("coder_status", Value::Null).await.is_ok());
		assert!(lenient.subscribe("coder_events", Value::Null).await.is_ok());
	}

	#[tokio::test]
	async fn event_stream_is_sealed() {
		let (ep, phone, key) = paired();
		let rpc = E2eRpc::new(Arc::new(Echo), ep.clone(), true);
		let s = phone.open(&ep, &key, "ws1").unwrap();
		let mut rx = rpc.subscribe("coder_events", json!({ "sid": s.sid })).await.unwrap();
		let frame = rx.recv().await.unwrap();
		assert_eq!(frame["sid"], s.sid.as_str());
		assert!(frame.get("event").is_none(), "plaintext leaked: {frame}");
		assert_eq!(s.read(&frame, AD_EVENT)["event"]["kind"], "hello");
	}

	#[test]
	fn replay_window_accepts_reordering_within_the_window() {
		let mut w = ReplayWindow::default();
		for n in [5u64, 3, 4, 0, 2100, 100] {
			assert!(w.fresh(n), "{n}");
			w.mark(n);
			assert!(!w.fresh(n), "{n} replay");
		}
		// 3 is now more than a window behind 2100.
		assert!(!w.fresh(3));
		assert!(w.fresh(2099));
	}

	/// Deterministic handshake + transport transcript from snow with
	/// fixed keys, shared with the PWA's vitest so its hand-written
	/// Noise initiator stays byte-identical to the IDE side. Regenerate
	/// with `MOON_WRITE_E2E_VECTORS=1 cargo test -p moon-remote vectors`.
	#[test]
	fn vectors() {
		let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
		let key = |seed: u8| [seed; 32];
		let (ide_priv, phone_priv) = (key(1), key(2));
		let (phone_eph, ide_eph, phone_eph2, ide_eph2) = (key(3), key(4), key(5), key(6));
		let psk = key(7);
		let pair_builder = || snow::Builder::new(PAIR_PATTERN.parse().unwrap());
		let pubkey = |private: &[u8; 32]| x25519_dalek::x25519(*private, x25519_dalek::X25519_BASEPOINT_BYTES);
		let ide_pub = pubkey(&ide_priv);
		let phone_pub = pubkey(&phone_priv);

		let mut buf = vec![0u8; 65535];
		let mut out = vec![0u8; 65535];
		let pair_payload = br#"{"label":"vector phone"}"#;
		let mut init = pair_builder()
			.local_private_key(&phone_priv)
			.unwrap()
			.remote_public_key(&ide_pub)
			.unwrap()
			.prologue(PAIR_PROLOGUE)
			.unwrap()
			.psk(2, &psk)
			.unwrap()
			.fixed_ephemeral_key_for_testing_only(&phone_eph)
			.build_initiator()
			.unwrap();
		let mut resp = pair_builder()
			.local_private_key(&ide_priv)
			.unwrap()
			.prologue(PAIR_PROLOGUE)
			.unwrap()
			.psk(2, &psk)
			.unwrap()
			.fixed_ephemeral_key_for_testing_only(&ide_eph)
			.build_responder()
			.unwrap();
		let n1 = init.write_message(pair_payload, &mut buf).unwrap();
		let pair_msg1 = buf[..n1].to_vec();
		resp.read_message(&pair_msg1, &mut out).unwrap();
		let pair_reply = br#"{"device_id":"d1","ide_label":"vector ide"}"#;
		let n2 = resp.write_message(pair_reply, &mut buf).unwrap();
		let pair_msg2 = buf[..n2].to_vec();
		init.read_message(&pair_msg2, &mut out).unwrap();

		let prologue = session_prologue("ws1");
		let session_builder = || snow::Builder::new(SESSION_PATTERN.parse().unwrap());
		let mut init = session_builder()
			.local_private_key(&phone_priv)
			.unwrap()
			.remote_public_key(&ide_pub)
			.unwrap()
			.prologue(&prologue)
			.unwrap()
			.fixed_ephemeral_key_for_testing_only(&phone_eph2)
			.build_initiator()
			.unwrap();
		let mut resp = session_builder()
			.local_private_key(&ide_priv)
			.unwrap()
			.prologue(&prologue)
			.unwrap()
			.fixed_ephemeral_key_for_testing_only(&ide_eph2)
			.build_responder()
			.unwrap();
		let n1 = init.write_message(&[], &mut buf).unwrap();
		let open_msg1 = buf[..n1].to_vec();
		resp.read_message(&open_msg1, &mut out).unwrap();
		let n2 = resp.write_message(&[], &mut buf).unwrap();
		let open_msg2 = buf[..n2].to_vec();
		init.read_message(&open_msg2, &mut out).unwrap();
		let (i2r, r2i) = init.dangerously_get_raw_split();
		assert_eq!((i2r, r2i), resp.dangerously_get_raw_split());

		let call = br#"{"id":1,"method":"coder_status","params":null}"#;
		let result = br#"{"id":1,"ok":{"ready":true}}"#;
		let event = br#"{"event":{"kind":"hello"}}"#;
		let v = json!({
			"pair_pattern": PAIR_PATTERN,
			"session_pattern": SESSION_PATTERN,
			"pair_prologue": hex(PAIR_PROLOGUE),
			"session_prologue": hex(&prologue),
			"ide_private": hex(&ide_priv),
			"ide_public": hex(&ide_pub),
			"phone_private": hex(&phone_priv),
			"phone_public": hex(&phone_pub),
			"pair_phone_ephemeral": hex(&phone_eph),
			"pair_psk": hex(&psk),
			"pair_payload": hex(pair_payload),
			"pair_msg1": hex(&pair_msg1),
			"pair_reply": hex(pair_reply),
			"pair_msg2": hex(&pair_msg2),
			"open_phone_ephemeral": hex(&phone_eph2),
			"open_msg1": hex(&open_msg1),
			"open_msg2": hex(&open_msg2),
			"key_i2r": hex(&i2r),
			"key_r2i": hex(&r2i),
			"call_n": 7,
			"call_plain": hex(call),
			"call_sealed": hex(&seal(&i2r, 7, AD_CALL, call)),
			"result_n": 3,
			"result_plain": hex(result),
			"result_sealed": hex(&seal(&r2i, 3, AD_RESULT, result)),
			"event_n": 4,
			"event_plain": hex(event),
			"event_sealed": hex(&seal(&r2i, 4, AD_EVENT, event)),
			"invite_secret": "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc",
			"invite_id": invite_id("BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc"),
		});
		let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../companion/src/lib/e2e-vectors.json");
		let rendered = format!("{}\n", serde_json::to_string_pretty(&v).unwrap());
		if std::env::var_os("MOON_WRITE_E2E_VECTORS").is_some() {
			std::fs::write(&path, &rendered).unwrap();
		}
		let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
		assert_eq!(
			on_disk, rendered,
			"e2e vectors drifted — regenerate with MOON_WRITE_E2E_VECTORS=1"
		);
	}

	#[test]
	fn pair_link_escapes_fragment_values() {
		let link = pair_link("wss://bridge.example/", "AB12-CD34", "my ide", "ws1", "k_-", "s");
		assert_eq!(
			link,
			"https://bridge.example/#pair=AB12-CD34&ide=my%20ide&ws=ws1&k=k_-&s=s"
		);
	}
}
