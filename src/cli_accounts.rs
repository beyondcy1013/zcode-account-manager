//! 通用 CLI 工具账号存储：Gemini/Antigravity、Codex、Claude Code 共用的
//! 快照、切换、删除机制。每个工具用 `ToolStore` 描述凭据路径与账号识别方式，
//! 备份统一放在各自目录下的 `account_backups/`。

use crate::{copy_path, remove_path, Roots};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    cmp::Reverse,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::accounts::{AccountManifest, AccountProfile};

pub const MANIFEST_VERSION: u32 = 1;
static ACCOUNT_ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// CLI 工具登录状态涉及的文件，相对路径基于该工具的 `base_dir`。
pub struct ToolPath {
    pub tag: &'static str,
    pub relative: &'static str,
}

/// 一个 CLI 工具的账号存储描述。
pub struct ToolStore {
    /// 稳定标识（如 "gemini"）。
    pub key: &'static str,
    /// 页签短名。
    pub tab: &'static str,
    /// 完整显示名，用于状态与确认文本。
    pub display: &'static str,
    /// 运行中提示里的进程名列表（如 "gemini / agy"）。
    pub cli_names: &'static str,
    /// 无邮箱时默认备份名前缀。
    pub id_prefix: &'static str,
    /// 无任何账号标识时的兜底备份名。
    pub fallback_name: &'static str,
    /// 该工具的配置根目录（如 ~/.gemini）。
    pub base_dir: fn(&Roots) -> PathBuf,
    pub paths: &'static [ToolPath],
    /// 快照与恢复的项目及顺序。
    pub tags: &'static [&'static str],
    /// 配置类项目：快照缺失时保留本机现状而不是清空。
    pub preserve_if_absent: &'static [&'static str],
    /// 「清空账号」时要删除的登录凭据项目（tags 的子集），删完即恢复未登录状态。
    pub clear_tags: &'static [&'static str],
    /// 从本地文件识别当前账号。
    pub detect: fn(&Roots) -> CliIdentity,
    /// 检测运行中会话的 pgrep -f 模式（仅类 Unix 平台使用）。
    pub process_pattern: &'static str,
}

/// CLI 工具当前登录账号的可读标识，全部字段尽力提取，允许缺失。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CliIdentity {
    pub email: Option<String>,
    /// 认证方式 / 端点等展示标签（已本地化，如 "ChatGPT 账号"）。
    pub auth_type: Option<String>,
    /// 稳定凭据摘要（SHA-256 前 12 位），用于把当前状态与既有备份做对比。
    pub fingerprint: Option<String>,
}

impl CliIdentity {
    pub fn is_present(&self) -> bool {
        self.email.is_some() || self.auth_type.is_some() || self.fingerprint.is_some()
    }

    /// 登录状态是否来自 Antigravity CLI（agy，仅 Gemini 工具会为 true）。
    pub fn is_antigravity(&self) -> bool {
        self.auth_type
            .as_deref()
            .is_some_and(|auth| auth.starts_with("Antigravity"))
    }

    /// 用账号标识推导默认备份名：邮箱前缀 > 指纹前缀。
    pub fn default_name(&self, prefix: &str) -> Option<String> {
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
                prefix
            };
            return Some(format!("{prefix}{}", &fingerprint[..len]));
        }
        None
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
        if let Some(auth) = self
            .auth_type
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            parts.push(auth.to_string());
        }
        if let Some(fingerprint) = self.fingerprint.as_deref() {
            let len = fingerprint.len().min(12);
            parts.push(format!("指纹 {}", &fingerprint[..len]));
        }
        if parts.is_empty() {
            "未检测到登录状态".into()
        } else {
            parts.join(" · ")
        }
    }
}

impl ToolStore {
    pub fn base(&self, roots: &Roots) -> PathBuf {
        (self.base_dir)(roots)
    }

    fn resolve(&self, roots: &Roots, tag: &str) -> PathBuf {
        let entry = self
            .paths
            .iter()
            .find(|entry| entry.tag == tag)
            .unwrap_or_else(|| panic!("unknown tag {tag} for store {}", self.key));
        entry
            .relative
            .split('/')
            .fold(self.base(roots), |path, part| path.join(part))
    }

    /// 测试与保留语义校验需要按标签定位真实路径。
    #[cfg(test)]
    pub fn path_by_tag(&self, roots: &Roots, tag: &str) -> PathBuf {
        self.resolve(roots, tag)
    }

    pub fn accounts_root(&self, roots: &Roots) -> PathBuf {
        self.base(roots).join("account_backups")
    }

    pub fn identity(&self, roots: &Roots) -> CliIdentity {
        (self.detect)(roots)
    }

