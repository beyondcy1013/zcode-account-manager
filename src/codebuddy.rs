//! CodeBuddy CLI 账号导入与切换支持。
//!
//! 兼容两类数据：
//! - WorkBuddy / wb-switch 导出的账号 JSON 数组；
//! - 单个 `~/.codebuddy/settings.json` 对象（可直接选择该文件导入）。
//!
//! 导入记录会转换为本项目通用的工具账号备份；导入只创建备份，不改变当前
//! 登录状态。CodeBuddy 的 `~/.codebuddy/settings.json` 把认证字段
//! （`env.CODEBUDDY_AUTH_TOKEN`、`env.CODEBUDDY_INTERNET_ENVIRONMENT`）与
//! 普通配置混在同一文件，因此切换/清空通过 `merge_restore`/`clear_file`
//! 钩子只替换认证字段，其余配置原样保留。

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
/// 指纹只来自真实 token；无 token 时不再用整文件摘要冒充凭据。
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
        } else if value
            .get("apiKeyHelper")
            .and_then(Value::as_str)
            .is_some_and(|helper| !helper.trim().is_empty())
        {
            identity.auth_type = Some("apiKeyHelper 外部凭据（本工具不接管）".to_string());
        }
    }
    identity
}

/// `merge_restore` 钩子：把快照中的认证字段合并进当前 settings.json，
/// 其余配置（apiKeyHelper、model、permissions、trustedDirectories 等）保留本机现状。
fn merge_settings(
    _tag: &str,
    current: Option<&[u8]>,
    snapshot: Option<&[u8]>,
) -> Result<Option<Vec<u8>>, String> {
    let snapshot_value = snapshot
        .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
        .filter(|value| value.is_object());
    let token_ok = snapshot_value
        .as_ref()
        .map(|value| {
            let probe = json!({ "env": value.get("env").cloned().unwrap_or(Value::Null) });
            matches!(token_from_env(&probe), Ok(Some(_)))
        })
        .unwrap_or(false);
    if !token_ok {
        return Err("目标备份不含 CodeBuddy 登录凭据（CODEBUDDY_AUTH_TOKEN）".to_string());
    }
    let snapshot_value = snapshot_value.unwrap();
    let snapshot_env = snapshot_value
        .get("env")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let token_value = snapshot_env.get(AUTH_TOKEN).cloned().unwrap_or(Value::Null);

    let mut base = match current {
        None => json!({}),
        Some(bytes) => {
            let value: Value = serde_json::from_slice(bytes)
                .map_err(|error| format!("当前 settings.json 不是合法 JSON: {error}"))?;
            if !value.is_object() {
                return Err("当前 settings.json 不是 JSON 对象".to_string());
            }
            value
        }
    };
    let object = base.as_object_mut().unwrap();
    let env = object
        .entry("env".to_string())
        .or_insert_with(|| json!({}));
    if !env.is_object() {
        *env = json!({});
    }
    let env_object = env.as_object_mut().unwrap();
    env_object.remove(AUTH_TOKEN);
    env_object.remove(REGION_ENV);
    env_object.insert(AUTH_TOKEN.to_string(), token_value);
    if let Some(region) = snapshot_env.get(REGION_ENV) {
        env_object.insert(REGION_ENV.to_string(), region.clone());
    }
    serde_json::to_vec_pretty(&base)
        .map(Some)
        .map_err(|error| error.to_string())
}

