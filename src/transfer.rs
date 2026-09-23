//! 账号导入导出：所有账号类型（ZCode 主账号 + Gemini / Codex / Claude /
//! CodeBuddy CLI）统一的跨机器迁移能力。
//!
//! 导出为单个 JSON「移植文件」（内嵌 base64 文件内容）：
//! - CLI 工具快照只有少量小文件 → 移植文件很小，可走剪贴板复制/粘贴导入；
//! - ZCode 主账号快照包含整棵目录树（Local Storage 等，很多个文件）→
//!   移植文件较大，只走文件导入导出，不提供剪贴板通道。
//!
//! 除移植格式外，也接受各工具的「原生格式」JSON（另一台机器上的
//! `auth.json` / `settings.json` / `.credentials.json` / WorkBuddy 数组等），
//! 具体识别由各工具模块的 `import_native_text` 完成。
//!
//! 导入只创建备份，不切换当前登录状态；切换仍由统一的切换流程执行。

use crate::cli_accounts::{now, ImportResult, ToolStore, MANIFEST_VERSION};
use crate::{accounts, claude, codebuddy, codex, gemini, Roots, FULL_TAGS};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::{fs, path::Path};

/// 移植文件的格式标识与版本。
pub const FORMAT_ID: &str = "zcode-account-manager-transfer";
pub const FORMAT_VERSION: u32 = 1;
/// ZCode 主账号在移植文件中的 tool 标识。
pub const ZCODE_TOOL_KEY: &str = "zcode";

/// 移植文件中的一个快照项目：标签对应路径下「相对路径 → base64 内容」。
/// `directory` 区分两种布局：文件型标签（如 credentials）只有一个文件，
/// 恢复时直接写到 `data/<tag>`；目录型标签（如 session_full）包含多级
/// 相对路径，恢复时重建 `data/<tag>/...` 目录树。
#[derive(Serialize, Deserialize)]
pub struct TransferItem {
    pub tag: String,
    #[serde(default)]
    pub directory: bool,
    pub files: BTreeMap<String, String>,
}

/// 移植文件中的一个账号。
#[derive(Serialize, Deserialize)]
pub struct TransferAccount {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    pub items: Vec<TransferItem>,
}

/// 移植文件本体。
#[derive(Serialize, Deserialize)]
pub struct TransferFile {
    pub format: String,
    pub version: u32,
    /// "zcode" 或某个 CLI 工具的 store.key。
    pub tool: String,
    #[serde(default)]
    pub exported_at: u64,
    pub accounts: Vec<TransferAccount>,
}

/// 导出文件的建议文件名（如 `zam-gemini-accounts.json`）。
pub fn default_file_name(tool: &str) -> String {
    format!("zam-{tool}-accounts.json")
}

