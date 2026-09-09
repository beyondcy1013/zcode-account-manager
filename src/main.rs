#![cfg_attr(windows, windows_subsystem = "windows")]

use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::thread;
use std::time::Duration;
use std::{
    env,
    ffi::OsStr,
    fs, io,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, ExitCode},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

mod accounts;
mod auto_send;
mod gemini;
mod gui;
mod identity;
mod single_instance;
mod tray;

#[derive(Clone, Copy)]
enum Root {
    UserProfile,
    AppData,
}

#[derive(Clone, Copy)]
struct Candidate {
    tag: &'static str,
    root: Root,
    relative: &'static str,
}

const CANDIDATES: &[Candidate] = &[
    Candidate {
        tag: "credentials",
        root: Root::UserProfile,
        relative: ".zcode/v2/credentials.json",
    },
    Candidate {
        tag: "plan_cache",
        root: Root::UserProfile,
        relative: ".zcode/v2/coding-plan-cache.json",
    },
    Candidate {
        tag: "telemetry",
        root: Root::UserProfile,
        relative: ".zcode/v2/telemetry-state.json",
    },
    Candidate {
        tag: "auth_dir",
        root: Root::UserProfile,
        relative: ".zcode/auth",
    },
    Candidate {
        tag: "session_cookies",
        root: Root::AppData,
        relative: "ZCode/session/Cookies",
    },
    Candidate {
        tag: "session_storage",
        root: Root::AppData,
        relative: "ZCode/session/Local Storage",
    },
    Candidate {
        tag: "session_indexeddb",
        root: Root::AppData,
        relative: "ZCode/session/IndexedDB",
    },
    Candidate {
        tag: "session_full",
        root: Root::AppData,
        relative: "ZCode/session",
    },
    Candidate {
        tag: "electron_store",
        root: Root::AppData,
        relative: "ZCode/rum-electron-store",
    },
    Candidate {
        tag: "updater_id",
        root: Root::AppData,
        relative: "ZCode/.updaterId",
    },
    Candidate {
        tag: "provider_config",
        root: Root::UserProfile,
        relative: ".zcode/v2/config.json",
    },
    Candidate {
        tag: "app_settings",
        root: Root::UserProfile,
        relative: ".zcode/v2/setting.json",
    },
    Candidate {
        tag: "cli_config",
        root: Root::UserProfile,
        relative: ".zcode/cli/config.json",
    },
];
const SAFE_TAGS: &[&str] = &["plan_cache", "session_cookies", "telemetry"];
const FULL_TAGS: &[&str] = &[
    "credentials",
    "plan_cache",
    "telemetry",
    "auth_dir",
    "session_full",
    "electron_store",
    "updater_id",
    "provider_config",
    "app_settings",
    "cli_config",
];
/// 后期才纳入备份的配置项。旧版本快照里没有它们，恢复时若快照缺失应保留本机
/// 现状而不是删除，否则用旧备份切换会把整机 provider 配置清空。
const PRESERVE_IF_ABSENT_TAGS: &[&str] = &["provider_config", "app_settings", "cli_config"];

#[derive(Default)]
struct AutoSendArgs {
    pinned: usize,
    message: String,
    dry_run: bool,
    at: Option<String>,
    daily: bool,
}

#[derive(Clone)]
struct Roots {
    user_profile: PathBuf,
    app_data: PathBuf,
}

/// Electron 应用数据根目录（ZCode 子目录的上层）。
/// Windows 回退 `%USERPROFILE%\AppData\Roaming`，macOS 为 `~/Library/Application Support`，
/// Linux 为 `$XDG_CONFIG_HOME`（缺省 `~/.config`，ZCode 桌面端实际位于 `~/.config/ZCode`）。
fn default_app_data_root(user_profile: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        user_profile.join("Library").join("Application Support")
    }
    #[cfg(target_os = "windows")]
    {
        user_profile.join("AppData").join("Roaming")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|value| value.is_absolute())
            .unwrap_or_else(|| user_profile.join(".config"))
    }
}

impl Roots {
    fn detect() -> Result<Self, String> {
        let user_profile = env::var_os("USERPROFILE")
            .or_else(|| env::var_os("HOME"))
            .map(PathBuf::from)
            .ok_or("未找到 USERPROFILE 环境变量")?;
        let app_data = env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| default_app_data_root(&user_profile));
        Ok(Self {
            user_profile,
            app_data,
        })
    }
    fn resolve(&self, candidate: Candidate) -> PathBuf {
        let root = match candidate.root {
            Root::UserProfile => &self.user_profile,
            Root::AppData => &self.app_data,
        };
        candidate
            .relative
            .split('/')
            .fold(root.clone(), |path, part| path.join(part))
    }
}

