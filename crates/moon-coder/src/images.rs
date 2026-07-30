//! Encoding for image attachments — the one place that turns
//! captured pixels into the `data:` URL the wire carries.
//!
//! Screenshots reach us as PNG (playwright, MCP servers, OS capture
//! shortcuts) and PNG is the worst case for request size: flat UI
//! colour compresses well, but lossless WebP of the same pixels is
//! another ~40% smaller. That ratio is worth a C dependency because
//! an image is re-sent on *every* round-trip for the rest of the
//! session — a screenshot's bytes are paid once per turn, not once
//! — and because the HF router caps a request body at 5 MiB
//! (ADR 0049). Re-encoding at capture is also the only shrink that
//! leaves prompt caching alone: the bytes are settled before they
//! enter the history, so nothing has to rewrite the prefix later.
//!
//! Measured over a 21-screenshot session (1440x900 playwright
//! frames, 5.03 MB of base64): lossless WebP at `method = 4` landed
//! 2.85 MB at 0.22 s/frame, pixel-identical. JPEG is not an option
//! — q95 came out *larger* than the source PNG (flat colour is
//! JPEG's worst case) and lower qualities blur exactly the small
//! text and 1px borders a UI review is looking at. A 256-colour
//! palette is smaller still, but it bands gradients and shadows,
//! which is the same objection.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use base64::Engine;

use crate::inference::{ChatMessage, ImageAttachment};

/// libwebp effort knob: 0 (fast, weak) … 6 (slow, strong). Over the
/// sample above, 4 produced 2.14 MB of WebP against 6's 1.83 MB —
/// 85% of the win for a seventh of the CPU (0.22 s vs 1.6 s per
/// frame). Capture happens while the user waits, so 4 it is.
const WEBP_METHOD: i32 = 4;

/// Transport format for re-encoded attachments. Vision endpoints
/// accept WebP across Anthropic, OpenAI, Gemini and the vLLM
/// servers most HF-router providers run. If one ever rejects it,
/// setting this to `None` restores verbatim PNG passthrough.
const REENCODE_TO: Option<&str> = Some("image/webp");

const PNG_MIME: &str = "image/png";

/// Build an attachment from raw image bytes, re-encoding PNG
/// sources as lossless WebP when that comes out smaller.
///
/// CPU-bound (~0.2 s for a 1440x900 frame): call it from
/// [`tokio::task::spawn_blocking`], not straight off an async task.
pub(crate) fn attachment_from_bytes(bytes: &[u8], mime: &str) -> ImageAttachment {
	let shrunk = shrink(bytes, mime);
	let (payload, mime) = match (shrunk.as_deref(), REENCODE_TO) {
		(Some(webp), Some(webp_mime)) => (webp, webp_mime),
		_ => (bytes, mime),
	};
	ImageAttachment {
		data_url: format!(
			"data:{mime};base64,{}",
			base64::engine::general_purpose::STANDARD.encode(payload)
		),
		mime: mime.to_string(),
	}
}

/// Same, for a payload that already arrived base64-encoded (MCP
/// `image` blocks, pasted composer attachments). A payload we can't
/// decode rides on verbatim — the provider may still make sense of
/// it, and dropping the image the user is asking about would be a
/// worse failure than sending bytes we couldn't inspect.
pub(crate) fn attachment_from_base64(data: &str, mime: &str) -> ImageAttachment {
	let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else {
		return ImageAttachment {
			data_url: format!("data:{mime};base64,{data}"),
			mime: mime.to_string(),
		};
	};
	attachment_from_bytes(&bytes, mime)
}

/// Re-encode an attachment that's already in `data:` URL form,
/// passing it through untouched when it isn't a decodable PNG.
pub(crate) fn reencode(attachment: ImageAttachment) -> ImageAttachment {
	if attachment.mime != PNG_MIME {
		return attachment;
	}
	let Some((_, payload)) = attachment.data_url.split_once(";base64,") else {
		return attachment;
	};
	attachment_from_base64(payload, PNG_MIME)
}

