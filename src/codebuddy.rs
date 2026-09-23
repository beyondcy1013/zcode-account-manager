//! CodeBuddy CLI 账号导入支持。
//!
//! 兼容两类数据：
//! - WorkBuddy / wb-switch 导出的账号 JSON 数组；
//! - 单个 `~/.codebuddy/settings.json` 对象（可直接选择该文件导入）。
//!
//! 导入记录会转换为本项目通用的工具账号备份；导入只创建备份，不改变当前
//! 登录状态。切换仍由 `cli_accounts::ToolStore` 统一执行，并保留非认证配置。

use serde_json::{json, Value};
use std::{collections::BTreeMap, fs};

use crate::cli_accounts::{short_digest, CliIdentity, ImportResult, ToolPath, ToolStore};
use crate::Roots;

const AUTH_TOKEN: &str = "CODEBUDDY_AUTH_TOKEN";
const REGION_ENV: &str = "CODEBUDDY_INTERNET_ENVIRONMENT";

/// 导入统计（历史别名，等价于 `cli_accounts::ImportResult`）。
pub type CodebuddyImportResult = ImportResult;

/// 提取 auth token，兼容裸 token、`Bearer token` 和非字符串值报错。
fn token_from_env(value: &Value) -> Result<Option<String>, String> {
    let env_raw = value
        .get("env")
        .and_then(Value::as_object)
        .and_then(|env| env.get(AUTH_TOKEN))
        .cloned();
    let raw = env_raw.unwrap_or_else(|| {
        value
            .get("access_token")
            .cloned()
            .or_else(|| {
                value
                    .get("auth_raw")
                    .and_then(|auth| auth.get("accessToken"))
                    .cloned()
            })
            .unwrap_or(Value::Null)
    });
    if raw.is_null() {
        return Err(
            "缺少 env.CODEBUDDY_AUTH_TOKEN 或 access_token/auth_raw.accessToken".to_string(),
        );
    }
    let Some(raw) = raw.as_str() else {
        return Err("认证令牌必须是字符串".to_string());
    };
    let token = raw
        .trim()
        .strip_prefix("Bearer ")
        .unwrap_or(raw.trim())
        .trim();
    Ok((!token.is_empty()).then(|| token.to_string()))
}

/// 从账号记录提取可选邮箱或用户名。
fn account_name(item: &Value, token: &str) -> String {
    let direct = item
        .get("email")
        .and_then(Value::as_str)
        .or_else(|| item.get("nickname").and_then(Value::as_str))
        .or_else(|| item.get("name").and_then(Value::as_str))
        .or_else(|| item.get("uid").and_then(Value::as_str))
        .map(str::trim)
        .filter(|name| !name.is_empty());
    direct
        .map(str::to_string)
        .unwrap_or_else(|| format!("CodeBuddy{}", &short_digest(token.as_bytes())[..8]))
}

/// 档位显示：国际版 / 国内版。
fn region_label(item: &Value) -> &'static str {
    let raw = item
        .get("variant")
        .and_then(Value::as_str)
        .or_else(|| item.get(REGION_ENV).and_then(Value::as_str))
        .map(str::trim)
        .unwrap_or_default();
    if raw.eq_ignore_ascii_case("ai")
        || raw.eq_ignore_ascii_case("public")
        || raw.eq_ignore_ascii_case("external")
    {
        "CodeBuddy 国际版"
    } else {
        "CodeBuddy 国内版"
    }
}

/// 从 WorkBuddy / wb-switch 记录或当前设置对象中确定档位。
fn region_environment(item: &Value) -> Option<Value> {
    if let Some(value) = item
        .get("env")
        .and_then(Value::as_object)
        .and_then(|env| env.get(REGION_ENV))
    {
        return Some(value.clone());
    }
    item.get("variant").and_then(Value::as_str).map(|variant| {
        if variant.eq_ignore_ascii_case("cn") || variant.eq_ignore_ascii_case("internal") {
            json!("internal")
        } else {
            json!(variant)
        }
    })
}

