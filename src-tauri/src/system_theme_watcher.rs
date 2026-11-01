//! Linux-only live watcher for the OS colour-scheme preference.
//!
//! WebKitGTK's `matchMedia('(prefers-color-scheme: dark)')` and
//! Tauri's `getCurrentWindow().onThemeChanged` both ignore GTK /
//! GNOME / KDE theme flips at runtime, so the frontend has no way to
//! learn about the change on its own. We subscribe to the XDG
//! Desktop Portal's `Settings.receive_color_scheme_changed` stream
//! on a background tokio task and re-broadcast each change as a
//! Tauri event, which the frontend picks up to repaint.
//!
//! The same value is pushed into GTK as the app theme, which is what
//! WebKitGTK derives `prefers-color-scheme` from: pages in the IDE's
//! browser tabs then render dark on a dark desktop.
//!
//! macOS and Windows don't need this — the webview's own
//! `onThemeChanged` event fires there — so the watcher compiles
//! down to a no-op shim on those platforms.

use tauri::AppHandle;

pub const SYSTEM_THEME_EVENT: &str = "system:theme-changed";

#[cfg(any(
	target_os = "linux",
	target_os = "freebsd",
	target_os = "dragonfly",
	target_os = "netbsd",
	target_os = "openbsd",
))]
pub fn spawn(app: AppHandle) {
	// `tauri::async_runtime::spawn`, not `tokio::spawn`: `setup` runs
	// before Tokio is bound to the current thread, so a bare
	// `tokio::spawn` panics with "there is no reactor running". See
	// the same note in `slack_poller::spawn` for the full story.
	tauri::async_runtime::spawn(async move {
		run(app).await;
	});
}

#[cfg(not(any(
	target_os = "linux",
	target_os = "freebsd",
	target_os = "dragonfly",
	target_os = "netbsd",
	target_os = "openbsd",
)))]
pub fn spawn(_app: AppHandle) {
	// Webview delivers change events directly on macOS/Windows.
}

#[cfg(any(
	target_os = "linux",
	target_os = "freebsd",
	target_os = "dragonfly",
	target_os = "netbsd",
	target_os = "openbsd",
))]
async fn run(app: AppHandle) {
	use ashpd::desktop::settings::Settings;
	use futures_util::StreamExt;
	use tauri::Emitter;

	use crate::commands::system::scheme_to_system_theme;

	let settings = match Settings::new().await {
		Ok(s) => s,
		Err(err) => {
			tracing::warn!(error = %err, "system theme watcher: could not open XDG Settings portal");
			return;
		}
	};
	let mut stream = match settings.receive_color_scheme_changed().await {
		Ok(s) => s,
		Err(err) => {
			tracing::warn!(error = %err, "system theme watcher: could not subscribe to color-scheme changes");
			return;
		}
	};
	tracing::info!("system theme watcher: subscribed to XDG color-scheme changes");
	match settings.color_scheme().await {
		Ok(scheme) => apply_to_webviews(&app, scheme_to_system_theme(scheme)),
		Err(err) => tracing::warn!(error = %err, "system theme watcher: could not read the color scheme"),
	}
	while let Some(scheme) = stream.next().await {
		let payload = scheme_to_system_theme(scheme);
		apply_to_webviews(&app, payload);
		if let Err(err) = app.emit(SYSTEM_THEME_EVENT, &payload) {
			tracing::warn!(error = %err, "failed to emit {SYSTEM_THEME_EVENT}");
		}
	}
	tracing::info!("system theme watcher: stream ended");
}

/// GTK's app-wide dark preference, which WebKitGTK maps onto
/// `prefers-color-scheme`. An unset portal preference counts as dark,
/// matching how the IDE's own theme resolves it.
#[cfg(any(
	target_os = "linux",
	target_os = "freebsd",
	target_os = "dragonfly",
	target_os = "netbsd",
	target_os = "openbsd",
))]
fn apply_to_webviews(app: &AppHandle, theme: moon_protocol::theme::SystemTheme) {
	use moon_protocol::theme::SystemTheme;

	let theme = match theme {
		SystemTheme::Light => tauri::Theme::Light,
		SystemTheme::Dark | SystemTheme::Unspecified => tauri::Theme::Dark,
	};
	app.set_theme(Some(theme));
}