/// 工具标识的展示名，用于跨工具导入时的错误提示。
pub fn tool_display_name(tool: &str) -> Option<&'static str> {
    match tool {
        ZCODE_TOOL_KEY => Some("ZCode 主程序"),
        "gemini" => Some("Gemini / Antigravity CLI"),
        "codex" => Some("Codex CLI"),
        "claude" => Some("Claude Code"),
        "codebuddy" => Some("CodeBuddy CLI"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 导出
// ---------------------------------------------------------------------------

/// 导出 CLI 工具的账号备份。`ids` 为空表示全部账号。
pub fn export_tool_accounts(
    store: &ToolStore,
    roots: &Roots,
    ids: &[String],
) -> Result<String, String> {
    let profiles = store.list_accounts(roots)?;
    let selected: Vec<_> = profiles
        .iter()
        .filter(|profile| ids.is_empty() || ids.contains(&profile.manifest.id))
        .collect();
    if selected.is_empty() {
        return Err(format!("没有可导出的 {} 账号备份", store.display));
    }
    let mut accounts = Vec::new();
    for profile in selected {
        let data = profile.directory.join("data");
        let mut items = Vec::new();
        for tag in store.tags {
            let path = data.join(tag);
            if !path.exists() {
                continue;
            }
            let files = collect_files(&path)?;
            if !files.is_empty() {
                items.push(TransferItem {
                    tag: tag.to_string(),
                    directory: path.is_dir(),
                    files,
                });
            }
        }
        if items.is_empty() {
            return Err(format!(
                "账号「{}」的备份数据缺失（data 目录为空），无法导出",
                profile.manifest.display_name()
            ));
        }
        accounts.push(TransferAccount {
            name: profile.manifest.name.clone(),
            alias: profile.manifest.alias.clone(),
            phone: None,
            identity: profile.manifest.identity.clone(),
            fingerprint: profile.manifest.fingerprint.clone(),
            items,
        });
    }
    render_transfer_file(store.key, accounts)
}

/// 导出 ZCode 主账号备份。`ids` 为空表示全部账号。
pub fn export_zcode_accounts(roots: &Roots, ids: &[String]) -> Result<String, String> {
    let profiles = accounts::list_accounts(roots)?;
    let selected: Vec<_> = profiles
        .iter()
        .filter(|profile| ids.is_empty() || ids.contains(&profile.manifest.id))
        .collect();
    if selected.is_empty() {
        return Err("没有可导出的 ZCode 账号备份".to_string());
    }
    let mut accounts = Vec::new();
    for profile in selected {
        let data = profile.directory.join("data");
        let mut items = Vec::new();
        for tag in FULL_TAGS {
            let path = data.join(tag);
            if !path.exists() {
                continue;
            }
            let files = collect_files(&path)?;
            if !files.is_empty() {
                items.push(TransferItem {
                    tag: tag.to_string(),
                    directory: path.is_dir(),
                    files,
                });
            }
        }
        if items.is_empty() {
            return Err(format!(
                "账号「{}」的备份数据缺失（data 目录为空），无法导出",
                profile.manifest.display_name()
            ));
        }
        accounts.push(TransferAccount {
            name: profile.manifest.name.clone(),
            alias: profile.manifest.alias.clone(),
            phone: profile.manifest.phone.clone(),
            identity: profile.manifest.identity.clone(),
            fingerprint: profile.manifest.fingerprint.clone(),
            items,
        });
    }
    render_transfer_file(ZCODE_TOOL_KEY, accounts)
}

fn render_transfer_file(tool: &str, accounts: Vec<TransferAccount>) -> Result<String, String> {
    let file = TransferFile {
        format: FORMAT_ID.to_string(),
        version: FORMAT_VERSION,
        tool: tool.to_string(),
        exported_at: now(),
        accounts,
    };
    serde_json::to_string_pretty(&file).map_err(|error| format!("生成导出文件失败: {error}"))
}

/// 递归收集 path 下所有文件：「相对路径 → base64」。目录以内部相对路径为键；
/// path 本身是文件时以文件名为键。空目录会被跳过。
fn collect_files(path: &Path) -> Result<BTreeMap<String, String>, String> {
    fn walk(path: &Path, prefix: &Path, files: &mut BTreeMap<String, String>) -> Result<(), String> {
        if path.is_dir() {
            for entry in fs::read_dir(path).map_err(|error| error.to_string())? {
                let entry = entry.map_err(|error| error.to_string())?;
                walk(&entry.path(), prefix, files)?;
            }
            return Ok(());
        }
        let bytes = fs::read(path).map_err(|error| format!("读取 {} 失败: {error}", path.display()))?;
        // 顶层文件（strip_prefix 为空）以文件名为键；导入时据此还原为单文件标签
        let relative = match path.strip_prefix(prefix) {
            Ok(relative) if !relative.as_os_str().is_empty() => {
                relative.to_string_lossy().replace('\\', "/")
            }
            _ => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .ok_or_else(|| format!("路径缺少文件名: {}", path.display()))?,
        };
        files.insert(relative, base64_encode(&bytes));
        Ok(())
    }
    let mut files = BTreeMap::new();
    walk(path, path, &mut files)?;
    Ok(files)
}

// ---------------------------------------------------------------------------
// 导入
// ---------------------------------------------------------------------------

/// 若文本是本项目移植文件则解析返回，否则返回 None（交由原生格式处理）。
fn try_parse_portable(text: &str) -> Option<Result<TransferFile, String>> {
    let value: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    let is_portable = value
        .get("format")
        .and_then(|format| format.as_str())
        .is_some_and(|format| format == FORMAT_ID);
    if !is_portable {
        return None;
    }
    Some(
        serde_json::from_value(value)
            .map_err(|error| format!("移植文件格式不正确: {error}")),
    )
}

/// 解析并导入文本：优先识别移植文件，否则按该工具的原生格式导入。
/// 任何一种方式都只创建备份，不切换当前登录状态。
pub fn import_tool_text(
    store: &ToolStore,
    roots: &Roots,
    text: &str,
) -> Result<ImportResult, String> {
    if text.trim().is_empty() {
        return Err("导入内容为空".to_string());
    }
    if let Some(portable) = try_parse_portable(text) {
        let file = portable?;
        if file.tool != store.key {
            return Err(tool_mismatch_message(&file.tool, store.display));
        }
        import_portable_tool_accounts(store, roots, &file)
    } else {
        match store.key {
            "gemini" => gemini::import_native_text(store, roots, text),
            "codex" => codex::import_native_text(store, roots, text),
            "claude" => claude::import_native_text(store, roots, text),
            "codebuddy" => codebuddy::import_native_text(store, roots, text),
            other => Err(format!("工具 {other} 暂不支持原生格式导入")),
        }
    }
}

/// 从文件导入 CLI 工具账号。
pub fn import_tool_file(
    store: &ToolStore,
    roots: &Roots,
    path: &Path,
) -> Result<ImportResult, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("读取导入文件失败: {error}"))?;
    import_tool_text(store, roots, &text)
}

