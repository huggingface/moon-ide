//! File-backed secret store for headless processes
//! ([ADR 0128](../../../specs/decisions/0128-file-backed-secrets-for-headless.md)).
//!
//! Every secret in moon-ide goes through `keyring::Entry`. On a
//! desktop that is the OS keyring; a server or a pod has none, so
//! `moon-remote` and `moon-bridge` install this store as the keyring
//! crate's default backend when [`ENV_SECRETS_DIR`] is set. Call
//! sites are unchanged.
//!
//! Two directories, one file per account:
//!
//! - the **store** is writable and holds what the process mints or
//!   rotates itself (HF sign-in, relay token, pairing and e2e keys);
//! - the optional **managed** directory is read-only and holds what
//!   the operator provisions (API keys mounted from a secret manager).
//!   A managed entry shadows the store and refuses writes, so a key
//!   set from the UI can never silently diverge from the provisioned
//!   one.

use std::any::Any;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use keyring::credential::{Credential, CredentialApi, CredentialBuilderApi, CredentialPersistence};

/// Writable directory. Setting it switches the process to file secrets.
pub const ENV_SECRETS_DIR: &str = "MOON_SECRETS_DIR";
/// Read-only directory of operator-provisioned secrets. Needs
/// [`ENV_SECRETS_DIR`] too.
pub const ENV_MANAGED_SECRETS_DIR: &str = "MOON_MANAGED_SECRETS_DIR";

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Installs the file store as the process-wide keyring backend when
/// the environment asks for it. Returns whether it did. Must run
/// before the first `keyring::Entry` is created.
pub fn install_from_env() -> anyhow::Result<bool> {
	let store = dir_from_env(ENV_SECRETS_DIR);
	let managed = dir_from_env(ENV_MANAGED_SECRETS_DIR);
	let Some(store) = store else {
		if managed.is_some() {
			anyhow::bail!("{ENV_MANAGED_SECRETS_DIR} is set without {ENV_SECRETS_DIR}");
		}
		return Ok(false);
	};
	let builder = FileSecrets::open(store, managed)?;
	tracing::info!(
		store = %builder.store.display(),
		managed = ?builder.managed.as_deref().map(Path::display),
		"using file-backed secrets"
	);
	keyring::set_default_credential_builder(Box::new(builder));
	Ok(true)
}

fn dir_from_env(name: &str) -> Option<PathBuf> {
	let value = std::env::var_os(name)?;
	if value.is_empty() {
		return None;
	}
	Some(PathBuf::from(value))
}

/// The keyring backend: builds one [`FileCredential`] per account.
#[derive(Debug, Clone)]
pub struct FileSecrets {
	store: PathBuf,
	managed: Option<PathBuf>,
}

impl FileSecrets {
	/// Creates the store directory (owner-only) when missing. An
	/// existing directory keeps its mode: on a mounted volume it is
	/// the operator's to set.
	pub fn open(store: PathBuf, managed: Option<PathBuf>) -> anyhow::Result<Self> {
		if !store.is_dir() {
			let mut dir = std::fs::DirBuilder::new();
			dir.recursive(true);
			#[cfg(unix)]
			{
				use std::os::unix::fs::DirBuilderExt;
				dir.mode(0o700);
			}
			dir
				.create(&store)
				.with_context(|| format!("creating secrets directory {}", store.display()))?;
		}
		if let Some(missing) = managed.as_deref().filter(|dir| !dir.is_dir()) {
			anyhow::bail!("managed secrets directory {} does not exist", missing.display());
		}
		Ok(Self { store, managed })
	}
}

impl CredentialBuilderApi for FileSecrets {
	/// `service` is ignored: the directory is already this install's
	/// namespace, and every caller passes the same one.
	fn build(&self, _target: Option<&str>, _service: &str, user: &str) -> keyring::Result<Box<Credential>> {
		let name = file_name(user)?;
		Ok(Box::new(FileCredential {
			path: self.store.join(&name),
			managed: self.managed.as_ref().map(|dir| dir.join(&name)),
		}))
	}

	fn as_any(&self) -> &dyn Any {
		self
	}

	fn persistence(&self) -> CredentialPersistence {
		CredentialPersistence::UntilDelete
	}
}

