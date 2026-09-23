//! DeepSeek Harness (DSH) native config adapter.
//!
//! DSH keeps every pi-ai provider under the llm-pi-ai.providers mapping in
//! ~/.dsh/settings.yaml, and the active provider/model under the top-level
//! agent-default-model section. CC Switch manages provider entries additively
//! (all providers coexist) and points agent-default-model at the provider the
//! user activates.
//!
//! Writes replace only the affected top-level YAML sections so every other
//! section (and its formatting) is preserved byte-for-byte.

use crate::config::{atomic_write_private, get_app_config_dir, get_home_dir};
use crate::error::AppError;
use crate::settings::{effective_backup_retain_count, get_dsh_override_dir};
use chrono::Local;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const PROVIDERS_SECTION: &str = "llm-pi-ai";
const PROVIDERS_KEY: &str = "providers";
const DEFAULT_MODEL_SECTION: &str = "agent-default-model";
const SETTINGS_FILENAME: &str = "settings.yaml";

fn write_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Resolve the DSH home directory.
///
/// Order: CC Switch explicit override, DSH_HOME (trimmed, used verbatim like
/// DSH itself), then ~/.dsh.
pub fn get_dsh_dir() -> PathBuf {
    if let Some(override_dir) = get_dsh_override_dir() {
        return override_dir;
    }

    if let Some(raw) = std::env::var_os("DSH_HOME") {
        let value = raw.to_string_lossy();
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }

    get_home_dir().join(".dsh")
}

pub fn get_dsh_settings_path() -> PathBuf {
    get_dsh_dir().join(SETTINGS_FILENAME)
}

fn read_settings_raw() -> Result<String, AppError> {
    let path = get_dsh_settings_path();
    if !path.exists() {
        return Ok(String::new());
    }
    fs::read_to_string(&path).map_err(|e| AppError::io(&path, e))
}

fn parse_settings(raw: &str) -> Result<serde_yaml::Value, AppError> {
    if raw.trim().is_empty() {
        return Ok(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    }
    serde_yaml::from_str(raw)
        .map_err(|e| AppError::Config(format!("DSH settings.yaml 解析失败: {e}")))
}

fn json_to_yaml(value: &serde_json::Value) -> Result<serde_yaml::Value, AppError> {
    serde_yaml::to_value(value)
        .map_err(|e| AppError::Config(format!("DSH 配置转换为 YAML 失败: {e}")))
}

fn yaml_to_json(value: &serde_yaml::Value) -> Result<serde_json::Value, AppError> {
    serde_json::to_value(value)
        .map_err(|e| AppError::Config(format!("DSH 配置转换为 JSON 失败: {e}")))
}

// ---------------------------------------------------------------------------
// YAML section-level replacement (preserves every untouched section)
// ---------------------------------------------------------------------------

fn is_top_level_key_line(line: &str) -> bool {
    if line.is_empty() {
        return false;
    }
    let first = line.as_bytes()[0];
    if matches!(first, b' ' | b'\t' | b'#' | b'-') {
        return false;
    }
    match line.find(':') {
        Some(colon) => {
            let after = &line[colon + 1..];
            after.is_empty() || after.starts_with([' ', '\t', '\r', '\n'])
        }
        None => false,
    }
}

fn find_section_range(raw: &str, section_key: &str) -> Option<(usize, usize)> {
    let target = format!("{section_key}:");
    let mut start = None;
    let mut offset = 0;
    for line in raw.split('\n') {
        if start.is_none() && is_top_level_key_line(line) && line.starts_with(&target) {
            let after = &line[target.len()..];
            if after.is_empty() || after.starts_with([' ', '\t', '\r']) {
                start = Some(offset);
            }
        } else if start.is_some() && is_top_level_key_line(line) {
            return Some((start.unwrap(), offset));
        }
        offset += line.len() + 1;
    }
    start.map(|start| (start, raw.len()))
}

fn serialize_section(key: &str, value: &serde_yaml::Value) -> Result<String, AppError> {
    let mut mapping = serde_yaml::Mapping::new();
    mapping.insert(serde_yaml::Value::String(key.to_string()), value.clone());
    serde_yaml::to_string(&serde_yaml::Value::Mapping(mapping))
        .map_err(|e| AppError::Config(format!("DSH 配置序列化失败: {e}")))
}

fn remove_all_sections(raw: &str, section_key: &str) -> String {
    let mut result = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some((start, end)) = find_section_range(rest, section_key) {
        result.push_str(&rest[..start]);
        rest = &rest[end..];
    }
    result.push_str(rest);
    result
}

fn replace_section(
    raw: &str,
    section_key: &str,
    value: &serde_yaml::Value,
) -> Result<String, AppError> {
    let serialized = serialize_section(section_key, value)?;
    if let Some((start, end)) = find_section_range(raw, section_key) {
        let mut result = String::with_capacity(raw.len());
        result.push_str(&raw[..start]);
        result.push_str(&serialized);
        let remainder = remove_all_sections(&raw[end..], section_key);
        if !serialized.ends_with('\n') && !remainder.is_empty() && !remainder.starts_with('\n') {
            result.push('\n');
        }
        result.push_str(&remainder);
        Ok(result)
    } else {
        let mut result = raw.to_string();
        if !result.is_empty() && !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str(&serialized);
        if !result.ends_with('\n') {
            result.push('\n');
        }
        Ok(result)
    }
}

fn create_backup(source: &str) -> Result<(), AppError> {
    if source.trim().is_empty() {
        return Ok(());
    }
    let backup_dir = get_app_config_dir().join("backups").join("dsh");
    fs::create_dir_all(&backup_dir).map_err(|e| AppError::io(&backup_dir, e))?;
    let stamp = Local::now().format("%Y%m%d_%H%M%S");
    let path = backup_dir.join(format!("settings_{stamp}.yaml"));
    atomic_write_private(&path, source.as_bytes())?;
    cleanup_backups(&backup_dir);
    Ok(())
}

fn cleanup_backups(dir: &Path) {
    let retain = effective_backup_retain_count();
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|ext| ext == "yaml" || ext == "yml")
        })
        .collect();
    if files.len() <= retain {
        return;
    }
    files.sort_by_key(|entry| entry.metadata().and_then(|m| m.modified()).ok());
    let remove_count = files.len() - retain;
    for entry in files.into_iter().take(remove_count) {
        if let Err(err) = fs::remove_file(entry.path()) {
            log::warn!("清理 DSH 旧备份失败 {}: {err}", entry.path().display());
        }
    }
}

