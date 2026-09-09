//! 自动定位 ZCode 桌面窗口，切换到指定置顶会话并发送消息（Linux X11）。
//!
//! 全部经 xdotool 合成输入实现，坐标为在 3440x1413 的 ZCode 主窗口上实测：
//! 1. 按窗口类 `^ZCode$` 找到主窗口（过滤掉 Electron 的辅助小窗口）并激活；
//! 2. 鼠标移到侧栏列表上，连续滚轮上滚，确保会话列表滚动到最顶部；
//! 3. 点击「已置顶」区第 N 行（点击后 ZCode 切换会话，输入框自动获得键盘焦点）；
//! 4. 把消息写入剪贴板后 Ctrl+V 粘贴并回车发送。
//!
//! 粘贴而不是直接键入的原因：xdotool 逐字键入中文时会被输入法拦截进入预编辑
//! 状态，之后第一次回车只会确认候选词，消息并不会发出。

use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// ZCode 主窗口的 WM_CLASS（res_class），锚定正则避免误匹配本工具自身窗口。
const WINDOW_CLASS: &str = "^ZCode$";
/// 主窗口最小尺寸：Electron 会创建 10x10 / 200x200 的隐藏辅助窗口，据此排除。
const MIN_WINDOW_SIZE: f64 = 400.0;

// 以下为窗口相对坐标（像素），来自实机校准：窗口顶部区域（导航 + 分组/项目
// 页签）高度固定，因此置顶行位置不随窗口大小变化。
const SIDEBAR_X: f64 = 200.0; // 侧栏列表行内安全点击位置
const SCROLL_HOVER_Y: f64 = 700.0; // 滚轮悬停点（须落在可滚动列表内）
const FIRST_PINNED_Y: f64 = 329.0; // 「已置顶」第 1 行行中心
const PINNED_ROW_PITCH: f64 = 37.5; // 相邻置顶行的行距

const WHEEL_UP_TIMES: u32 = 40; // 上滚次数，超出即可，多余滚动无副作用
const WHEEL_INTERVAL: Duration = Duration::from_millis(30);

/// 自动发送要执行的步骤，可通过多选自由组合（定位+滚动+悬停始终执行）。
#[derive(Clone, Debug)]
pub struct SendSteps {
    /// 点击目标置顶会话行完成切换。
    pub click_session: bool,
    /// 点击输入框并把消息粘贴进去。
    pub input_message: bool,
    /// 按回车发送。
    pub send_enter: bool,
}

impl SendSteps {
    /// 完整发送流程。
    pub fn full() -> Self {
        Self {
            click_session: true,
            input_message: true,
            send_enter: true,
        }
    }

    /// 仅定位：滚动到顶部并悬停在目标行上。
    pub fn locate_only() -> Self {
        Self {
            click_session: false,
            input_message: false,
            send_enter: false,
        }
    }

    fn any(&self) -> bool {
        self.click_session || self.input_message || self.send_enter
    }
}

#[derive(Clone, Debug)]
pub struct AutoSendRequest {
    /// 「已置顶」区域中的会话序号，从 1 开始。
    pub pinned_index: usize,
    pub message: String,
    pub steps: SendSteps,
}

/// 「已置顶」区第 N 行的窗口相对 y 坐标。
fn pinned_row_y(pinned_index: usize) -> f64 {
    FIRST_PINNED_Y + (pinned_index.max(1) - 1) as f64 * PINNED_ROW_PITCH
}

