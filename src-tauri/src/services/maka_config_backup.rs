//! Maka LLM 配置的本机备份与恢复。
//!
//! 备份同时包含 `llm-connections.json` 和 `credentials.json`。后者包含明文
//! API Token，因此备份目录固定为 0700、备份文件固定为 0600，且 IPC 只返回
//! 元数据，不会把配置或凭据内容发送到前端。

use crate::config::{atomic_write, get_app_config_dir};
use crate::error::AppError;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

const BACKUP_SCHEMA_VERSION: u32 = 1;
const MANUAL_BACKUP: &str = "manual";
const SAFETY_BACKUP: &str = "safety";
const BACKUP_PREFIX: &str = "maka_llm_";
const BACKUP_SUFFIX: &str = ".json";
const CREDENTIAL_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const CREDENTIAL_LOCK_POLL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MakaLlmBackupEntry {
    pub filename: String,
    pub size_bytes: u64,
    pub created_at: String,
    pub backup_type: String,
    pub connection_count: usize,
    pub credential_count: usize,
    pub includes_credentials: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MakaLlmRestoreResult {
    pub safety_backup_filename: Option<String>,
    pub connection_count: usize,
    pub credential_count: usize,
    pub includes_credentials: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MakaLlmBackupFile {
    schema_version: u32,
    backup_type: String,
    created_at: String,
    llm_connections: Value,
    credentials: Option<Value>,
}

struct CredentialFileLock {
    path: PathBuf,
}

impl CredentialFileLock {
    fn acquire(credentials_path: &Path, timeout: Duration) -> Result<Self, AppError> {
        let parent = credentials_path
            .parent()
            .ok_or_else(|| AppError::Config("Maka 凭据路径无效".to_string()))?;
        secure_directory(parent)?;

        let mut lock_name = credentials_path.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock_path = PathBuf::from(lock_name);
        let deadline = Instant::now() + timeout;
        loop {
            match fs::create_dir(&lock_path) {
                Ok(()) => return Ok(Self { path: lock_path }),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    if Instant::now() >= deadline {
                        return Err(AppError::Lock(format!(
                            "Maka 正在更新凭据（{}），请稍后重试",
                            lock_path.display()
                        )));
                    }
                    thread::sleep(CREDENTIAL_LOCK_POLL);
                }
                Err(error) => return Err(AppError::io(&lock_path, error)),
            }
        }
    }
}

impl Drop for CredentialFileLock {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir(&self.path) {
            log::warn!("清理 Maka 凭据锁失败 {}: {}", self.path.display(), error);
        }
    }
}

pub struct MakaConfigBackupService;

impl MakaConfigBackupService {
    pub fn create_backup() -> Result<MakaLlmBackupEntry, AppError> {
        let workspace = maka_workspace_dir()?;
        let credentials_path = workspace.join("credentials.json");
        let _credential_lock =
            CredentialFileLock::acquire(&credentials_path, CREDENTIAL_LOCK_TIMEOUT)?;
        create_backup_at(&workspace, &maka_backup_dir(), MANUAL_BACKUP)
    }

    pub fn list_backups() -> Result<Vec<MakaLlmBackupEntry>, AppError> {
        list_backups_at(&maka_backup_dir())
    }

    pub fn restore_backup(filename: &str) -> Result<MakaLlmRestoreResult, AppError> {
        restore_backup_at(&maka_workspace_dir()?, &maka_backup_dir(), filename)
    }

    pub fn delete_backup(filename: &str) -> Result<(), AppError> {
        validate_backup_filename(filename)?;
        let path = maka_backup_dir().join(filename);
        if !path.is_file() {
            return Err(AppError::InvalidInput(format!(
                "Maka LLM 备份不存在: {filename}"
            )));
        }
        fs::remove_file(&path).map_err(|error| AppError::io(&path, error))?;
        log::info!("已删除 Maka LLM 备份: {filename}");
        Ok(())
    }
}

