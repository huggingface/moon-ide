//! OS-level "agent running / finished" indicator.
//!
//! Each moon-ide process owns one workspace, so process-wide agent
//! activity (any live coder turn loop, see
//! `CoderHandle::watch_running_turns`) maps 1:1 onto per-workspace
//! status. Two surfaces, driven together:
//!
//! - the window icon gets an amber (running) / green (finished)
//!   dot on top of the workspace badge — visible in alt-tab and
//!   ungrouped taskbars on X11;
//! - a **permanent** system-tray icon (StatusNotifierItem on
//!   Linux), one per workspace process, painted with the
//!   workspace badge. It gains the same amber dot while agents
//!   run, flips green when the last one settles unfocused, and
//!   drops the dot once the window is focused again — but the
//!   icon itself stays, doubling as a per-workspace "bring me
//!   back" affordance (ADR 0061).
//!
//! A turn that settles while the window is already focused skips
//! the "finished" state entirely — the user is watching the panel.
//! When it settles unfocused we additionally raise the WM urgency
//! hint (`request_user_attention`) so grouped taskbars that hide
//! per-window icons still flash.
//!
//! Linux tray caveat: appindicator-style trays deliver no
//! left-click events, only menu activation, so the menu carries
//! "Focus window" and "Close window" items. The click handler is
//! still installed for platforms that do deliver clicks.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

use crate::window_icon::{generate_workspace_icon, IconStatus, ICON_SIZE};

/// Tray registration id — one per process, so a fixed string is
/// unique within the app that owns it.
const TRAY_ID: &str = "agent-status";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
	Idle,
	Running,
	/// Last turn settled while the window was unfocused; cleared
	/// (back to `Idle`) on the next focus.
	Done,
}

pub struct AgentIndicator {
	app: AppHandle,
	workspace_id: String,
	color: Mutex<Option<String>>,
	focused: AtomicBool,
	status: Mutex<Status>,
	tray: Mutex<Option<TrayIcon>>,
}

impl AgentIndicator {
	/// Build the indicator and spawn the watcher task that follows
	/// the coder's running-turn count. Workspace mode only — the
	/// preboot picker has no agents.
	pub fn spawn(
		app: &AppHandle,
		workspace_id: String,
		override_color: Option<String>,
		coder: &moon_coder::CoderHandle,
	) -> Arc<Self> {
		let focused = app
			.get_webview_window("main")
			.and_then(|w| w.is_focused().ok())
			.unwrap_or(true);
		let indicator = Arc::new(Self {
			app: app.clone(),
			workspace_id,
			color: Mutex::new(override_color),
			focused: AtomicBool::new(focused),
			status: Mutex::new(Status::Idle),
			tray: Mutex::new(None),
		});
		let mut rx = coder.watch_running_turns();
		let for_task = Arc::clone(&indicator);
		tauri::async_runtime::spawn(async move {
			// Ends when the coder (and with it the sender) is
			// dropped, i.e. at process teardown.
			while rx.changed().await.is_ok() {
				let count = *rx.borrow_and_update();
				for_task.on_running_count(count);
			}
		});
		// The tray icon is permanent: show it right away in its
		// idle (plain badge) state.
		indicator.render();
		indicator
	}

	/// Window focus flips, forwarded from the builder's
	/// `on_window_event`. Focusing the window acknowledges a
	/// pending "finished" state.
	pub fn set_focused(&self, focused: bool) {
		self.focused.store(focused, Ordering::Relaxed);
		if !focused {
			return;
		}
		{
			let mut status = self.status.lock().expect("agent indicator status poisoned");
			if *status != Status::Done {
				return;
			}
			*status = Status::Idle;
		}
		self.render();
	}

	/// User picked a new badge colour (`workspace_set_color`).
	/// Re-renders both surfaces so the activity dot survives the
	/// colour change.
	pub fn set_color(&self, color: Option<String>) {
		*self.color.lock().expect("agent indicator color poisoned") = color;
		self.render();
	}