/// `clear_file` 钩子：只移除 env 下的认证键，保留其余配置；文件本身保留。
fn clear_settings(_tag: &str, current: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let mut value: Value = serde_json::from_slice(current)
        .map_err(|error| format!("当前 settings.json 不是合法 JSON: {error}"))?;
    if !value.is_object() {
        return Err("当前 settings.json 不是 JSON 对象".to_string());
    }
    let has_auth_keys = value
        .get("env")
        .and_then(Value::as_object)
        .is_some_and(|env| env.contains_key(AUTH_TOKEN) || env.contains_key(REGION_ENV));
    if !has_auth_keys {
        // 没有认证键可删：原样返回，避免无意义地重排用户文件格式
        return Ok(Some(current.to_vec()));
    }
    if let Some(env) = value.get_mut("env").and_then(Value::as_object_mut) {
        env.remove(AUTH_TOKEN);
        env.remove(REGION_ENV);
        if env.is_empty() {
            value
                .as_object_mut()
                .unwrap()
                .remove("env");
        }
    }
    serde_json::to_vec_pretty(&value)
        .map(Some)
        .map_err(|error| error.to_string())
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
    require_fingerprint: true,
    merge_restore: Some(merge_settings),
    clear_file: Some(clear_settings),
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
            require_fingerprint: false,
            merge_restore: None,
            clear_file: None,
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

    fn write_settings(roots: &Roots, value: &Value) -> std::path::PathBuf {
        let path = roots.user_profile.join(".codebuddy").join("settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
        path
    }

    fn write_backup(roots: &Roots, id: &str, files: &[(&str, &Value)]) {
        let directory = STORE.accounts_root(roots).join(id);
        let data = directory.join("data");
        fs::create_dir_all(&data).unwrap();
        for (name, value) in files {
            fs::write(
                data.join(name),
                serde_json::to_vec_pretty(value).unwrap(),
            )
            .unwrap();
        }
        let manifest = json!({
            "version": 1,
            "id": id,
            "name": format!("账号-{id}"),
            "created_at": 1,
            "updated_at": 1,
            "item_count": files.len(),
        });
        fs::write(
            directory.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn switch_preserves_non_auth_settings() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let path = write_settings(
            &roots,
            &json!({
                "apiKeyHelper": "/usr/local/bin/helper",
                "model": "codebuddy-x",
                "trustedDirectories": ["/work"],
                "env": {
                    "CODEBUDDY_AUTH_TOKEN": "token-a",
                    "CODEBUDDY_INTERNET_ENVIRONMENT": "internal",
                    "OTHER_ENV": "keep-me"
                }
            }),
        );

        import_codebuddy_text(
            &STORE,
            &roots,
            r#"{"email":"b@example.com","env":{"CODEBUDDY_AUTH_TOKEN":"token-b","CODEBUDDY_INTERNET_ENVIRONMENT":"ai"}}"#,
        )
        .unwrap();
        let accounts = STORE.list_accounts(&roots).unwrap();
        assert_eq!(accounts.len(), 1);
        STORE.switch_account(&roots, &accounts[0]).unwrap();

        let merged: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(merged["env"][AUTH_TOKEN], "token-b");
        assert_eq!(merged["env"][REGION_ENV], "ai");
        assert_eq!(merged["env"]["OTHER_ENV"], "keep-me");
        assert_eq!(merged["apiKeyHelper"], "/usr/local/bin/helper");
        assert_eq!(merged["model"], "codebuddy-x");
        assert_eq!(merged["trustedDirectories"], json!(["/work"]));
        assert_eq!(
            STORE.active_account(&roots).as_deref(),
            Some(accounts[0].manifest.id.as_str())
        );
    }

    #[test]
    fn switch_supports_legacy_data_settings_json_layout() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let path = write_settings(
            &roots,
            &json!({"apiKeyHelper": "helper", "env": {"CODEBUDDY_AUTH_TOKEN": "token-a"}}),
        );
        // 旧布局：快照文件按原文件名存放在 data/settings.json 而非 data/settings
        write_backup(
            &roots,
            "legacy-acc",
            &[(
                "settings.json",
                &json!({"env": {"CODEBUDDY_AUTH_TOKEN": "token-c"}}),
            )],
        );
        let accounts = STORE.list_accounts(&roots).unwrap();
        assert_eq!(accounts.len(), 1);
        STORE.switch_account(&roots, &accounts[0]).unwrap();
        let merged: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(merged["env"][AUTH_TOKEN], "token-c");
        assert_eq!(merged["apiKeyHelper"], "helper");
    }

    #[test]
    fn switch_without_credentials_errors_and_rolls_back() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let path = write_settings(
            &roots,
            &json!({
                "apiKeyHelper": "helper",
                "env": {"CODEBUDDY_AUTH_TOKEN": "token-a"}
            }),
        );
        let before = fs::read(&path).unwrap();
        write_backup(
            &roots,
            "no-creds",
            &[("settings", &json!({"trustedDirectories": ["/elsewhere"]}))],
        );
        let accounts = STORE.list_accounts(&roots).unwrap();
        let error = STORE.switch_account(&roots, &accounts[0]).unwrap_err();
        assert!(
            error.contains("CODEBUDDY_AUTH_TOKEN"),
            "错误应说明缺少凭据: {error}"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "切换失败后当前 settings.json 应与切换前一致"
        );
    }

    #[test]
    fn switch_rollback_restores_exact_bytes_when_local_has_no_token() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        // 本机真实场景：只有 apiKeyHelper+model，没有 token。safety 快照
        // 同样没有凭据，回滚必须走整文件复制而不是 merge_restore。
        let path = write_settings(
            &roots,
            &json!({"apiKeyHelper": "helper", "model": "codebuddy-x"}),
        );
        let before = fs::read(&path).unwrap();
        write_backup(
            &roots,
            "no-creds",
            &[("settings", &json!({"trustedDirectories": ["/elsewhere"]}))],
        );
        let accounts = STORE.list_accounts(&roots).unwrap();
        let error = STORE.switch_account(&roots, &accounts[0]).unwrap_err();
        assert!(
            error.starts_with("切换失败，已恢复原账号"),
            "回滚应成功: {error}"
        );
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn save_without_token_is_rejected_and_detect_has_no_fingerprint() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        write_settings(
            &roots,
            &json!({"apiKeyHelper": "/usr/local/bin/helper", "model": "x"}),
        );
        let identity = detect_codebuddy(&roots);
        assert_eq!(identity.fingerprint, None);
        assert_eq!(
            identity.auth_type.as_deref(),
            Some("apiKeyHelper 外部凭据（本工具不接管）")
        );
        let error = STORE.save_current_account(&roots, None, None).unwrap_err();
        assert!(error.contains("登录凭据"), "错误应说明缺少凭据: {error}");
        assert_eq!(STORE.list_accounts(&roots).unwrap().len(), 0);
    }

    #[test]
    fn clear_removes_only_auth_keys_and_backs_up() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let path = write_settings(
            &roots,
            &json!({
                "apiKeyHelper": "helper",
                "model": "codebuddy-x",
                "env": {
                    "CODEBUDDY_AUTH_TOKEN": "token-a",
                    "CODEBUDDY_INTERNET_ENVIRONMENT": "internal",
                    "OTHER_ENV": "keep-me"
                }
            }),
        );

        let backup_name = STORE.clear_account(&roots).unwrap();
        assert!(backup_name.is_some());
        assert!(path.exists(), "清空后配置文件应保留");
        let cleared: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(cleared["env"][AUTH_TOKEN], Value::Null);
        assert_eq!(cleared["env"][REGION_ENV], Value::Null);
        assert_eq!(cleared["env"]["OTHER_ENV"], "keep-me");
        assert_eq!(cleared["apiKeyHelper"], "helper");
        assert_eq!(cleared["model"], "codebuddy-x");

        let accounts = STORE.list_accounts(&roots).unwrap();
        assert_eq!(accounts.len(), 1, "清空前应生成自动备份");
        let backup: Value = serde_json::from_slice(
            &fs::read(accounts[0].directory.join("data").join("settings")).unwrap(),
        )
        .unwrap();
        assert_eq!(backup["env"][AUTH_TOKEN], "token-a");

        // 已无 token：再次清空不报错也不新增备份
        let second = STORE.clear_account(&roots).unwrap();
        assert_eq!(second, None);
        assert_eq!(STORE.list_accounts(&roots).unwrap().len(), 1);
    }
}
