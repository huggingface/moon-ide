//! Tauri commands for local llama.cpp autocomplete (`next_edit_*` IPC).

use std::sync::Arc;

use moon_core::next_edit;
use moon_core::NextEditServerSupervisor;
use moon_protocol::next_edit::{
	NextEditCompleteParams, NextEditCompleteResult, NextEditProbeResult, NextEditServerSnapshot,
	NextEditServerStartParams,
};
use moon_protocol::MoonError;
use tauri::State;

use crate::state::AppState;

#[tauri::command]
pub async fn next_edit_probe(base_url: String) -> Result<NextEditProbeResult, MoonError> {
	let client = reqwest::Client::builder()
		.build()
		.map_err(|e| MoonError::internal(format!("http client: {e}")))?;
	Ok(next_edit::probe(&client, &base_url).await)
}

#[tauri::command]
pub async fn next_edit_complete(params: NextEditCompleteParams) -> Result<NextEditCompleteResult, MoonError> {
	let client = reqwest::Client::builder()
		.build()
		.map_err(|e| MoonError::internal(format!("http client: {e}")))?;
	next_edit::complete(&client, params).await
}

#[tauri::command]
pub async fn next_edit_server_start(
	state: State<'_, AppState>,
	params: NextEditServerStartParams,
) -> Result<NextEditServerSnapshot, MoonError> {
	let sup = state.next_edit_server.clone();
	NextEditServerSupervisor::start(Arc::clone(&sup), params).await?;
	Ok(sup.snapshot().await)
}

#[tauri::command]
pub async fn next_edit_server_stop(state: State<'_, AppState>) -> Result<NextEditServerSnapshot, MoonError> {
	state.next_edit_server.stop().await?;
	Ok(state.next_edit_server.snapshot().await)
}

#[tauri::command]
pub async fn next_edit_server_status(state: State<'_, AppState>) -> Result<NextEditServerSnapshot, MoonError> {
	Ok(state.next_edit_server.snapshot().await)
}