	fn on_running_count(&self, count: usize) {
		let next = {
			let mut status = self.status.lock().expect("agent indicator status poisoned");
			let next = if count > 0 {
				Status::Running
			} else if *status == Status::Running && !self.focused.load(Ordering::Relaxed) {
				Status::Done
			} else {
				Status::Idle
			};
			if *status == next {
				return;
			}
			*status = next;
			next
		};
		self.render();
		if next == Status::Done {
			self.flash_taskbar();
		}
	}

	/// Raise the WM urgency hint so the taskbar button flashes /
	/// highlights even where per-window icons don't show (e.g.
	/// Cinnamon's grouped window list resolves icons from the
	/// .desktop file). The WM clears the hint on focus by itself.
	fn flash_taskbar(&self) {
		let Some(window) = self.app.get_webview_window("main") else {
			return;
		};
		if let Err(err) = window.request_user_attention(Some(tauri::UserAttentionType::Informational)) {
			tracing::debug!(error = %err, "request_user_attention failed");
		}
	}

	fn render(&self) {
		let status = *self.status.lock().expect("agent indicator status poisoned");
		let color = self.color.lock().expect("agent indicator color poisoned").clone();
		let icon_status = match status {
			Status::Idle => IconStatus::Plain,
			Status::Running => IconStatus::Running,
			Status::Done => IconStatus::Done,
		};
		if let Some(window) = self.app.get_webview_window("main") {
			crate::window_icon::apply_workspace_icon(&window, &self.workspace_id, color.as_deref(), icon_status);
		}
		match status {
			Status::Idle => self.show_tray(icon_status, color.as_deref(), None),
			Status::Running => self.show_tray(icon_status, color.as_deref(), Some("agent running")),
			Status::Done => self.show_tray(icon_status, color.as_deref(), Some("agents finished")),
		}
	}

	fn show_tray(&self, icon_status: IconStatus, color: Option<&str>, verb: Option<&str>) {
		let rgba = generate_workspace_icon(&self.workspace_id, color, icon_status);
		let image = tauri::image::Image::new(&rgba, ICON_SIZE, ICON_SIZE);
		let tooltip = match verb {
			Some(verb) => format!("moon-ide — {}: {verb}", self.workspace_id),
			None => format!("moon-ide — {}", self.workspace_id),
		};
		let mut tray = self.tray.lock().expect("agent indicator tray poisoned");
		if let Some(existing) = tray.as_ref() {
			if let Err(err) = existing.set_icon(Some(image)) {
				tracing::warn!(error = %err, "failed to update tray icon");
			}
			if let Err(err) = existing.set_tooltip(Some(&tooltip)) {
				tracing::debug!(error = %err, "failed to update tray tooltip");
			}
			return;
		}
		match self.build_tray(image, &tooltip) {
			Ok(built) => *tray = Some(built),
			Err(err) => tracing::warn!(error = %err, "failed to create agent-status tray icon"),
		}
	}

	fn build_tray(&self, image: tauri::image::Image<'_>, tooltip: &str) -> tauri::Result<TrayIcon> {
		let focus_item = MenuItem::with_id(&self.app, "focus", "Focus window", true, None::<&str>)?;
		let separator = PredefinedMenuItem::separator(&self.app)?;
		let close_item = MenuItem::with_id(&self.app, "close", "Close window", true, None::<&str>)?;
		let menu = Menu::with_items(&self.app, &[&focus_item, &separator, &close_item])?;
		TrayIconBuilder::with_id(TRAY_ID)
			.icon(image)
			.tooltip(tooltip)
			.menu(&menu)
			.show_menu_on_left_click(false)
			.on_menu_event(|app, event| match event.id().as_ref() {
				"focus" => crate::focus_socket::focus_main_window(app),
				// Same as the `window_close` command: with one
				// window per process, closing the window means
				// exiting — the `ExitRequested` hook runs the
				// `stop_all` teardown.
				"close" => app.exit(0),
				_ => {}
			})
			.on_tray_icon_event(|tray, event| {
				// Left-click focuses where the platform delivers
				// clicks at all (not appindicator Linux).
				if let TrayIconEvent::Click {
					button: MouseButton::Left,
					button_state: MouseButtonState::Up,
					..
				} = event
				{
					crate::focus_socket::focus_main_window(tray.app_handle());
				}
			})
			.build(&self.app)
	}
}
