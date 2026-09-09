use crate::{
    candidate_by_tag, copy_path, remove_path, Roots, FULL_TAGS, PRESERVE_IF_ABSENT_TAGS,
};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Reverse,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const MANIFEST_VERSION: u32 = 1;
static ACCOUNT_ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountManifest {
    pub version: u32,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub alias: Option<String>,
    /// 选填的手机号码，便于识别账号归属；不影响登录数据。
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub identity: Option<String>,
    #[serde(default)]
    pub fingerprint: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    pub item_count: usize,
}

impl AccountManifest {
    /// 展示名优先使用用户设置的别名，否则回落到保存时自动提取的默认名。
    pub fn display_name(&self) -> &str {
        self.alias
            .as_deref()
            .map(str::trim)
            .filter(|alias| !alias.is_empty())
            .unwrap_or(&self.name)
    }
}

#[derive(Clone, Debug)]
pub struct AccountProfile {
    pub directory: PathBuf,
    pub manifest: AccountManifest,
}

#[derive(Serialize, Deserialize)]
struct ActiveAccount {
    id: String,
}

pub fn accounts_root(roots: &Roots) -> PathBuf {
    roots.user_profile.join(".zcode").join("account_backups")
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

fn snapshot_to(roots: &Roots, target: &Path, manifest: &AccountManifest) -> Result<(), String> {
    let parent = target.parent().ok_or("账户备份目录无效")?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temp = parent.join(format!(".{}.tmp", manifest.id));
    if temp.exists() {
        remove_path(&temp).map_err(|error| error.to_string())?;
    }
    fs::create_dir_all(temp.join("data")).map_err(|error| error.to_string())?;

    let result = (|| {
        for tag in FULL_TAGS {
            let candidate = candidate_by_tag(tag);
            let source = roots.resolve(candidate);
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
        return Err(format!("保存账户备份失败: {error}"));
    }
    if old.exists() {
        remove_path(&old).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn count_present_items(roots: &Roots) -> usize {
    FULL_TAGS
        .iter()
        .filter(|tag| roots.resolve(candidate_by_tag(tag)).exists())
        .count()
}

pub fn save_current_account(
    roots: &Roots,
    name: Option<&str>,
    existing: Option<&AccountProfile>,
) -> Result<AccountProfile, String> {
    let identity = crate::identity::detect(roots);
    save_current_account_with_identity(roots, name, existing, &identity)
}

pub fn save_current_account_with_identity(
    roots: &Roots,
    name: Option<&str>,
    existing: Option<&AccountProfile>,
    identity: &crate::identity::AccountIdentity,
) -> Result<AccountProfile, String> {
    let name = name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| identity.default_name())
        .unwrap_or_else(|| "我的账号".into());
    if count_present_items(roots) == 0 {
        return Err("未检测到可备份的 ZCode 账户数据".into());
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

/// 设置或清除账户别名；alias 传 None/空白即清除。
pub fn set_alias(
    _roots: &Roots,
    profile: &AccountProfile,
    alias: Option<&str>,
) -> Result<AccountProfile, String> {
    let alias = alias
        .map(str::trim)
        .filter(|alias| !alias.is_empty())
        .map(str::to_string);
    let mut manifest = profile.manifest.clone();
    manifest.alias = alias;
    let path = profile.directory.join("manifest.json");
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?;
    fs::write(path, bytes).map_err(|error| error.to_string())?;
    Ok(AccountProfile {
        directory: profile.directory.clone(),
        manifest,
    })
}

/// 设置或清除账户手机号码；空值即清除。
pub fn set_phone(
    _roots: &Roots,
    profile: &AccountProfile,
    phone: Option<&str>,
) -> Result<AccountProfile, String> {
    let phone = phone
        .map(str::trim)
        .filter(|phone| !phone.is_empty())
        .map(str::to_string);
    let mut manifest = profile.manifest.clone();
    manifest.phone = phone;
    let path = profile.directory.join("manifest.json");
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?;
    fs::write(path, bytes).map_err(|error| error.to_string())?;
    Ok(AccountProfile {
        directory: profile.directory.clone(),
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
    for tag in FULL_TAGS {
        // 旧版本快照里没有后期新增的配置项，此时保留本机现状而不是清空；
        // 其余项目必须先清理，避免上一账号的残留和新账号混在一起。
        if PRESERVE_IF_ABSENT_TAGS.contains(&tag) && !data.join(tag).exists() {
            continue;
        }
        let destination = roots.resolve(candidate_by_tag(tag));
        if destination.exists() {
            remove_path(&destination).map_err(|error| format!("清理当前 {tag} 失败: {error}"))?;
        }
    }
    for tag in FULL_TAGS {
        let source = data.join(tag);
        if source.exists() {
            copy_path(&source, &roots.resolve(candidate_by_tag(tag)))
                .map_err(|error| format!("恢复 {tag} 失败: {error}"))?;
        }
    }
    Ok(())
}

pub fn switch_account(roots: &Roots, target: &AccountProfile) -> Result<(), String> {
    let data = target.directory.join("data");
    if !data.is_dir() {
        return Err("账户备份不完整：缺少 data 目录".into());
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
            Ok(()) => Err(format!("切换失败，已恢复原账户: {error}")),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::AccountIdentity;
    use tempfile::TempDir;

    fn roots(temp: &TempDir) -> Roots {
        Roots {
            user_profile: temp.path().join("user"),
            app_data: temp.path().join("appdata"),
        }
    }

    fn stub_identity(fingerprint: &str) -> AccountIdentity {
        AccountIdentity {
            username: None,
            user_id: None,
            provider: None,
            fingerprint: Some(fingerprint.into()),
        }
    }

    #[test]
    fn saves_lists_and_restores_multiple_accounts() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let credentials = roots.resolve(candidate_by_tag("credentials"));
        fs::create_dir_all(credentials.parent().unwrap()).unwrap();
        fs::write(&credentials, b"account-a").unwrap();
        let account_a = save_current_account_with_identity(
            &roots,
            Some("账户 A"),
            None,
            &AccountIdentity::default(),
        )
        .unwrap();

        fs::write(&credentials, b"account-b").unwrap();
        let account_b = save_current_account_with_identity(
            &roots,
            Some("账户 B"),
            None,
            &AccountIdentity::default(),
        )
        .unwrap();
        assert_eq!(list_accounts(&roots).unwrap().len(), 2);

        fs::write(&credentials, b"account-b-newest").unwrap();
        switch_account(&roots, &account_a).unwrap();
        assert_eq!(fs::read(&credentials).unwrap(), b"account-a");
        assert_eq!(
            active_account(&roots).as_deref(),
            Some(account_a.manifest.id.as_str())
        );

        switch_account(&roots, &account_b).unwrap();
        assert_eq!(fs::read(&credentials).unwrap(), b"account-b-newest");
    }

    #[test]
    fn restore_replaces_provider_config_but_preserves_it_for_legacy_backups() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let credentials = roots.resolve(candidate_by_tag("credentials"));
        let provider_config = roots.resolve(candidate_by_tag("provider_config"));
        fs::create_dir_all(credentials.parent().unwrap()).unwrap();
        fs::write(&credentials, b"old-login").unwrap();
        fs::write(&provider_config, br#"{"keep":"mine"}"#).unwrap();

        // 旧版快照只含凭据：凭据被替换，本机 provider 配置保留不清空。
        let legacy = temp.path().join("legacy").join("data");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("credentials"), b"legacy-login").unwrap();
        restore_data(&roots, &legacy).unwrap();
        assert_eq!(fs::read(&credentials).unwrap(), b"legacy-login");
        assert_eq!(fs::read(&provider_config).unwrap(), br#"{"keep":"mine"}"#);

        // 新版快照包含 provider 配置：随快照整体替换。
        let modern = temp.path().join("modern").join("data");
        fs::create_dir_all(&modern).unwrap();
        fs::write(modern.join("credentials"), b"modern-login").unwrap();
        fs::write(modern.join("provider_config"), br#"{"keep":"theirs"}"#).unwrap();
        restore_data(&roots, &modern).unwrap();
        assert_eq!(fs::read(&credentials).unwrap(), b"modern-login");
        assert_eq!(fs::read(&provider_config).unwrap(), br#"{"keep":"theirs"}"#);
    }

    #[test]
    fn derives_default_name_from_identity_fingerprint() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let credentials = roots.resolve(candidate_by_tag("credentials"));
        fs::create_dir_all(credentials.parent().unwrap()).unwrap();
        fs::write(&credentials, b"account-a").unwrap();

        let profile = save_current_account_with_identity(
            &roots,
            None,
            None,
            &stub_identity("84ac0f0dabcdef"),
        )
        .unwrap();
        assert_eq!(profile.manifest.name, "账号84ac0f0d");
        assert_eq!(profile.manifest.alias, None);
        assert_eq!(
            profile.manifest.fingerprint.as_deref(),
            Some("84ac0f0dabcdef")
        );
        assert!(profile.manifest.identity.is_some());

        // 显式名称优先于默认名
        let named = save_current_account_with_identity(
            &roots,
            Some("  工作号  "),
            None,
            &stub_identity("84ac0f0dabcdef"),
        )
        .unwrap();
        assert_eq!(named.manifest.name, "工作号");
    }

    #[test]
    fn alias_overrides_display_and_can_be_cleared() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let credentials = roots.resolve(candidate_by_tag("credentials"));
        fs::create_dir_all(credentials.parent().unwrap()).unwrap();
        fs::write(&credentials, b"account-a").unwrap();
        let profile = save_current_account_with_identity(
            &roots,
            None,
            None,
            &stub_identity("84ac0f0dabcdef"),
        )
        .unwrap();

        let renamed = set_alias(&roots, &profile, Some("  主力号 ")).unwrap();
        assert_eq!(renamed.manifest.alias.as_deref(), Some("主力号"));
        assert_eq!(renamed.manifest.name, "账号84ac0f0d");
        assert_eq!(renamed.manifest.display_name(), "主力号");

        let reloaded = list_accounts(&roots).unwrap().remove(0);
        assert_eq!(reloaded.manifest.display_name(), "主力号");

        let cleared = set_alias(&roots, &reloaded, Some("   ")).unwrap();
        assert_eq!(cleared.manifest.alias, None);
        assert_eq!(cleared.manifest.display_name(), "账号84ac0f0d");
    }

    #[test]
    fn phone_is_saved_cleared_and_preserved_on_update() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let credentials = roots.resolve(candidate_by_tag("credentials"));
        fs::create_dir_all(credentials.parent().unwrap()).unwrap();
        fs::write(&credentials, b"account-a").unwrap();
        let profile = save_current_account_with_identity(
            &roots,
            None,
            None,
            &stub_identity("84ac0f0dabcdef"),
        )
        .unwrap();
        assert_eq!(profile.manifest.phone, None);

        let with_phone = set_phone(&roots, &profile, Some(" 13800138000 ")).unwrap();
        assert_eq!(with_phone.manifest.phone.as_deref(), Some("13800138000"));

        // 更新备份（覆盖保存）时保留手机号码
        fs::write(&credentials, b"account-a-new").unwrap();
        let updated = save_current_account(&roots, Some(&with_phone.manifest.name), Some(&with_phone))
            .unwrap();
        assert_eq!(updated.manifest.phone.as_deref(), Some("13800138000"));

        // 从磁盘重新加载仍然存在，空值即清除
        let reloaded = list_accounts(&roots)
            .unwrap()
            .into_iter()
            .find(|p| p.manifest.id == with_phone.manifest.id)
            .unwrap();
        assert_eq!(reloaded.manifest.phone.as_deref(), Some("13800138000"));
        let cleared = set_phone(&roots, &reloaded, Some("   ")).unwrap();
        assert_eq!(cleared.manifest.phone, None);
    }

    #[test]
    fn old_manifests_without_new_fields_still_load() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let dir = accounts_root(&roots).join("account-legacy");
        fs::create_dir_all(dir.join("data")).unwrap();
        fs::write(
            dir.join("manifest.json"),
            r#"{"version":1,"id":"account-legacy","name":"旧备份","created_at":1,"updated_at":2,"item_count":1}"#,
        )
        .unwrap();
        let profiles = list_accounts(&roots).unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].manifest.alias, None);
        assert_eq!(profiles[0].manifest.phone, None);
        assert_eq!(profiles[0].manifest.display_name(), "旧备份");
    }

    #[test]
    fn rejects_empty_state() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        assert!(save_current_account_with_identity(
            &roots,
            Some("账户"),
            None,
            &AccountIdentity::default()
        )
        .is_err());
        assert!(save_current_account_with_identity(
            &roots,
            None,
            None,
            &AccountIdentity::default()
        )
        .is_err());
    }

    #[test]
    fn new_account_ids_do_not_collide() {
        assert_ne!(new_account_id(), new_account_id());
    }
}
