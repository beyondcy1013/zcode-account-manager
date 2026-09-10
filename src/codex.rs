//! OpenAI Codex CLI 的账号识别与存储描述。
//!
//! 登录状态位于 `~/.codex`：
//! - `auth.json`：ChatGPT OAuth 登录（`tokens.id_token/refresh_token`）或
//!   API Key（`OPENAI_API_KEY`），两者都可能是当前生效的认证方式；
//! - `config.toml`：用户配置（模型、Provider 等），切换账号时保留本机现状。
//! 快照与切换机制由 `cli_accounts` 提供。

use crate::cli_accounts::{short_digest, CliIdentity, ToolPath, ToolStore};
use crate::Roots;
use serde_json::Value;
use std::{fs, path::PathBuf};

static CODEX_PATHS: &[ToolPath] = &[
    ToolPath {
        tag: "auth",
        relative: "auth.json",
    },
    ToolPath {
        tag: "config",
        relative: "config.toml",
    },
];
static CODEX_TAGS: &[&str] = &["auth", "config"];
/// config.toml 是用户配置：快照缺失时保留本机现状。
static CODEX_PRESERVE_IF_ABSENT: &[&str] = &["config"];
/// 「清空账号」只删登录凭据；config.toml 是用户配置，保留。
static CODEX_CLEAR_TAGS: &[&str] = &["auth"];

pub fn codex_dir(roots: &Roots) -> PathBuf {
    roots.user_profile.join(".codex")
}

pub static STORE: ToolStore = ToolStore {
    key: "codex",
    tab: "Codex",
    display: "Codex CLI",
    cli_names: "codex",
    id_prefix: "Codex",
    fallback_name: "我的Codex账号",
    base_dir: codex_dir,
    paths: CODEX_PATHS,
    tags: CODEX_TAGS,
    preserve_if_absent: CODEX_PRESERVE_IF_ABSENT,
    clear_tags: CODEX_CLEAR_TAGS,
    detect,
    process_pattern: r"(^|/)codex( |$)",
};

