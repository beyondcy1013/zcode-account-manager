//! 跨平台剪贴板文本读写，供账号导入（粘贴移植文件）与导出（复制到剪贴板）使用。
//!
//! 实现方式与依赖保持轻量，不引入 GUI 框架之外的 crates：
//! - Linux (X11)：`xclip`（与「自动发送」一致）；
//! - Windows：`powershell` 的 Get-Clipboard / Set-Clipboard，通过 base64
//!   中转避免编码与引号问题；子进程一律走 `silent_command` 防止弹出控制台窗口；
//! - macOS：`pbpaste` / `pbcopy`。

use std::io::Write;
use std::process::Stdio;

/// 读取当前剪贴板文本；剪贴板为空返回空串，无法访问时报错。
pub fn read_text() -> Result<String, String> {
    #[cfg(target_os = "linux")]
    fn platform_read() -> Result<String, String> {
        let output = crate::silent_command("xclip")
            .args(["-selection", "clipboard", "-o"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|error| format!("无法启动 xclip（请先安装）: {error}"))?;
        if !output.status.success() {
            // xclip 在剪贴板为空时也以非零退出，视为空剪贴板
            return Ok(String::new());
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    #[cfg(target_os = "windows")]
    fn platform_read() -> Result<String, String> {
        let output = crate::silent_command("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes((Get-Clipboard -Raw)))",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|error| format!("无法启动 powershell: {error}"))?;
        if !output.status.success() {
            return Err("读取剪贴板失败（powershell 非零退出）".into());
        }
        let encoded = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if encoded.is_empty() {
            return Ok(String::new());
        }
        let bytes = crate::transfer::base64_decode(&encoded)
            .map_err(|error| format!("剪贴板内容异常: {error}"))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    #[cfg(target_os = "macos")]
    fn platform_read() -> Result<String, String> {
        let output = crate::silent_command("pbpaste")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|error| format!("无法启动 pbpaste: {error}"))?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    platform_read()
}

/// 把文本写入系统剪贴板。
pub fn write_text(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Err("剪贴板内容不能为空".into());
    }

    #[cfg(target_os = "linux")]
    fn platform_write(text: &str) -> Result<(), String> {
        let mut child = crate::silent_command("xclip")
            .args(["-selection", "clipboard"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("无法启动 xclip（请先安装）: {error}"))?;
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

    #[cfg(target_os = "windows")]
    fn platform_write(text: &str) -> Result<(), String> {
        // base64 中转：避免大文本超出命令行长度限制与引号转义问题
        let encoded = crate::transfer::base64_encode(text.as_bytes());
        let mut child = crate::silent_command("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$b=[Console]::In.ReadToEnd(); Set-Clipboard -Value ([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($b)))",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("无法启动 powershell: {error}"))?;
        child
            .stdin
            .take()
            .ok_or("powershell stdin 不可用")?
            .write_all(encoded.as_bytes())
            .map_err(|error| format!("写入剪贴板失败: {error}"))?;
        let status = child
            .wait()
            .map_err(|error| format!("等待 powershell 退出失败: {error}"))?;
        if !status.success() {
            return Err("写入剪贴板失败（powershell 非零退出）".into());
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn platform_write(text: &str) -> Result<(), String> {
        let mut child = crate::silent_command("pbcopy")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("无法启动 pbcopy: {error}"))?;
        child
            .stdin
            .take()
            .ok_or("pbcopy stdin 不可用")?
            .write_all(text.as_bytes())
            .map_err(|error| format!("写入剪贴板失败: {error}"))?;
        let status = child
            .wait()
            .map_err(|error| format!("等待 pbcopy 退出失败: {error}"))?;
        if !status.success() {
            return Err("写入剪贴板失败（pbcopy 非零退出）".into());
        }
        Ok(())
    }

    platform_write(text)
}