#[derive(Default)]
struct CleanOptions {
    safe: bool,
    no_backup: bool,
    backup_dir: Option<PathBuf>,
}
enum Action {
    Gui { start_hidden: bool },
    InteractiveCli,
    Inspect,
    Whoami,
    Backup(Option<PathBuf>),
    Clean(CleanOptions),
    AutoSend {
        pinned: usize,
        message: String,
        dry_run: bool,
        at: Option<String>,
        daily: bool,
    },
    Help,
    Version,
    CheckUpdate,
}

#[derive(Clone, Copy, PartialEq)]
enum Lang {
    Zh,
    En,
}

fn lang() -> Lang {
    match env::var("ZCODE_LANG")
        .ok()
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("en") | Some("en-us") | Some("english") => Lang::En,
        _ => Lang::Zh,
    }
}

fn tr(zh: &str, en: &str) -> String {
    if lang() == Lang::En {
        en.into()
    } else {
        zh.into()
    }
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Action, String> {
    let mut args = args.into_iter();
    let Some(action) = args.next() else {
        return Ok(Action::Gui { start_hidden: false });
    };
    if ["--help", "-h"].contains(&action.as_str()) {
        return Ok(Action::Help);
    }
    if ["--version", "-V"].contains(&action.as_str()) {
        return Ok(Action::Version);
    }
    if action == "--check-update" {
        return Ok(Action::CheckUpdate);
    }
    if action == "--hidden" {
        // 随桌面自启动时使用：启动后隐藏到系统托盘
        return Ok(Action::Gui { start_hidden: true });
    }
    let mut options = CleanOptions::default();
    let mut auto_send = AutoSendArgs::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--safe" if action == "clean" => options.safe = true,
            "--no-backup" if action == "clean" => options.no_backup = true,
            "--backup-dir" if action == "clean" || action == "backup" => {
                options.backup_dir = Some(PathBuf::from(
                    args.next().ok_or("--backup-dir 缺少目录参数")?,
                ));
            }
            "--pinned" if action == "send" => {
                let value = args.next().ok_or("--pinned 缺少序号参数")?;
                auto_send.pinned = value.parse().map_err(|_| "--pinned 需要正整数序号")?;
            }
            "--message" if action == "send" => {
                auto_send.message = args.next().ok_or("--message 缺少消息内容")?;
            }
            "--dry-run" if action == "send" => auto_send.dry_run = true,
            "--at" if action == "send" => {
                auto_send.at = Some(args.next().ok_or("--at 缺少时间参数（HH:MM）")?);
            }
            "--daily" if action == "send" => auto_send.daily = true,
            _ => return Err(format!("未知参数: {arg}")),
        }
    }
    match action.as_str() {
        "interactive" => Ok(Action::InteractiveCli),
        "inspect" => Ok(Action::Inspect),
        "--whoami" => Ok(Action::Whoami),
        "backup" => Ok(Action::Backup(options.backup_dir)),
        "clean" => Ok(Action::Clean(options)),
        "send" => {
            if auto_send.pinned == 0 {
                return Err("send 命令需要 --pinned 提供置顶会话序号".into());
            }
            if !auto_send.dry_run && auto_send.message.is_empty() {
                return Err("send 命令需要 --message 提供非空消息内容".into());
            }
            if auto_send.daily && auto_send.at.is_none() {
                return Err("--daily 需要与 --at 搭配使用".into());
            }
            Ok(Action::AutoSend {
                pinned: auto_send.pinned,
                message: auto_send.message,
                dry_run: auto_send.dry_run,
                at: auto_send.at,
                daily: auto_send.daily,
            })
        }
        _ => Err(format!("未知命令: {action}")),
    }
}

/// 桌面客户端进程名：Linux 上桌面端可执行名为 ZCode（Windows 分支直接使用 ZCode.exe）。
/// 注意不要用 -f 全命令行匹配，否则会误杀 zcode CLI 与本工具自身。
#[cfg(not(windows))]
const DESKTOP_PROCESS_NAME: &str = "ZCode";