/// 读取 `~/.codex/auth.json` 识别当前账号：ChatGPT OAuth 优先，API Key 补充。
pub fn detect(roots: &Roots) -> CliIdentity {
    let mut identity = CliIdentity::default();
    let auth_path = codex_dir(roots).join("auth.json");
    let Some(bytes) = fs::read(&auth_path).ok() else {
        return identity;
    };
    let value = serde_json::from_slice::<Value>(&bytes).ok();

    // ChatGPT OAuth 登录：email 来自 id_token，指纹取稳定不变的 refresh_token
    if let Some(value) = value.as_ref() {
        let tokens = value.get("tokens").filter(|tokens| !tokens.is_null());
        if let Some(tokens) = tokens {
            let id_token = tokens
                .get("id_token")
                .and_then(Value::as_str)
                .unwrap_or_default();
            identity.email = crate::identity::decode_jwt_identity(id_token);
            identity.fingerprint = tokens
                .get("refresh_token")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .map(|token| short_digest(token.as_bytes()))
                .or_else(|| Some(short_digest(&bytes)));
            identity.auth_type = Some(match value.get("auth_mode").and_then(Value::as_str) {
                Some("chatgpt") | None => "ChatGPT 账号".to_string(),
                Some(other) => format!("Codex {other}"),
            });
        }
    }

    // API Key 模式（auth.json 里只写 OPENAI_API_KEY）
    if let Some(value) = value.as_ref() {
        if identity.auth_type.is_none() {
            if let Some(key) = value
                .get("OPENAI_API_KEY")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|key| !key.is_empty())
            {
                identity.auth_type = Some("API Key".to_string());
                if identity.fingerprint.is_none() {
                    identity.fingerprint = Some(short_digest(key.as_bytes()));
                }
            }
        }
    }

    // 都识别不出时退回整个文件摘要，至少保证备份可区分
    if identity.fingerprint.is_none() {
        identity.fingerprint = Some(short_digest(&bytes));
    }
    identity
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

    fn write_auth(roots: &Roots, contents: &str) {
        let path = STORE.path_by_tag(roots, "auth");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn detects_chatgpt_oauth_identity() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let id_token = jwt_with_claims(r#"{"email":"dev@gmail.com","email_verified":true}"#);
        write_auth(
            &roots,
            &format!(
                r#"{{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{{"id_token":"{id_token}","access_token":"at","refresh_token":"rt1","account_id":"acc"}},"last_refresh":"2026-03-04"}}"#
            ),
        );

        let identity = detect(&roots);
        assert_eq!(identity.email.as_deref(), Some("dev@gmail.com"));
        assert_eq!(identity.auth_type.as_deref(), Some("ChatGPT 账号"));
        let fingerprint = identity.fingerprint.clone().unwrap();
        assert_eq!(fingerprint.len(), 12);
        assert_eq!(identity.default_name("Codex").as_deref(), Some("dev"));

        // access_token 变化（token 刷新）不影响指纹
        write_auth(
            &roots,
            &format!(
                r#"{{"auth_mode":"chatgpt","tokens":{{"id_token":"{id_token}","access_token":"at2","refresh_token":"rt1","account_id":"acc"}}}}"#
            ),
        );
        assert_eq!(
            detect(&roots).fingerprint.as_deref(),
            Some(fingerprint.as_str())
        );

        // refresh_token 变化即视为不同账号
        write_auth(
            &roots,
            &format!(
                r#"{{"tokens":{{"id_token":"{id_token}","access_token":"at","refresh_token":"rt2","account_id":"acc"}}}}"#
            ),
        );
        assert_ne!(
            detect(&roots).fingerprint.as_deref(),
            Some(fingerprint.as_str())
        );
    }

    #[test]
    fn detects_api_key_identity() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        write_auth(&roots, r#"{"OPENAI_API_KEY":"sk-test-abcdef123456"}"#);

        let identity = detect(&roots);
        assert_eq!(identity.email, None);
        assert_eq!(identity.auth_type.as_deref(), Some("API Key"));
        assert!(identity.fingerprint.is_some());
        assert_eq!(
            identity.default_name("Codex").as_deref(),
            Some(format!("Codex{}", &identity.fingerprint.clone().unwrap()[..8]).as_str())
        );
    }

    #[test]
    fn empty_state_and_fallback_digest() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        assert_eq!(detect(&roots), CliIdentity::default());

        // 非 JSON 的 auth.json：指纹退回整个文件摘要
        write_auth(&roots, "opaque-bytes");
        let identity = detect(&roots);
        assert_eq!(identity.auth_type, None);
        assert!(identity.fingerprint.is_some());
    }

    #[test]
    fn switch_swaps_auth_and_preserves_config() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        write_auth(&roots, r#"{"OPENAI_API_KEY":"sk-key-a"}"#);
        let account_a = STORE.save_current_account(&roots, Some("Codex A"), None).unwrap();

        write_auth(&roots, r#"{"OPENAI_API_KEY":"sk-key-b"}"#);
        let account_b = STORE.save_current_account(&roots, Some("Codex B"), None).unwrap();

        // config.toml 不在快照里（保存时不存在），切换后保留本机现状
        let config = STORE.path_by_tag(&roots, "config");
        fs::write(&config, b"user-config").unwrap();

        STORE.switch_account(&roots, &account_a).unwrap();
        assert_eq!(
            fs::read(STORE.path_by_tag(&roots, "auth")).unwrap(),
            br#"{"OPENAI_API_KEY":"sk-key-a"}"#,
        );
        assert_eq!(fs::read(&config).unwrap(), b"user-config");

        STORE.switch_account(&roots, &account_b).unwrap();
        assert_eq!(
            fs::read(STORE.path_by_tag(&roots, "auth")).unwrap(),
            br#"{"OPENAI_API_KEY":"sk-key-b"}"#,
        );
    }
}