/// Maps a keyring account to a file name. The result stays inside
/// `[-._a-zA-Z0-9]` — the key charset of a Kubernetes Secret — so a
/// mounted Secret can provide any account as-is. `:` (the separator
/// in `coder-provider:<id>`) becomes `.`; a leading `.` is refused
/// because those names are ours (temp files) and the kubelet's
/// (`..data`).
pub fn file_name(account: &str) -> keyring::Result<String> {
	let invalid = |reason: &str| keyring::Error::Invalid("user".into(), reason.into());
	if account.is_empty() {
		return Err(invalid("empty account"));
	}
	let name: String = account.chars().map(|c| if c == ':' { '.' } else { c }).collect();
	if name.starts_with('.') {
		return Err(invalid("account may not start with '.' or ':'"));
	}
	if !name
		.chars()
		.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
	{
		return Err(invalid(
			"account may only contain letters, digits, '-', '_', '.' and ':'",
		));
	}
	Ok(name)
}

#[derive(Debug)]
struct FileCredential {
	path: PathBuf,
	managed: Option<PathBuf>,
}

impl FileCredential {
	fn managed_secret(&self) -> keyring::Result<Option<Vec<u8>>> {
		let Some(path) = &self.managed else {
			return Ok(None);
		};
		let Some(mut secret) = read_optional(path)? else {
			return Ok(None);
		};
		// Secret managers and `echo` both tend to leave one.
		if secret.last() == Some(&b'\n') {
			secret.pop();
			if secret.last() == Some(&b'\r') {
				secret.pop();
			}
		}
		Ok(Some(secret))
	}

	fn refuse_if_managed(&self) -> keyring::Result<()> {
		let Some(path) = &self.managed else {
			return Ok(());
		};
		if !path.exists() {
			return Ok(());
		}
		Err(keyring::Error::NoStorageAccess(
			format!(
				"{} is a managed secret; change it where it is provisioned",
				path.display()
			)
			.into(),
		))
	}
}

impl CredentialApi for FileCredential {
	fn set_secret(&self, secret: &[u8]) -> keyring::Result<()> {
		self.refuse_if_managed()?;
		write_atomic(&self.path, secret).map_err(platform)
	}

	fn get_secret(&self) -> keyring::Result<Vec<u8>> {
		if let Some(secret) = self.managed_secret()? {
			return Ok(secret);
		}
		read_optional(&self.path)?.ok_or(keyring::Error::NoEntry)
	}

	fn delete_credential(&self) -> keyring::Result<()> {
		self.refuse_if_managed()?;
		match std::fs::remove_file(&self.path) {
			Ok(()) => Ok(()),
			Err(err) if err.kind() == io::ErrorKind::NotFound => Err(keyring::Error::NoEntry),
			Err(err) => Err(platform(err)),
		}
	}

	fn as_any(&self) -> &dyn Any {
		self
	}
}

fn platform(err: io::Error) -> keyring::Error {
	keyring::Error::PlatformFailure(Box::new(err))
}

fn read_optional(path: &Path) -> keyring::Result<Option<Vec<u8>>> {
	match std::fs::read(path) {
		Ok(bytes) => Ok(Some(bytes)),
		Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
		Err(err) => Err(platform(err)),
	}
}

/// Temp file in the same directory, then rename: a crash mid-write
/// leaves the previous secret, never a truncated one.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
	let dir = path
		.parent()
		.ok_or_else(|| io::Error::other("secret path has no parent"))?;
	let name = path
		.file_name()
		.ok_or_else(|| io::Error::other("secret path has no file name"))?
		.to_string_lossy();
	let temp = dir.join(format!(
		".{name}.{}-{}.tmp",
		std::process::id(),
		TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
	));
	let mut options = std::fs::OpenOptions::new();
	options.write(true).create_new(true);
	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt;
		options.mode(0o600);
	}
	let written = options.open(&temp).and_then(|mut file| {
		file.write_all(bytes)?;
		file.sync_all()
	});
	let renamed = written.and_then(|()| std::fs::rename(&temp, path));
	if renamed.is_err() {
		let _ = std::fs::remove_file(&temp);
	}
	renamed
}

#[cfg(test)]
mod tests {
	use super::*;

	fn credential(secrets: &FileSecrets, account: &str) -> Box<Credential> {
		secrets.build(None, "moon-ide", account).expect("build credential")
	}