/// 结束 ZCode 前记录的桌面客户端可执行文件路径；关闭后只有它知道该重启哪个程序。
static REMEMBERED_ZCODE_EXE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// 探测正在运行的 ZCode 桌面客户端的真实可执行文件路径。
#[cfg(not(windows))]
fn detect_zcode_desktop_exe() -> Option<PathBuf> {
    let output = Command::new("pgrep")
        .args(["-x", DESKTOP_PROCESS_NAME])
        .output()
        .ok()?;
    for pid in String::from_utf8_lossy(&output.stdout).split_whitespace() {
        let Ok(link) = fs::read_link(format!("/proc/{pid}/exe")) else {
            continue;
        };
        // 进程所属文件被替换时 readlink 会带 " (deleted)" 后缀
        let mut path = link.to_string_lossy().to_string();
        if let Some(real) = path.strip_suffix(" (deleted)") {
            path = real.to_string();
        }
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// Windows 上用 PowerShell 查询 ZCode.exe 的真实路径。
#[cfg(windows)]
fn detect_zcode_desktop_exe() -> Option<PathBuf> {
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "(Get-Process -Name ZCode -ErrorAction SilentlyContinue | Select-Object -First 1).Path",
        ])
        .output()
        .ok()?;
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    if path.is_file() { Some(path) } else { None }
}

/// 在结束 ZCode 之前调用，否则进程关闭后就无从得知桌面端装在哪里。
fn remember_zcode_desktop_exe() {
    if let Some(path) = detect_zcode_desktop_exe() {
        if let Ok(mut slot) = REMEMBERED_ZCODE_EXE.lock() {
            *slot = Some(path);
        }
    }
}

fn zcode_running() -> bool {
    #[cfg(windows)]
    return Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq ZCode.exe", "/FO", "CSV", "/NH"])
        .output()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .to_ascii_lowercase()
                .contains("zcode.exe")
        })
        .unwrap_or(false);
    #[cfg(not(windows))]
    return Command::new("pgrep")
        .arg("-x")
        .arg(DESKTOP_PROCESS_NAME)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
}

#[derive(Deserialize)]
struct UpdateManifest {
    version: String,
    url: String,
    sha256: Option<String>,
}

const DEFAULT_UPDATE_MANIFEST_URL: &str = "https://github.com/beyondcy1013/zcode-account-manager/releases/latest/download/update-manifest.json";

fn fetch_update() -> Result<Option<UpdateManifest>, String> {
    let url = env::var_os("ZCODE_UPDATE_MANIFEST_URL")
        .unwrap_or_else(|| DEFAULT_UPDATE_MANIFEST_URL.into());
    let output = Command::new("curl")
        .args(["-fsSL", "--max-time", "5"])
        .arg(url)
        .output();
    let output = output.map_err(|error| format!("curl: {error}"))?;
    if !output.status.success() {
        return Err(tr("更新服务器暂时不可用", "update server is unavailable"));
    }
    let manifest: UpdateManifest =
        serde_json::from_slice(&output.stdout).map_err(|error| format!("manifest: {error}"))?;
    let current = Version::parse(env!("CARGO_PKG_VERSION")).map_err(|error| error.to_string())?;
    let remote = Version::parse(&manifest.version).map_err(|error| error.to_string())?;
    Ok((remote > current).then_some(manifest))
}

fn check_update() {
    match fetch_update() {
        Ok(Some(update)) => println!(
            "{}",
            tr(
                &format!(
                    "[更新] 发现新版本 {}，当前版本 {}。",
                    update.version,
                    env!("CARGO_PKG_VERSION")
                ),
                &format!(
                    "[Update] Version {} is available (current {}).",
                    update.version,
                    env!("CARGO_PKG_VERSION")
                )
            )
        ),
        Ok(None) => println!(
            "{}",
            tr(
                "[更新] 当前已是最新版本。",
                "[Update] You are using the latest version."
            )
        ),
        Err(error) => println!(
            "{}: {error}",
            tr("[更新] 检查失败", "[Update] Check failed")
        ),
    }
}