/// Re-encode a batch off the async runtime. Empty in, empty out
/// without touching the thread pool.
pub(crate) async fn reencode_all(attachments: Vec<ImageAttachment>) -> Vec<ImageAttachment> {
	let mut out = Vec::with_capacity(attachments.len());
	for attachment in attachments {
		let before = attachment.data_url.len();
		// The clone is the fallback if the encode task dies: losing
		// a screenshot the user is asking about is worse than a
		// memcpy.
		let fallback = attachment.clone();
		let encoded = tokio::task::spawn_blocking(move || reencode(attachment))
			.await
			.unwrap_or_else(|err| {
				tracing::warn!(error = %err, "image re-encode task died; keeping the attachment verbatim");
				fallback
			});
		if encoded.data_url.len() < before {
			tracing::debug!(
				before,
				after = encoded.data_url.len(),
				"re-encoded image attachment as lossless webp"
			);
		}
		out.push(encoded);
	}
	out
}

/// How much base64 image payload we'll put on the wire before
/// dropping the oldest attachments, and how far back down to go
/// once we start.
///
/// The HF router rejects a request body over 5 MiB (moon-landing's
/// `express.json({ limit: "5MB" })` on `/v1/chat/completions`).
/// Everything else in a long session — tool output, source files,
/// the system prompt, tool definitions — is bounded by compaction
/// at 80% of the context window, so roughly 1 MB of text. Images
/// get the rest, minus margin.
///
/// The gap between `ceiling` and `floor` is what keeps prompt
/// caching viable. Dropping an image rewrites the prompt prefix
/// from that point on, so a plain "keep the newest N bytes" rule
/// would invalidate the cache on *every* new screenshot. Cutting
/// deep and rarely trades that for one recompute per ~2 MB of new
/// screenshots.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ImageWireBudget {
	pub(crate) ceiling: usize,
	pub(crate) floor: usize,
}

/// Raise these (or return `None` from
/// [`crate::inference::InferenceClient::image_wire_budget`]) when
/// the router's body limit goes up — nothing is destroyed by
/// elision, so a bigger budget immediately puts the images back.
pub(crate) const HF_IMAGE_WIRE_BUDGET: ImageWireBudget = ImageWireBudget {
	ceiling: 3_500_000,
	floor: 1_500_000,
};

/// Bytes of image payload `messages` would put on the wire, ignoring
/// anything already marked for elision.
pub(crate) fn wire_bytes(messages: &[ChatMessage], elided: &HashSet<u64>) -> usize {
	images_in_order(messages)
		.filter(|img| !elided.contains(&image_key(img)))
		.map(|img| img.data_url.len())
		.sum()
}

/// Mark oldest-first until the payload is back under `budget.floor`,
/// but only once it has crossed `budget.ceiling`. Returns how many
/// images were newly marked.
///
/// `elided` is the session's running set and only ever grows, which
/// is what makes the wire prefix stable between events. Keying on
/// the payload rather than a position keeps it correct across
/// compaction (which deletes messages outright) and incidentally
/// dedupes: two byte-identical screenshots share a key, so they
/// elide together.
pub(crate) fn plan_elision(messages: &[ChatMessage], budget: ImageWireBudget, elided: &mut HashSet<u64>) -> usize {
	let mut bytes = wire_bytes(messages, elided);
	if bytes <= budget.ceiling {
		return 0;
	}
	let mut newly_elided = 0;
	for image in images_in_order(messages) {
		if bytes <= budget.floor {
			break;
		}
		if !elided.insert(image_key(image)) {
			continue;
		}
		bytes = bytes.saturating_sub(image.data_url.len());
		newly_elided += 1;
	}
	newly_elided
}

