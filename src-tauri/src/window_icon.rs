//! Per-workspace window icon generation.
//!
//! Each `moon-ide` process owns one workspace and one OS window
//! (Phase 7). To let the user pick the right window out of an
//! alt-tab stack of multiple `moon-ide`s, we paint the window's
//! `_NET_WM_ICON` with a deterministically-coloured badge derived
//! from the workspace id. Same id → same colour across launches.
//!
//! Platform note: on X11 this sets a per-window pixmap that the
//! window manager honours in alt-tab and the taskbar. On Wayland,
//! most compositors look icons up by `app_id` (against the
//! system icon theme / `.desktop` file) and ignore per-window
//! pixmaps, so this is a best-effort affordance there — the
//! function still runs and Tauri still records the icon, the
//! compositor just may not surface it. macOS and Windows pick
//! the per-window icon up.
//!
//! The rendering is hand-rolled RGBA so we don't take a `image`
//! / glyph-rasterizer dependency for what amounts to a few
//! hundred lines of pixel math.

const SIZE: u32 = 128;
const MARGIN: f32 = 6.0;
const CORNER_RADIUS: f32 = 22.0;

/// Build a 128×128 RGBA icon for `workspace_id`. The badge is a
/// rounded square in a workspace-specific colour with a small
/// off-centre white crescent on top — the crescent keeps a hint
/// of moon-ide branding while the hue does the differentiation.
///
/// `override_color` is an optional user-chosen `#rrggbb` (case-
/// insensitive, 3-char shorthand allowed) that wins over the
/// hash-derived hue. Pass `None` (default for fresh workspaces)
/// or an unparseable string and we silently fall back to the
/// FNV-1a hue, so a corrupted catalog entry never produces an
/// invisible icon.
///
/// Returns row-major RGBA bytes ready to hand to `tauri::image::
/// Image::new(&bytes, SIZE, SIZE)`.
pub fn generate_workspace_icon(workspace_id: &str, override_color: Option<&str>) -> Vec<u8> {
	let (r, g, b) = override_color
		.and_then(parse_hex_colour)
		.unwrap_or_else(|| workspace_colour(workspace_id));
	let mut buf = vec![0u8; (SIZE * SIZE * 4) as usize];

	let size = SIZE as f32;
	let rect_x0 = MARGIN;
	let rect_y0 = MARGIN;
	let rect_x1 = size - MARGIN;
	let rect_y1 = size - MARGIN;

	// Crescent: a full white disk with a workspace-coloured
	// disk subtracted from its upper-right. Sized to feel
	// comfortable inside the rounded square. The cut disk is
	// drawn in the same colour as the badge so the crescent
	// "carves out" of the badge without exposing transparency
	// inside the rounded square.
	let disk_cx = 60.0;
	let disk_cy = 70.0;
	let disk_r = 36.0;
	let cut_cx = 72.0;
	let cut_cy = 58.0;
	let cut_r = 33.0;

	for y in 0..SIZE {
		for x in 0..SIZE {
			let fx = x as f32 + 0.5;
			let fy = y as f32 + 0.5;
			let coverage = rounded_rect_coverage(fx, fy, rect_x0, rect_y0, rect_x1, rect_y1, CORNER_RADIUS);
			if coverage <= 0.0 {
				continue;
			}
			let inside_disk = (fx - disk_cx).hypot(fy - disk_cy) <= disk_r;
			let inside_cut = (fx - cut_cx).hypot(fy - cut_cy) <= cut_r;
			let crescent = inside_disk && !inside_cut;
			let (pr, pg, pb) = if crescent { (240u8, 245, 252) } else { (r, g, b) };
			let idx = ((y * SIZE + x) * 4) as usize;
			buf[idx] = pr;
			buf[idx + 1] = pg;
			buf[idx + 2] = pb;
			// `coverage` is 0..1 from the 3×3 supersample below;
			// scaling by 255 gives a soft AA edge on the rounded
			// corners. The inner crescent edge is hard-cut on
			// purpose — supersampling the disk subtraction
			// independently would double the cost for a detail
			// nobody can see at 32×32 alt-tab thumbnail size.
			buf[idx + 3] = (coverage * 255.0).round().clamp(0.0, 255.0) as u8;
		}
	}
	buf
}

/// Pixel dimensions of [`generate_workspace_icon`]'s output.
/// Exposed so callers can hand the right `width` / `height` to
/// `tauri::image::Image::new` without hard-coding the constant
/// in two places.
pub const ICON_SIZE: u32 = SIZE;