    pub fn count_present_items(&self, roots: &Roots) -> usize {
        self.tags
            .iter()
            .filter(|tag| self.resolve(roots, tag).exists())
            .count()
    }

    fn snapshot_to(&self, roots: &Roots, target: &Path, manifest: &AccountManifest) -> Result<(), String> {
        let parent = target.parent().ok_or(format!("{} 备份目录无效", self.display))?;
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let temp = parent.join(format!(".{}.tmp", manifest.id));
        if temp.exists() {
            remove_path(&temp).map_err(|error| error.to_string())?;
        }
        fs::create_dir_all(temp.join("data")).map_err(|error| error.to_string())?;

        let result = (|| {
            for tag in self.tags {
                let source = self.resolve(roots, tag);
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
            return Err(format!("保存 {} 备份失败: {error}", self.display));
        }
        if old.exists() {
            remove_path(&old).map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub fn save_current_account(
        &self,
        roots: &Roots,
        name: Option<&str>,
        existing: Option<&AccountProfile>,
    ) -> Result<AccountProfile, String> {
        let identity = (self.detect)(roots);
        let name = name
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .or_else(|| identity.default_name(self.id_prefix))
            .unwrap_or_else(|| self.fallback_name.to_string());
        if self.count_present_items(roots) == 0 {
            return Err(format!("未检测到可备份的 {} 账号数据", self.display));
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
            item_count: self.count_present_items(roots),
        };
        let directory = self.accounts_root(roots).join(&id);
        self.snapshot_to(roots, &directory, &manifest)?;
        set_active_account(self, roots, Some(&id))?;
        Ok(AccountProfile {
            directory,
            manifest,
        })
    }

    pub fn list_accounts(&self, roots: &Roots) -> Result<Vec<AccountProfile>, String> {
        let root = self.accounts_root(roots);
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

    fn restore_data(&self, roots: &Roots, data: &Path) -> Result<(), String> {        for tag in self.tags {
            // 配置类项目在旧快照缺失时保留本机现状；其余项目必须先清理，
            // 避免上一账号的残留和新账号混在一起。
            if self.preserve_if_absent.contains(&tag) && !data.join(tag).exists() {
                continue;
            }
            let destination = self.resolve(roots, tag);
            if destination.exists() {
                remove_path(&destination)
                    .map_err(|error| format!("清理当前 {tag} 失败: {error}"))?;
            }
        }
        for tag in self.tags {
            let source = data.join(tag);
            if source.exists() {
                copy_path(&source, &self.resolve(roots, tag))
                    .map_err(|error| format!("恢复 {tag} 失败: {error}"))?;
            }
        }
        Ok(())
    }

    pub fn switch_account(&self, roots: &Roots, target: &AccountProfile) -> Result<(), String> {
        let data = target.directory.join("data");
        if !data.is_dir() {
            return Err(format!("{} 备份不完整：缺少 data 目录", self.display));
        }
        if let Some(current_id) = self.active_account(roots) {
            if current_id != target.manifest.id {
                if let Some(current) = self
                    .list_accounts(roots)?
                    .into_iter()
                    .find(|profile| profile.manifest.id == current_id)
                {
                    self.save_current_account(roots, Some(&current.manifest.name), Some(&current))?;
                }
            }
        }
        let safety_root = self.accounts_root(roots).join(".switch-safety");
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
            item_count: self.count_present_items(roots),
        };
        self.snapshot_to(roots, &safety_root, &safety_manifest)?;

        if let Err(error) = self.restore_data(roots, &data) {
            let rollback = self.restore_data(roots, &safety_root.join("data"));
            return match rollback {
                Ok(()) => Err(format!("切换失败，已恢复原账号: {error}")),
                Err(rollback_error) => Err(format!(
                    "切换失败且自动回滚失败: {error}; 回滚错误: {rollback_error}; 安全备份位于 {}",
                    safety_root.display()
                )),
            };
        }
        set_active_account(self, roots, Some(&target.manifest.id))?;
        Ok(())
    }

    pub fn delete_account(&self, roots: &Roots, profile: &AccountProfile) -> Result<(), String> {
        remove_path(&profile.directory).map_err(|error| error.to_string())?;
        if self.active_account(roots).as_deref() == Some(profile.manifest.id.as_str()) {
            set_active_account(self, roots, None)?;
        }
        Ok(())
    }

    /// 清空登录状态，恢复到未登录的原始状态，方便重新登录。
    /// 清空前自动备份当前状态（与既有备份指纹一致时更新它而不是新建），
    /// 之后随时可以在列表中切回。返回自动备份的展示名；已是未登录状态
    /// （没有任何可清除内容）时返回 None。
    pub fn clear_account(&self, roots: &Roots) -> Result<Option<String>, String> {
        let has_files = self
            .tags
            .iter()
            .any(|tag| self.resolve(roots, tag).exists());
        let backup_name = if has_files {
            let identity = (self.detect)(roots);
            let existing = identity.fingerprint.as_deref().and_then(|fingerprint| {
                self.list_accounts(roots).ok()?.into_iter().find(|profile| {
                    profile.manifest.fingerprint.as_deref() == Some(fingerprint)
                })
            });
            // 更新已有备份时沿用其名称，避免自动改名
            let name = existing
                .as_ref()
                .map(|profile| profile.manifest.name.as_str());
            let profile = self.save_current_account(roots, name, existing.as_ref())?;
            Some(profile.manifest.display_name().to_string())
        } else {
            None
        };
        let mut removed = false;
        for tag in self.clear_tags {
            let path = self.resolve(roots, tag);
            if path.exists() {
                remove_path(&path).map_err(|error| format!("清除 {tag} 失败: {error}"))?;
                removed = true;
            }
        }
        set_active_account(self, roots, None)?;
        Ok(if removed || backup_name.is_some() {
            backup_name
        } else {
            None
        })
    }

    pub fn active_account(&self, roots: &Roots) -> Option<String> {
        let bytes = fs::read(self.accounts_root(roots).join("active.json")).ok()?;
        serde_json::from_slice::<ActiveAccount>(&bytes)
            .ok()
            .map(|value| value.id)
    }

    pub fn open_accounts_folder(&self, roots: &Roots) -> Result<(), String> {
        let root = self.accounts_root(roots);
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

    /// 尽力检测该 CLI 是否有会话正在运行（仅类 Unix 平台；Windows 侧不做检测）。
    /// 运行中的会话可能在切换后把旧凭据回写覆盖，界面据此提示用户先退出会话。
    pub fn cli_running(&self) -> bool {
        #[cfg(windows)]
        {
            let _ = self.process_pattern;
            false
        }
        #[cfg(not(windows))]
        {
            Command::new("pgrep")
                .args(["-f", self.process_pattern])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        }
    }

    /// 供其他模块的单元测试验证配置保留语义。
    #[cfg(test)]
    pub fn restore_data_public_for_test(&self, roots: &Roots, data: &Path) -> Result<(), String> {
        self.restore_data(roots, data)
    }
}

#[derive(Serialize, Deserialize)]
struct ActiveAccount {
    id: String,
}

fn set_active_account(store: &ToolStore, roots: &Roots, id: Option<&str>) -> Result<(), String> {
    let path = store.accounts_root(roots).join("active.json");
    if let Some(id) = id {
        fs::create_dir_all(store.accounts_root(roots)).map_err(|error| error.to_string())?;
        let bytes = serde_json::to_vec_pretty(&ActiveAccount { id: id.into() })
            .map_err(|error| error.to_string())?;
        fs::write(path, bytes).map_err(|error| error.to_string())?;
    } else if path.exists() {
        fs::remove_file(path).map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub(crate) fn now() -> u64 {
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

pub(crate) fn read_json(path: &Path) -> Option<Value> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 稳定凭据摘要：SHA-256 前 12 位十六进制。
pub(crate) fn short_digest(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    hex_encode(&digest)[..12].to_string()
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
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

    fn test_base(roots: &Roots) -> PathBuf {
        roots.user_profile.join(".testtool")
    }

    static TEST_PATHS: &[ToolPath] = &[
        ToolPath {
            tag: "creds",
            relative: "creds.json",
        },
        ToolPath {
            tag: "cfg",
            relative: "cfg.toml",
        },
    ];
    static TEST_TAGS: &[&str] = &["creds", "cfg"];
    static TEST_PRESERVE: &[&str] = &["cfg"];
    static TEST_CLEAR: &[&str] = &["creds"];
    static TEST_STORE: ToolStore = ToolStore {
        key: "testtool",
        tab: "Test",
        display: "Test CLI",
        cli_names: "testcli",
        id_prefix: "Test",
        fallback_name: "我的Test账号",
        base_dir: test_base,
        paths: TEST_PATHS,
        tags: TEST_TAGS,
        preserve_if_absent: TEST_PRESERVE,
        clear_tags: TEST_CLEAR,
        detect: |_| CliIdentity::default(),
        process_pattern: r"(^|/)testcli( |$)",
    };

    #[test]
    fn saves_lists_and_switches_accounts() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let creds = TEST_STORE.path_by_tag(&roots, "creds");
        fs::create_dir_all(creds.parent().unwrap()).unwrap();
        fs::write(&creds, b"account-a").unwrap();
        let account_a = TEST_STORE
            .save_current_account(&roots, Some("账户 A"), None)
            .unwrap();

        fs::write(&creds, b"account-b").unwrap();
        let account_b = TEST_STORE
            .save_current_account(&roots, Some("账户 B"), None)
            .unwrap();
        assert_eq!(TEST_STORE.list_accounts(&roots).unwrap().len(), 2);

        fs::write(&creds, b"account-b-newest").unwrap();
        TEST_STORE.switch_account(&roots, &account_a).unwrap();
        assert_eq!(fs::read(&creds).unwrap(), b"account-a");
        assert_eq!(
            TEST_STORE.active_account(&roots).as_deref(),
            Some(account_a.manifest.id.as_str())
        );
        TEST_STORE.switch_account(&roots, &account_b).unwrap();
        assert_eq!(fs::read(&creds).unwrap(), b"account-b-newest");
    }

    #[test]
    fn preserves_config_missing_from_snapshot() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let creds = TEST_STORE.path_by_tag(&roots, "creds");
        let cfg = TEST_STORE.path_by_tag(&roots, "cfg");
        fs::create_dir_all(creds.parent().unwrap()).unwrap();
        fs::write(&creds, b"login").unwrap();

        // 快照只有凭据：切换后本机配置保留
        let account = TEST_STORE
            .save_current_account(&roots, Some("账户"), None)
            .unwrap();
        fs::write(&cfg, br#"{"keep":"mine"}"#).unwrap();
        TEST_STORE.switch_account(&roots, &account).unwrap();
        assert_eq!(
            fs::read(&cfg).unwrap(),
            br#"{"keep":"mine"}"#,
            "快照缺失的配置项应保留本机现状"
        );

        // 快照包含配置：整体替换
        let data = temp.path().join("other").join("data");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("creds"), b"other-login").unwrap();
        fs::write(data.join("cfg"), br#"{"keep":"theirs"}"#).unwrap();
        TEST_STORE.restore_data(&roots, &data).unwrap();
        assert_eq!(fs::read(&cfg).unwrap(), br#"{"keep":"theirs"}"#);
    }

    #[test]
    fn rejects_empty_state_and_uses_fallback_name() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        assert!(TEST_STORE.save_current_account(&roots, None, None).is_err());

        // 有文件但检测不到标识时，用兜底名保存
        let creds = TEST_STORE.path_by_tag(&roots, "creds");
        fs::create_dir_all(creds.parent().unwrap()).unwrap();
        fs::write(&creds, b"opaque").unwrap();
        let profile = TEST_STORE.save_current_account(&roots, None, None).unwrap();
        assert_eq!(profile.manifest.name, "我的Test账号");
    }

    #[test]
    fn identity_default_name_prefers_email_then_prefix() {
        let mut identity = CliIdentity::default();
        assert_eq!(identity.default_name("Codex"), None);
        identity.fingerprint = Some("84ac0f0dabcdef".into());
        assert_eq!(identity.default_name("Codex").as_deref(), Some("Codex84ac0f0d"));
        identity.email = Some("someone@example.com".into());
        assert_eq!(identity.default_name("Codex").as_deref(), Some("someone"));
    }

    #[test]
    fn clear_backs_up_then_removes_credentials_and_keeps_config() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let creds = TEST_STORE.path_by_tag(&roots, "creds");
        let cfg = TEST_STORE.path_by_tag(&roots, "cfg");
        fs::create_dir_all(creds.parent().unwrap()).unwrap();
        fs::write(&creds, b"login-state").unwrap();
        fs::write(&cfg, b"user-config").unwrap();

        let backup_name = TEST_STORE.clear_account(&roots).unwrap();
        assert_eq!(backup_name.as_deref(), Some("我的Test账号"));
        assert!(!creds.exists(), "登录凭据应被清除");
        assert_eq!(fs::read(&cfg).unwrap(), b"user-config", "用户配置应保留");
        assert_eq!(TEST_STORE.active_account(&roots), None);

        // 自动备份可从列表找回，数据完整可切回
        let profiles = TEST_STORE.list_accounts(&roots).unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(
            fs::read(profiles[0].directory.join("data").join("creds")).unwrap(),
            b"login-state"
        );
    }

    #[test]
    fn clear_is_noop_when_already_logged_out() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        assert_eq!(TEST_STORE.clear_account(&roots).unwrap(), None);
        assert_eq!(TEST_STORE.list_accounts(&roots).unwrap().len(), 0);
    }
}