/// 导入 ZCode 主账号（仅支持移植文件；快照含大量文件，不走剪贴板）。
pub fn import_zcode_text(roots: &Roots, text: &str) -> Result<ImportResult, String> {
    if text.trim().is_empty() {
        return Err("导入内容为空".to_string());
    }
    let file = match try_parse_portable(text) {
        Some(portable) => portable?,
        None => {
            return Err(
                "ZCode 账号导入仅支持本工具导出的移植文件（zam-zcode-accounts.json）".to_string(),
            )
        }
    };
    if file.tool != ZCODE_TOOL_KEY {
        return Err(tool_mismatch_message(&file.tool, "ZCode 主程序"));
    }
    import_portable_zcode_accounts(roots, &file)
}

/// 从文件导入 ZCode 主账号。
pub fn import_zcode_file(roots: &Roots, path: &Path) -> Result<ImportResult, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("读取导入文件失败: {error}"))?;
    import_zcode_text(roots, &text)
}

fn tool_mismatch_message(actual_tool: &str, target_display: &str) -> String {
    let belongs_to = tool_display_name(actual_tool)
        .map(|name| format!("属于 {name}"))
        .unwrap_or_else(|| format!("的工具标识为 “{actual_tool}”"));
    format!(
        "导入内容{belongs_to}，与目标 {target_display} 不匹配；请在对应的账号页导入"
    )
}

fn import_portable_tool_accounts(
    store: &ToolStore,
    roots: &Roots,
    file: &TransferFile,
) -> Result<ImportResult, String> {
    let mut result = ImportResult::default();
    let mut errors = Vec::new();
    for (index, account) in file.accounts.iter().enumerate() {
        match import_one_tool_account(store, roots, account) {
            Ok(()) => result.imported += 1,
            Err(error) => {
                result.skipped += 1;
                let name = if account.name.trim().is_empty() {
                    format!("第 {} 项", index + 1)
                } else {
                    format!("「{}」", account.name)
                };
                errors.push(format!("{name}: {error}"));
            }
        }
    }
    if result.imported == 0 {
        return Err(errors.join("; "));
    }
    Ok(result)
}

fn import_one_tool_account(
    store: &ToolStore,
    roots: &Roots,
    account: &TransferAccount,
) -> Result<(), String> {
    let mut files = BTreeMap::new();
    for item in &account.items {
        let bytes = decode_item_files(&item.files)?;
        // CLI 工具每个标签对应一个文件；多文件（历史异常）时拼接为二进制流无意义，直接拒绝
        if bytes.len() != 1 {
            return Err(format!("标签 {} 应只包含一个文件", item.tag));
        }
        let (_, content) = bytes.into_iter().next().expect("checked length");
        files.insert(item.tag.clone(), content);
    }
    store
        .import_account_files(
            roots,
            &account.name,
            account.alias.as_deref(),
            account.identity.as_deref(),
            account.fingerprint.as_deref(),
            &files,
        )
        .map(|_| ())
}

fn import_portable_zcode_accounts(
    roots: &Roots,
    file: &TransferFile,
) -> Result<ImportResult, String> {
    let mut result = ImportResult::default();
    let mut errors = Vec::new();
    for (index, account) in file.accounts.iter().enumerate() {
        match import_one_zcode_account(roots, account) {
            Ok(()) => result.imported += 1,
            Err(error) => {
                result.skipped += 1;
                let name = if account.name.trim().is_empty() {
                    format!("第 {} 项", index + 1)
                } else {
                    format!("「{}」", account.name)
                };
                errors.push(format!("{name}: {error}"));
            }
        }
    }
    if result.imported == 0 {
        return Err(errors.join("; "));
    }
    Ok(result)
}

