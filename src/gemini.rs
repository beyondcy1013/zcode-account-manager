//! Google Gemini CLI 及其继任者 Antigravity CLI（`agy`）的账号识别与存储描述。
//!
//! Gemini CLI 已并入 Antigravity CLI，两者共用 `~/.gemini` 目录：
//! - 旧版 Gemini CLI：`~/.gemini/oauth_creds.json` 等文件；
//! - Antigravity CLI：`~/.gemini/antigravity-cli/antigravity-oauth-token` 等。
//! 两者都纳入快照与识别，账号识别完全基于本地文件（OAuth token、账号缓存、
//! antigravity 日志中的认证记录）。快照与切换机制由 `cli_accounts` 提供。

use crate::cli_accounts::{read_json, short_digest, CliIdentity, ToolPath, ToolStore};
use crate::Roots;
use serde_json::Value;
use std::{fs, path::PathBuf};

/// Gemini / Antigravity CLI 登录状态涉及的文件，均位于 `~/.gemini` 下。
static GEMINI_PATHS: &[ToolPath] = &[
    // Antigravity CLI（agy）
    ToolPath {
        tag: "agy_oauth_token",
        relative: "antigravity-cli/antigravity-oauth-token",
    },
    ToolPath {
        tag: "agy_settings",
        relative: "antigravity-cli/settings.json",
    },
    // 旧版 Gemini CLI
    ToolPath {
        tag: "oauth_creds",
        relative: "oauth_creds.json",
    },
    ToolPath {
        tag: "google_accounts",
        relative: "google_accounts.json",
    },
    ToolPath {
        tag: "google_web_accounts",
        relative: "google_web_accounts.json",
    },
    ToolPath {
        tag: "settings",
        relative: "settings.json",
    },
    ToolPath {
        tag: "adc_creds",
        relative: "application_default_credentials.json",
    },
];

