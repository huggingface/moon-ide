//! OS-level colour-scheme probe.
//!
//! Linux (any WebKitGTK-backed build, really) needs help here:
//! `matchMedia('(prefers-color-scheme: dark)')` and Tauri's own
//! `getCurrentWindow().theme()` both ignore the GTK / GNOME / KDE
//! preference and default to light, so System mode paints light
//! under a dark desktop. We bypass the webview entirely and read the
//! XDG Desktop Portal's `org.freedesktop.appearance color-scheme`
//! setting via ashpd, which is the same channel Firefox / Chromium
//! actually listen on.
//!
//! macOS and Windows _do_ get the right answer from the webview, so
//! there we just forward `tauri::WebviewWindow::theme()` through the
//! same command signature to keep the frontend cross-platform.

use moon_protocol::theme::SystemTheme;
use moon_protocol::MoonError;
use tauri::AppHandle;

#[tauri::command]
pub async fn system_theme(app: AppHandle) -> Result<SystemTheme, MoonError> {
	detect(&app).await
}

#[cfg(any(
	target_os = "linux",
	target_os = "freebsd",
	target_os = "dragonfly",
	target_os = "netbsd",
	target_os = "openbsd",
))]
async fn detect(_app: &AppHandle) -> Result<SystemTheme, MoonError> {
	use ashpd::desktop::settings::Settings;

	let settings = Settings::new()
		.await
		.map_err(|e| MoonError::internal(format!("open XDG settings portal: {e}")))?;
	let scheme = settings
		.color_scheme()
		.await
		.map_err(|e| MoonError::internal(format!("read XDG color-scheme: {e}")))?;
	Ok(scheme_to_system_theme(scheme))
}

#[cfg(not(any(
	target_os = "linux",
	target_os = "freebsd",
	target_os = "dragonfly",
	target_os = "netbsd",
	target_os = "openbsd",
)))]
async fn detect(app: &AppHandle) -> Result<SystemTheme, MoonError> {
	use tauri::Manager;

	let Some(win) = app.get_webview_window("main") else {
		return Ok(SystemTheme::Unspecified);
	};
	let theme = win
		.theme()
		.map_err(|e| MoonError::internal(format!("read webview theme: {e}")))?;
	let resolved = match theme {
		tauri::Theme::Dark => SystemTheme::Dark,
		tauri::Theme::Light => SystemTheme::Light,
		_ => SystemTheme::Unspecified,
	};
	Ok(resolved)
}

#[cfg(any(
	target_os = "linux",
	target_os = "freebsd",
	target_os = "dragonfly",
	target_os = "netbsd",
	target_os = "openbsd",
))]
pub(crate) fn scheme_to_system_theme(scheme: ashpd::desktop::settings::ColorScheme) -> SystemTheme {
	use ashpd::desktop::settings::ColorScheme;
	match scheme {
		ColorScheme::PreferDark => SystemTheme::Dark,
		ColorScheme::PreferLight => SystemTheme::Light,
		ColorScheme::NoPreference => SystemTheme::Unspecified,
	}
}