	fn store_only(dir: &Path) -> FileSecrets {
		FileSecrets::open(dir.join("store"), None).expect("open store")
	}

	fn with_managed(dir: &Path) -> FileSecrets {
		let managed = dir.join("managed");
		std::fs::create_dir(&managed).expect("create managed dir");
		FileSecrets::open(dir.join("store"), Some(managed)).expect("open store")
	}

	#[test]
	fn secret_round_trips_and_deletes() {
		let dir = tempfile::tempdir().expect("tempdir");
		let secrets = store_only(dir.path());
		let entry = credential(&secrets, "hf-oauth");

		assert!(matches!(entry.get_password(), Err(keyring::Error::NoEntry)));
		entry.set_password("first").expect("set");
		entry.set_password("second\n").expect("overwrite");
		assert_eq!(entry.get_password().expect("get"), "second\n");

		// A second credential for the same account sees the same file.
		assert_eq!(
			credential(&secrets, "hf-oauth").get_password().expect("get"),
			"second\n"
		);

		entry.delete_credential().expect("delete");
		assert!(matches!(entry.get_password(), Err(keyring::Error::NoEntry)));
		assert!(matches!(entry.delete_credential(), Err(keyring::Error::NoEntry)));
	}

	#[test]
	fn writes_leave_no_temp_files() {
		let dir = tempfile::tempdir().expect("tempdir");
		let secrets = store_only(dir.path());
		credential(&secrets, "coder-provider:prov-1-ab")
			.set_password("key")
			.expect("set");

		let names: Vec<String> = std::fs::read_dir(dir.path().join("store"))
			.expect("read store")
			.map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
			.collect();
		assert_eq!(names, ["coder-provider.prov-1-ab"]);
	}

	#[cfg(unix)]
	#[test]
	fn store_and_files_are_owner_only() {
		use std::os::unix::fs::PermissionsExt;

		let dir = tempfile::tempdir().expect("tempdir");
		let secrets = store_only(dir.path());
		credential(&secrets, "remote-bridge")
			.set_password("token")
			.expect("set");

		let mode = |path: &Path| std::fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
		assert_eq!(mode(&dir.path().join("store")), 0o700);
		assert_eq!(mode(&dir.path().join("store/remote-bridge")), 0o600);
	}

	#[test]
	fn managed_entry_shadows_the_store_and_refuses_writes() {
		let dir = tempfile::tempdir().expect("tempdir");
		let secrets = with_managed(dir.path());
		let entry = credential(&secrets, "coder-web-search:tavily");
		entry.set_password("from-ui").expect("set before it is managed");

		std::fs::write(dir.path().join("managed/coder-web-search.tavily"), "provisioned\r\n").expect("write managed");

		assert_eq!(entry.get_password().expect("get"), "provisioned");
		assert!(matches!(
			entry.set_password("other"),
			Err(keyring::Error::NoStorageAccess(_))
		));
		assert!(matches!(
			entry.delete_credential(),
			Err(keyring::Error::NoStorageAccess(_))
		));
	}

	#[test]
	fn unmanaged_accounts_still_use_the_store() {
		let dir = tempfile::tempdir().expect("tempdir");
		let secrets = with_managed(dir.path());
		let entry = credential(&secrets, "companion-e2e-identity");

		entry.set_password("minted").expect("set");
		assert_eq!(entry.get_password().expect("get"), "minted");
		entry.delete_credential().expect("delete");
	}

	#[test]
	fn missing_managed_directory_is_an_error() {
		let dir = tempfile::tempdir().expect("tempdir");
		let result = FileSecrets::open(dir.path().join("store"), Some(dir.path().join("absent")));
		assert!(result.is_err());
	}

	#[test]
	fn account_names_map_to_secret_keys() {
		assert_eq!(file_name("hf-oauth").expect("plain"), "hf-oauth");
		assert_eq!(
			file_name("coder-slack:hugging-face").expect("namespaced"),
			"coder-slack.hugging-face"
		);

		for bad in ["", ".hidden", ":x", "../escape", "a/b", "a b", "é"] {
			assert!(
				matches!(file_name(bad), Err(keyring::Error::Invalid(_, _))),
				"{bad:?} should be refused"
			);
		}
	}
}
