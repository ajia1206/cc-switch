#![allow(non_snake_case)]

use crate::codex_accounts::{CodexAccountSummary, CodexAccountSwitchResult};

#[tauri::command]
pub fn codex_list_account_snapshots() -> Result<Vec<CodexAccountSummary>, String> {
    crate::codex_accounts::list_accounts().map_err(Into::into)
}

#[tauri::command]
pub fn codex_capture_current_account(label: Option<String>) -> Result<CodexAccountSummary, String> {
    crate::codex_accounts::capture_current(label).map_err(Into::into)
}

#[tauri::command]
pub fn codex_switch_account(accountKey: String) -> Result<CodexAccountSwitchResult, String> {
    crate::codex_accounts::switch_account(accountKey).map_err(Into::into)
}

#[tauri::command]
pub fn codex_rollback_last_account_switch() -> Result<CodexAccountSwitchResult, String> {
    crate::codex_accounts::rollback_last_switch().map_err(Into::into)
}

#[tauri::command]
pub fn codex_restart_app() -> Result<bool, String> {
    crate::codex_accounts::restart_codex_app().map_err(Into::into)
}
