//! Native screenshots of an IDE browser tab (ADR 0103).
//!
//! The tab is an `<iframe>` inside the window's own webview, so a
//! screenshot is the platform webview's snapshot of its visible area,
//! cropped to the iframe's on-screen rect. The frontend measures that
//! rect; this side only captures and crops.

use base64::Engine as _;
use moon_protocol::MoonError;

/// The visible part of the calling webview inside `x/y/width/height`
/// (CSS px, relative to the viewport), as a base64 PNG.
/// `viewport_width` is the webview's CSS width, used to find the
/// snapshot's pixel scale without trusting platform DPI reporting.
#[tauri::command]
pub async fn browser_capture(
	webview: tauri::Webview,
	x: f64,
	y: f64,
	width: f64,
	height: f64,
	viewport_width: f64,
) -> Result<String, MoonError> {
	if width < 1.0 || height < 1.0 || viewport_width < 1.0 {
		return Err(MoonError::invalid("the browser tab isn't visible"));
	}
	let png = snapshot_png(
		&webview,
		Crop {
			x,
			y,
			width,
			height,
			viewport_width,
		},
	)
	.await?;
	Ok(base64::engine::general_purpose::STANDARD.encode(png))
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct Crop {
	x: f64,
	y: f64,
	width: f64,
	height: f64,
	viewport_width: f64,
}

#[cfg(target_os = "linux")]
async fn snapshot_png(webview: &tauri::Webview, crop: Crop) -> Result<Vec<u8>, MoonError> {
	use webkit2gtk::{SnapshotOptions, SnapshotRegion, WebViewExt};

	let (tx, rx) = tokio::sync::oneshot::channel::<Result<Vec<u8>, String>>();
	webview
		.with_webview(move |platform| {
			let view = platform.inner();
			view.snapshot(
				SnapshotRegion::Visible,
				SnapshotOptions::NONE,
				None::<&gio::Cancellable>,
				move |result| {
					let png = result
						.map_err(|err| err.to_string())
						.and_then(|surface| crop_to_png(surface, &crop));
					let _ = tx.send(png);
				},
			);
		})
		.map_err(|err| MoonError::internal(format!("webview unavailable: {err}")))?;
	rx.await
		.map_err(|_| MoonError::internal("snapshot was dropped"))?
		.map_err(|err| MoonError::internal(format!("snapshot failed: {err}")))
}

#[cfg(target_os = "linux")]
fn crop_to_png(surface: cairo::Surface, crop: &Crop) -> Result<Vec<u8>, String> {
	let source = cairo::ImageSurface::try_from(surface).map_err(|_| "snapshot isn't an image surface".to_string())?;
	// Work in raw pixels: the snapshot may carry a device scale, and
	// the crop rect is CSS px.
	source.set_device_scale(1.0, 1.0);
	let scale = f64::from(source.width()) / crop.viewport_width;
	let left = (crop.x * scale).round().max(0.0);
	let top = (crop.y * scale).round().max(0.0);
	let width = ((crop.width * scale).round()).min(f64::from(source.width()) - left);
	let height = ((crop.height * scale).round()).min(f64::from(source.height()) - top);
	if width < 1.0 || height < 1.0 {
		return Err("the browser tab is outside the visible area".into());
	}
	let out =
		cairo::ImageSurface::create(cairo::Format::ARgb32, width as i32, height as i32).map_err(|err| err.to_string())?;
	{
		let cx = cairo::Context::new(&out).map_err(|err| err.to_string())?;
		cx.set_source_surface(&source, -left, -top)
			.map_err(|err| err.to_string())?;
		cx.paint().map_err(|err| err.to_string())?;
	}
	let mut png = Vec::new();
	out.write_to_png(&mut png).map_err(|err| err.to_string())?;
	Ok(png)
}

#[cfg(not(target_os = "linux"))]
async fn snapshot_png(_webview: &tauri::Webview, _crop: Crop) -> Result<Vec<u8>, MoonError> {
	Err(MoonError::invalid(
		"browser screenshots are only implemented on Linux so far",
	))
}