fn maka_workspace_dir() -> Result<PathBuf, AppError> {
    dirs::data_dir()
        .map(|dir| dir.join("Maka/workspaces/default"))
        .ok_or_else(|| AppError::Config("无法定位 Maka 配置目录".to_string()))
}

fn maka_backup_dir() -> PathBuf {
    get_app_config_dir().join("backups/maka-llm")
}

fn create_backup_at(
    workspace: &Path,
    backup_dir: &Path,
    backup_type: &str,
) -> Result<MakaLlmBackupEntry, AppError> {
    let connections_path = workspace.join("llm-connections.json");
    if !connections_path.is_file() {
        return Err(AppError::Config(format!(
            "未找到 Maka LLM 配置: {}",
            connections_path.display()
        )));
    }

    let llm_connections = read_json_value(&connections_path)?;
    validate_connections(&llm_connections)?;

    let credentials_path = workspace.join("credentials.json");
    let credentials = if credentials_path.is_file() {
        let value = read_json_value(&credentials_path)?;
        validate_credentials(&value)?;
        Some(value)
    } else {
        None
    };

    secure_directory(backup_dir)?;
    let now = Utc::now();
    let bundle = MakaLlmBackupFile {
        schema_version: BACKUP_SCHEMA_VERSION,
        backup_type: backup_type.to_string(),
        created_at: now.to_rfc3339(),
        llm_connections,
        credentials,
    };
    let serialized =
        serde_json::to_vec_pretty(&bundle).map_err(|source| AppError::JsonSerialize { source })?;
    let filename = unique_backup_filename(backup_dir, backup_type, &now.format("%Y%m%d_%H%M%S"));
    let path = backup_dir.join(&filename);
    secure_atomic_write(&path, &serialized)?;

    let entry = backup_entry_from_bundle(&path, filename, &bundle)?;
    log::info!(
        "已创建 Maka LLM {}备份: {}（{} 个连接，{} 个凭据）",
        if backup_type == SAFETY_BACKUP {
            "安全"
        } else {
            ""
        },
        entry.filename,
        entry.connection_count,
        entry.credential_count
    );
    Ok(entry)
}

fn list_backups_at(backup_dir: &Path) -> Result<Vec<MakaLlmBackupEntry>, AppError> {
    if !backup_dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    for item in fs::read_dir(backup_dir).map_err(|error| AppError::io(backup_dir, error))? {
        let item = match item {
            Ok(item) => item,
            Err(error) => {
                log::warn!("读取 Maka LLM 备份目录项失败: {error}");
                continue;
            }
        };
        let path = item.path();
        let filename = item.file_name().to_string_lossy().into_owned();
        if validate_backup_filename(&filename).is_err() || !path.is_file() {
            continue;
        }
        match read_backup_file(&path)
            .and_then(|bundle| backup_entry_from_bundle(&path, filename.clone(), &bundle))
        {
            Ok(entry) => entries.push(entry),
            Err(error) => log::warn!("跳过无效 Maka LLM 备份 {}: {}", path.display(), error),
        }
    }
    entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(entries)
}

