//! Google Gemini CLI 及其继任者 Antigravity CLI（`agy`）的多账号备份与切换。
//!
//! Gemini CLI 已并入 Antigravity CLI，两者共用 `~/.gemini` 目录：
//! - 旧版 Gemini CLI：`~/.gemini/oauth_creds.json` 等文件；
//! - Antigravity CLI：`~/.gemini/antigravity-cli/antigravity-oauth-token` 等。
//! 两者都纳入快照与识别，账号识别完全基于本地文件（OAuth token、账号缓存、
//! antigravity 日志中的认证记录）。

use crate::{copy_path, remove_path, Roots};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    cmp::Reverse,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::accounts::{AccountManifest, AccountProfile};

const MANIFEST_VERSION: u32 = 1;
static ACCOUNT_ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Gemini / Antigravity CLI 登录状态涉及的文件，均位于 `~/.gemini` 下。
struct GeminiPath {
    tag: &'static str,
    relative: &'static str,
}

const GEMINI_PATHS: &[GeminiPath] = &[
    // Antigravity CLI（agy）
    GeminiPath {
        tag: "agy_oauth_token",
        relative: "antigravity-cli/antigravity-oauth-token",
    },
    GeminiPath {
        tag: "agy_settings",
        relative: "antigravity-cli/settings.json",
    },
    // 旧版 Gemini CLI
    GeminiPath {
        tag: "oauth_creds",
        relative: "oauth_creds.json",
    },
    GeminiPath {
        tag: "google_accounts",
        relative: "google_accounts.json",
    },
    GeminiPath {
        tag: "google_web_accounts",
        relative: "google_web_accounts.json",
    },
    GeminiPath {
        tag: "settings",
        relative: "settings.json",
    },
    GeminiPath {
        tag: "adc_creds",
        relative: "application_default_credentials.json",
    },
];

/// 快照与恢复的项目及顺序。
const GEMINI_TAGS: &[&str] = &[
    "agy_oauth_token",
    "agy_settings",
    "oauth_creds",
    "google_accounts",
    "google_web_accounts",
    "settings",
    "adc_creds",
];
/// 用户配置类项目：快照缺失时保留本机现状而不是清空，避免旧备份切换清掉
/// 用户自定义设置（含认证方式选择）。
const GEMINI_PRESERVE_IF_ABSENT_TAGS: &[&str] = &["settings", "agy_settings"];

/// 当前 Gemini 登录状态的可读标识，全部字段尽力提取，允许缺失。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GeminiIdentity {
    pub email: Option<String>,
    /// settings.json 中选择的认证方式（如 oauth-personal / gemini-api-key）。
    pub auth_type: Option<String>,
    /// oauth_creds.json 中 refresh_token 的摘要（SHA-256 前 12 位）；
    /// token 刷新会重写 access_token，但 refresh_token 稳定，可用于匹配既有备份。
    pub fingerprint: Option<String>,
}

impl GeminiIdentity {
    pub fn is_present(&self) -> bool {
        self.email.is_some() || self.auth_type.is_some() || self.fingerprint.is_some()
    }

    /// 用账号标识推导默认备份名：邮箱前缀 > 指纹前缀。
    pub fn default_name(&self) -> Option<String> {
        if let Some(email) = self
            .email
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let base = email.split('@').next().unwrap_or(email);
            if !base.is_empty() {
                return Some(base.to_string());
            }
        }
        if let Some(fingerprint) = self.fingerprint.as_deref() {
            let len = fingerprint.len().min(8);
            let prefix = if self.is_antigravity() {
                "Antigravity"
            } else {
                "Gemini"
            };
            return Some(format!("{prefix}{}", &fingerprint[..len]));
        }
        None
    }

    /// 登录状态是否来自 Antigravity CLI（agy）。
    pub fn is_antigravity(&self) -> bool {
        self.auth_type
            .as_deref()
            .is_some_and(|auth| auth.starts_with("Antigravity"))
    }

    /// 一行文本描述，用于界面展示。
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(email) = self
            .email
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            parts.push(email.to_string());
        }
        if let Some(selected) = self
            .auth_type
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            parts.push(selected.to_string());
        }
        if let Some(fingerprint) = self.fingerprint.as_deref() {
            let len = fingerprint.len().min(12);
            parts.push(format!("指纹 {}", &fingerprint[..len]));
        }
        if parts.is_empty() {
            "未检测到 Gemini 登录状态".into()
        } else {
            parts.join(" · ")
        }
    }
}

