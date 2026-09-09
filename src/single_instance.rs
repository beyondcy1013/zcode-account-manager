//! 单实例保障：首个实例监听本机回环端口，后续实例连接并发送激活请求后退出。

use eframe::egui;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

/// 单实例握手端口。只绑定回环地址，不对外监听，不会触发防火墙提示。
const HANDSHAKE_PORT: u16 = 45_963;
const ACTIVATE: &[u8] = b"activate";

pub enum Instance {
    /// 首个实例。端口被其他程序占用时监听器为空（放弃激活功能，照常运行）。
    First(Option<TcpListener>),
    /// 已有实例在运行，激活请求已发送。
    AlreadyRunning,
}

/// 尝试成为首个实例；若已有实例在运行则通知它把窗口带回前台。
pub fn acquire() -> Instance {
    if request_activation() {
        return Instance::AlreadyRunning;
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, HANDSHAKE_PORT)).ok();
    if listener.is_none() {
        // 端口被占但连接失败，可能是并发启动的竞态，稍候再试一次激活请求
        thread::sleep(Duration::from_millis(250));
        if request_activation() {
            return Instance::AlreadyRunning;
        }
    }
    Instance::First(listener)
}

fn request_activation() -> bool {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, HANDSHAKE_PORT));
    let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(400)) else {
        return false;
    };
    stream.write_all(ACTIVATE).is_ok()
}

/// 在首个实例中持续接收激活请求，把窗口带回前台。
pub fn serve(listener: TcpListener, ctx: egui::Context) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let mut buffer = [0u8; ACTIVATE.len()];
        let _ = stream.read(&mut buffer);
        bring_to_front(&ctx);
    }
}

/// 恢复最小化、置顶并聚焦窗口。
pub fn bring_to_front(ctx: &egui::Context) {
    use egui::{ViewportCommand, WindowLevel};
    ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
    // 短暂置顶再恢复普通层级，配合 Focus 绕过系统焦点防抢夺，确保窗口被带到前台
    ctx.send_viewport_cmd(ViewportCommand::WindowLevel(WindowLevel::AlwaysOnTop));
    ctx.send_viewport_cmd(ViewportCommand::Focus);
    ctx.send_viewport_cmd(ViewportCommand::WindowLevel(WindowLevel::Normal));
}