fn restore_backup_at(
    workspace: &Path,
    backup_dir: &Path,
    filename: &str,
) -> Result<MakaLlmRestoreResult, AppError> {
    validate_backup_filename(filename)?;
    let backup_path = backup_dir.join(filename);
    if !backup_path.is_file() {
        return Err(AppError::InvalidInput(format!(
            "Maka LLM 备份不存在: {filename}"
        )));
    }
    let bundle = read_backup_file(&backup_path)?;
    let connection_count = validate_connections(&bundle.llm_connections)?;
    let credential_count = bundle
        .credentials
        .as_ref()
        .map(validate_credentials)
        .transpose()?
        .unwrap_or(0);

    let credentials_path = workspace.join("credentials.json");
    let _credential_lock = CredentialFileLock::acquire(&credentials_path, CREDENTIAL_LOCK_TIMEOUT)?;

    let connections_path = workspace.join("llm-connections.json");
    let previous_connections = read_optional(&connections_path)?;
    let previous_credentials = read_optional(&credentials_path)?;
    if previous_connections.is_none() && previous_credentials.is_some() {
        return Err(AppError::Config(
            "Maka 当前凭据缺少配套连接配置，无法创建安全备份，恢复已取消".to_string(),
        ));
    }
    let safety_backup = if previous_connections.is_some() {
        Some(create_backup_at(workspace, backup_dir, SAFETY_BACKUP)?.filename)
    } else {
        None
    };

    let connections_bytes = serde_json::to_vec_pretty(&bundle.llm_connections)
        .map_err(|source| AppError::JsonSerialize { source })?;
    let credentials_bytes = bundle
        .credentials
        .as_ref()
        .map(serde_json::to_vec_pretty)
        .transpose()
        .map_err(|source| AppError::JsonSerialize { source })?;

    let apply_result = (|| {
        match credentials_bytes.as_deref() {
            Some(bytes) => secure_atomic_write(&credentials_path, bytes)?,
            None => restore_optional_secure(&credentials_path, None)?,
        }
        atomic_write(&connections_path, &connections_bytes)?;

        let restored_connections = read_json_value(&connections_path)?;
        if restored_connections != bundle.llm_connections {
            return Err(AppError::Config(
                "Maka LLM 连接配置恢复后校验不一致".to_string(),
            ));
        }
        match bundle.credentials.as_ref() {
            Some(expected) => {
                let restored_credentials = read_json_value(&credentials_path)?;
                if &restored_credentials != expected {
                    return Err(AppError::Config(
                        "Maka LLM 凭据恢复后校验不一致".to_string(),
                    ));
                }
            }
            None if credentials_path.exists() => {
                return Err(AppError::Config(
                    "Maka LLM 无凭据备份恢复后仍残留凭据".to_string(),
                ));
            }
            None => {}
        }
        Ok(())
    })();

    if let Err(error) = apply_result {
        let credentials_rollback = restore_optional_secure(&credentials_path, previous_credentials);
        let connections_rollback = restore_optional(&connections_path, previous_connections);
        if let Err(rollback_error) = credentials_rollback.and(connections_rollback) {
            return Err(AppError::Config(format!(
                "Maka LLM 配置恢复失败: {error}；自动回滚也失败: {rollback_error}"
            )));
        }
        return Err(error);
    }

    log::info!(
        "已恢复 Maka LLM 备份: {filename}（{} 个连接，{} 个凭据）",
        connection_count,
        credential_count
    );
    Ok(MakaLlmRestoreResult {
        safety_backup_filename: safety_backup,
        connection_count,
        credential_count,
        includes_credentials: bundle.credentials.is_some(),
    })
}

fn read_backup_file(path: &Path) -> Result<MakaLlmBackupFile, AppError> {
    let bytes = fs::read(path).map_err(|error| AppError::io(path, error))?;
    let bundle: MakaLlmBackupFile =
        serde_json::from_slice(&bytes).map_err(|source| AppError::json(path, source))?;
    if bundle.schema_version != BACKUP_SCHEMA_VERSION {
        return Err(AppError::Config(format!(
            "不支持的 Maka LLM 备份版本: {}",
            bundle.schema_version
        )));
    }
    if bundle.backup_type != MANUAL_BACKUP && bundle.backup_type != SAFETY_BACKUP {
        return Err(AppError::Config("Maka LLM 备份类型无效".to_string()));
    }
    validate_connections(&bundle.llm_connections)?;
    if let Some(credentials) = bundle.credentials.as_ref() {
        validate_credentials(credentials)?;
    }
    Ok(bundle)
}