fn install_update(update: &UpdateManifest) -> Result<(), String> {
    let current = env::current_exe().map_err(|error| error.to_string())?;
    let next = current.with_extension("exe.new");
    println!(
        "{}",
        tr(
            "[更新] 正在下载新版本...",
            "[Update] Downloading new version..."
        )
    );
    let status = Command::new("curl")
        .args(["-fL", "--retry", "2", "-o"])
        .arg(&next)
        .arg(&update.url)
        .status()
        .map_err(|error| error.to_string())?;
    if !status.success() {
        return Err(tr("下载更新失败", "update download failed"));
    }
    if let Some(expected) = &update.sha256 {
        // 直接用内置 sha2 计算，避免依赖平台专用的 certutil/shasum
        let bytes = fs::read(&next).map_err(|error| error.to_string())?;
        let actual: String = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if !actual.eq_ignore_ascii_case(expected) {
            let _ = fs::remove_file(&next);
            return Err(tr(
                "更新文件校验失败",
                "update checksum verification failed",
            ));
        }
    }
    #[cfg(windows)]
    {
        let script = current.with_extension("update.cmd");
        let body = format!("@echo off\r\ntimeout /t 2 /nobreak >nul\r\nmove /y \"{}\" \"{}\" >nul\r\nstart \"\" \"{}\"\r\ndel \"%~f0\"\r\n", next.display(), current.display(), current.display());
        fs::write(&script, body).map_err(|error| error.to_string())?;
        Command::new("cmd")
            .args(["/C", "start", "", &script.to_string_lossy()])
            .spawn()
            .map_err(|error| error.to_string())?;
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&next, fs::Permissions::from_mode(0o755))
            .map_err(|error| error.to_string())?;
        // Linux 允许把新文件重命名到正在运行的程序路径上，替换后启动新版本
        fs::rename(&next, &current).map_err(|error| error.to_string())?;
        let mut command = Command::new(&current);
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        use std::os::unix::process::CommandExt;
        let _ = command.process_group(0);
        command
            .spawn()
            .map_err(|error| error.to_string())?;
    }
    println!(
        "{}",
        tr(
            "[更新] 下载和校验完成，即将替换并重启。",
            "[Update] Downloaded and verified. Restarting with the new version."
        )
    );
    Ok(())
}

fn terminate_zcode() -> Result<(), String> {
    remember_zcode_desktop_exe();
    println!("[步骤 2/4] 正在强行结束 ZCode 进程...");
    #[cfg(windows)]
    let result = Command::new("taskkill")
        .args(["/F", "/T", "/IM", "ZCode.exe"])
        .output();
    #[cfg(not(windows))]
    let result = Command::new("pkill")
        .args(["-TERM", "-x", DESKTOP_PROCESS_NAME])
        .output();
    match result {
        Ok(output) if output.status.success() || !zcode_running() => {
            for _ in 0..10 {
                if !zcode_running() {
                    println!("[完成] ZCode 进程已结束。");
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(500));
            }
            #[cfg(not(windows))]
            {
                let _ = Command::new("pkill")
                    .args(["-KILL", "-x", DESKTOP_PROCESS_NAME])
                    .output();
                for _ in 0..6 {
                    if !zcode_running() {
                        println!("[完成] ZCode 进程已结束。");
                        return Ok(());
                    }
                    thread::sleep(Duration::from_millis(500));
                }
            }
            Err("强行结束后仍检测到 ZCode 进程，请手动结束后重试".into())
        }
        Ok(output) => Err(format!(
            "结束 ZCode 失败: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Err(error) => Err(format!("调用进程结束命令失败: {error}")),
    }
}

/// 启动 ZCode 桌面客户端。优先使用环境变量 ZCODE_APP_PATH，其次使用结束前
/// 记录的真实路径，最后探测常见安装位置。确认进程稳定存活后才算成功。
pub fn launch_zcode() -> Result<(), String> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(path) = env::var_os("ZCODE_APP_PATH") {
        candidates.push(PathBuf::from(path));
    }
    if let Ok(slot) = REMEMBERED_ZCODE_EXE.lock() {
        if let Some(path) = slot.clone() {
            candidates.push(path);
        }
    }
    if let Some(path) = detect_zcode_desktop_exe() {
        candidates.push(path);
    }
    #[cfg(windows)]
    {
        if let Some(local) = env::var_os("LOCALAPPDATA") {
            let local = PathBuf::from(local);
            candidates.push(local.join("Programs").join("ZCode").join("ZCode.exe"));
            candidates.push(local.join("ZCode").join("ZCode.exe"));
        }
        for base in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(dir) = env::var_os(base) {
                candidates.push(PathBuf::from(dir).join("ZCode").join("ZCode.exe"));
            }
        }
    }
    #[cfg(not(windows))]
    {
        for path in [
            "/usr/bin/ZCode",
            "/usr/local/bin/ZCode",
            "/opt/ZCode/zcode",
            "/opt/ZCode/ZCode",
        ] {
            candidates.push(PathBuf::from(path));
        }
    }
    let executable = candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or("未找到 ZCode 客户端程序，可设置环境变量 ZCODE_APP_PATH 指向 ZCode 主程序后重试")?;

    // 桌面入口可能带有必需参数（本机 root 下必须 --no-sandbox，否则 Electron
    // 沙盒检查会直接 FATAL），重启时必须沿用与正常双击启动相同的参数
    let extra_args = desktop_entry_args(&executable).unwrap_or_default();
    let mut child = match spawn_zcode(&executable, &extra_args) {
        Ok(child) => child,
        Err(error) => {
            if extra_args.is_empty() && running_as_root() {
                spawn_zcode(&executable, &["--no-sandbox".to_string()])?
            } else {
                return Err(error);
            }
        }
    };
    // 由后台线程回收子进程，避免残留僵尸进程干扰后续 pgrep 检测
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// 启动并确认进程短期稳定存活，避免出现"提示已启动但实际秒退"。
fn spawn_zcode(executable: &Path, args: &[String]) -> Result<Child, String> {
    let mut command = Command::new(executable);
    command.args(args);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        command.creation_flags(DETACHED_PROCESS);
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;
        let _ = command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("启动 ZCode 失败 ({}): {error}", executable.display()))?;
    for _ in 0..12 {
        thread::sleep(Duration::from_millis(250));
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = child.wait();
                return Err(format!(
                    "ZCode 启动后立即退出（{status}），可设置 ZCODE_APP_PATH 指向正确的桌面客户端后重试"
                ));
            }
            Ok(None) => {}
            Err(error) => {
                let _ = child.wait();
                return Err(format!("无法确认 ZCode 启动状态: {error}"));
            }
        }
    }
    Ok(child)
}