fn import_one_zcode_account(roots: &Roots, account: &TransferAccount) -> Result<(), String> {
    let name = account.name.trim();
    if name.is_empty() {
        return Err("账号名称不能为空".to_string());
    }
    // 只接受已知标签，未知标签（来自更新版本工具的导出）跳过而不是整体失败
    let mut items = Vec::new();
    for item in &account.items {
        if !FULL_TAGS.contains(&item.tag.as_str()) {
            continue;
        }
        let files = decode_item_files(&item.files)?;
        if files.is_empty() {
            continue;
        }
        // 文件型标签必须是单文件；目录型标签保持相对路径结构
        if !item.directory && files.len() != 1 {
            return Err(format!("标签 {} 应只包含一个文件", item.tag));
        }
        items.push((item.tag.clone(), item.directory, files));
    }
    if items.is_empty() {
        return Err("没有可导入的快照项目".to_string());
    }
    let timestamp = now();
    let manifest = accounts::AccountManifest {
        version: MANIFEST_VERSION,
        id: accounts::new_account_id(),
        name: name.to_string(),
        alias: account
            .alias
            .as_deref()
            .map(str::trim)
            .filter(|alias| !alias.is_empty())
            .map(str::to_string),
        phone: account
            .phone
            .as_deref()
            .map(str::trim)
            .filter(|phone| !phone.is_empty())
            .map(str::to_string),
        identity: account.identity.clone(),
        fingerprint: account.fingerprint.clone(),
        created_at: timestamp,
        updated_at: timestamp,
        item_count: items.len(),
    };
    let directory = accounts::accounts_root(roots).join(&manifest.id);
    let data = directory.join("data");
    fs::create_dir_all(&data).map_err(|error| format!("创建导入目录失败: {error}"))?;
    for (tag, directory, files) in items {
        if directory {
            for (relative, bytes) in files {
                let destination = data.join(&tag).join(&relative);
                let parent = destination
                    .parent()
                    .ok_or_else(|| "导入路径无效".to_string())?;
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                fs::write(&destination, bytes).map_err(|error| {
                    format!("写入 {} 失败: {error}", destination.display())
                })?;
            }
        } else {
            // 文件型标签：data/<tag> 本身就是文件
            let (_, bytes) = files.into_iter().next().expect("checked length");
            let destination = data.join(&tag);
            fs::write(&destination, bytes).map_err(|error| {
                format!("写入 {} 失败: {error}", destination.display())
            })?;
        }
    }
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?;
    fs::write(directory.join("manifest.json"), bytes).map_err(|error| error.to_string())?;
    Ok(())
}

/// 解码一个项目内的全部文件；标签内相对路径不允许跳出快照目录。
fn decode_item_files(files: &BTreeMap<String, String>) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut decoded = Vec::new();
    for (relative, encoded) in files {
        if relative.contains("..") || relative.starts_with('/') {
            return Err(format!("非法的文件路径: {relative}"));
        }
        let bytes =
            base64_decode(encoded).map_err(|error| format!("解码 {relative} 失败: {error}"))?;
        decoded.push((relative.clone(), bytes));
    }
    Ok(decoded)
}

// ---------------------------------------------------------------------------
// base64（标准字母表 + 填充，容忍空白；不引入额外依赖）
// ---------------------------------------------------------------------------