fn write_section_locked(section_key: &str, value: &serde_yaml::Value) -> Result<(), AppError> {
    let raw = read_settings_raw()?;
    let updated = replace_section(&raw, section_key, value)?;
    create_backup(&raw)?;
    let path = get_dsh_settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    }
    atomic_write_private(&path, updated.as_bytes())
}

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

fn yaml_get<'a>(value: &'a serde_yaml::Value, keys: &[&str]) -> Option<&'a serde_yaml::Value> {
    let mut current = value;
    for key in keys {
        current = current.get(*key)?;
    }
    Some(current)
}

/// Read every managed pi-ai provider keyed by provider id.
pub fn get_providers() -> Result<serde_json::Map<String, serde_json::Value>, AppError> {
    let settings = parse_settings(&read_settings_raw()?)?;
    let Some(providers) = yaml_get(&settings, &[PROVIDERS_SECTION, PROVIDERS_KEY]) else {
        return Ok(serde_json::Map::new());
    };
    match yaml_to_json(providers)? {
        serde_json::Value::Object(map) => Ok(map),
        _ => Ok(serde_json::Map::new()),
    }
}

/// Read one provider entry by id.
#[allow(dead_code)] // public read API for future per-provider inspection
pub fn get_provider(id: &str) -> Result<Option<serde_json::Value>, AppError> {
    Ok(get_providers()?.get(id).cloned())
}

/// Upsert one provider entry, preserving on-disk fields the payload omits.
pub fn set_provider(id: &str, config: serde_json::Value) -> Result<(), AppError> {
    let _guard = write_lock()
        .lock()
        .map_err(|_| AppError::Config("DSH settings.yaml 写锁已损坏".to_string()))?;
    let raw = read_settings_raw()?;
    let mut settings = parse_settings(&raw)?;
    let root = settings
        .as_mapping_mut()
        .ok_or_else(|| AppError::Config("DSH settings.yaml 顶层必须是映射".to_string()))?;
    let providers = ensure_child_mapping(root, PROVIDERS_SECTION, PROVIDERS_KEY)?;
    let key = serde_yaml::Value::String(id.to_string());
    let mut incoming = json_to_yaml(&config)?;
    if let Some(existing) = providers.get(&key) {
        merge_missing_fields(existing, &mut incoming);
    }
    providers.insert(key, incoming);
    let section = yaml_get(&settings, &[PROVIDERS_SECTION])
        .cloned()
        .ok_or_else(|| AppError::Config("DSH llm-pi-ai 配置缺失".to_string()))?;
    write_section_locked(PROVIDERS_SECTION, &section)
}

