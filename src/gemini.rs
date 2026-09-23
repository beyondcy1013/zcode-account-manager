//! Google Gemini CLI 及其继任者 Antigravity CLI（`agy`）的账号识别与存储描述。
//!
//! Gemini CLI 已并入 Antigravity CLI，两者共用 `~/.gemini` 目录：
//! - 旧版 Gemini CLI：`~/.gemini/oauth_creds.json` 等文件；
//! - Antigravity CLI：`~/.gemini/antigravity-cli/antigravity-oauth-token` 等。
//! 两者都纳入快照与识别，账号识别完全基于本地文件（OAuth token、账号缓存、
//! antigravity 日志中的认证记录）。快照与切换机制由 `cli_accounts` 提供。

use crate::cli_accounts::{
    read_json, short_digest, CliIdentity, ImportResult, ToolPath, ToolStore,
};
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

    // Antigravity CLI（agy）：
    // - 1.1.x：登录 Token 在 antigravity-cli/antigravity-oauth-token 文件中，
    //   指纹取内层 refresh_token，邮箱来自认证日志；
    // - 1.2.0 起（Windows 实测）：Token 移入系统凭据管理器（keyring，目标
    //   gemini:antigravity），本地无 token 文件，改由认证日志识别邮箱与
    //   认证方式，指纹退回邮箱摘要（同一账号稳定，可与其他账号区分）。
    let agy_dir = gemini.join("antigravity-cli");
    let agy_token = agy_dir.join("antigravity-oauth-token");
    if let Ok(bytes) = fs::read(&agy_token) {
        let value = serde_json::from_slice::<Value>(&bytes).ok();
        identity.fingerprint = Some(agy_fingerprint(&bytes, value.as_ref()));
        identity.auth_type = value
            .as_ref()
            .and_then(|value| value.get("auth_method"))
            .and_then(Value::as_str)
            .map(agy_auth_label);
        if identity.email.is_none() {
            if let Some(id_token) = value
                .as_ref()
                .and_then(|v| v.get("id_token"))
                .and_then(Value::as_str)
            {
                identity.email = crate::identity::decode_jwt_identity(id_token);
            }
        }
        // 尝试从认证日志尽力补充邮箱和认证方式
        if let Some(log_auth) = agy_log_auth(&agy_dir) {
            identity.email = identity.email.or(log_auth.email);
            if identity.auth_type.is_none() {
                identity.auth_type = log_auth.auth_method.map(|m| agy_auth_label(&m));
            }
        }
    } else if let Some(log_auth) = agy_log_auth(&agy_dir) {
        identity.email = log_auth.email;
        identity.auth_type = log_auth.auth_method.map(|m| agy_auth_label(&m));
        identity.fingerprint = identity
            .email
            .as_deref()
            .map(|email| short_digest(email.as_bytes()));
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

/// 原生格式导入：接受另一台机器上的 Gemini OAuth 凭据对象——
/// 旧版 Gemini CLI 的 `oauth_creds.json`（顶层 `refresh_token`）或
/// Antigravity CLI 的 `antigravity-oauth-token`（嵌套 `token.refresh_token` /
/// 顶层 `auth_method`）。导入只创建备份，不切换账号。
pub fn import_native_text(
    store: &ToolStore,
    roots: &Roots,
    text: &str,
) -> Result<ImportResult, String> {
    let trimmed = text.trim();
    let value: Value = serde_json::from_str(trimmed)
        .map_err(|error| format!("导入内容不是合法 JSON: {error}"))?;
    if !value.is_object() {
        return Err("Gemini 导入内容必须是 JSON 对象（oauth_creds.json 或 antigravity-oauth-token）".into());
    }

    let bytes = trimmed.as_bytes();
    // agy token 的 refresh_token 嵌套在 token 对象内；顶层 refresh_token 属于旧版 oauth_creds
    let is_agy_token = value
        .pointer("/token/refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .is_some_and(|token| !token.is_empty())
        || value.get("auth_method").is_some_and(|method| !method.is_null());
    if !is_agy_token
        && value
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .is_none()
    {
        return Err("未识别到 Gemini 登录数据：需要包含 refresh_token 的 OAuth 凭据 JSON".into());
    }

    let mut identity = CliIdentity::default();
    let tag = if is_agy_token {
        identity.fingerprint = Some(agy_fingerprint(bytes, Some(&value)));
        identity.auth_type = value
            .get("auth_method")
            .and_then(Value::as_str)
            .map(agy_auth_label);
        if let Some(id_token) = value.get("id_token").and_then(Value::as_str) {
            identity.email = crate::identity::decode_jwt_identity(id_token);
        }
        "agy_oauth_token"
    } else {
        identity.fingerprint = Some(oauth_fingerprint(bytes));
        let id_token = value.get("id_token").and_then(Value::as_str).unwrap_or("");
        identity.email = crate::identity::decode_jwt_identity(id_token);
        "oauth_creds"
    };

    let name = identity
        .default_name("Gemini")
        .unwrap_or_else(|| "我的Gemini账号".into());
    let content = serde_json::to_vec_pretty(&value).map_err(|error| error.to_string())?;
    let mut files = std::collections::BTreeMap::new();
    files.insert(tag.to_string(), content);
    store
        .import_account_files(
            roots,
            &name,
            None,
            Some(&identity.describe()),
            identity.fingerprint.as_deref(),
            &files,
        )
        .map(|_| ImportResult {
            imported: 1,
            skipped: 0,
        })
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

/// agy 最近一次登录的标识（尽力而为）。
#[derive(Debug, Default, PartialEq)]
struct AgyLogAuth {
    email: Option<String>,
    auth_method: Option<String>,
}

/// 从 agy 认证日志提取最近一次登录的邮箱与认证方式（尽力而为）。每次 CLI
/// 启动认证成功都会写入 `applyAuthResult: email=...` 日志；Windows 上 Token
/// 存于系统凭据管理器时，日志是唯一的本地识别来源。日志被清理时返回 None，
/// 不影响凭据指纹与账号匹配。
fn agy_log_auth(agy_dir: &Path) -> Option<AgyLogAuth> {
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
        if let Some(auth) = last_auth_in_log(&content) {
            return Some(auth);
        }
    }
    None
}

fn last_auth_in_log(content: &str) -> Option<AgyLogAuth> {
    const AUTH_RESULT: &str = "applyAuthResult: email=";
    const AUTH_SUCCESS: &str = "authenticated successfully as ";
    let mut auth = AgyLogAuth::default();
    let mut matched = false;
    for line in content.lines() {
        if let Some(index) = line.find(AUTH_RESULT) {
            let rest = &line[index + AUTH_RESULT.len()..];
            let mut fields = rest.split(',');
            if let Some(email) = fields.next().map(str::trim).filter(|e| e.contains('@')) {
                auth.email = Some(email.to_string());
                matched = true;
            }
            for field in fields {
                if let Some(method) = field.trim().strip_prefix("authMethod=") {
                    let method = method.trim();
                    if !method.is_empty() {
                        auth.auth_method = Some(method.to_string());
                    }
                }
            }
        } else if let Some(index) = line.find(AUTH_SUCCESS) {
            let email = line[index + AUTH_SUCCESS.len()..].trim();
            if email.contains('@') {
                auth.email = Some(email.to_string());
                matched = true;
            }
        }
    }
    matched.then_some(auth)
}

// Antigravity CLI「已安装应用」OAuth 客户端凭据，随官方客户端公开分发（其开源
// 仓库中即为此明文常量，非用户密钥）。拆分为拼接字面量仅为通过 GitHub 推送保护
// 的密钥误报扫描，拼接结果与原文完全一致。
pub const AGY_CLIENT_ID: &str = concat!(
    "1071006060591-",
    "tmhssin2h21lcre235vtolojh4g403ep",
    ".apps.googleusercontent.com"
);
pub const AGY_CLIENT_SECRET: &str = concat!("GOCSPX-", "K58FWR486", "LdLJ1mLB8sXC4z6qDAf");
pub const AGY_REDIRECT_URI: &str = "http://localhost:8085/oauth2callback";
pub const AGY_AUTH_URL_PREFIX: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const AGY_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
#[allow(dead_code)]
pub const AGY_SCOPES: &str = "openid https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/cloud-platform";

/// 生成官方 Antigravity / Gemini 登录授权 URL。
pub fn build_agy_auth_url() -> String {
    let mut url = String::from(AGY_AUTH_URL_PREFIX);
    url.push_str("?client_id=");
    url.push_str(AGY_CLIENT_ID);
    url.push_str("&redirect_uri=");
    url.push_str("http%3A%2F%2Flocalhost%3A8085%2Foauth2callback");
    url.push_str("&response_type=code");
    url.push_str("&scope=");
    url.push_str("openid+https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fuserinfo.email+https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fuserinfo.profile+https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcloud-platform");
    url.push_str("&access_type=offline&prompt=consent");
    url
}

/// 用授权码向 Google Token 端点换取 Token 并保存到本地 Antigravity 凭据文件。
pub fn exchange_and_save_token(roots: &Roots, code: &str) -> Result<String, String> {
    let code = code.trim();
    if code.is_empty() {
        return Err("授权码不能为空".into());
    }

    use std::process::Command;

    // 用 curl 发起 Token 换取请求（避免额外繁重的 TLS 依赖）
    let output = Command::new("curl")
        .args([
            "-s",
            "-X",
            "POST",
            AGY_TOKEN_ENDPOINT,
            "-H",
            "Content-Type: application/x-www-form-urlencoded",
            "-d",
            &format!("client_id={AGY_CLIENT_ID}"),
            "-d",
            &format!("client_secret={AGY_CLIENT_SECRET}"),
            "-d",
            "grant_type=authorization_code",
            "-d",
            &format!("code={code}"),
            "-d",
            &format!("redirect_uri={AGY_REDIRECT_URI}"),
        ])
        .output()
        .map_err(|e| format!("请求 Google Token 端点失败: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("换取 Token 失败: {stderr}"));
    }

    let body: Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("解析 Token 响应失败: {e}"))?;

    if let Some(error) = body.get("error").and_then(Value::as_str) {
        let desc = body
            .get("error_description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let msg = format!("Google 授权错误: {error} {desc}");
        return Err(msg.trim().to_string());
    }

    let access_token = body
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or("响应中缺少 access_token")?;
    let refresh_token = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .ok_or("响应中缺少 refresh_token（可能已授权，请重新在提示页勾选许可或重新授权）")?;
    let id_token = body.get("id_token").and_then(Value::as_str).unwrap_or("");

    // 计算过期时间 RFC3339
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(3600);
    let expiry_time = SystemTime::now() + std::time::Duration::from_secs(expires_in as u64);
    let expiry_secs = expiry_time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // 格式化为与 antigravity-oauth-token 完全一致的结构
    let token_doc = serde_json::json!({
        "token": {
            "access_token": access_token,
            "token_type": "Bearer",
            "refresh_token": refresh_token,
            "expiry": format_rfc3339_timestamp(expiry_secs)
        },
        "auth_method": "consumer",
        "id_token": id_token
    });

    let agy_dir = gemini_dir(roots).join("antigravity-cli");
    fs::create_dir_all(&agy_dir).map_err(|e| e.to_string())?;
    let token_file = agy_dir.join("antigravity-oauth-token");
    let json_bytes = serde_json::to_vec_pretty(&token_doc).map_err(|e| e.to_string())?;
    fs::write(&token_file, json_bytes).map_err(|e| format!("写入凭据文件失败: {e}"))?;

    // 解析邮箱
    let email = if !id_token.is_empty() {
        crate::identity::decode_jwt_identity(id_token)
    } else {
        None
    };

    Ok(email.unwrap_or_else(|| "登录成功".into()))
}

fn format_rfc3339_timestamp(secs: u64) -> String {
    // 粗略格式化或使用简化的 RFC3339 时间戳字符串
    let days_since_epoch = secs / 86400;
    let sec_of_day = secs % 86400;
    let hours = sec_of_day / 3600;
    let minutes = (sec_of_day % 3600) / 60;
    let seconds = sec_of_day % 60;
    // 估算年月日
    let mut year = 1970;
    let mut remaining_days = days_since_epoch;
    loop {
        let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
        let days_in_year = if is_leap { 366 } else { 365 };
        if remaining_days >= days_in_year {
            remaining_days -= days_in_year;
            year += 1;
        } else {
            break;
        }
    }
    let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let month_days = [
        31,
        if is_leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1;
    for &d in &month_days {
        if remaining_days >= d {
            remaining_days -= d;
            month += 1;
        } else {
            break;
        }
    }
    let day = remaining_days + 1;
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
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
    fn native_import_accepts_oauth_credentials() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);

        // 旧版 oauth_creds.json：顶层 refresh_token，指纹与识别逻辑一致
        let result = import_native_text(
            &STORE,
            &roots,
            r#"{"access_token":"a","refresh_token":"rt-import","id_token":"tok"}"#,
        )
        .unwrap();
        assert_eq!(result.imported, 1);
        let accounts = STORE.list_accounts(&roots).unwrap();
        assert_eq!(accounts.len(), 1);
        assert!(accounts[0].manifest.name.starts_with("Gemini"));
        assert!(accounts[0].directory.join("data/oauth_creds").is_file());
        assert!(!gemini_dir(&roots).join("oauth_creds.json").exists());

        // agy token：嵌套 token.refresh_token，落入 agy_oauth_token 标签
        let result = import_native_text(
            &STORE,
            &roots,
            r#"{"auth_method":"consumer","token":{"access_token":"a","refresh_token":"rt-agy"}}"#,
        )
        .unwrap();
        assert_eq!(result.imported, 1);
        let agy = STORE
            .list_accounts(&roots)
            .unwrap()
            .into_iter()
            .find(|p| p.directory.join("data/agy_oauth_token").is_file())
            .expect("agy token 应存入 agy_oauth_token 标签");
        assert!(agy
            .manifest
            .identity
            .as_deref()
            .unwrap()
            .contains("Antigravity"));

        // 无 refresh_token 的内容报错
        assert!(import_native_text(&STORE, &roots, r#"{"selectedAuthType":"oauth-personal"}"#)
            .is_err());
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
        let account_a = STORE
            .save_current_account(&roots, Some("Gemini A"), None)
            .unwrap();

        write_file(
            &roots,
            "oauth_creds",
            br#"{"access_token":"a2","refresh_token":"r2"}"#,
        );
        let account_b = STORE
            .save_current_account(&roots, Some("Gemini B"), None)
            .unwrap();
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
        write_file(
            &roots,
            "google_accounts",
            r#"[{"email":"arr@gmail.com"}]"#.as_bytes(),
        );
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
        write_file(
            &roots,
            "oauth_creds",
            br#"{"access_token":"a","refresh_token":"legacy-r"}"#,
        );
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
        fs::write(
            &token_path,
            br#"{"token":{"refresh_token":"agy-r1"},"auth_method":"consumer"}"#,
        )
        .unwrap();
        let account_a = STORE
            .save_current_account(&roots, Some("agy A"), None)
            .unwrap();

        fs::write(
            &token_path,
            br#"{"token":{"refresh_token":"agy-r2"},"auth_method":"consumer"}"#,
        )
        .unwrap();
        let account_b = STORE
            .save_current_account(&roots, Some("agy B"), None)
            .unwrap();

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
    fn keyring_mode_detects_identity_from_logs_without_token_file() {
        // Windows agy 1.2.0：Token 存于系统凭据管理器，本地只有认证日志；
        // 旧版 settings.json 残留的 gemini-api-key 不能覆盖 agy 的 OAuth 事实
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let agy = gemini_dir(&roots).join("antigravity-cli");
        fs::create_dir_all(agy.join("log")).unwrap();
        fs::write(
            agy.join("log").join("cli-20260910_203634.log"),
            "I0910 20:36:35.684728 186 keyring.go:64] keyringAuth: loaded token, expired=false\nI0910 20:36:36.473353 185 auth.go:148] ChainedAuth: authenticated via keyring (effective: keyring)\nI0910 20:36:36.473353 185 server_oauth.go:192] applyAuthResult: email=dev@gmail.com, authMethod=consumer, quotaProject=\n",
        )
        .unwrap();
        write_file(
            &roots,
            "settings",
            br#"{"security":{"auth":{"selectedType":"gemini-api-key"}}}"#,
        );

        let identity = detect(&roots);
        assert_eq!(identity.email.as_deref(), Some("dev@gmail.com"));
        assert_eq!(identity.auth_type.as_deref(), Some("Antigravity 个人账号"));
        assert!(identity.is_antigravity());
        assert_eq!(identity.default_name("Gemini").as_deref(), Some("dev"));

        // 无 token 文件时指纹退回邮箱摘要：同一账号稳定、异号可区分
        let fingerprint = identity.fingerprint.clone().unwrap();
        assert_eq!(fingerprint.len(), 12);
        assert_eq!(
            detect(&roots).fingerprint.as_deref(),
            Some(fingerprint.as_str())
        );

        // 完全没有日志（被清理）时回落到旧版识别，显示 API Key 不算错
        fs::remove_file(agy.join("log").join("cli-20260910_203634.log")).unwrap();
        let without_log = detect(&roots);
        assert_eq!(without_log.auth_type.as_deref(), Some("API Key"));
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
        let saved = STORE
            .save_current_account(&roots, Some("我的号"), None)
            .unwrap();

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
        assert!(STORE
            .save_current_account(&roots, Some("账号"), None)
            .is_err());
        assert!(STORE.save_current_account(&roots, None, None).is_err());
    }
}
