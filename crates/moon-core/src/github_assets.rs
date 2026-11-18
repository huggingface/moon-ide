//! Images attached to pull requests and issues of a private
//! repository.
//!
//! GitHub stores them at `github.com/user-attachments/assets/<id>`
//! and serves them only to a browser signed in to github.com: the
//! IDE's webview gets a 404, and so does a request carrying an API
//! token. What works is a short-lived signed URL, which GitHub hands
//! out when it renders Markdown for an authenticated caller. So the
//! addresses are run through the Markdown API and the signed ones
//! read back out of the HTML.

use moon_protocol::{MoonError, MoonResult};

use crate::inbox::{run_gh, GhError};

/// Addresses signed in one call; more are left unsigned.
const MAX_ASSETS: usize = 40;

/// The asset id of an attachment address: the last path segment of
/// `https://github.com/user-attachments/assets/<id>` or of the older
/// `https://github.com/<owner>/<repo>/assets/<n>/<id>`. `None` for
/// any other URL, and for an id that isn't plain hex and dashes (it
/// goes into Markdown text).
pub fn asset_id(url: &str) -> Option<&str> {
	let path = url.strip_prefix("https://github.com/")?;
	let segments: Vec<&str> = path.split('/').collect();
	let id = match segments.as_slice() {
		["user-attachments", "assets", id] => id,
		[_owner, _repo, "assets", user, id] if user.bytes().all(|b| b.is_ascii_digit()) => id,
		_ => return None,
	};
	let plain = id.len() >= 8 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
	plain.then_some(*id)
}

/// Every `src="…"` of `html`, entities decoded as far as a URL has
/// them.
fn image_sources(html: &str) -> Vec<String> {
	let mut sources = Vec::new();
	let mut rest = html;
	while let Some(start) = rest.find("src=\"") {
		let after = &rest[start + 5..];
		let Some(end) = after.find('"') else {
			break;
		};
		sources.push(after[..end].replace("&amp;", "&"));
		rest = &after[end + 1..];
	}
	sources
}

/// For each of `urls`, the signed URL in `html` that serves it: the
/// one whose file name carries its asset id.
fn match_signed(urls: &[String], html: &str) -> Vec<Option<String>> {
	let sources = image_sources(html);
	urls
		.iter()
		.map(|url| {
			let id = asset_id(url)?;
			sources
				.iter()
				.find(|source| !source.starts_with("https://github.com/") && source.contains(id))
				.cloned()
		})
		.collect()
}

fn is_repo_name(repo: &str) -> bool {
	let plain = |part: &str| {
		!part.is_empty()
			&& part
				.bytes()
				.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
	};
	repo
		.split_once('/')
		.is_some_and(|(owner, name)| plain(owner) && plain(name))
}

/// A signed, loadable URL for each attachment address in `urls`, in
/// order, as GitHub gives them to the `gh` user for `repo`
/// (`owner/name`). They stop working after a few minutes. `None` for
/// an address that isn't an attachment or that GitHub wouldn't sign.
pub async fn sign_asset_urls(repo: &str, urls: &[String]) -> MoonResult<Vec<Option<String>>> {
	if !is_repo_name(repo) {
		return Err(MoonError::invalid(format!("not a repository name: {repo}")));
	}
	let wanted: Vec<&String> = urls
		.iter()
		.filter(|url| asset_id(url).is_some())
		.take(MAX_ASSETS)
		.collect();
	if wanted.is_empty() {
		return Ok(vec![None; urls.len()]);
	}
	let text: String = wanted.iter().map(|url| format!("![]({url})\n\n")).collect();
	let text_arg = format!("text={text}");
	let context_arg = format!("context={repo}");
	let output = run_gh(&["api", "markdown", "-f", "mode=gfm", "-f", &context_arg, "-f", &text_arg])
		.await
		.map_err(|err: GhError| MoonError::IoError(err.detail()))?;
	if !output.success {
		let reason = output.stderr.lines().next().unwrap_or("gh api markdown failed");
		return Err(MoonError::IoError(reason.to_owned()));
	}
	Ok(match_signed(urls, &String::from_utf8_lossy(&output.stdout)))
}

#[cfg(test)]
mod tests {
	use super::*;

	const NEW: &str = "https://github.com/user-attachments/assets/368a38b4-7a05-4c27-8c4b-c7b5f9349a7f";
	const OLD: &str = "https://github.com/acme/app/assets/15983122/0aa16e6e-8ce2-4b7b-88b8-f591ca5a4560";

	#[test]
	fn attachment_addresses_are_recognised_and_nothing_else() {
		assert_eq!(asset_id(NEW), Some("368a38b4-7a05-4c27-8c4b-c7b5f9349a7f"));
		assert_eq!(asset_id(OLD), Some("0aa16e6e-8ce2-4b7b-88b8-f591ca5a4560"));
		for other in [
			"https://github.com/acme/app/blob/main/logo.png",
			"https://github.com/acme/app/assets/someone/0aa16e6e-8ce2-4b7b",
			"https://user-images.githubusercontent.com/1/2-abc.png",
			"https://github.com/user-attachments/assets/x)![](https://evil.test/a",
			"https://github.com/user-attachments/assets/",
			"http://github.com/user-attachments/assets/368a38b4-7a05",
		] {
			assert_eq!(asset_id(other), None, "{other}");
		}
	}

	#[test]
	fn signed_urls_are_matched_back_by_asset_id() {
		// What the Markdown API answers, order swapped, with an image
		// it left alone.
		let html = "<p dir=\"auto\">\
			<a href=\"https://private-user-images.githubusercontent.com/1/2-0aa16e6e-8ce2-4b7b-88b8-f591ca5a4560.png?jwt=b&amp;x=1\">\
			<img src=\"https://private-user-images.githubusercontent.com/1/2-0aa16e6e-8ce2-4b7b-88b8-f591ca5a4560.png?jwt=b&amp;x=1\" alt=\"\"></a>\
			<img src=\"https://private-user-images.githubusercontent.com/1/9-368a38b4-7a05-4c27-8c4b-c7b5f9349a7f.png?jwt=a\">\
			<img src=\"https://github.com/user-attachments/assets/11111111-2222-3333-4444-555555555555\"></p>";
		let urls = [
			NEW.to_owned(),
			"https://example.test/logo.png".to_owned(),
			OLD.to_owned(),
			"https://github.com/user-attachments/assets/11111111-2222-3333-4444-555555555555".to_owned(),
		];
		assert_eq!(
			match_signed(&urls, html),
			[
				Some(
					"https://private-user-images.githubusercontent.com/1/9-368a38b4-7a05-4c27-8c4b-c7b5f9349a7f.png?jwt=a"
						.to_owned()
				),
				None,
				Some(
					"https://private-user-images.githubusercontent.com/1/2-0aa16e6e-8ce2-4b7b-88b8-f591ca5a4560.png?jwt=b&x=1"
						.to_owned()
				),
				None,
			]
		);
	}

	#[tokio::test]
	async fn odd_input_never_reaches_gh() {
		assert!(sign_asset_urls("acme/app; rm -rf", &[NEW.to_owned()]).await.is_err());
		assert!(sign_asset_urls("acme", &[NEW.to_owned()]).await.is_err());
		let none = sign_asset_urls("acme/app", &["https://example.test/a.png".to_owned()]).await;
		assert_eq!(none.unwrap(), [None]);
	}
}
