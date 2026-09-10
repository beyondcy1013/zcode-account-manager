use crate::Roots;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

/// 当前登录账号的可读标识，全部字段尽力提取，允许缺失。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccountIdentity {
    pub username: Option<String>,
    pub user_id: Option<String>,
    pub provider: Option<String>,
    /// credentials.json 内容摘要（SHA-256 前 12 位），用于把当前状态与既有备份做对比。
    pub fingerprint: Option<String>,
}

impl AccountIdentity {
    pub fn is_present(&self) -> bool {
        self.username.is_some() || self.user_id.is_some() || self.fingerprint.is_some()
    }

    /// 用账号标识推导默认备份名：用户名 > 用户ID > 指纹前缀。
    pub fn default_name(&self) -> Option<String> {
        if let Some(username) = self
            .username
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let base = username.split('@').next().unwrap_or(username);
            if !base.is_empty() {
                return Some(base.to_string());
            }
        }
        if let Some(user_id) = self
            .user_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(format!("用户{user_id}"));
        }
        if let Some(fingerprint) = self.fingerprint.as_deref() {
            let len = fingerprint.len().min(8);
            return Some(format!("账号{}", &fingerprint[..len]));
        }
        None
    }

    /// 一行文本描述，用于界面展示。
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(username) = self
            .username
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            parts.push(username.to_string());
        }
        if let Some(user_id) = self
            .user_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            parts.push(format!("ID {user_id}"));
        }
        if let Some(provider) = self
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            parts.push(provider.to_string());
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

/// 读取本地文件并尽力调用 CLI，汇总当前账号标识。
pub fn detect(roots: &Roots) -> AccountIdentity {
    let mut identity = detect_from_files(roots);
    detect_from_cli(&mut identity);
    identity
}

/// 仅基于本地文件提取（不含 CLI 调用，测试可保持确定性）。
pub fn detect_from_files(roots: &Roots) -> AccountIdentity {
    let mut identity = AccountIdentity::default();
    let v2 = roots.user_profile.join(".zcode").join("v2");

    if let Ok(value) = fs::read(v2.join("config.json"))
        .map_err(|_| ())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).map_err(|_| ()))
    {
        identity.user_id = jwt_user_id_from_config(&value);
    }
    if identity.provider.is_none() {
        if let Ok(value) = fs::read(v2.join("setting.json"))
            .map_err(|_| ())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).map_err(|_| ()))
        {
            identity.provider = value
                .get("providerFamilyDomain")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
    }
    if let Ok(bytes) = fs::read(v2.join("credentials.json")) {
        let digest = Sha256::digest(&bytes);
        identity.fingerprint = Some(hex_encode(&digest)[..12].to_string());
    }
    identity
}