pub fn remove_provider(id: &str) -> Result<bool, AppError> {
    let _guard = write_lock()
        .lock()
        .map_err(|_| AppError::Config("DSH settings.yaml 写锁已损坏".to_string()))?;
    let raw = read_settings_raw()?;
    let mut settings = parse_settings(&raw)?;
    let Some(root) = settings.as_mapping_mut() else {
        return Ok(false);
    };
    let Some(pi) = root.get_mut(serde_yaml::Value::String(PROVIDERS_SECTION.to_string())) else {
        return Ok(false);
    };
    let Some(pi_map) = pi.as_mapping_mut() else {
        return Ok(false);
    };
    let Some(providers) = pi_map.get_mut(serde_yaml::Value::String(PROVIDERS_KEY.to_string()))
    else {
        return Ok(false);
    };
    let Some(providers_map) = providers.as_mapping_mut() else {
        return Ok(false);
    };
    let removed = providers_map
        .remove(serde_yaml::Value::String(id.to_string()))
        .is_some();
    if !removed {
        return Ok(false);
    }
    let section = yaml_get(&settings, &[PROVIDERS_SECTION])
        .cloned()
        .ok_or_else(|| AppError::Config("DSH llm-pi-ai 配置缺失".to_string()))?;
    write_section_locked(PROVIDERS_SECTION, &section)?;
    Ok(true)
}