fn auth_label(selected: &str) -> String {
    match selected {
        "oauth-personal" => "OAuth 个人账号",
        "gemini-api-key" => "API Key",
        "vertex-ai" => "Vertex AI",
        "cloud-shell" => "Cloud Shell",
        other => other,
    }
    .to_string()
}

pub fn gemini_dir(roots: &Roots) -> PathBuf {
    roots.user_profile.join(".gemini")
}

pub fn accounts_root(roots: &Roots) -> PathBuf {
    gemini_dir(roots).join("account_backups")
}

fn resolve_path(roots: &Roots, tag: &str) -> PathBuf {
    let entry = GEMINI_PATHS
        .iter()
        .find(|entry| entry.tag == tag)
        .expect("known gemini tag");
    entry
        .relative
        .split('/')
        .fold(gemini_dir(roots), |path, part| path.join(part))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn new_account_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = ACCOUNT_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("account-{nanos}-{sequence}")
}

/// 读取 Gemini / Antigravity CLI 登录状态文件，汇总当前账号标识。
/// Antigravity CLI（agy）优先；旧版 Gemini CLI 文件仅用于补齐缺失字段。
pub fn detect(roots: &Roots) -> GeminiIdentity {
    let gemini = gemini_dir(roots);
    let mut identity = GeminiIdentity::default();

    // Antigravity CLI（agy）：token 文件含内层 Google OAuth token 与 auth_method
    let agy_token = gemini.join("antigravity-cli").join("antigravity-oauth-token");
    if let Ok(bytes) = fs::read(&agy_token) {
        let value = serde_json::from_slice::<Value>(&bytes).ok();
        identity.fingerprint = Some(agy_fingerprint(&bytes, value.as_ref()));
        identity.auth_type = value
            .as_ref()
            .and_then(|value| value.get("auth_method"))
            .and_then(Value::as_str)
            .map(agy_auth_label);
        // agy 的 token 文件不含 id_token，邮箱只能从认证日志尽力提取
        identity.email = agy_email_from_logs(&gemini.join("antigravity-cli"));
    }

    // 旧版 Gemini CLI：google_accounts / oauth_creds(含 id_token)
    if identity.email.is_none() {
        if let Some(value) = read_json(&gemini.join("google_accounts.json")) {
            identity.email = email_from_google_accounts(&value);
        }
        if identity.email.is_none() {
            if let Some(value) = read_json(&gemini.join("oauth_creds.json")) {
                let id_token = value.get("id_token").and_then(Value::as_str).unwrap_or("");
                identity.email = crate::identity::decode_jwt_identity(id_token);
            }
        }
    }
    if identity.auth_type.is_none() {
        if let Some(value) = read_json(&gemini.join("settings.json")) {
            identity.auth_type = value
                .pointer("/security/auth/selectedType")
                .or_else(|| value.get("selectedAuthType"))
                .and_then(Value::as_str)
                .map(auth_label);
        }
    }
    if identity.fingerprint.is_none() {
        if let Ok(bytes) = fs::read(gemini.join("oauth_creds.json")) {
            identity.fingerprint = Some(oauth_fingerprint(&bytes));
        }
    }
    identity
}