fn backup_entry_from_bundle(
    path: &Path,
    filename: String,
    bundle: &MakaLlmBackupFile,
) -> Result<MakaLlmBackupEntry, AppError> {
    Ok(MakaLlmBackupEntry {
        filename,
        size_bytes: fs::metadata(path)
            .map_err(|error| AppError::io(path, error))?
            .len(),
        created_at: bundle.created_at.clone(),
        backup_type: bundle.backup_type.clone(),
        connection_count: validate_connections(&bundle.llm_connections)?,
        credential_count: bundle
            .credentials
            .as_ref()
            .map(validate_credentials)
            .transpose()?
            .unwrap_or(0),
        includes_credentials: bundle.credentials.is_some(),
    })
}

fn validate_connections(value: &Value) -> Result<usize, AppError> {
    let object = value
        .as_object()
        .ok_or_else(|| AppError::Config("Maka LLM 连接配置必须是对象".to_string()))?;
    let connections = object
        .get("connections")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::Config("Maka LLM 连接配置缺少 connections 数组".to_string()))?;
    if let Some(default_slug) = object.get("defaultSlug") {
        if !default_slug.is_null() && !default_slug.is_string() {
            return Err(AppError::Config(
                "Maka LLM defaultSlug 必须是字符串或 null".to_string(),
            ));
        }
    }
    for connection in connections {
        let item = connection
            .as_object()
            .ok_or_else(|| AppError::Config("Maka LLM 连接条目必须是对象".to_string()))?;
        if item
            .get("slug")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
        {
            return Err(AppError::Config("Maka LLM 连接条目缺少 slug".to_string()));
        }
        if item
            .get("providerType")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
        {
            return Err(AppError::Config(
                "Maka LLM 连接条目缺少 providerType".to_string(),
            ));
        }
    }
    Ok(connections.len())
}

fn validate_credentials(value: &Value) -> Result<usize, AppError> {
    let object = value
        .as_object()
        .ok_or_else(|| AppError::Config("Maka credentials.json 必须是对象".to_string()))?;
    if !object.get("version").is_some_and(Value::is_number) {
        return Err(AppError::Config(
            "Maka credentials.json 缺少有效 version".to_string(),
        ));
    }
    let values = object
        .get("values")
        .and_then(Value::as_object)
        .ok_or_else(|| AppError::Config("Maka credentials.json 缺少 values 对象".to_string()))?;
    if values.values().any(|value| !value.is_string()) {
        return Err(AppError::Config(
            "Maka credentials.json 包含非字符串凭据".to_string(),
        ));
    }
    Ok(values.len())
}

fn validate_backup_filename(filename: &str) -> Result<(), AppError> {
    let valid = filename.starts_with(BACKUP_PREFIX)
        && filename.ends_with(BACKUP_SUFFIX)
        && !filename.contains("..")
        && !filename.contains('/')
        && !filename.contains('\\')
        && !filename.contains('\0');
    if !valid {
        return Err(AppError::InvalidInput(
            "Maka LLM 备份文件名无效".to_string(),
        ));
    }
    Ok(())
}

fn unique_backup_filename(
    backup_dir: &Path,
    backup_type: &str,
    timestamp: &impl std::fmt::Display,
) -> String {
    let type_suffix = if backup_type == SAFETY_BACKUP {
        "_safety"
    } else {
        ""
    };
    let base = format!("{BACKUP_PREFIX}{timestamp}{type_suffix}");
    let first = format!("{base}{BACKUP_SUFFIX}");
    if !backup_dir.join(&first).exists() {
        return first;
    }
    for sequence in 1..=9999 {
        let candidate = format!("{base}_{sequence}{BACKUP_SUFFIX}");
        if !backup_dir.join(&candidate).exists() {
            return candidate;
        }
    }
    format!("{base}_{}{BACKUP_SUFFIX}", Uuid::new_v4())
}

fn read_json_value(path: &Path) -> Result<Value, AppError> {
    let bytes = fs::read(path).map_err(|error| AppError::io(path, error))?;
    serde_json::from_slice(&bytes).map_err(|source| AppError::json(path, source))
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, AppError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::io(path, error)),
    }
}