/// 快照与恢复的项目及顺序。
static GEMINI_TAGS: &[&str] = &[
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
static GEMINI_PRESERVE_IF_ABSENT: &[&str] = &["settings", "agy_settings"];
/// 「清空账号」要删除的登录凭据与账号缓存（保留 agy_settings 个性化设置）。
static GEMINI_CLEAR_TAGS: &[&str] = &[
    "agy_oauth_token",
    "oauth_creds",
    "google_accounts",
    "google_web_accounts",
    "settings",
    "adc_creds",
];

pub fn gemini_dir(roots: &Roots) -> PathBuf {
    roots.user_profile.join(".gemini")
}

pub static STORE: ToolStore = ToolStore {
    key: "gemini",
    tab: "Gemini",
    display: "Gemini / Antigravity CLI (agy)",
    cli_names: "Gemini CLI / agy",
    id_prefix: "Gemini",
    fallback_name: "我的Gemini账号",
    base_dir: gemini_dir,
    paths: GEMINI_PATHS,
    tags: GEMINI_TAGS,
    preserve_if_absent: GEMINI_PRESERVE_IF_ABSENT,
    clear_tags: GEMINI_CLEAR_TAGS,
    launch_commands: &["agy", "gemini"],
    detect,
    process_pattern: r"(^|/)(gemini|agy)( |$)",
};

/// 读取 Gemini / Antigravity CLI 登录状态文件，汇总当前账号标识。
/// Antigravity CLI（agy）优先；旧版 Gemini CLI 文件仅用于补齐缺失字段。
pub fn detect(roots: &Roots) -> CliIdentity {
    let gemini = gemini_dir(roots);
    let mut identity = CliIdentity::default();

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

/// settings.json 中认证方式的展示标签。
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

/// agy 的 auth_method 展示标签。
fn agy_auth_label(method: &str) -> String {
    match method {
        "consumer" => "Antigravity 个人账号".to_string(),
        other => format!("Antigravity {other}"),
    }
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

/// 旧版 Gemini CLI 凭据指纹优先取 refresh_token：token 刷新会重写
/// access_token 等字段，refresh_token 稳定不变；文件不是 JSON 或缺少
/// refresh_token 时退回整个文件摘要。
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

use std::{
    path::Path,
    time::SystemTime,
};

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
        let path = STORE.path_by_tag(roots, tag);
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
        let account_a = STORE.save_current_account(&roots, Some("Gemini A"), None).unwrap();

        write_file(
            &roots,
            "oauth_creds",
            br#"{"access_token":"a2","refresh_token":"r2"}"#,
        );
        let account_b = STORE.save_current_account(&roots, Some("Gemini B"), None).unwrap();
        assert_eq!(STORE.list_accounts(&roots).unwrap().len(), 2);

        // access_token 变化但 refresh_token 不变：指纹稳定，仍与 B 匹配
        write_file(
            &roots,
            "oauth_creds",
            br#"{"access_token":"a2-refreshed","refresh_token":"r2"}"#,
        );
        STORE.switch_account(&roots, &account_a).unwrap();
        assert_eq!(
            fs::read(STORE.path_by_tag(&roots, "oauth_creds")).unwrap(),
            br#"{"access_token":"a1","refresh_token":"r1"}"#,
        );
        assert_eq!(
            STORE.active_account(&roots).as_deref(),
            Some(account_a.manifest.id.as_str())
        );

        // 切走前当前状态（a2-refreshed）已自动保存进 B，再切回应恢复
        STORE.switch_account(&roots, &account_b).unwrap();
        assert_eq!(
            fs::read(STORE.path_by_tag(&roots, "oauth_creds")).unwrap(),
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
        STORE.restore_data_public_for_test(&roots, &legacy).unwrap();
        assert_eq!(
            fs::read(STORE.path_by_tag(&roots, "oauth_creds")).unwrap(),
            b"legacy-login"
        );
        assert_eq!(
            fs::read(STORE.path_by_tag(&roots, "settings")).unwrap(),
            br#"{"keep":"mine"}"#
        );

        // 新版快照包含 settings：随快照整体替换。
        let modern = temp.path().join("modern").join("data");
        fs::create_dir_all(&modern).unwrap();
        fs::write(modern.join("oauth_creds"), b"modern-login").unwrap();
        fs::write(modern.join("settings"), br#"{"keep":"theirs"}"#).unwrap();
        STORE.restore_data_public_for_test(&roots, &modern).unwrap();
        assert_eq!(
            fs::read(STORE.path_by_tag(&roots, "oauth_creds")).unwrap(),
            b"modern-login"
        );
        assert_eq!(
            fs::read(STORE.path_by_tag(&roots, "settings")).unwrap(),
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
        assert_eq!(identity.default_name("Gemini").as_deref(), Some("dev"));
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
        assert_eq!(detect(&roots).email.as_deref(), Some("arr@gmail.com"),);

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
        assert_eq!(identity.default_name("Gemini").as_deref(), Some("dev"));
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
            without_log.default_name("Gemini").as_deref(),
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
        let token_path = STORE.path_by_tag(&roots, "agy_oauth_token");
        fs::create_dir_all(token_path.parent().unwrap()).unwrap();
        fs::write(&token_path, br#"{"token":{"refresh_token":"agy-r1"},"auth_method":"consumer"}"#)
            .unwrap();
        let account_a = STORE.save_current_account(&roots, Some("agy A"), None).unwrap();

        fs::write(&token_path, br#"{"token":{"refresh_token":"agy-r2"},"auth_method":"consumer"}"#)
            .unwrap();
        let account_b = STORE.save_current_account(&roots, Some("agy B"), None).unwrap();

        STORE.switch_account(&roots, &account_a).unwrap();
        assert_eq!(
            fs::read(&token_path).unwrap(),
            br#"{"token":{"refresh_token":"agy-r1"},"auth_method":"consumer"}"#,
        );
        STORE.switch_account(&roots, &account_b).unwrap();
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
        assert_eq!(identity.describe(), "未检测到登录状态");
        assert_eq!(identity.default_name("Gemini"), None);
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
        let profile = STORE.save_current_account(&roots, None, None).unwrap();
        // 无邮箱时按指纹前缀自动命名：Gemini + 8 位指纹
        assert!(profile.manifest.name.starts_with("Gemini"));
        assert_eq!(profile.manifest.name.len(), 6 + 8);
        let renamed = crate::accounts::set_alias(&roots, &profile, Some("主力 Gmail")).unwrap();
        assert_eq!(renamed.manifest.display_name(), "主力 Gmail");
        let reloaded = STORE.list_accounts(&roots).unwrap().remove(0);
        assert_eq!(reloaded.manifest.display_name(), "主力 Gmail");
    }

    #[test]
    fn clear_updates_matched_backup_instead_of_duplicating() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        write_file(
            &roots,
            "oauth_creds",
            br#"{"access_token":"a1","refresh_token":"r1"}"#,
        );
        let saved = STORE.save_current_account(&roots, Some("我的号"), None).unwrap();

        // 清空：指纹与既有备份一致 → 更新它而不是新建，凭据被删除
        let cleared = STORE.clear_account(&roots).unwrap();
        assert_eq!(cleared.as_deref(), Some("我的号"));
        assert!(!STORE.path_by_tag(&roots, "oauth_creds").exists());
        assert_eq!(STORE.active_account(&roots), None);
        assert_eq!(STORE.list_accounts(&roots).unwrap().len(), 1);

        // 再次登录同一账号（refresh_token 不变）后再清空：仍只有一份备份且内容已更新
        write_file(
            &roots,
            "oauth_creds",
            br#"{"access_token":"a1-new","refresh_token":"r1"}"#,
        );
        assert!(STORE.clear_account(&roots).unwrap().is_some());
        let profiles = STORE.list_accounts(&roots).unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].manifest.id, saved.manifest.id);
        assert_eq!(
            fs::read(profiles[0].directory.join("data").join("oauth_creds")).unwrap(),
            br#"{"access_token":"a1-new","refresh_token":"r1"}"#,
        );

        // 已是未登录状态时无需清空
        assert_eq!(STORE.clear_account(&roots).unwrap(), None);
    }

    #[test]
    fn rejects_empty_gemini_state() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        assert!(STORE.save_current_account(&roots, Some("账号"), None).is_err());
        assert!(STORE.save_current_account(&roots, None, None).is_err());
    }
}