/// 在 XDG 应用目录中查找 Exec 指向该可执行文件的 .desktop 桌面入口，
/// 提取其启动参数（跳过可执行文件本身和 %U 等字段代码）。
#[cfg(not(windows))]
fn desktop_entry_args(executable: &Path) -> Option<Vec<String>> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(home) = env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }
    if let Some(data_dirs) = env::var_os("XDG_DATA_DIRS") {
        for dir in env::split_paths(&data_dirs) {
            dirs.push(dir.join("applications"));
        }
    } else {
        dirs.push(PathBuf::from("/usr/local/share/applications"));
        dirs.push(PathBuf::from("/usr/share/applications"));
    }
    let target = fs::canonicalize(executable).unwrap_or_else(|_| executable.to_path_buf());
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("desktop") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Some(exec_line) = text.lines().find_map(|line| line.strip_prefix("Exec=")) else {
                continue;
            };
            let mut tokens = exec_line.split_whitespace();
            let first = tokens.next()?.trim_matches('"');
            let first_path = PathBuf::from(first);
            let first_canon = fs::canonicalize(&first_path).unwrap_or(first_path);
            if first_canon != target {
                continue;
            }
            let args: Vec<String> = tokens
                .filter(|token| !token.starts_with('%'))
                .map(str::to_string)
                .collect();
            return Some(args);
        }
    }
    None
}

/// Windows 没有 .desktop 桌面入口文件，无需沿用启动参数。
#[cfg(windows)]
fn desktop_entry_args(_executable: &Path) -> Option<Vec<String>> {
    None
}

/// 桌面端是否以 root 身份运行（root 下 Electron 必须 --no-sandbox 才能启动）。
#[cfg(not(windows))]
fn running_as_root() -> bool {
    matches!(
        Command::new("id").arg("-u").output(),
        Ok(output) if String::from_utf8_lossy(&output.stdout).trim() == "0"
    )
}

#[cfg(windows)]
fn running_as_root() -> bool {
    false
}

fn count_entries(path: &Path) -> io::Result<u64> {
    let mut count = 0;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        count += 1;
        if entry.file_type()?.is_dir() {
            count += count_entries(&entry.path())?;
        }
    }
    Ok(count)
}

fn inspect(roots: &Roots) -> io::Result<()> {
    println!("============================================================\n  ZCode 本地记录与状态检测\n============================================================");
    println!(
        "[*] 进程状态: {}",
        if zcode_running() {
            "[警告] ZCode 正在运行（请先关闭客户端）"
        } else {
            "[正常] ZCode 未运行"
        }
    );
    println!("\n[+] 检查关键本地记录路径:");
    let mut found = false;
    for candidate in CANDIDATES {
        let path = roots.resolve(*candidate);
        if path.is_file() {
            found = true;
            println!(
                "  [存在] {:20} -> {} ({} bytes)",
                candidate.tag,
                path.display(),
                fs::metadata(&path)?.len()
            );
        } else if path.is_dir() {
            found = true;
            println!(
                "  [存在] {:20} -> {} ({} entries)",
                candidate.tag,
                path.display(),
                count_entries(&path)?
            );
        } else {
            println!("  [缺失] {:20} -> {}", candidate.tag, path.display());
        }
    }
    if !found {
        println!("\n提示: 当前环境下未检测到相关 ZCode 本地缓存或已被清理。");
    }
    println!("============================================================");
    Ok(())
}