fn ensure_child_mapping<'a>(
    mapping: &'a mut serde_yaml::Mapping,
    section: &str,
    child: &str,
) -> Result<&'a mut serde_yaml::Mapping, AppError> {
    let section_key = serde_yaml::Value::String(section.to_string());
    if !mapping.contains_key(&section_key) {
        mapping.insert(
            section_key.clone(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }
    let section_value = mapping
        .get_mut(&section_key)
        .ok_or_else(|| AppError::Config("DSH llm-pi-ai 配置缺失".to_string()))?;
    if !section_value.is_mapping() {
        return Err(AppError::Config("DSH llm-pi-ai 配置必须是映射".to_string()));
    }
    let child_key = serde_yaml::Value::String(child.to_string());
    let section_map = section_value
        .as_mapping_mut()
        .ok_or_else(|| AppError::Config("DSH llm-pi-ai 配置必须是映射".to_string()))?;
    if !section_map.contains_key(&child_key) {
        section_map.insert(
            child_key.clone(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }
    section_map
        .get_mut(&child_key)
        .and_then(|value| value.as_mapping_mut())
        .ok_or_else(|| AppError::Config("DSH llm-pi-ai.providers 必须是映射".to_string()))
}

/// Carry over on-disk keys the incoming payload did not mention.
fn merge_missing_fields(existing: &serde_yaml::Value, incoming: &mut serde_yaml::Value) {
    let (serde_yaml::Value::Mapping(existing_map), serde_yaml::Value::Mapping(incoming_map)) =
        (existing, incoming)
    else {
        return;
    };
    for (key, value) in existing_map {
        if !incoming_map.contains_key(key) {
            incoming_map.insert(key.clone(), value.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// Active provider / model
// ---------------------------------------------------------------------------

/// Read the active agent-default-model section as (provider, model).
#[allow(dead_code)] // public read API; set_default_model is the write side
pub fn get_default_model() -> Result<Option<(String, String)>, AppError> {
    let settings = parse_settings(&read_settings_raw()?)?;
    let Some(section) = yaml_get(&settings, &[DEFAULT_MODEL_SECTION]) else {
        return Ok(None);
    };
    let provider = section
        .get("provider")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let model = section
        .get("model")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    Ok(provider.map(|provider| (provider, model.unwrap_or_default())))
}

/// Point agent-default-model at a provider and (optionally) its first model.
pub fn set_default_model(provider_id: &str, model_id: Option<&str>) -> Result<(), AppError> {
    let _guard = write_lock()
        .lock()
        .map_err(|_| AppError::Config("DSH settings.yaml 写锁已损坏".to_string()))?;
    let raw = read_settings_raw()?;
    let mut settings = parse_settings(&raw)?;
    let root = settings
        .as_mapping_mut()
        .ok_or_else(|| AppError::Config("DSH settings.yaml 顶层必须是映射".to_string()))?;
    let key = serde_yaml::Value::String(DEFAULT_MODEL_SECTION.to_string());
    if !root.contains_key(&key) {
        root.insert(
            key.clone(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }
    let section = root
        .get_mut(&key)
        .and_then(|value| value.as_mapping_mut())
        .ok_or_else(|| AppError::Config("DSH agent-default-model 必须是映射".to_string()))?;
    section.insert(
        serde_yaml::Value::String("provider".to_string()),
        serde_yaml::Value::String(provider_id.to_string()),
    );
    if let Some(model_id) = model_id.map(str::trim).filter(|model| !model.is_empty()) {
        section.insert(
            serde_yaml::Value::String("model".to_string()),
            serde_yaml::Value::String(model_id.to_string()),
        );
    }
    let section = yaml_get(&settings, &[DEFAULT_MODEL_SECTION])
        .cloned()
        .ok_or_else(|| AppError::Config("DSH agent-default-model 配置缺失".to_string()))?;
    write_section_locked(DEFAULT_MODEL_SECTION, &section)
}

/// First non-empty model id in a provider payload models list.
pub fn first_model_id(settings_config: &serde_json::Value) -> Option<String> {
    settings_config
        .get("models")
        .and_then(|value| value.as_array())
        .and_then(|models| models.first())
        .and_then(|model| model.get("id"))
        .and_then(|id| id.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    struct TempDsh {
        _dir: tempfile::TempDir,
        previous: Option<std::ffi::OsString>,
    }

    impl TempDsh {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let previous = std::env::var_os("DSH_HOME");
            std::env::set_var("DSH_HOME", dir.path());
            Self {
                _dir: dir,
                previous,
            }
        }
    }

    impl Drop for TempDsh {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var("DSH_HOME", value),
                None => std::env::remove_var("DSH_HOME"),
            }
        }
    }

    fn write_settings(text: &str) {
        fs::write(get_dsh_settings_path(), text).unwrap();
    }

    #[test]
    #[serial]
    fn set_provider_preserves_unrelated_sections_and_existing_fields() {
        let _home = TempDsh::new();
        write_settings(
            "locale:\n  preference: zh\nllm-pi-ai:\n  providers:\n    keep:\n      baseURL: http://keep/v1\nagent-default-model:\n  provider: keep\n  model: m1\n",
        );

        set_provider(
            "keep",
            serde_json::json!({"displayName": "Keep", "baseURL": "http://keep/v1", "apiKeyEnv": "KEEP_KEY"}),
        )
        .unwrap();
        set_provider(
            "new",
            serde_json::json!({"baseURL": "http://new/v1", "models": [{"id": "n1"}]}),
        )
        .unwrap();

        let raw = fs::read_to_string(get_dsh_settings_path()).unwrap();
        assert!(raw.contains("locale:"));
        assert!(raw.contains("preference: zh"));
        assert!(raw.contains("agent-default-model:"));

        let providers = get_providers().unwrap();
        assert_eq!(providers.len(), 2);
        assert_eq!(providers["keep"]["apiKeyEnv"], "KEEP_KEY");
        assert_eq!(providers["keep"]["displayName"], "Keep");
        assert_eq!(providers["new"]["baseURL"], "http://new/v1");
    }

    #[test]
    #[serial]
    fn remove_provider_drops_only_the_target() {
        let _home = TempDsh::new();
        write_settings("llm-pi-ai:\n  providers:\n    a:\n      baseURL: http://a/v1\n    b:\n      baseURL: http://b/v1\n");
        assert!(remove_provider("a").unwrap());
        assert!(!remove_provider("missing").unwrap());
        let providers = get_providers().unwrap();
        assert_eq!(providers.len(), 1);
        assert!(providers.contains_key("b"));
    }

    #[test]
    #[serial]
    fn set_default_model_updates_active_provider() {
        let _home = TempDsh::new();
        write_settings("llm-pi-ai:\n  providers: {}\nagent-default-model:\n  provider: old\n  model: old-model\n  reasoningEffort: high\n");
        set_default_model("new", Some("new-model")).unwrap();
        let (provider, model) = get_default_model().unwrap().unwrap();
        assert_eq!(provider, "new");
        assert_eq!(model, "new-model");
        let raw = fs::read_to_string(get_dsh_settings_path()).unwrap();
        assert!(raw.contains("reasoningEffort: high"));
    }
}
