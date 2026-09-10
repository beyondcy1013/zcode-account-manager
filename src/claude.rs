//! Anthropic Claude Code 的账号识别与存储描述。
//!
//! 登录状态位于：
//! - `~/.claude/.credentials.json`：claude.ai OAuth 登录凭据（存在时纳入快照）；
//! - `~/.claude/settings.json`：用户配置，含 `env.ANTHROPIC_AUTH_TOKEN` /
//!   `ANTHROPIC_API_KEY` / `ANTHROPIC_BASE_URL` 等自定义端点认证——通过替换
//!   该文件即可在多个 API 端点账号之间切换，快照缺失时保留本机现状。
//!
//! `~/.claude.json` 含大量本机项目状态，不参与快照；仅在其 `oauthAccount`
//! 中尽力补齐邮箱显示。快照与切换机制由 `cli_accounts` 提供。

use crate::cli_accounts::{read_json, short_digest, CliIdentity, ToolPath, ToolStore};
use crate::Roots;
use serde_json::Value;
use std::{fs, path::PathBuf};

static CLAUDE_PATHS: &[ToolPath] = &[
    ToolPath {
        tag: "credentials",
        relative: ".credentials.json",
    },
    ToolPath {
        tag: "settings",
        relative: "settings.json",
    },
];
static CLAUDE_TAGS: &[&str] = &["credentials", "settings"];
/// settings.json 是用户配置（含端点认证）：快照缺失时保留本机现状。
static CLAUDE_PRESERVE_IF_ABSENT: &[&str] = &["settings"];
/// 「清空账号」删除 OAuth 凭据与 settings.json（其中的 env Token 就是端点
/// 账号的登录状态，清掉才恢复未登录；清空前会自动备份，可随时切回）。
static CLAUDE_CLEAR_TAGS: &[&str] = &["credentials", "settings"];

pub fn claude_dir(roots: &Roots) -> PathBuf {
    roots.user_profile.join(".claude")
}

pub static STORE: ToolStore = ToolStore {
    key: "claude",
    tab: "Claude",
    display: "Claude Code",
    cli_names: "claude",
    id_prefix: "Claude",
    fallback_name: "我的Claude账号",
    base_dir: claude_dir,
    paths: CLAUDE_PATHS,
    tags: CLAUDE_TAGS,
    preserve_if_absent: CLAUDE_PRESERVE_IF_ABSENT,
    clear_tags: CLAUDE_CLEAR_TAGS,
    launch_commands: &["claude"],
    detect,
    process_pattern: r"(^|/)claude( |$)",
};

/// 识别当前 Claude Code 账号：OAuth 凭据优先，settings 中的 API Token 补充。
pub fn detect(roots: &Roots) -> CliIdentity {
    let mut identity = CliIdentity::default();
    let claude = claude_dir(roots);

    // OAuth 登录：accessToken 为 JWT，指纹取稳定不变的 refreshToken
    let credentials_path = claude.join(".credentials.json");
    if let Ok(bytes) = fs::read(&credentials_path) {
        let value = serde_json::from_slice::<Value>(&bytes).ok();
        if let Some(oauth) = value
            .as_ref()
            .and_then(|value| value.get("claudeAiOauth"))
            .filter(|oauth| !oauth.is_null())
        {
            let access_token = oauth
                .get("accessToken")
                .and_then(Value::as_str)
                .unwrap_or_default();
            identity.email = crate::identity::decode_jwt_identity(access_token);
            identity.fingerprint = oauth
                .get("refreshToken")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .map(|token| short_digest(token.as_bytes()))
                .or_else(|| Some(short_digest(&bytes)));
            identity.auth_type = Some("Claude OAuth".to_string());
        } else {
            identity.fingerprint = Some(short_digest(&bytes));
        }
    }

    // settings.json：自定义端点 / API Token（常见的中转与第三方账号接入方式）
    if let Some(value) = read_json(&claude.join("settings.json")) {
        let env = value.get("env");
        let token = ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"]
            .iter()
            .find_map(|key| {
                env.and_then(|env| env.get(key))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|token| !token.is_empty())
            })
            .or_else(|| {
                value
                    .get("primaryApiKey")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|token| !token.is_empty())
            });
        if let Some(token) = token {
            if identity.auth_type.is_none() {
                let endpoint = env
                    .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
                    .and_then(Value::as_str)
                    .and_then(endpoint_host);
                identity.auth_type = Some(match endpoint {
                    Some(host) => format!("API 端点 {host}"),
                    None => "API Key".to_string(),
                });
            }
            if identity.fingerprint.is_none() {
                identity.fingerprint = Some(short_digest(token.as_bytes()));
            }
        }
    }

    // OAuth 邮箱缺失时从 ~/.claude.json 的 oauthAccount 尽力补齐（仅展示用）
    if identity.email.is_none() {
        if let Some(value) = read_json(&roots.user_profile.join(".claude.json")) {
            identity.email = value
                .pointer("/oauthAccount/emailAddress")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|email| !email.is_empty())
                .map(str::to_string);
        }
    }
    identity
}

