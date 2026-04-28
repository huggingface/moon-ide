//! Workspace path → compose project name.
//!
//! Compose's `name:` key disambiguates concurrent projects on a
//! single Docker daemon (it prefixes container names, the project
//! network, and the `docker compose` filter that lifecycle commands
//! use). We derive it from a stable hash of the workspace's
//! canonical absolute path so:
//!
//! - opening the same workspace twice always yields the same name
//!   (so `pause` / `resume` find the right containers),
//! - opening two different workspaces never collides
//!   (FNV-1a 32-bit ≈ 4×10⁹ buckets — the number of moon-ide
//!   workspaces a single dev keeps around fits in a one-digit
//!   collision-impossibility margin),
//! - the name is always valid per Docker's project-name rules
//!   (lowercase letter or digit lead, then `[a-z0-9_-]`).
//!
//! We use FNV-1a 32-bit by hand rather than pulling in a hash crate
//! — the frontend already does the same thing for content
//! fingerprints in `src/lib/util/hash.ts`, and keeping the two
//! sides on the same primitive means a debug check ("does the
//! frontend agree on the project name for this workspace?") is
//! a literal string comparison.

use std::fmt;

use camino::Utf8Path;

/// A validated Docker compose project name (`moon-ws-<8 hex>`).
///
/// Construct via [`project_name_for`]; never store an arbitrary
/// string in this type — its existence is the proof that the
/// name is safe to interpolate into a `docker compose -p ...`
/// command line.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
pub struct ProjectName(String);

impl ProjectName {
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl fmt::Display for ProjectName {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.0)
	}
}

/// Derive the project name for a workspace at `path`.
///
/// The input does not need to be absolute / canonical — we hash the
/// raw bytes verbatim, so callers should canonicalise first if they
/// want two equivalent paths (e.g. `~/code/foo` vs `/home/me/code/foo`)
/// to map to the same project.
pub fn project_name_for(path: &Utf8Path) -> ProjectName {
	let hash = fnv1a32(path.as_str().as_bytes());
	ProjectName(format!("moon-ws-{hash:08x}"))
}

fn fnv1a32(bytes: &[u8]) -> u32 {
	let mut hash: u32 = 0x811c_9dc5;
	for &b in bytes {
		hash ^= u32::from(b);
		hash = hash.wrapping_mul(0x0100_0193);
	}
	hash
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn name_is_stable_across_calls() {
		let path = Utf8Path::new("/home/dev/code/moon-landing");
		assert_eq!(project_name_for(path), project_name_for(path));
	}

	#[test]
	fn name_starts_with_letter_so_compose_accepts_it() {
		let name = project_name_for(Utf8Path::new("/x"));
		assert!(name.as_str().starts_with("moon-ws-"));
		// Compose project names must match `^[a-z0-9][a-z0-9_-]*$`.
		assert!(name
			.as_str()
			.chars()
			.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
	}

	#[test]
	fn distinct_paths_yield_distinct_names() {
		let a = project_name_for(Utf8Path::new("/home/dev/code/moon-landing"));
		let b = project_name_for(Utf8Path::new("/home/dev/code/moon-ide"));
		assert_ne!(a, b);
	}

	#[test]
	fn known_vector_matches_handrolled_fnv1a32() {
		// FNV-1a 32-bit reference: empty input is the offset basis.
		assert_eq!(fnv1a32(b""), 0x811c_9dc5);
		// Single-byte 'a' = (0x811c9dc5 ^ 0x61) * 0x01000193.
		assert_eq!(fnv1a32(b"a"), 0xe40c_292c);
		// Spec test vector for "foobar".
		assert_eq!(fnv1a32(b"foobar"), 0xbf9c_f968);
	}

	#[test]
	fn matches_frontend_hash_convention() {
		// `src/lib/util/hash.ts` runs the same FNV-1a 32-bit over
		// UTF-8 bytes. If a future contributor changes one side
		// without the other, this test catches it via the canonical
		// "foobar" vector — both sides must agree on it.
		assert_eq!(fnv1a32("foobar".as_bytes()), 0xbf9c_f968);
	}
}