/// Drop the marked images from a *copy* of the history that's about
/// to go on the wire, leaving a note in the text so the model reads
/// "there was an image here and it's gone" rather than silently
/// seeing a claim of an attachment it can't find.
///
/// The session's own history is untouched: the panel still renders
/// every screenshot, reload still replays them, and raising the
/// budget brings them straight back.
pub(crate) fn apply_elision(messages: &mut [ChatMessage], elided: &HashSet<u64>) {
	if elided.is_empty() {
		return;
	}
	for message in messages {
		let (content, images) = match message {
			ChatMessage::User { content, images } => (content, images),
			ChatMessage::Tool { content, images, .. } => (content, images),
			_ => continue,
		};
		let before = images.len();
		images.retain(|image| !elided.contains(&image_key(image)));
		let dropped = before - images.len();
		if dropped == 0 {
			continue;
		}
		content.push_str(&format!(
			"\n[{dropped} earlier image(s) dropped from context to keep the request under the provider's size limit — read the file again if you still need to look at it]"
		));
	}
}

fn images_in_order(messages: &[ChatMessage]) -> impl Iterator<Item = &ImageAttachment> {
	messages.iter().flat_map(|message| match message {
		ChatMessage::User { images, .. } | ChatMessage::Tool { images, .. } => images.as_slice(),
		_ => &[],
	})
}

fn image_key(image: &ImageAttachment) -> u64 {
	let mut hasher = std::collections::hash_map::DefaultHasher::new();
	image.data_url.hash(&mut hasher);
	hasher.finish()
}

/// `Some(webp)` only when re-encoding is on, the source is a PNG we
/// can decode, and the result is actually smaller. JPEG and WebP
/// sources are already compressed — transcoding those losslessly
/// inflates them, so they're left alone.
fn shrink(bytes: &[u8], mime: &str) -> Option<Vec<u8>> {
	REENCODE_TO?;
	if mime != PNG_MIME {
		return None;
	}
	let webp = png_to_lossless_webp(bytes)?;
	(webp.len() < bytes.len()).then_some(webp)
}