fn copy_path(source: &Path, destination: &Path) -> io::Result<()> {
    if source.is_dir() {
        fs::create_dir_all(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_path(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, destination)?;
        println!(
            "  [已备份] {} -> {}",
            source.display(),
            destination.display()
        );
    }
    Ok(())
}

fn candidate_by_tag(tag: &str) -> Candidate {
    *CANDIDATES
        .iter()
        .find(|candidate| candidate.tag == tag)
        .expect("known candidate tag")
}

fn remove_path(path: &Path) -> io::Result<()> {
    if path.is_dir() && !path.is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn backup(roots: &Roots, backup_root: &Path) -> io::Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let target = backup_root.join(format!("zcode_backup_{stamp}"));
    fs::create_dir_all(&target)?;
    println!("[*] 正在备份现有配置到: {}", target.display());
    let mut copied = 0;
    for candidate in CANDIDATES {
        let source = roots.resolve(*candidate);
        if source.exists() {
            copy_path(&source, &target.join(candidate.tag))?;
            copied += 1;
        }
    }
    println!("[完成] 备份完成，已保存 {copied} 个关键项目。");
    Ok(target)
}

fn clean(roots: &Roots, options: CleanOptions) -> Result<(), String> {
    if zcode_running() {
        return Err("检测到 ZCode 进程正在运行，请先完全退出 ZCode 后再执行清理".into());
    }
    if !options.no_backup {
        let root = options
            .backup_dir
            .unwrap_or_else(|| roots.user_profile.join(".zcode").join("reset_backups"));
        backup(roots, &root).map_err(|e| format!("备份失败，已停止清理: {e}"))?;
    }
    println!("\n[*] 开始清理本地登录及权益缓存...");
    let selected = if options.safe { SAFE_TAGS } else { FULL_TAGS };
    let (mut cleaned, mut failures) = (0, 0);
    for tag in selected {
        let candidate = CANDIDATES
            .iter()
            .find(|c| c.tag == *tag)
            .expect("known tag");
        let path = roots.resolve(*candidate);
        if !path.exists() {
            println!("  [跳过] {tag} 不存在");
            continue;
        }
        let entries = collect_entries(&path)
            .map_err(|error| format!("读取待清理路径失败 {}: {error}", path.display()))?;
        let result = if path.is_dir() && !path.is_symlink() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        match result {
            Ok(()) => {
                for entry in entries {
                    println!(
                        "  [{}] {}",
                        if entry.is_dir {
                            "已清除目录"
                        } else {
                            "已清除文件"
                        },
                        entry.path.display()
                    );
                    cleaned += 1;
                }
            }
            Err(error) => {
                eprintln!("  [清理失败] {tag} ({}): {error}", path.display());
                failures += 1;
            }
        }
    }
    println!("\n[步骤 4/4] 清理完成，共清除 {cleaned} 个文件或目录。");
    if failures > 0 {
        Err(format!("有 {failures} 项清理失败"))
    } else {
        Ok(())
    }
}

struct PathEntry {
    path: PathBuf,
    is_dir: bool,
}

fn collect_entries(path: &Path) -> io::Result<Vec<PathEntry>> {
    fn visit(path: &Path, entries: &mut Vec<PathEntry>) -> io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        let is_dir = metadata.is_dir() && !metadata.file_type().is_symlink();
        if is_dir {
            for child in fs::read_dir(path)? {
                visit(&child?.path(), entries)?;
            }
        }
        entries.push(PathEntry {
            path: path.to_path_buf(),
            is_dir,
        });
        Ok(())
    }

    let mut entries = Vec::new();
    visit(path, &mut entries)?;
    Ok(entries)
}

fn read_choice(prompt: &str) -> Result<String, String> {
    print!("{prompt}");
    io::stdout()
        .flush()
        .map_err(|error| format!("输出失败: {error}"))?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .map_err(|error| format!("读取输入失败: {error}"))?;
    Ok(input.trim().to_string())
}