fn read_json(path: &Path) -> Option<Value> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// agy 凭据指纹优先取内层 refresh_token（token 刷新只重写 access_token/expiry），
/// 文件不是 JSON 或缺少 refresh_token 时退回整个文件摘要。
fn agy_fingerprint(bytes: &[u8], value: Option<&Value>) -> String {
    let refresh = value
        .and_then(|value| value.pointer("/token/refresh_token"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty());
    match refresh {
        Some(token) => short_digest(token.as_bytes()),
        None => short_digest(bytes),
    }
}

fn agy_auth_label(method: &str) -> String {
    match method {
        "consumer" => "Antigravity 个人账号".to_string(),
        other => format!("Antigravity {other}"),
    }
}

/// 从 agy 认证日志提取账号邮箱（尽力而为）。每次 CLI 启动认证成功都会写入
/// `applyAuthResult: email=...` 日志；日志被清理时返回 None，不影响指纹匹配。
fn agy_email_from_logs(agy_dir: &Path) -> Option<String> {
    let log_dir = agy_dir.join("log");
    let mut files: Vec<(SystemTime, PathBuf)> = fs::read_dir(log_dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter_map(|path| {
            let modified = fs::metadata(&path).ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();
    // 最新日志优先：账号邮箱取最近一次认证成功的记录
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in files {
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        if let Some(email) = last_email_in_log(&content) {
            return Some(email);
        }
    }
    None
}

fn last_email_in_log(content: &str) -> Option<String> {
    const AUTH_RESULT: &str = "applyAuthResult: email=";
    const AUTH_SUCCESS: &str = "authenticated successfully as ";
    let mut found = None;
    for line in content.lines() {
        let candidate = if let Some(index) = line.find(AUTH_RESULT) {
            line[index + AUTH_RESULT.len()..].split(',').next()
        } else if let Some(index) = line.find(AUTH_SUCCESS) {
            Some(line[index + AUTH_SUCCESS.len()..].trim())
        } else {
            None
        };
        if let Some(value) = candidate.map(str::trim).filter(|value| value.contains('@')) {
            found = Some(value.to_string());
        }
    }
    found
}

/// google_accounts.json 兼容对象与数组两种缓存格式，取第一处的 email。
fn email_from_google_accounts(value: &Value) -> Option<String> {
    let entry = match value {
        Value::Array(items) => items.first()?,
        object @ Value::Object(_) => object,
        _ => return None,
    };
    entry
        .get("email")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|email| !email.is_empty())
        .map(str::to_string)
}

/// 指纹优先取 refresh_token：token 刷新会重写 access_token 等字段，
/// refresh_token 稳定不变；文件不是 JSON 或缺少 refresh_token 时退回整个文件摘要。
fn oauth_fingerprint(bytes: &[u8]) -> String {
    if let Some(value) = serde_json::from_slice::<Value>(bytes).ok().as_ref() {
        if let Some(token) = value
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|token| !token.is_empty())
        {
            return short_digest(token.as_bytes());
        }
    }
    short_digest(bytes)
}

fn short_digest(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    hex_encode(&digest)[..12].to_string()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn count_present_items(roots: &Roots) -> usize {
    GEMINI_TAGS
        .iter()
        .filter(|tag| resolve_path(roots, tag).exists())
        .count()
}

fn snapshot_to(roots: &Roots, target: &Path, manifest: &AccountManifest) -> Result<(), String> {
    let parent = target.parent().ok_or("Gemini 备份目录无效")?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temp = parent.join(format!(".{}.tmp", manifest.id));
    if temp.exists() {
        remove_path(&temp).map_err(|error| error.to_string())?;
    }
    fs::create_dir_all(temp.join("data")).map_err(|error| error.to_string())?;

    let result = (|| {
        for tag in GEMINI_TAGS {
            let source = resolve_path(roots, tag);
            if source.exists() {
                copy_path(&source, &temp.join("data").join(tag))
                    .map_err(|error| format!("备份 {tag} 失败: {error}"))?;
            }
        }
        let bytes = serde_json::to_vec_pretty(manifest).map_err(|error| error.to_string())?;
        fs::write(temp.join("manifest.json"), bytes).map_err(|error| error.to_string())?;
        Ok::<(), String>(())
    })();
    if let Err(error) = result {
        let _ = remove_path(&temp);
        return Err(error);
    }

    let old = parent.join(format!(".{}.old", manifest.id));
    if old.exists() {
        remove_path(&old).map_err(|error| error.to_string())?;
    }
    if target.exists() {
        fs::rename(target, &old).map_err(|error| format!("暂存旧备份失败: {error}"))?;
    }
    if let Err(error) = fs::rename(&temp, target) {
        if old.exists() {
            let _ = fs::rename(&old, target);
        }
        return Err(format!("保存 Gemini 备份失败: {error}"));
    }
    if old.exists() {
        remove_path(&old).map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub fn save_current_account(
    roots: &Roots,
    name: Option<&str>,
    existing: Option<&AccountProfile>,
) -> Result<AccountProfile, String> {
    let identity = detect(roots);
    let name = name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| identity.default_name())
        .unwrap_or_else(|| "我的Gemini账号".into());
    if count_present_items(roots) == 0 {
        return Err("未检测到可备份的 Gemini 账号数据".into());
    }
    let timestamp = now();
    let (id, created_at) = existing
        .map(|profile| (profile.manifest.id.clone(), profile.manifest.created_at))
        .unwrap_or_else(|| (new_account_id(), timestamp));
    let manifest = AccountManifest {
        version: MANIFEST_VERSION,
        id: id.clone(),
        name,
        alias: existing.and_then(|profile| profile.manifest.alias.clone()),
        phone: existing.and_then(|profile| profile.manifest.phone.clone()),
        identity: identity.is_present().then(|| identity.describe()),
        fingerprint: identity.fingerprint.clone(),
        created_at,
        updated_at: timestamp,
        item_count: count_present_items(roots),
    };
    let directory = accounts_root(roots).join(&id);
    snapshot_to(roots, &directory, &manifest)?;
    set_active_account(roots, Some(&id))?;
    Ok(AccountProfile {
        directory,
        manifest,
    })
}

pub fn list_accounts(roots: &Roots) -> Result<Vec<AccountProfile>, String> {
    let root = accounts_root(roots);
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut profiles = Vec::new();
    for entry in fs::read_dir(root).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        if !entry
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir()
            || entry.file_name().to_string_lossy().starts_with('.')
        {
            continue;
        }
        let manifest_path = entry.path().join("manifest.json");
        let bytes = match fs::read(&manifest_path) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let manifest = match serde_json::from_slice::<AccountManifest>(&bytes) {
            Ok(manifest) if manifest.version == MANIFEST_VERSION => manifest,
            _ => continue,
        };
        if manifest.id != entry.file_name().to_string_lossy() {
            continue;
        }
        profiles.push(AccountProfile {
            directory: entry.path(),
            manifest,
        });
    }
    profiles.sort_by_key(|profile| Reverse(profile.manifest.updated_at));
    Ok(profiles)
}

fn restore_data(roots: &Roots, data: &Path) -> Result<(), String> {
    for tag in GEMINI_TAGS {
        // 配置类项目在旧快照缺失时保留本机现状；其余项目必须先清理，
        // 避免上一账号的残留和新账号混在一起。
        if GEMINI_PRESERVE_IF_ABSENT_TAGS.contains(&tag) && !data.join(tag).exists() {
            continue;
        }
        let destination = resolve_path(roots, tag);
        if destination.exists() {
            remove_path(&destination).map_err(|error| format!("清理当前 {tag} 失败: {error}"))?;
        }
    }
    for tag in GEMINI_TAGS {
        let source = data.join(tag);
        if source.exists() {
            copy_path(&source, &resolve_path(roots, tag))
                .map_err(|error| format!("恢复 {tag} 失败: {error}"))?;
        }
    }
    Ok(())
}

pub fn switch_account(roots: &Roots, target: &AccountProfile) -> Result<(), String> {
    let data = target.directory.join("data");
    if !data.is_dir() {
        return Err("Gemini 备份不完整：缺少 data 目录".into());
    }
    if let Some(current_id) = active_account(roots) {
        if current_id != target.manifest.id {
            if let Some(current) = list_accounts(roots)?
                .into_iter()
                .find(|profile| profile.manifest.id == current_id)
            {
                save_current_account(roots, Some(&current.manifest.name), Some(&current))?;
            }
        }
    }
    let safety_root = accounts_root(roots).join(".switch-safety");
    let safety_manifest = AccountManifest {
        version: MANIFEST_VERSION,
        id: "switch-safety".into(),
        name: "切换前自动备份".into(),
        alias: None,
        phone: None,
        identity: None,
        fingerprint: None,
        created_at: now(),
        updated_at: now(),
        item_count: count_present_items(roots),
    };
    snapshot_to(roots, &safety_root, &safety_manifest)?;

    if let Err(error) = restore_data(roots, &data) {
        let rollback = restore_data(roots, &safety_root.join("data"));
        return match rollback {
            Ok(()) => Err(format!("切换失败，已恢复原账号: {error}")),
            Err(rollback_error) => Err(format!(
                "切换失败且自动回滚失败: {error}; 回滚错误: {rollback_error}; 安全备份位于 {}",
                safety_root.display()
            )),
        };
    }
    set_active_account(roots, Some(&target.manifest.id))?;
    Ok(())
}

pub fn delete_account(roots: &Roots, profile: &AccountProfile) -> Result<(), String> {
    remove_path(&profile.directory).map_err(|error| error.to_string())?;
    if active_account(roots).as_deref() == Some(profile.manifest.id.as_str()) {
        set_active_account(roots, None)?;
    }
    Ok(())
}

pub fn active_account(roots: &Roots) -> Option<String> {
    let bytes = fs::read(accounts_root(roots).join("active.json")).ok()?;
    serde_json::from_slice::<ActiveAccount>(&bytes)
        .ok()
        .map(|value| value.id)
}

#[derive(Serialize, Deserialize)]
struct ActiveAccount {
    id: String,
}

fn set_active_account(roots: &Roots, id: Option<&str>) -> Result<(), String> {
    let path = accounts_root(roots).join("active.json");
    if let Some(id) = id {
        fs::create_dir_all(accounts_root(roots)).map_err(|error| error.to_string())?;
        let bytes = serde_json::to_vec_pretty(&ActiveAccount { id: id.into() })
            .map_err(|error| error.to_string())?;
        fs::write(path, bytes).map_err(|error| error.to_string())?;
    } else if path.exists() {
        fs::remove_file(path).map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub fn open_accounts_folder(roots: &Roots) -> Result<(), String> {
    let root = accounts_root(roots);
    fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    #[cfg(windows)]
    std::process::Command::new("explorer")
        .arg(&root)
        .spawn()
        .map_err(|error| error.to_string())?;
    #[cfg(not(windows))]
    std::process::Command::new("xdg-open")
        .arg(&root)
        .spawn()
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// 尽力检测是否有 Gemini CLI / Antigravity CLI 会话正在运行（仅类 Unix 平台；
/// Windows 上两者分别由 node/Go 解释或编译，Windows 侧不做检测）。运行中的会话
/// 可能在切换后把旧凭据回写覆盖，界面据此提示用户先退出会话。
pub fn cli_running() -> bool {
    #[cfg(windows)]
    {
        false
    }
    #[cfg(not(windows))]
    {
        use std::process::{Command, Stdio};
        Command::new("pgrep")
            .args(["-f", r"(^|/)(gemini|agy)( |$)"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn roots(temp: &TempDir) -> Roots {
        Roots {
            user_profile: temp.path().join("user"),
            app_data: temp.path().join("appdata"),
        }
    }

    fn write_file(roots: &Roots, tag: &str, contents: &[u8]) {
        let path = resolve_path(roots, tag);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn saves_lists_and_switches_gemini_accounts() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        write_file(
            &roots,
            "oauth_creds",
            br#"{"access_token":"a1","refresh_token":"r1"}"#,
        );
        let account_a = save_current_account(&roots, Some("Gemini A"), None).unwrap();

        write_file(
            &roots,
            "oauth_creds",
            br#"{"access_token":"a2","refresh_token":"r2"}"#,
        );
        let account_b = save_current_account(&roots, Some("Gemini B"), None).unwrap();
        assert_eq!(list_accounts(&roots).unwrap().len(), 2);

        // access_token 变化但 refresh_token 不变：指纹稳定，仍与 B 匹配
        write_file(
            &roots,
            "oauth_creds",
            br#"{"access_token":"a2-refreshed","refresh_token":"r2"}"#,
        );
        switch_account(&roots, &account_a).unwrap();
        assert_eq!(
            fs::read(resolve_path(&roots, "oauth_creds")).unwrap(),
            br#"{"access_token":"a1","refresh_token":"r1"}"#,
        );
        assert_eq!(
            active_account(&roots).as_deref(),
            Some(account_a.manifest.id.as_str())
        );

        // 切走前当前状态（a2-refreshed）已自动保存进 B，再切回应恢复
        switch_account(&roots, &account_b).unwrap();
        assert_eq!(
            fs::read(resolve_path(&roots, "oauth_creds")).unwrap(),
            br#"{"access_token":"a2-refreshed","refresh_token":"r2"}"#,
        );
    }

    #[test]
    fn restore_replaces_settings_but_preserves_it_when_snapshot_lacks_it() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        write_file(&roots, "oauth_creds", b"old-login");
        write_file(&roots, "settings", br#"{"keep":"mine"}"#);

        // 旧版快照只含凭据：凭据被替换，本机 settings 保留不清空。
        let legacy = temp.path().join("legacy").join("data");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("oauth_creds"), b"legacy-login").unwrap();
        restore_data(&roots, &legacy).unwrap();
        assert_eq!(
            fs::read(resolve_path(&roots, "oauth_creds")).unwrap(),
            b"legacy-login"
        );
        assert_eq!(
            fs::read(resolve_path(&roots, "settings")).unwrap(),
            br#"{"keep":"mine"}"#
        );

        // 新版快照包含 settings：随快照整体替换。
        let modern = temp.path().join("modern").join("data");
        fs::create_dir_all(&modern).unwrap();
        fs::write(modern.join("oauth_creds"), b"modern-login").unwrap();
        fs::write(modern.join("settings"), br#"{"keep":"theirs"}"#).unwrap();
        restore_data(&roots, &modern).unwrap();
        assert_eq!(
            fs::read(resolve_path(&roots, "oauth_creds")).unwrap(),
            b"modern-login"
        );
        assert_eq!(
            fs::read(resolve_path(&roots, "settings")).unwrap(),
            br#"{"keep":"theirs"}"#
        );
    }

    #[test]
    fn detects_email_auth_type_and_stable_fingerprint() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        write_file(
            &roots,
            "oauth_creds",
            br#"{"access_token":"a1","refresh_token":"r1","id_token":"not-a-jwt"}"#,
        );
        write_file(&roots, "google_accounts", br#"{"email":"dev@gmail.com"}"#);
        write_file(
            &roots,
            "settings",
            br#"{"security":{"auth":{"selectedType":"oauth-personal"}}}"#,
        );

        let identity = detect(&roots);
        assert_eq!(identity.email.as_deref(), Some("dev@gmail.com"));
        assert_eq!(identity.auth_type.as_deref(), Some("OAuth 个人账号"));
        assert!(!identity.is_antigravity());
        let fingerprint = identity.fingerprint.clone().unwrap();
        assert_eq!(fingerprint.len(), 12);
        assert_eq!(identity.default_name().as_deref(), Some("dev"));
        assert!(identity.describe().contains("OAuth 个人账号"));

        // access_token 刷新后指纹不变
        write_file(
            &roots,
            "oauth_creds",
            br#"{"access_token":"a1-new","refresh_token":"r1","id_token":"not-a-jwt"}"#,
        );
        assert_eq!(
            detect(&roots).fingerprint.as_deref(),
            Some(fingerprint.as_str())
        );

        // google_accounts 也兼容数组缓存格式
        fs::remove_file(gemini_dir(&roots).join("google_accounts.json")).unwrap();
        write_file(&roots, "google_accounts", r#"[{"email":"arr@gmail.com"}]"#.as_bytes());
        assert_eq!(
            detect(&roots).email.as_deref(),
            Some("arr@gmail.com"),
        );

        // refresh_token 变化即视为不同账号
        write_file(
            &roots,
            "oauth_creds",
            br#"{"access_token":"a1","refresh_token":"r-other"}"#,
        );
        assert_ne!(
            detect(&roots).fingerprint.as_deref(),
            Some(fingerprint.as_str())
        );
    }

    #[test]
    fn detects_antigravity_token_email_from_logs() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let agy = gemini_dir(&roots).join("antigravity-cli");
        fs::create_dir_all(agy.join("log")).unwrap();
        fs::write(
            agy.join("antigravity-oauth-token"),
            br#"{"token":{"access_token":"ya29.a","token_type":"Bearer","refresh_token":"agy-r1","expiry":"2026-09-09T10:00:00Z"},"auth_method":"consumer"}"#,
        )
        .unwrap();
        fs::write(
            agy.join("log").join("cli-20260909_100000.log"),
            "I0909 10:00:00.000000 1 server_oauth.go:192] applyAuthResult: email=dev@gmail.com, authMethod=consumer, quotaProject=\nI0909 10:00:00.000001 1 server_oauth.go:197] OAuth: authenticated successfully as dev@gmail.com\n",
        )
        .unwrap();

        let identity = detect(&roots);
        assert_eq!(identity.email.as_deref(), Some("dev@gmail.com"));
        assert_eq!(identity.auth_type.as_deref(), Some("Antigravity 个人账号"));
        assert!(identity.is_antigravity());
        assert_eq!(identity.default_name().as_deref(), Some("dev"));
        // 指纹 = 内层 refresh_token 摘要
        let fingerprint = identity.fingerprint.clone().unwrap();
        assert_eq!(fingerprint.len(), 12);

        // token 刷新（access_token/expiry 变化）不影响指纹
        fs::write(
            agy.join("antigravity-oauth-token"),
            br#"{"token":{"access_token":"ya29.b","token_type":"Bearer","refresh_token":"agy-r1","expiry":"2026-09-09T11:00:00Z"},"auth_method":"consumer"}"#,
        )
        .unwrap();
        assert_eq!(
            detect(&roots).fingerprint.as_deref(),
            Some(fingerprint.as_str())
        );

        // 日志缺失时仍能凭指纹识别，默认名回落到 Antigravity 前缀
        fs::remove_file(agy.join("log").join("cli-20260909_100000.log")).unwrap();
        let without_log = detect(&roots);
        assert_eq!(without_log.email, None);
        assert_eq!(
            without_log.default_name().as_deref(),
            Some(format!("Antigravity{}", &fingerprint[..8]).as_str())
        );
    }

    #[test]
    fn antigravity_fingerprint_preferred_over_legacy_gemini_cli() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        write_file(&roots, "oauth_creds", br#"{"access_token":"a","refresh_token":"legacy-r"}"#);
        let legacy_only = detect(&roots);

        let agy = gemini_dir(&roots).join("antigravity-cli");
        fs::create_dir_all(&agy).unwrap();
        fs::write(
            agy.join("antigravity-oauth-token"),
            br#"{"token":{"access_token":"ya29.a","refresh_token":"agy-r"},"auth_method":"consumer"}"#,
        )
        .unwrap();
        let both = detect(&roots);

        assert_ne!(legacy_only.fingerprint, both.fingerprint);
        assert_eq!(both.auth_type.as_deref(), Some("Antigravity 个人账号"));
    }

    #[test]
    fn switch_swaps_antigravity_token() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let token_path = resolve_path(&roots, "agy_oauth_token");
        fs::create_dir_all(token_path.parent().unwrap()).unwrap();
        fs::write(&token_path, br#"{"token":{"refresh_token":"agy-r1"},"auth_method":"consumer"}"#)
            .unwrap();
        let account_a = save_current_account(&roots, Some("agy A"), None).unwrap();

        fs::write(&token_path, br#"{"token":{"refresh_token":"agy-r2"},"auth_method":"consumer"}"#)
            .unwrap();
        let account_b = save_current_account(&roots, Some("agy B"), None).unwrap();

        switch_account(&roots, &account_a).unwrap();
        assert_eq!(
            fs::read(&token_path).unwrap(),
            br#"{"token":{"refresh_token":"agy-r1"},"auth_method":"consumer"}"#,
        );
        switch_account(&roots, &account_b).unwrap();
        assert_eq!(
            fs::read(&token_path).unwrap(),
            br#"{"token":{"refresh_token":"agy-r2"},"auth_method":"consumer"}"#,
        );
    }

    #[test]
    fn empty_state_describes_missing_login() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let identity = detect(&roots);
        assert_eq!(identity.describe(), "未检测到 Gemini 登录状态");
        assert_eq!(identity.default_name(), None);
        assert!(!identity.is_present());
    }

    #[test]
    fn alias_updates_display_name() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        write_file(
            &roots,
            "oauth_creds",
            br#"{"access_token":"a1","refresh_token":"r1"}"#,
        );
        let profile = save_current_account(&roots, None, None).unwrap();
        // 无邮箱时按指纹前缀自动命名：Gemini + 8 位指纹
        assert!(profile.manifest.name.starts_with("Gemini"));
        assert_eq!(profile.manifest.name.len(), 6 + 8);
        let renamed = crate::accounts::set_alias(&roots, &profile, Some("主力 Gmail")).unwrap();
        assert_eq!(renamed.manifest.display_name(), "主力 Gmail");
        let reloaded = list_accounts(&roots).unwrap().remove(0);
        assert_eq!(reloaded.manifest.display_name(), "主力 Gmail");
    }

    #[test]
    fn rejects_empty_gemini_state() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        assert!(save_current_account(&roots, Some("账号"), None).is_err());
        assert!(save_current_account(&roots, None, None).is_err());
    }

    #[test]
    fn new_account_ids_do_not_collide() {
        assert_ne!(new_account_id(), new_account_id());
    }
}