/// Decode a PNG and re-encode it as lossless WebP. `None` for
/// anything that isn't 8-bit RGB / RGBA after the standard
/// expansions — 16-bit and greyscale PNGs are rare as screenshots
/// and small enough that the conversion isn't worth the extra
/// channel shuffling.
fn png_to_lossless_webp(bytes: &[u8]) -> Option<Vec<u8>> {
	let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
	decoder.set_transformations(png::Transformations::normalize_to_color8());
	let mut reader = decoder.read_info().ok()?;
	let mut buf = vec![0; reader.output_buffer_size()?];
	let info = reader.next_frame(&mut buf).ok()?;
	buf.truncate(info.buffer_size());

	let encoder = match reader.output_color_type() {
		(png::ColorType::Rgb, png::BitDepth::Eight) => webp::Encoder::from_rgb(&buf, info.width, info.height),
		(png::ColorType::Rgba, png::BitDepth::Eight) => webp::Encoder::from_rgba(&buf, info.width, info.height),
		_ => return None,
	};
	let mut config = webp::WebPConfig::new().ok()?;
	config.lossless = 1;
	// In lossless mode libwebp reads `quality` as how hard to try
	// rather than how much to throw away, so 100 costs time, not
	// fidelity.
	config.quality = 100.0;
	config.method = WEBP_METHOD;
	Some(encoder.encode_advanced(&config).ok()?.to_vec())
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A gradient over a flat field: compresses like a screenshot
	/// (large uniform areas) without being so trivial that PNG
	/// already wins.
	fn sample_png(width: u32, height: u32) -> (Vec<u8>, Vec<u8>) {
		let mut pixels = Vec::with_capacity((width * height * 3) as usize);
		for y in 0..height {
			for x in 0..width {
				let banded = if x < width / 2 {
					[30, 60, 120]
				} else {
					[(x % 256) as u8, (y % 256) as u8, 200]
				};
				pixels.extend_from_slice(&banded);
			}
		}
		let mut out = Vec::new();
		let mut encoder = png::Encoder::new(std::io::Cursor::new(&mut out), width, height);
		encoder.set_color(png::ColorType::Rgb);
		encoder.set_depth(png::BitDepth::Eight);
		encoder.write_header().unwrap().write_image_data(&pixels).unwrap();
		(out, pixels)
	}

	#[test]
	fn png_attachments_become_smaller_lossless_webp() {
		let (png, pixels) = sample_png(320, 240);
		let attachment = attachment_from_bytes(&png, PNG_MIME);
		assert_eq!(attachment.mime, "image/webp");
		assert!(
			attachment.data_url.starts_with("data:image/webp;base64,"),
			"data URL should advertise the re-encoded mime: {}",
			&attachment.data_url[..40]
		);

		let payload = attachment.data_url.split_once(";base64,").unwrap().1;
		let webp = base64::engine::general_purpose::STANDARD.decode(payload).unwrap();
		assert!(
			webp.len() < png.len(),
			"webp {} should undercut png {}",
			webp.len(),
			png.len()
		);

		// The whole point is that the model sees the same pixels.
		let decoded = webp::Decoder::new(&webp).decode().expect("webp decodes");
		assert_eq!(
			&*decoded,
			pixels.as_slice(),
			"lossless re-encode must be pixel-identical"
		);
	}

	#[test]
	fn non_png_sources_pass_through_untouched() {
		// Pretend-JPEG bytes: re-encoding a lossy source losslessly
		// would inflate it, so the source has to survive verbatim.
		let jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4];
		let attachment = attachment_from_bytes(&jpeg, "image/jpeg");
		assert_eq!(attachment.mime, "image/jpeg");
		let payload = attachment.data_url.split_once(";base64,").unwrap().1;
		assert_eq!(base64::engine::general_purpose::STANDARD.decode(payload).unwrap(), jpeg);
	}

	#[test]
	fn undecodable_png_keeps_the_original_bytes() {
		let garbage = b"not a png at all".to_vec();
		let attachment = attachment_from_bytes(&garbage, PNG_MIME);
		assert_eq!(attachment.mime, PNG_MIME);
		let payload = attachment.data_url.split_once(";base64,").unwrap().1;
		assert_eq!(
			base64::engine::general_purpose::STANDARD.decode(payload).unwrap(),
			garbage
		);
	}

	#[test]
	fn reencode_rewrites_a_data_url_in_place() {
		let (png, _) = sample_png(160, 120);
		let original = ImageAttachment {
			data_url: format!(
				"data:image/png;base64,{}",
				base64::engine::general_purpose::STANDARD.encode(&png)
			),
			mime: PNG_MIME.to_string(),
		};
		let shrunk = reencode(original.clone());
		assert_eq!(shrunk.mime, "image/webp");
		assert!(shrunk.data_url.len() < original.data_url.len());
	}

	#[test]
	fn reencode_leaves_malformed_and_non_png_attachments_alone() {
		let malformed = ImageAttachment {
			data_url: "data:image/png,oops-no-base64-marker".to_string(),
			mime: PNG_MIME.to_string(),
		};
		assert_eq!(reencode(malformed.clone()), malformed);

		let webp_already = ImageAttachment {
			data_url: "data:image/webp;base64,AAAA".to_string(),
			mime: "image/webp".to_string(),
		};
		assert_eq!(reencode(webp_already.clone()), webp_already);
	}

	#[tokio::test]
	async fn reencode_all_is_a_no_op_on_an_empty_batch() {
		assert!(reencode_all(Vec::new()).await.is_empty());
	}

	/// One tool message per image, payload padded to `kb` so the
	/// budget arithmetic is readable.
	fn history_with_images(sizes_kb: &[usize]) -> Vec<ChatMessage> {
		let mut messages = vec![ChatMessage::System { content: "sys".into() }];
		for (idx, kb) in sizes_kb.iter().enumerate() {
			messages.push(ChatMessage::Tool {
				tool_call_id: format!("call_{idx}"),
				content: "{\"content\":\"[image attached]\"}".into(),
				images: vec![{
					// Exact `kb`, prefix included, so the budget
					// arithmetic below is exact too.
					let prefix = format!("data:image/webp;base64,{idx}");
					let padding = "A".repeat(kb * 1000 - prefix.len());
					ImageAttachment {
						data_url: format!("{prefix}{padding}"),
						mime: "image/webp".into(),
					}
				}],
			});
		}
		messages
	}

	const TEST_BUDGET: ImageWireBudget = ImageWireBudget {
		ceiling: 1_000_000,
		floor: 400_000,
	};

	#[test]
	fn payload_under_the_ceiling_is_left_alone() {
		let messages = history_with_images(&[200, 200, 200]);
		let mut elided = HashSet::new();
		assert_eq!(plan_elision(&messages, TEST_BUDGET, &mut elided), 0);
		assert!(elided.is_empty());
	}

	#[test]
	fn crossing_the_ceiling_drops_oldest_first_down_to_the_floor() {
		// 6 x 200 kB = 1.2 MB, over the 1 MB ceiling. Dropping the
		// four oldest lands at 400 kB, i.e. the floor.
		let messages = history_with_images(&[200, 200, 200, 200, 200, 200]);
		let mut elided = HashSet::new();
		assert_eq!(plan_elision(&messages, TEST_BUDGET, &mut elided), 4);
		assert!(wire_bytes(&messages, &elided) <= TEST_BUDGET.floor);

		// Hysteresis: the same history a turn later must not shift
		// the prefix again, or every round-trip would invalidate the
		// provider's prompt cache.
		assert_eq!(plan_elision(&messages, TEST_BUDGET, &mut elided), 0);

		// The two survivors are the newest two.
		let kept: Vec<&ImageAttachment> = images_in_order(&messages)
			.filter(|image| !elided.contains(&image_key(image)))
			.collect();
		assert_eq!(kept.len(), 2);
		assert!(kept[0].data_url.starts_with("data:image/webp;base64,4"));
		assert!(kept[1].data_url.starts_with("data:image/webp;base64,5"));
	}

	#[test]
	fn elision_strips_the_wire_copy_and_leaves_a_note() {
		let messages = history_with_images(&[200, 200, 200, 200, 200, 200]);
		let mut elided = HashSet::new();
		plan_elision(&messages, TEST_BUDGET, &mut elided);

		let mut wire = messages.clone();
		apply_elision(&mut wire, &elided);
		assert_eq!(wire_bytes(&wire, &HashSet::new()), wire_bytes(&messages, &elided));

		let stripped = wire.iter().filter(|message| match message {
			ChatMessage::Tool { images, content, .. } => images.is_empty() && content.contains("dropped from context"),
			_ => false,
		});
		assert_eq!(stripped.count(), 4, "each stripped message should say so");

		// The session's own history is untouched — the panel still
		// renders these and a raised budget restores them.
		assert_eq!(images_in_order(&messages).count(), 6);
	}

	#[test]
	fn byte_identical_images_share_a_key_and_elide_together() {
		let duplicate = ImageAttachment {
			data_url: format!("data:image/webp;base64,{}", "A".repeat(600_000)),
			mime: "image/webp".into(),
		};
		let messages = vec![
			ChatMessage::Tool {
				tool_call_id: "call_0".into(),
				content: "{}".into(),
				images: vec![duplicate.clone()],
			},
			ChatMessage::Tool {
				tool_call_id: "call_1".into(),
				content: "{}".into(),
				images: vec![duplicate],
			},
		];
		let mut elided = HashSet::new();
		assert_eq!(plan_elision(&messages, TEST_BUDGET, &mut elided), 1);
		assert_eq!(elided.len(), 1);
		assert_eq!(wire_bytes(&messages, &elided), 0, "both copies leave the wire");
	}
}