fn interactive() -> Result<(), String> {
    let roots = Roots::detect()?;
    println!("ZCode Account Manager {}", env!("CARGO_PKG_VERSION"));
    {
        match fetch_update() {
            Ok(Some(update)) => {
                println!(
                    "{}",
                    tr(
                        &format!("[更新] 发现新版本 {}。", update.version),
                        &format!("[Update] Version {} is available.", update.version)
                    )
                );
                if matches!(
                    read_choice(&tr(
                        "是否立即安装更新？输入 Y 确认: ",
                        "Install now? Enter Y to confirm: "
                    ))?
                    .to_ascii_lowercase()
                    .as_str(),
                    "y" | "yes"
                ) {
                    install_update(&update)?;
                    return Ok(());
                }
            }
            Ok(None) => println!(
                "{}",
                tr(
                    "[更新] 当前已是最新版本。",
                    "[Update] You are using the latest version."
                )
            ),
            Err(error) => println!(
                "{}: {error}",
                tr("[更新] 检查失败", "[Update] Check failed")
            ),
        }
    }
    println!("[步骤 1/4] 工具已运行。");
    println!("[步骤 2/4] 正在检测 ZCode 进程和本地文件...\n");
    inspect(&roots).map_err(|error| error.to_string())?;

    if zcode_running() {
        println!("\n[警告] 检测到 ZCode 正在运行。");
        println!("强行结束 ZCode 后继续清理可能导致未保存内容丢失。");
        let choice = read_choice(&tr(
            "是否强行结束并清理？输入 Y 确认，其他键取消: ",
            "Force-close ZCode and continue? Enter Y to confirm, anything else cancels: ",
        ))?;
        if !matches!(choice.to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("\n[退出] 已取消，未结束进程，未清理任何文件。");
            return Ok(());
        }
        terminate_zcode()?;
    }

    println!("\n请选择操作:");
    println!("  1 - 完整清理（自动备份）");
    println!("  2 - 安全清理（自动备份，保留登录凭据）");
    println!("  0 - 退出，不做修改");
    match read_choice("请输入 0、1 或 2，然后按回车: ")?.as_str() {
        "1" => {
            println!("\n[步骤 3/4] 开始完整清理。");
            clean(&roots, CleanOptions::default())
        }
        "2" => {
            println!("\n[步骤 3/4] 开始安全清理。");
            clean(
                &roots,
                CleanOptions {
                    safe: true,
                    ..CleanOptions::default()
                },
            )
        }
        _ => {
            println!("\n[退出] 未清理任何文件。");
            Ok(())
        }
    }
}

fn print_help(program: &OsStr) {
    let exe = Path::new(program).display();
    println!("ZCode Account Manager {}\n\nUsage / 用法:\n  {exe}\n  {exe} interactive\n  {exe} inspect\n  {exe} --whoami\n  {exe} backup [--backup-dir DIR]\n  {exe} clean [--safe] [--no-backup] [--backup-dir DIR]\n  {exe} send --pinned N --message \"...\" [--dry-run] [--at HH:MM] [--daily]  (Linux X11)\n  {exe} --check-update\n\nLanguage / 语言: set ZCODE_LANG=en or zh", env!("CARGO_PKG_VERSION"));
}

fn format_wait(seconds: u64) -> String {
    if seconds >= 3600 {
        format!("{}小时{}分", seconds / 3600, seconds % 3600 / 60)
    } else if seconds >= 60 {
        format!("{}分{}秒", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}秒")
    }
}