fn parse_import_items(text: &str) -> Result<Vec<Value>, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("导入文件内容为空".to_string());
    }
    let value: Value =
        serde_json::from_str(trimmed).map_err(|error| format!("导入文件不是合法 JSON: {error}"))?;
    match value {
        Value::Array(items) => {
            if items.iter().any(|item| !item.is_object()) {
                return Err("导入数组中的每个账号必须是 JSON 对象".to_string());
            }
            Ok(items)
        }
        Value::Object(_) => Ok(vec![value]),
        _ => Err("导入内容必须是 JSON 账号对象或账号数组".to_string()),
    }
}

fn import_item(store: &ToolStore, roots: &Roots, item: &Value) -> Result<(), String> {
    let token = token_from_env(item)?;
    let Some(token) = token else {
        return Err("CODEBUDDY_AUTH_TOKEN 不能为空".to_string());
    };

    let mut settings = json!({
        "env": {
            AUTH_TOKEN: token,
        }
    });
    if let Some(region) = region_environment(item) {
        settings["env"][REGION_ENV] = region;
    }

    let bytes = serde_json::to_vec_pretty(&settings).map_err(|error| error.to_string())?;
    let mut files = BTreeMap::new();
    files.insert("settings".to_string(), bytes);
    let identity = CliIdentity {
        auth_type: Some(region_label(item).to_string()),
        fingerprint: Some(short_digest(token.as_bytes())),
        email: None,
    };
    store
        .import_account_files(
            roots,
            &account_name(item, &token),
            None,
            Some(&identity.describe()),
            identity.fingerprint.as_deref(),
            &files,
        )
        .map(|_| ())
        .map_err(|error| format!("保存 CodeBuddy 导入账号失败: {error}"))
}

/// 解析并导入 JSON 文本，便于测试与剪贴板导入扩展。
pub fn import_codebuddy_text(
    store: &ToolStore,
    roots: &Roots,
    text: &str,
) -> Result<CodebuddyImportResult, String> {
    let items = parse_import_items(text)?;
    let mut result = CodebuddyImportResult::default();
    let mut errors = Vec::new();
    for (index, item) in items.iter().enumerate() {
        match import_item(store, roots, item) {
            Ok(()) => result.imported += 1,
            Err(error) => {
                result.skipped += 1;
                errors.push(format!("第 {} 项: {error}", index + 1));
            }
        }
    }
    if result.imported == 0 {
        return Err(errors.join("; "));
    }
    Ok(result)
}

/// 原生格式导入入口（WorkBuddy 数组 / settings.json 对象），供统一的
/// `transfer::import_tool_text` 分发。
pub fn import_native_text(
    store: &ToolStore,
    roots: &Roots,
    text: &str,
) -> Result<ImportResult, String> {
    import_codebuddy_text(store, roots, text)
}

/// 根据当前 `~/.codebuddy/settings.json` 识别登录状态。
pub fn detect_codebuddy(roots: &Roots) -> CliIdentity {
    let path = roots.user_profile.join(".codebuddy").join("settings.json");
    let Some(bytes) = fs::read(path).ok() else {
        return CliIdentity::default();
    };
    let value = serde_json::from_slice::<Value>(&bytes).ok();
    let mut identity = CliIdentity::default();
    if let Some(value) = value.as_ref() {
        if let Ok(Some(token)) = token_from_env(value) {
            identity.auth_type = Some(region_label(value).to_string());
            identity.fingerprint = Some(short_digest(token.as_bytes()));
        }
    }
    if identity.fingerprint.is_none() {
        identity.fingerprint = Some(short_digest(&bytes));
    }
    identity
}

pub fn codebuddy_dir(roots: &Roots) -> std::path::PathBuf {
    roots.user_profile.join(".codebuddy")
}

static CODEBUDDY_PATHS: &[ToolPath] = &[ToolPath {
    tag: "settings",
    relative: "settings.json",
}];
static CODEBUDDY_TAGS: &[&str] = &["settings"];
static CODEBUDDY_CLEAR_TAGS: &[&str] = &["settings"];