/// 从 API 端点 URL 提取主机名用于展示（如 open.bigmodel.cn）。
fn endpoint_host(url: &str) -> Option<&str> {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let host = without_scheme.split('/').next()?;
    (!host.is_empty()).then_some(host)
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

    fn jwt_with_claims(claims: &str) -> String {
        fn encode(part: &[u8]) -> String {
            const TABLE: &[u8] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = String::new();
            for chunk in part.chunks(3) {
                let b = [
                    chunk[0],
                    chunk.get(1).copied().unwrap_or(0),
                    chunk.get(2).copied().unwrap_or(0),
                ];
                out.push(TABLE[(b[0] >> 2) as usize] as char);
                out.push(TABLE[(((b[0] & 0x03) << 4) | (b[1] >> 4)) as usize] as char);
                if chunk.len() > 1 {
                    out.push(TABLE[(((b[1] & 0x0f) << 2) | (b[2] >> 6)) as usize] as char);
                }
                if chunk.len() > 2 {
                    out.push(TABLE[(b[2] & 0x3f) as usize] as char);
                }
            }
            out
        }
        format!(
            "{}.{}.sig",
            encode(br#"{"alg":"RS256"}"#),
            encode(claims.as_bytes())
        )
    }

    fn write_file(roots: &Roots, tag: &str, contents: &str) {
        let path = STORE.path_by_tag(roots, tag);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn detects_oauth_identity() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let access = jwt_with_claims(r#"{"sub":"acc-uuid","email":"dev@gmail.com"}"#);
        write_file(
            &roots,
            "credentials",
            &format!(
                r#"{{"claudeAiOauth":{{"accessToken":"{access}","refreshToken":"rt-1","expiresAt":1790000000000,"scopes":["user:inference"]}}}}"#
            ),
        );

        let identity = detect(&roots);
        assert_eq!(identity.email.as_deref(), Some("dev@gmail.com"));
        assert_eq!(identity.auth_type.as_deref(), Some("Claude OAuth"));
        let fingerprint = identity.fingerprint.clone().unwrap();
        assert_eq!(fingerprint.len(), 12);

        // accessToken 刷新不影响指纹
        write_file(
            &roots,
            "credentials",
            &format!(
                r#"{{"claudeAiOauth":{{"accessToken":"{access}x","refreshToken":"rt-1","expiresAt":1790000000001,"scopes":["user:inference"]}}}}"#
            ),
        );
        assert_eq!(
            detect(&roots).fingerprint.as_deref(),
            Some(fingerprint.as_str())
        );
    }

    #[test]
    fn detects_endpoint_token_from_settings() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        write_file(
            &roots,
            "settings",
            r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"token-glm","ANTHROPIC_BASE_URL":"https://open.bigmodel.cn/api/anthropic"}}"#,
        );

        let identity = detect(&roots);
        assert_eq!(identity.email, None);
        assert_eq!(
            identity.auth_type.as_deref(),
            Some("API 端点 open.bigmodel.cn")
        );
        let glm_fingerprint = identity.fingerprint.clone().unwrap();

        // 不同端点 / token 即视为不同账号
        write_file(
            &roots,
            "settings",
            r#"{"env":{"ANTHROPIC_API_KEY":"token-mm","ANTHROPIC_BASE_URL":"https://api.minimaxi.com/anthropic"}}"#,
        );
        let other = detect(&roots);
        assert_eq!(
            other.auth_type.as_deref(),
            Some("API 端点 api.minimaxi.com")
        );
        assert_ne!(other.fingerprint, Some(glm_fingerprint));
    }

    #[test]
    fn switch_swaps_credentials_and_settings() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        write_file(&roots, "settings", r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"token-a"}}"#);
        let account_a = STORE.save_current_account(&roots, Some("Claude A"), None).unwrap();

        write_file(&roots, "settings", r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"token-b"}}"#);
        let account_b = STORE.save_current_account(&roots, Some("Claude B"), None).unwrap();

        STORE.switch_account(&roots, &account_a).unwrap();
        assert_eq!(
            fs::read(STORE.path_by_tag(&roots, "settings")).unwrap(),
            br#"{"env":{"ANTHROPIC_AUTH_TOKEN":"token-a"}}"#,
        );
        STORE.switch_account(&roots, &account_b).unwrap();
        assert_eq!(
            fs::read(STORE.path_by_tag(&roots, "settings")).unwrap(),
            br#"{"env":{"ANTHROPIC_AUTH_TOKEN":"token-b"}}"#,
        );
    }

    #[test]
    fn endpoint_host_parses_urls() {
        assert_eq!(
            endpoint_host("https://open.bigmodel.cn/api/anthropic"),
            Some("open.bigmodel.cn")
        );
        assert_eq!(endpoint_host("http://localhost:8080"), Some("localhost:8080"));
        assert_eq!(endpoint_host(""), None);
    }
}