fn run() -> Result<(), String> {
    let mut raw = env::args_os();
    let program = raw
        .next()
        .unwrap_or_else(|| "zcode-account-manager.exe".into());
    let args = raw
        .map(|arg| {
            arg.into_string()
                .map_err(|_| "命令行参数不是有效文本".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    match parse_args(args)? {
        Action::Gui { start_hidden } => gui::launch(start_hidden)?,
        Action::InteractiveCli => interactive()?,
        Action::Help => print_help(&program),
        Action::Version => println!("zcode-account-manager {}", env!("CARGO_PKG_VERSION")),
        Action::CheckUpdate => check_update(),
        Action::Inspect => inspect(&Roots::detect()?).map_err(|e| e.to_string())?,
        Action::Whoami => {
            let roots = Roots::detect()?;
            let identity = identity::detect(&roots);
            println!("当前账号: {}", identity.describe());
            println!(
                "默认备份名: {}",
                identity
                    .default_name()
                    .unwrap_or_else(|| "（未检测到登录状态）".into())
            );
            let gemini_identity = gemini::detect(&roots);
            println!("Gemini 账号: {}", gemini_identity.describe());
            if zcode_running() {
                println!("ZCode 桌面客户端: 运行中");
            } else {
                println!("ZCode 桌面客户端: 未运行");
            }
        }
        Action::Backup(dir) => {
            let roots = Roots::detect()?;
            let root =
                dir.unwrap_or_else(|| roots.user_profile.join(".zcode").join("reset_backups"));
            backup(&roots, &root).map_err(|e| e.to_string())?;
        }
        Action::Clean(options) => clean(&Roots::detect()?, options)?,
        Action::AutoSend {
            pinned,
            message,
            dry_run,
            at,
            daily,
        } => {
            // 定时模式：等待到目标时刻再执行（Ctrl+C 可中断）
            if let Some(at_time) = &at {
                let wait = auto_send::seconds_until(at_time, daily)?;
                if daily {
                    println!("[定时] 将在每天 {} 自动发送（首次 {} 后），Ctrl+C 取消。", at_time, format_wait(wait));
                } else {
                    println!("[定时] 将在 {} 后自动发送，Ctrl+C 取消。", format_wait(wait));
                }
                let mut remaining = wait;
                while remaining > 0 {
                    let tick = remaining.min(2);
                    thread::sleep(Duration::from_secs(tick));
                    remaining -= tick;
                }
                println!("[定时] 时间到，开始发送...");
            }
            let request = auto_send::AutoSendRequest {
                pinned_index: pinned,
                message,
                steps: if dry_run {
                    auto_send::SendSteps::locate_only()
                } else {
                    auto_send::SendSteps::full()
                },
            };
            let mut progress = |line: &str| eprintln!("[自动发送] {line}");
            auto_send::run(&request, &mut progress)?;
        }
    }
    Ok(())
}

/// windows_subsystem="windows" 的进程不自带控制台；带命令行参数启动时附着父进程
/// 控制台并重定向标准句柄，保证 inspect/backup/clean 等 CLI 子命令可见可用。
/// 双击打开 GUI 时不调用，不会闪现控制台窗口。
#[cfg(windows)]
fn attach_parent_console() {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Console::{
        AttachConsole, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE,
    };
    if env::args_os().len() <= 1 {
        return;
    }
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        // std 按需在首次使用时读取 STD_*_HANDLE；mem::forget 让句柄存活到进程结束
        if let Ok(output) = fs::OpenOptions::new().write(true).open("CONOUT$") {
            let handle = output.as_raw_handle();
            std::mem::forget(output);
            SetStdHandle(STD_OUTPUT_HANDLE, handle);
            SetStdHandle(STD_ERROR_HANDLE, handle);
        }
        if let Ok(input) = fs::OpenOptions::new().read(true).open("CONIN$") {
            let handle = input.as_raw_handle();
            std::mem::forget(input);
            SetStdHandle(STD_INPUT_HANDLE, handle);
        }
    }
}

#[cfg(not(windows))]
fn attach_parent_console() {}

fn main() -> ExitCode {
    attach_parent_console();
    let result = match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("错误: {error}");
            ExitCode::FAILURE
        }
    };
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn no_arguments_enters_gui_mode() {
        assert!(matches!(
            parse_args(Vec::new()).unwrap(),
            Action::Gui { start_hidden: false }
        ));
        assert!(matches!(
            parse_args(vec!["--hidden".to_string()]).unwrap(),
            Action::Gui { start_hidden: true }
        ));
    }

    #[test]
    fn clean_defaults_to_protected_mode() {
        let options = CleanOptions::default();
        assert!(!options.safe);
        assert!(!options.no_backup);
    }

    #[test]
    fn parses_safe_clean() {
        match parse_args(["clean", "--safe", "--no-backup"].map(String::from)).unwrap() {
            Action::Clean(o) => {
                assert!(o.safe);
                assert!(o.no_backup);
            }
            _ => panic!("unexpected action"),
        }
    }
    #[test]
    fn resolves_candidate_path() {
        let roots = Roots {
            user_profile: PathBuf::from(r"C:\Users\tester"),
            app_data: PathBuf::from(r"C:\Users\tester\AppData\Roaming"),
        };
        assert!(roots
            .resolve(CANDIDATES[0])
            .ends_with(Path::new(".zcode").join("v2").join("credentials.json")));
    }
}