/// CodeBuddy CLI 账号存储定义。
pub static STORE: ToolStore = ToolStore {
    key: "codebuddy",
    tab: "CodeBuddy",
    display: "CodeBuddy CLI",
    cli_names: "codebuddy",
    id_prefix: "CodeBuddy",
    fallback_name: "我的CodeBuddy账号",
    base_dir: codebuddy_dir,
    paths: CODEBUDDY_PATHS,
    tags: CODEBUDDY_TAGS,
    preserve_if_absent: &[],
    clear_tags: CODEBUDDY_CLEAR_TAGS,
    launch_commands: &["codebuddy"],
    detect: detect_codebuddy,
    process_pattern: r"(^|/)codebuddy( |$)",
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_accounts::ToolPath;
    use tempfile::TempDir;

    static PATHS: &[ToolPath] = &[ToolPath {
        tag: "settings",
        relative: "settings.json",
    }];

    fn store(base: fn(&Roots) -> std::path::PathBuf) -> ToolStore {
        ToolStore {
            key: "codebuddy-test",
            tab: "CodeBuddy",
            display: "CodeBuddy CLI",
            cli_names: "codebuddy",
            id_prefix: "CodeBuddy",
            fallback_name: "CodeBuddy账号",
            base_dir: base,
            paths: PATHS,
            tags: &["settings"],
            preserve_if_absent: &[],
            clear_tags: &["settings"],
            launch_commands: &[],
            detect: |_| CliIdentity::default(),
            process_pattern: "codebuddy",
        }
    }

    fn roots(temp: &TempDir) -> Roots {
        Roots {
            user_profile: temp.path().join("user"),
            app_data: temp.path().join("appdata"),
        }
    }

    fn base(roots: &Roots) -> std::path::PathBuf {
        roots.user_profile.join(".codebuddy")
    }

    #[test]
    fn imports_array_without_switching_active() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let store = store(base);
        let result = import_codebuddy_text(
            &store,
            &roots,
            r#"[{"email":"a@example.com","variant":"ai","env":{"CODEBUDDY_AUTH_TOKEN":"ta"}}]"#,
        )
        .unwrap();
        assert_eq!(
            result,
            CodebuddyImportResult {
                imported: 1,
                skipped: 0
            }
        );
        let accounts = store.list_accounts(&roots).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].manifest.name, "a@example.com");
        assert!(store.active_account(&roots).is_none());
        assert!(!base(&roots).join("settings.json").exists());
    }

    #[test]
    fn imports_settings_object() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let store = store(base);
        let result = import_codebuddy_text(
            &store,
            &roots,
            r#"{"env":{"CODEBUDDY_AUTH_TOKEN":"Bearer tb","CODEBUDDY_INTERNET_ENVIRONMENT":"internal"}}"#,
        )
        .unwrap();
        assert_eq!(result.imported, 1);
        assert_eq!(store.list_accounts(&roots).unwrap().len(), 1);
    }

    #[test]
    fn reports_invalid_records() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let store = store(base);
        let result = import_codebuddy_text(
            &store,
            &roots,
            r#"[
                {"email":"bad@example.com"},
                {"nickname":"bad-token","env":{"CODEBUDDY_AUTH_TOKEN":""}},
                {"email":"ok@example.com","env":{"CODEBUDDY_AUTH_TOKEN":"ok"}}
            ]"#,
        )
        .unwrap();
        assert_eq!(result.imported, 1);
        assert_eq!(result.skipped, 2);

        let error =
            import_codebuddy_text(&store, &roots, r#"[{"email":"bad@example.com"}]"#).unwrap_err();
        assert!(error.contains("第 1 项"));
    }

    #[test]
    fn imports_workbuddy_switch_record() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let store = store(base);
        let result = import_codebuddy_text(
            &store,
            &roots,
            r#"{
                "access_token": "Bearer wb-token",
                "auth_raw": {"accessToken": "unused"},
                "domain": "www.codebuddy.cn",
                "nickname": "WorkBuddy 用户",
                "uid": "uid-workbuddy",
                "profile_raw": {"nickname": "档案昵称", "phoneNumber": "13800000000"}
            }"#,
        )
        .unwrap();
        assert_eq!(
            result,
            CodebuddyImportResult {
                imported: 1,
                skipped: 0
            }
        );

        let accounts = store.list_accounts(&roots).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].manifest.name, "WorkBuddy 用户");
        // 快照内文件按标签命名（data/settings），与 save_current_account 的布局一致，
        // 恢复（切换）时才能按标签找到文件
        let stored = fs::read_to_string(accounts[0].directory.join("data/settings")).unwrap();
        let settings: Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(settings["env"][AUTH_TOKEN], "wb-token");
    }
}