#[derive(Debug, Clone, Copy)]
struct Window {
    id: i64,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

fn xdotool(args: &[&str]) -> Result<String, String> {
    let output = Command::new("xdotool")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("无法启动 xdotool（请先安装）：{error}"))?;
    if !output.status.success() {
        return Err(format!(
            "xdotool {} 失败: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// 解析 `xdotool getwindowgeometry --shell` 的 KEY=VALUE 输出。
fn parse_geometry_shell(text: &str) -> Option<(f64, f64, f64, f64)> {
    let mut x = None;
    let mut y = None;
    let mut width = None;
    let mut height = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "X" => x = value.trim().parse().ok(),
            "Y" => y = value.trim().parse().ok(),
            "WIDTH" => width = value.trim().parse().ok(),
            "HEIGHT" => height = value.trim().parse().ok(),
            _ => {}
        }
    }
    Some((x?, y?, width?, height?))
}

/// 查找 ZCode 主窗口：匹配类名后取面积最大者，排除辅助小窗口。
#[cfg(target_os = "linux")]
fn find_window() -> Result<Window, String> {
    let output = xdotool(&["search", "--onlyvisible", "--class", WINDOW_CLASS])?;
    let mut best: Option<Window> = None;
    for token in output.split_whitespace() {
        let Ok(id) = token.parse::<i64>() else {
            continue;
        };
        let shell = match xdotool(&["getwindowgeometry", "--shell", token]) {
            Ok(shell) => shell,
            Err(_) => continue,
        };
        let Some((x, y, width, height)) = parse_geometry_shell(&shell) else {
            continue;
        };
        if width < MIN_WINDOW_SIZE || height < MIN_WINDOW_SIZE {
            continue;
        }
        let window = Window {
            id,
            x,
            y,
            width,
            height,
        };
        let area = width * height;
        if best.map(|current| area > current.width * current.height).unwrap_or(true) {
            best = Some(window);
        }
    }
    best.ok_or_else(|| {
        "未找到 ZCode 桌面客户端窗口，请确认 ZCode 已启动且未被最小化到其他工作区".into()
    })
}

fn sleep(duration: Duration) {
    thread::sleep(duration);
}

/// 读取当前剪贴板文本，用于发送后恢复。剪贴板为空或不可读时返回 None。
#[cfg(target_os = "linux")]
fn clipboard_get() -> Option<String> {
    let output = Command::new("xclip")
        .args(["-selection", "clipboard", "-o"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// 写入剪贴板。xclip 会自行 fork 到后台提供选区服务后立即返回。
#[cfg(target_os = "linux")]
fn clipboard_set(text: &str) -> Result<(), String> {
    use std::io::Write;
    let mut child = Command::new("xclip")
        .args(["-selection", "clipboard"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("无法启动 xclip（请先安装）：{error}"))?;
    child
        .stdin
        .take()
        .ok_or("xclip stdin 不可用")?
        .write_all(text.as_bytes())
        .map_err(|error| format!("写入剪贴板失败: {error}"))?;
    // stdin 关闭后 xclip fork 出选区服务进程并退出，等待退出码确认写入成功
    let status = child
        .wait()
        .map_err(|error| format!("等待 xclip 退出失败: {error}"))?;
    if !status.success() {
        return Err("写入剪贴板失败（xclip 非零退出）".into());
    }
    Ok(())
}

/// 执行完整流程。`progress` 逐步汇报进度，供 GUI 实时展示。
pub fn run(request: &AutoSendRequest, progress: &mut dyn FnMut(&str)) -> Result<(), String> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (request, progress);
        return Err("自动发送目前仅支持 Linux X11 桌面环境".into());
    }
    #[cfg(target_os = "linux")]
    {
        run_linux(request, progress)
    }
}

#[cfg(target_os = "linux")]
fn run_linux(request: &AutoSendRequest, progress: &mut dyn FnMut(&str)) -> Result<(), String> {
    let pinned_index = request.pinned_index.max(1);
    if request.steps.input_message && request.message.trim().is_empty() {
        return Err("勾选了「粘贴输入」但消息内容为空".into());
    }
    progress("查找并激活 ZCode 主窗口...");
    let window = find_window()?;
    xdotool(&["windowactivate", "--sync", &window.id.to_string()])?;
    sleep(Duration::from_millis(400));

    progress("滚动会话列表到顶部...");
    let hover_x = window.x + SIDEBAR_X;
    let hover_y = window.y + SCROLL_HOVER_Y;
    xdotool(&["mousemove", &hover_x.to_string(), &hover_y.to_string()])?;
    sleep(Duration::from_millis(150));
    for _ in 0..WHEEL_UP_TIMES {
        xdotool(&["click", "4"])?;
        sleep(WHEEL_INTERVAL);
    }
    sleep(Duration::from_millis(500));

    let row_y = window.y + pinned_row_y(pinned_index);
    xdotool(&["mousemove", &hover_x.to_string(), &row_y.to_string()])?;
    if !request.steps.any() {
        progress(&format!(
            "定位测试完成：已滚动到顶部，鼠标悬停在置顶会话 {pinned_index} 行上（未点击）"
        ));
        return Ok(());
    }

    if request.steps.click_session {
        progress(&format!("点击置顶会话 {pinned_index}，等待输入框聚焦..."));
        sleep(Duration::from_millis(150));
        xdotool(&["click", "1"])?;
        // 切换会话会触发聊天视图重挂载与输入框自动聚焦，留足渲染时间
        sleep(Duration::from_millis(1400));
    }

    if request.steps.input_message {
        // 点击已激活的会话行不会把焦点移入输入框（只有切换会话时才会自动聚焦），
        // 因此统一再点一次输入框文本区域。会话视图下输入框固定在聊天列底部，
        // 实测只有框内左侧的文本区响应点击聚焦（光标形状为 I 形），
        // (1250, 底部往上 118) 覆盖占位文字所在行。
        let input_x = window.x + 1250.0;
        let input_y = window.y + window.height - 118.0;
        xdotool(&["mousemove", &input_x.to_string(), &input_y.to_string()])?;
        sleep(Duration::from_millis(150));
        xdotool(&["click", "1"])?;
        sleep(Duration::from_millis(600));

        progress("粘贴消息到输入框...");
        let saved_clipboard = clipboard_get();
        clipboard_set(&request.message)?;
        // xclip 父进程在 fork 出选区服务进程后立即退出，剪贴板所有权要稍后
        // 才真正建立；不留间隔直接 Ctrl+V 会粘贴到旧内容甚至空内容。
        sleep(Duration::from_millis(300));
        xdotool(&["key", "--clearmodifiers", "ctrl+v"])?;
        sleep(Duration::from_millis(400));
        // 粘贴完成后即可恢复剪贴板，避免与后续操作竞态
        if let Some(saved) = saved_clipboard {
            if saved != request.message {
                let _ = clipboard_set(&saved);
            }
        }
    }

    if request.steps.send_enter {
        progress("回车发送...");
        xdotool(&["key", "--clearmodifiers", "Return"])?;
        sleep(Duration::from_millis(300));
    }
    progress("完成。");
    Ok(())
}

/// 解析 HH:MM 时刻为 (小时, 分钟)。
fn parse_hhmm(hhmm: &str) -> Result<(u32, u32), String> {
    let (h, m) = hhmm
        .split_once(':')
        .ok_or_else(|| format!("时间格式应为 HH:MM，当前为「{hhmm}」"))?;
    let hour: u32 = h
        .trim()
        .parse()
        .map_err(|_| format!("时间格式应为 HH:MM，当前为「{hhmm}」"))?;
    let minute: u32 = m
        .trim()
        .parse()
        .map_err(|_| format!("时间格式应为 HH:MM，当前为「{hhmm}」"))?;
    if hour > 23 || minute > 59 {
        return Err(format!("时间超出范围：{hhmm}"));
    }
    Ok((hour, minute))
}

/// 计算当前时刻到今天/下一个目标 HH:MM 的秒数。
/// `allow_next_day` 为 false 时目标时刻不能早于当前时刻。
fn day_seconds_diff(now_hms: (u32, u32, u32), target_hm: (u32, u32), allow_next_day: bool) -> Result<i64, String> {
    let now_secs = (now_hms.0 * 3600 + now_hms.1 * 60 + now_hms.2) as i64;
    let target = (target_hm.0 * 3600 + target_hm.1 * 60) as i64;
    let mut diff = target - now_secs;
    if diff < 0 {
        if !allow_next_day {
            return Err(format!(
                "定时时刻 {:02}:{:02} 已过（当前 {:02}:{:02}:{:02}），请选择之后的时刻或勾选每天重复",
                target_hm.0, target_hm.1, now_hms.0, now_hms.1, now_hms.2
            ));
        }
        diff += 86_400;
    }
    Ok(diff)
}

/// 用系统 `date` 读取本地时间，返回距离下一个 HH:MM 的秒数。
/// `daily` 为 true 时目标时刻已过则顺延到明天。
pub fn seconds_until(hhmm: &str, daily: bool) -> Result<u64, String> {
    let target = parse_hhmm(hhmm)?;
    let output = Command::new("date")
        .arg("+%H:%M:%S")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("读取系统时间失败：{error}"))?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let mut parts = text.split(':');
    let (h, m, s) = match (parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(m), Some(s)) => (h, m, s),
        _ => return Err(format!("系统时间输出异常：{text}")),
    };
    let parse = |v: &str| -> Result<u32, String> {
        v.parse()
            .map_err(|_| format!("系统时间输出异常：{text}"))
    };
    let now = (parse(h)?, parse(m)?, parse(s)?);
    Ok(day_seconds_diff(now, target, daily)?.max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_row_offsets_follow_measured_pitch() {
        assert!((pinned_row_y(1) - 329.0).abs() < f64::EPSILON);
        assert!((pinned_row_y(2) - 366.5).abs() < f64::EPSILON);
        assert!((pinned_row_y(4) - 441.5).abs() < f64::EPSILON);
        assert!((pinned_row_y(0) - 329.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parses_xdotool_shell_geometry() {
        let sample = "WINDOW=41943044\nX=0\nY=27\nWIDTH=3440\nHEIGHT=1413\nSCREEN=0\n";
        let (x, y, width, height) = parse_geometry_shell(sample).unwrap();
        assert_eq!((x, y, width, height), (0.0, 27.0, 3440.0, 1413.0));
        assert!(parse_geometry_shell("WINDOW=1\nX=0\nWIDTH=10\n").is_none());
    }

    #[test]
    fn parses_hhmm_and_rejects_bad_input() {
        assert_eq!(parse_hhmm("09:30").unwrap(), (9, 30));
        assert_eq!(parse_hhmm(" 0 : 5 ").unwrap(), (0, 5));
        assert_eq!(parse_hhmm("23:59").unwrap(), (23, 59));
        assert!(parse_hhmm("9点30").is_err());
        assert!(parse_hhmm("24:00").is_err());
        assert!(parse_hhmm("10:60").is_err());
        assert!(parse_hhmm("0930").is_err());
    }

    #[test]
    fn day_diff_handles_past_and_next_day() {
        let now = (10, 0, 0);
        assert_eq!(day_seconds_diff(now, (11, 30), false).unwrap(), 5_400);
        assert_eq!(day_seconds_diff(now, (10, 0), false).unwrap(), 0);
        assert!(day_seconds_diff(now, (9, 0), false).is_err());
        assert_eq!(day_seconds_diff(now, (9, 0), true).unwrap(), 82_800);
        // 目标时刻等于当前时刻：视为立即触发，而不是顺延一天
        assert_eq!(day_seconds_diff(now, (10, 0), true).unwrap(), 0);
    }

    #[test]
    fn steps_flags_work() {
        assert!(SendSteps::full().any());
        assert!(!SendSteps::locate_only().any());
        assert!(SendSteps {
            click_session: false,
            input_message: false,
            send_enter: true
        }
        .any());
    }
}