fn jwt_user_id_from_config(value: &Value) -> Option<String> {
    let providers = value.get("provider")?.as_object()?;
    for provider in providers.values() {
        let api_key = provider
            .get("options")
            .and_then(|options| options.get("apiKey"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if let Some(id) = decode_jwt_identity(api_key) {
            return Some(id);
        }
    }
    None
}

/// 从 JWT apiKey 的载荷中提取账号标识（无需验签，仅读取声明）。
pub fn decode_jwt_identity(token: &str) -> Option<String> {
    if !token.starts_with("eyJ") {
        return None;
    }
    let payload = token.split('.').nth(1)?;
    let claims: Value = serde_json::from_slice(&base64_url_decode(payload)?).ok()?;
    for key in ["email", "user_id", "sub", "uid", "name"] {
        if let Some(value) = claims.get(key).and_then(Value::as_str) {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// `zcode status` 输出 JSON（含 user.username / user.id / provider），3 秒内未返回则放弃。
fn detect_from_cli(identity: &mut AccountIdentity) {
    let mut command = Command::new("zcode");
    command
        .arg("status")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        // Windows 上 zcode 是控制台程序：GUI 调用时不能弹出黑框
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return,
    };
    let mut stdout = child.stdout.take();
    let (sender, receiver) = mpsc::channel::<String>();
    thread::spawn(move || {
        let mut output = String::new();
        if let Some(stream) = stdout.as_mut() {
            let _ = stream.read_to_string(&mut output);
        }
        let _ = sender.send(output);
    });
    let output = match receiver.recv_timeout(Duration::from_secs(3)) {
        Ok(output) => output,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
    };
    let _ = child.wait();

    let Some(start) = output.find('{') else {
        return;
    };
    let Ok(value) = serde_json::from_str::<Value>(&output[start..]) else {
        return;
    };
    if identity.username.is_none() {
        identity.username = value
            .pointer("/user/username")
            .or_else(|| value.pointer("/user/name"))
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    if identity.user_id.is_none() {
        identity.user_id = value
            .pointer("/user/id")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    if identity.provider.is_none() {
        identity.provider = value
            .get("provider")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
}

fn base64_url_decode(input: &str) -> Option<Vec<u8>> {
    fn value_of(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some((byte - b'A') as u32),
            b'a'..=b'z' => Some((byte - b'a') as u32 + 26),
            b'0'..=b'9' => Some((byte - b'0') as u32 + 52),
            b'-' | b'+' => Some(62),
            b'_' | b'/' => Some(63),
            _ => None,
        }
    }
    let mut decoded = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        buffer = (buffer << 6) | value_of(byte)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            decoded.push((buffer >> bits) as u8);
        }
    }
    Some(decoded)
}

fn hex_encode(bytes: &[u8]) -> String {
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
            encode(br#"{"alg":"HS256"}"#),
            encode(claims.as_bytes())
        )
    }

    #[test]
    fn decodes_user_id_from_jwt_api_key() {
        let token = jwt_with_claims(r#"{"user_id":"51701756825023338","iat":1782452955}"#);
        assert_eq!(
            decode_jwt_identity(&token).as_deref(),
            Some("51701756825023338")
        );
        assert_eq!(
            decode_jwt_identity("84ac0f0dabcdef0123456789.0123456789abcdef"),
            None
        );
        assert_eq!(decode_jwt_identity("not.a.jwt"), None);
    }

    #[test]
    fn extracts_identity_from_local_files() {
        let temp = TempDir::new().unwrap();
        let roots = roots(&temp);
        let v2 = roots.user_profile.join(".zcode").join("v2");
        fs::create_dir_all(&v2).unwrap();
        let token = jwt_with_claims(r#"{"sub":"12345"}"#);
        fs::write(
            v2.join("config.json"),
            format!(r#"{{"provider":{{"p1":{{"options":{{"apiKey":"{token}"}}}}}}}}"#),
        )
        .unwrap();
        fs::write(
            v2.join("setting.json"),
            r#"{"providerFamilyDomain":"bigmodel"}"#,
        )
        .unwrap();
        fs::write(v2.join("credentials.json"), b"secret-material").unwrap();

        let identity = detect_from_files(&roots);
        assert_eq!(identity.user_id.as_deref(), Some("12345"));
        assert_eq!(identity.provider.as_deref(), Some("bigmodel"));
        assert_eq!(identity.fingerprint.as_deref().map(str::len), Some(12));
        assert_eq!(identity.default_name().as_deref(), Some("用户12345"));
    }

    #[test]
    fn default_name_prefers_username_and_handles_email() {
        let identity = AccountIdentity {
            username: Some("someone@example.com".into()),
            user_id: Some("42".into()),
            ..Default::default()
        };
        assert_eq!(identity.default_name().as_deref(), Some("someone"));

        let id_only = AccountIdentity {
            user_id: Some("51701756825023338".into()),
            ..Default::default()
        };
        assert_eq!(
            id_only.default_name().as_deref(),
            Some("用户51701756825023338")
        );

        let fp_only = AccountIdentity {
            fingerprint: Some("84ac0f0dabcd".into()),
            ..Default::default()
        };
        assert_eq!(fp_only.default_name().as_deref(), Some("账号84ac0f0d"));
        assert_eq!(AccountIdentity::default().default_name(), None);
    }

    #[test]
    fn describe_joins_available_parts() {
        assert_eq!(AccountIdentity::default().describe(), "未检测到登录状态");
        let identity = AccountIdentity {
            username: Some("tester".into()),
            provider: Some("bigmodel".into()),
            fingerprint: Some("a1b2c3d4e5f6".into()),
            ..Default::default()
        };
        assert_eq!(identity.describe(), "tester · bigmodel · 指纹 a1b2c3d4e5f6");
    }
}