fn restore_optional(path: &Path, previous: Option<Vec<u8>>) -> Result<(), AppError> {
    match previous {
        Some(bytes) => atomic_write(path, &bytes),
        None if path.exists() => fs::remove_file(path).map_err(|error| AppError::io(path, error)),
        None => Ok(()),
    }
}

fn restore_optional_secure(path: &Path, previous: Option<Vec<u8>>) -> Result<(), AppError> {
    match previous {
        Some(bytes) => secure_atomic_write(path, &bytes),
        None if path.exists() => fs::remove_file(path).map_err(|error| AppError::io(path, error)),
        None => Ok(()),
    }
}

fn secure_directory(path: &Path) -> Result<(), AppError> {
    fs::create_dir_all(path).map_err(|error| AppError::io(path, error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| AppError::io(path, error))?;
    }
    Ok(())
}

fn secure_atomic_write(path: &Path, data: &[u8]) -> Result<(), AppError> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Config("安全写入路径无效".to_string()))?;
    secure_directory(parent)?;
    let filename = path
        .file_name()
        .ok_or_else(|| AppError::Config("安全写入文件名无效".to_string()))?
        .to_string_lossy();
    let temp_path = parent.join(format!(".{filename}.{}.tmp", Uuid::new_v4()));

    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp_path)
            .map_err(|error| AppError::io(&temp_path, error))?;
        file.write_all(data)
            .map_err(|error| AppError::io(&temp_path, error))?;
        file.sync_all()
            .map_err(|error| AppError::io(&temp_path, error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o600))
                .map_err(|error| AppError::io(&temp_path, error))?;
        }
        #[cfg(windows)]
        if path.exists() {
            fs::remove_file(path).map_err(|error| AppError::io(path, error))?;
        }
        fs::rename(&temp_path, path).map_err(|error| AppError::IoContext {
            context: format!(
                "安全原子替换失败: {} -> {}",
                temp_path.display(),
                path.display()
            ),
            source: error,
        })?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_fixture(workspace: &Path, slug: &str, token: &str) {
        fs::create_dir_all(workspace).unwrap();
        fs::write(
            workspace.join("llm-connections.json"),
            serde_json::to_vec_pretty(&json!({
                "defaultSlug": slug,
                "connections": [{
                    "slug": slug,
                    "name": "Fixture",
                    "providerType": "openai-responses-compatible",
                    "defaultModel": "gpt-test",
                    "enabled": true
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        secure_atomic_write(
            &workspace.join("credentials.json"),
            &serde_json::to_vec_pretty(&json!({
                "version": 1,
                "values": {format!("{slug}:apiKey"): token}
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn creates_metadata_only_backup_with_protected_permissions() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let backup_dir = root.path().join("backups");
        write_fixture(&workspace, "relay", "secret-one");

        let entry = create_backup_at(&workspace, &backup_dir, MANUAL_BACKUP).unwrap();
        assert_eq!(entry.connection_count, 1);
        assert_eq!(entry.credential_count, 1);
        assert!(entry.includes_credentials);
        assert_eq!(entry.backup_type, MANUAL_BACKUP);

        let listed = list_backups_at(&backup_dir).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].filename, entry.filename);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = fs::metadata(&backup_dir).unwrap().permissions().mode() & 0o777;
            let file_mode = fs::metadata(backup_dir.join(entry.filename))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
            assert_eq!(file_mode, 0o600);
        }
    }

    #[test]
    fn restore_replaces_connections_and_credentials_and_creates_safety_backup() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let backup_dir = root.path().join("backups");
        write_fixture(&workspace, "before", "secret-before");
        let target = create_backup_at(&workspace, &backup_dir, MANUAL_BACKUP).unwrap();

        write_fixture(&workspace, "after", "secret-after");
        let result = restore_backup_at(&workspace, &backup_dir, &target.filename).unwrap();
        assert_eq!(result.connection_count, 1);
        assert_eq!(result.credential_count, 1);
        assert!(result.safety_backup_filename.is_some());

        let restored_connections =
            read_json_value(&workspace.join("llm-connections.json")).unwrap();
        assert_eq!(restored_connections["defaultSlug"], "before");
        let restored_credentials = read_json_value(&workspace.join("credentials.json")).unwrap();
        assert_eq!(
            restored_credentials["values"]["before:apiKey"],
            "secret-before"
        );

        let backups = list_backups_at(&backup_dir).unwrap();
        assert_eq!(backups.len(), 2);
        assert!(backups
            .iter()
            .any(|entry| entry.backup_type == SAFETY_BACKUP));
    }

    #[test]
    fn restore_without_credentials_removes_current_credentials_and_keeps_safety_copy() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let backup_dir = root.path().join("backups");
        write_fixture(&workspace, "without-creds", "temporary-secret");
        fs::remove_file(workspace.join("credentials.json")).unwrap();
        let target = create_backup_at(&workspace, &backup_dir, MANUAL_BACKUP).unwrap();
        assert!(!target.includes_credentials);

        write_fixture(&workspace, "current", "secret-that-must-not-survive");
        let result = restore_backup_at(&workspace, &backup_dir, &target.filename).unwrap();

        assert!(!workspace.join("credentials.json").exists());
        let safety_filename = result.safety_backup_filename.expect("safety backup");
        let safety = read_backup_file(&backup_dir.join(safety_filename)).unwrap();
        assert_eq!(
            safety.credentials.unwrap()["values"]["current:apiKey"],
            "secret-that-must-not-survive"
        );
    }

    #[test]
    fn restore_fails_closed_when_credentials_exist_without_connections() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let backup_dir = root.path().join("backups");
        write_fixture(&workspace, "target", "temporary-secret");
        fs::remove_file(workspace.join("credentials.json")).unwrap();
        let target = create_backup_at(&workspace, &backup_dir, MANUAL_BACKUP).unwrap();

        fs::remove_file(workspace.join("llm-connections.json")).unwrap();
        secure_atomic_write(
            &workspace.join("credentials.json"),
            &serde_json::to_vec_pretty(&json!({
                "version": 1,
                "values": {"orphan:apiKey": "must-survive"}
            }))
            .unwrap(),
        )
        .unwrap();

        let error = restore_backup_at(&workspace, &backup_dir, &target.filename)
            .expect_err("restore must not delete credentials without a safety backup");
        assert!(error.to_string().contains("无法创建安全备份"));
        let credentials = read_json_value(&workspace.join("credentials.json")).unwrap();
        assert_eq!(credentials["values"]["orphan:apiKey"], "must-survive");
        assert!(!workspace.join("llm-connections.json").exists());
    }

    #[test]
    fn rejects_path_traversal_and_invalid_backup_payloads() {
        assert!(validate_backup_filename("../credentials.json").is_err());
        assert!(validate_connections(&json!({"connections": {}})).is_err());
        assert!(validate_credentials(&json!({"version": 1, "values": {"x": 1}})).is_err());
    }

    #[test]
    fn credential_lock_fails_closed_while_another_writer_holds_it() {
        let root = tempfile::tempdir().unwrap();
        let credentials_path = root.path().join("credentials.json");
        let first = CredentialFileLock::acquire(&credentials_path, Duration::from_millis(50))
            .expect("acquire first lock");

        let error = CredentialFileLock::acquire(&credentials_path, Duration::from_millis(50))
            .err()
            .expect("second writer must not steal lock");
        assert!(error.to_string().contains("Maka 正在更新凭据"));

        drop(first);
        CredentialFileLock::acquire(&credentials_path, Duration::from_millis(50))
            .expect("lock should be reusable after release");
    }
}