/// Render and apply the per-workspace icon to `window`. Logs and
/// swallows any failure — a bad icon shouldn't break window
/// startup or a colour-change roundtrip.
pub fn apply_workspace_icon<R: tauri::Runtime>(
	window: &tauri::WebviewWindow<R>,
	workspace_id: &str,
	override_color: Option<&str>,
) {
	let rgba = generate_workspace_icon(workspace_id, override_color);
	let image = tauri::image::Image::new(&rgba, ICON_SIZE, ICON_SIZE);
	if let Err(err) = window.set_icon(image) {
		tracing::warn!(error = %err, workspace_id = %workspace_id, "failed to set per-workspace window icon");
	}
}

/// Deterministic workspace colour. FNV-1a hash → hue, fixed
/// saturation + lightness chosen to read well against dark and
/// light desktop wallpapers. Two workspaces with the same name
/// (impossible — slugs are unique) would land on the same hue;
/// that's the desired property, not a bug.
fn workspace_colour(workspace_id: &str) -> (u8, u8, u8) {
	const FNV_OFFSET: u64 = 0xcbf29ce484222325;
	const FNV_PRIME: u64 = 0x100000001b3;
	let mut h = FNV_OFFSET;
	for byte in workspace_id.as_bytes() {
		h ^= u64::from(*byte);
		h = h.wrapping_mul(FNV_PRIME);
	}
	let hue = (h % 360) as f32;
	hsl_to_rgb(hue, 0.58, 0.52)
}

fn hsl_to_rgb(h_deg: f32, s: f32, l: f32) -> (u8, u8, u8) {
	// Standard HSL → RGB conversion. `h_deg` in [0, 360).
	let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
	let h_prime = h_deg / 60.0;
	let x = c * (1.0 - ((h_prime % 2.0) - 1.0).abs());
	let (r1, g1, b1) = match h_prime as u32 {
		0 => (c, x, 0.0),
		1 => (x, c, 0.0),
		2 => (0.0, c, x),
		3 => (0.0, x, c),
		4 => (x, 0.0, c),
		_ => (c, 0.0, x),
	};
	let m = l - c / 2.0;
	let to_u8 = |v: f32| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
	(to_u8(r1), to_u8(g1), to_u8(b1))
}

fn rounded_rect_coverage(fx: f32, fy: f32, x0: f32, y0: f32, x1: f32, y1: f32, radius: f32) -> f32 {
	// 3×3 supersample to soften the corner curves. Cheap — the
	// hot loop only runs `SIZE*SIZE = 16_384` times, and 9
	// extra hits-vs-misses per pixel is well below 1 ms total
	// in release.
	let mut hits = 0u32;
	let mut total = 0u32;
	for dy in 0..3 {
		for dx in 0..3 {
			let sx = fx + (dx as f32 - 1.0) * (1.0 / 3.0);
			let sy = fy + (dy as f32 - 1.0) * (1.0 / 3.0);
			if point_in_rounded_rect(sx, sy, x0, y0, x1, y1, radius) {
				hits += 1;
			}
			total += 1;
		}
	}
	hits as f32 / total as f32
}

/// Parse `#rrggbb` / `#rgb` (case-insensitive, optional leading
/// `#`) into an `(r, g, b)` triple. Returns `None` on any
/// formatting issue — callers fall back to the hash-derived
/// colour rather than surfacing a parse error to the user.
fn parse_hex_colour(s: &str) -> Option<(u8, u8, u8)> {
	let hex = s.trim().trim_start_matches('#');
	match hex.len() {
		3 => {
			let r = u8::from_str_radix(&hex[0..1], 16).ok()?;
			let g = u8::from_str_radix(&hex[1..2], 16).ok()?;
			let b = u8::from_str_radix(&hex[2..3], 16).ok()?;
			Some((r * 17, g * 17, b * 17))
		}
		6 => {
			let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
			let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
			let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
			Some((r, g, b))
		}
		_ => None,
	}
}

fn point_in_rounded_rect(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
	if x < x0 || x > x1 || y < y0 || y > y1 {
		return false;
	}
	// Inside the rectangular interior (not in a corner region).
	if (x >= x0 + r && x <= x1 - r) || (y >= y0 + r && y <= y1 - r) {
		return true;
	}
	// Within a corner region — test against the nearest corner
	// centre.
	let cx = if x < x0 + r { x0 + r } else { x1 - r };
	let cy = if y < y0 + r { y0 + r } else { y1 - r };
	(x - cx).hypot(y - cy) <= r
}