const B64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        out.push(B64_ALPHABET[(b[0] >> 2) as usize] as char);
        out.push(B64_ALPHABET[(((b[0] & 0x03) << 4) | (b[1] >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64_ALPHABET[(((b[1] & 0x0f) << 2) | (b[2] >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(B64_ALPHABET[(b[2] & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub(crate) fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    fn value_of(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let cleaned: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    // 最多一个 '=' 尾部填充
    let body_end = cleaned
        .iter()
        .position(|byte| *byte == b'=')
        .unwrap_or(cleaned.len());
    let body = &cleaned[..body_end];
    if cleaned.len() - body.len() > 2 || cleaned[body_end..].iter().any(|b| *b != b'=') {
        return Err("填充字符位置不正确".to_string());
    }
    if body.is_empty() {
        return Ok(Vec::new());
    }
    if body.len() % 4 == 1 {
        return Err("长度不合法".to_string());
    }
    let mut out = Vec::with_capacity(body.len() * 3 / 4);
    for chunk in body.chunks(4) {
        let mut sextets = [0u8; 4];
        for (slot, byte) in sextets.iter_mut().zip(chunk) {
            *slot = value_of(*byte).ok_or_else(|| "包含非法字符".to_string())?;
        }
        out.push((sextets[0] << 2) | (sextets[1] >> 4));
        if chunk.len() > 2 {
            out.push((sextets[1] << 4) | (sextets[2] >> 2));
        }
        if chunk.len() > 3 {
            out.push((sextets[2] << 6) | sextets[3]);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_accounts::{CliIdentity, ToolPath};
    use serde_json::Value;
    use tempfile::TempDir;

    fn roots(temp: &TempDir) -> Roots {
        Roots {
            user_profile: temp.path().join("user"),
            app_data: temp.path().join("appdata"),
        }
    }

    static PATHS: &[ToolPath] = &[
        ToolPath {
            tag: "auth",
            relative: "auth.json",
        },
        ToolPath {
            tag: "config",
            relative: "config.toml",
        },
    ];

    fn store(base: fn(&Roots) -> std::path::PathBuf) -> ToolStore {
        ToolStore {
            key: "codex",
            tab: "Codex",
            display: "Codex CLI",
            cli_names: "codex",
            id_prefix: "Codex",
            fallback_name: "Codex账号",
            base_dir: base,
            paths: PATHS,
            tags: &["auth", "config"],
            preserve_if_absent: &["config"],
            clear_tags: &["auth"],
            launch_commands: &[],
            detect: |_| CliIdentity::default(),
            process_pattern: "codex",
        }
    }

    fn base(roots: &Roots) -> std::path::PathBuf {
        roots.user_profile.join(".codex")
    }

    #[test]
    fn base64_round_trips_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        for data in [&b""[..], b"x", b"xyz", "中文凭据内容".as_bytes(), &[0u8, 255, 128, 7]] {
            let encoded = base64_encode(data);
            assert_eq!(base64_decode(&encoded).unwrap(), data);
        }
        // 容忍换行空白
        assert_eq!(base64_decode("Zm9v\nYmFy").unwrap(), b"foobar");
        assert!(base64_decode("Zm9v!!").is_err());
    }

    #[test]
    fn tool_export_import_round_trip() {
        let temp = TempDir::new().unwrap();
        let root = roots(&temp);
        let tool_store = store(base);
        let auth = tool_store.path_by_tag(&root, "auth");
        fs::create_dir_all(auth.parent().unwrap()).unwrap();
        fs::write(&auth, br#"{"OPENAI_API_KEY":"sk-round-trip"}"#).unwrap();
        let saved = tool_store
            .save_current_account(&root, Some("机器A账号"), None)
            .unwrap();

        let export = export_tool_accounts(&tool_store, &root, &[]).unwrap();
        let value: Value = serde_json::from_str(&export).unwrap();
        assert_eq!(value["format"], FORMAT_ID);
        assert_eq!(value["tool"], "codex");
        assert_eq!(value["accounts"].as_array().unwrap().len(), 1);

        // 在“另一台机器”导入：只创建备份，不落地凭据文件
        let temp_b = TempDir::new().unwrap();
        let root_b = roots(&temp_b);
        let tool_store_b = store(base);
        let result = import_tool_text(&tool_store_b, &root_b, &export).unwrap();
        assert_eq!(result.imported, 1);
        assert_eq!(result.skipped, 0);
        assert!(!base(&root_b).join("auth.json").exists());
        let accounts = tool_store_b.list_accounts(&root_b).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].manifest.name, "机器A账号");
        assert_eq!(
            fs::read(accounts[0].directory.join("data").join("auth")).unwrap(),
            br#"{"OPENAI_API_KEY":"sk-round-trip"}"#
        );

        // 指定 id 只导出该账号
        let single = export_tool_accounts(&tool_store, &root, &[saved.manifest.id.clone()]).unwrap();
        let single_value: Value = serde_json::from_str(&single).unwrap();
        assert_eq!(single_value["accounts"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn portable_import_rejects_tool_mismatch() {
        let temp = TempDir::new().unwrap();
        let root = roots(&temp);
        let tool_store = store(base);
        let auth = tool_store.path_by_tag(&root, "auth");
        fs::create_dir_all(auth.parent().unwrap()).unwrap();
        fs::write(&auth, b"login").unwrap();
        tool_store.save_current_account(&root, Some("Codex账号"), None).unwrap();
        let export = export_tool_accounts(&tool_store, &root, &[]).unwrap();

        let temp_b = TempDir::new().unwrap();
        let root_b = roots(&temp_b);
        let error = import_zcode_text(&root_b, &export).unwrap_err();
        assert!(error.contains("Codex CLI"), "错误应指向所属工具: {error}");

        // ZCode 导出导入到 CLI 工具同样报错
        let zcode_export = format!(
            r#"{{"format":"{FORMAT_ID}","version":1,"tool":"zcode","accounts":[],"exported_at":1}}"#
        );
        let error = import_tool_text(&tool_store, &root, &zcode_export).unwrap_err();
        assert!(error.contains("ZCode"), "错误应指向所属工具: {error}");
    }

    #[test]
    fn zcode_export_import_round_trip_with_directory_items() {
        let temp = TempDir::new().unwrap();
        let root = roots(&temp);
        let credentials = root.resolve(crate::candidate_by_tag("credentials"));
        fs::create_dir_all(credentials.parent().unwrap()).unwrap();
        fs::write(&credentials, b"zcode-login").unwrap();
        // 目录类项目：Local Storage 下的多文件
        let storage = root
            .resolve(crate::candidate_by_tag("session_storage"))
            .join("leveldb");
        fs::create_dir_all(&storage).unwrap();
        fs::write(storage.join("000003.log"), b"level-data").unwrap();
        fs::write(storage.join("CURRENT"), b"MANIFEST-000001").unwrap();
        accounts::save_current_account(&root, Some("ZCode 主号"), None)
            .unwrap();

        let export = export_zcode_accounts(&root, &[]).unwrap();
        let value: Value = serde_json::from_str(&export).unwrap();
        assert_eq!(value["tool"], "zcode");
        // 目录项展开为多个相对路径文件（session_full 是整棵 session 目录树）
        let storage_item = value["accounts"][0]["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["tag"] == "session_full")
            .expect("目录类项目应纳入导出");
        assert!(storage_item["files"].as_object().unwrap().len() >= 2);

        // 另一台机器导入并切换，验证目录树完整还原
        let temp_b = TempDir::new().unwrap();
        let root_b = roots(&temp_b);
        let result = import_zcode_text(&root_b, &export).unwrap();
        assert_eq!(result.imported, 1);
        let profiles = accounts::list_accounts(&root_b).unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].manifest.name, "ZCode 主号");
        // 导入只创建备份；切换后目录树与单文件标签都应完整还原
        accounts::switch_account(&root_b, &profiles[0]).unwrap();
        assert_eq!(
            fs::read(root_b.resolve(crate::candidate_by_tag("credentials"))).unwrap(),
            b"zcode-login"
        );
        let restored = root_b.resolve(crate::candidate_by_tag("session_storage"));
        assert_eq!(
            fs::read(restored.join("leveldb/000003.log")).unwrap(),
            b"level-data"
        );
        assert_eq!(
            fs::read(restored.join("leveldb/CURRENT")).unwrap(),
            b"MANIFEST-000001"
        );

        // 篡改路径越界的导入应被拒绝
        let malicious = export.replace(
            "leveldb/000003.log",
            "../escape.txt",
        );
        assert!(import_zcode_text(&root_b, &malicious).is_err());
    }

    #[test]
    fn native_dispatch_falls_through_to_tool_parser() {
        let temp = TempDir::new().unwrap();
        let root = roots(&temp);
        let tool_store = store(base);
        // codex 原生 auth.json
        let result = import_tool_text(
            &tool_store,
            &root,
            r#"{"OPENAI_API_KEY":"sk-native"}"#,
        )
        .unwrap();
        assert_eq!(result.imported, 1);
        let accounts = tool_store.list_accounts(&root).unwrap();
        assert_eq!(accounts.len(), 1);
        assert!(accounts[0].manifest.name.starts_with("Codex"));
    }

    #[test]
    fn empty_text_is_rejected() {
        let temp = TempDir::new().unwrap();
        let root = roots(&temp);
        let tool_store = store(base);
        assert!(import_tool_text(&tool_store, &root, "  ").is_err());
        assert!(import_zcode_text(&root, "").is_err());
    }
}
