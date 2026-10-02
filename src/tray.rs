//! 系统托盘图标。
//! - Windows：Shell_NotifyIconW 注册托盘图标，左键/双击唤起主窗口，右键弹出菜单；
//! - Linux：按 XEmbed 协议嵌入桌面面板的 legacy 托盘区；
//! - 其他平台：无托盘，仅返回错误提示，不影响主程序。

use eframe::egui;
use std::sync::atomic::AtomicBool;

/// 托盘「退出」菜单/按钮请求真正退出；窗口关闭事件据此决定退出还是隐藏。
pub static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// 启动托盘线程；失败只返回错误信息，不影响主程序。
pub fn spawn(ctx: egui::Context) -> Result<(), String> {
    imp::spawn(ctx)
}

#[cfg(target_os = "windows")]
mod imp {
    use super::EXIT_REQUESTED;
    use crate::single_instance;
    use eframe::egui;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        CreateBitmap, CreateDIBSection, DeleteObject, GetDC, ReleaseDC, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
        DestroyMenu, DispatchMessageW, GetCursorPos, GetMessageW, PostMessageW, PostQuitMessage,
        RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, TrackPopupMenu,
        TranslateMessage, HWND_MESSAGE, ICONINFO, MF_STRING, MSG, TPM_BOTTOMALIGN, TPM_RETURNCMD,
        TPM_RIGHTBUTTON, WM_APP, WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP,
        WNDCLASSW,
    };

    const TRAY_ID: u32 = 1;
    /// 托盘回调消息必须取 WM_APP 以上区间，避免与系统消息冲突。
    const TRAY_CALLBACK: u32 = WM_APP + 1;
    const MENU_OPEN: i32 = 1;
    const MENU_EXIT: i32 = 2;

    static CTX: OnceLock<egui::Context> = OnceLock::new();
    static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

    pub fn spawn(ctx: egui::Context) -> Result<(), String> {
        let icon = decode_tray_icon();
        std::thread::Builder::new()
            .name("win32-tray".into())
            .spawn(move || {
                if let Err(error) = unsafe { run(ctx, &icon) } {
                    eprintln!("托盘图标不可用：{error}");
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    unsafe fn run(ctx: egui::Context, icon: &(Vec<u8>, i32, i32)) -> Result<(), String> {
        let _ = CTX.set(ctx);
        let hinstance = GetModuleHandleW(std::ptr::null());
        if hinstance.is_null() {
            return Err("GetModuleHandleW 失败".into());
        }
        let class_name = wide("ZCodeAccountManagerTray");
        let mut class: WNDCLASSW = std::mem::zeroed();
        class.lpfnWndProc = Some(tray_wndproc);
        class.hInstance = hinstance;
        class.lpszClassName = class_name.as_ptr();
        if RegisterClassW(&class) == 0 {
            return Err("RegisterClassW 失败".into());
        }
        // HWND_MESSAGE：消息专用窗口，只为接收托盘回调，不显示
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return Err("CreateWindowExW 失败".into());
        }
        // explorer 重启后托盘区重建，系统会广播 TaskbarCreated，记下消息号以便补挂图标
        TASKBAR_CREATED.store(
            RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            Ordering::Relaxed,
        );

        if unsafe { add_tray_icon(hwnd, icon) } == 0 {
            return Err("Shell_NotifyIconW 添加托盘图标失败".into());
        }

        let mut msg: MSG = std::mem::zeroed();
        while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        unsafe { remove_tray_icon(hwnd) };
        Ok(())
    }

    /// 返回 1 表示添加成功。Windows 托盘不提供退出按钮之外的交互，全靠回调消息。
    unsafe fn add_tray_icon(hwnd: HWND, icon: &(Vec<u8>, i32, i32)) -> i32 {
        let hicon = build_hicon(&icon.0, icon.1, icon.2);
        if hicon.is_null() {
            return 0;
        }
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_ID;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = TRAY_CALLBACK;
        nid.hIcon = hicon;
        fill_utf16(&mut nid.szTip, "ZCode 账户管家");
        unsafe { Shell_NotifyIconW(NIM_ADD, &nid) }
    }

    unsafe fn remove_tray_icon(hwnd: HWND) {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_ID;
        unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
    }

    /// 由 RGBA 像素构造 32bpp HICON：预乘 alpha 后写入 DIB，掩码位图内容不参与合成。
    unsafe fn build_hicon(
        rgba: &[u8],
        width: i32,
        height: i32,
    ) -> windows_sys::Win32::UI::WindowsAndMessaging::HICON {
        let mut premultiplied = rgba.to_vec();
        let (pixels, _) = premultiplied.as_chunks_mut::<4>();
        for channel in pixels {
            let alpha = channel[3] as u16;
            for byte in &mut channel[..3] {
                *byte = (*byte as u16 * alpha / 255) as u8;
            }
        }
        let hdc = unsafe { GetDC(std::ptr::null_mut()) };
        let mut pixels: *mut c_void = std::ptr::null_mut();
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                // 负高度 = 自顶向下的行序，与 PNG 解码结果一致
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            },
            bmiColors: [std::mem::zeroed()],
        };
        let color = unsafe {
            CreateDIBSection(
                hdc,
                &bmi,
                DIB_RGB_COLORS,
                &mut pixels,
                std::ptr::null_mut(),
                0,
            )
        };
        if !color.is_null() && !pixels.is_null() {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    premultiplied.as_ptr(),
                    pixels as *mut u8,
                    premultiplied.len(),
                )
            };
        }
        let mask = unsafe { CreateBitmap(width, height, 1, 1, std::ptr::null()) };
        let info = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = unsafe { CreateIconIndirect(&info) };
        if !mask.is_null() {
            unsafe { DeleteObject(mask) };
        }
        if !color.is_null() {
            unsafe { DeleteObject(color) };
        }
        if !hdc.is_null() {
            unsafe { ReleaseDC(std::ptr::null_mut(), hdc) };
        }
        icon
    }

    unsafe extern "system" fn tray_wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == TRAY_CALLBACK {
            // 未启用 NOTIFYICON_VERSION_4 时，lParam 直接是鼠标消息
            match lparam as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => activate(),
                WM_RBUTTONUP => unsafe { show_menu(hwnd) },
                _ => {}
            }
            return 0;
        }
        let taskbar_created = TASKBAR_CREATED.load(Ordering::Relaxed);
        if taskbar_created != 0 && msg == taskbar_created {
            // explorer 重启导致图标丢失，重新挂载（失败也无需打扰用户）
            let icon = decode_tray_icon();
            unsafe { add_tray_icon(hwnd, &icon) };
            return 0;
        }
        if msg == WM_DESTROY {
            PostQuitMessage(0);
            return 0;
        }
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn activate() {
        if let Some(ctx) = CTX.get() {
            single_instance::bring_to_front(ctx);
        }
    }

    unsafe fn show_menu(hwnd: HWND) {
        let mut point: POINT = std::mem::zeroed();
        unsafe { GetCursorPos(&mut point) };
        let menu = unsafe { CreatePopupMenu() };
        if menu.is_null() {
            return;
        }
        unsafe {
            AppendMenuW(
                menu,
                MF_STRING,
                MENU_OPEN as usize,
                wide("打开主窗口").as_ptr(),
            );
            AppendMenuW(menu, MF_STRING, MENU_EXIT as usize, wide("退出").as_ptr());
        }
        // TrackPopupMenu 前必须把菜单窗口设为前台，否则点击菜单外不会关闭菜单
        unsafe { SetForegroundWindow(hwnd) };
        let chosen = unsafe {
            TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
                point.x,
                point.y,
                0,
                hwnd,
                std::ptr::null(),
            )
        };
        unsafe { PostMessageW(hwnd, WM_NULL, 0, 0) };
        unsafe { DestroyMenu(menu) };
        match chosen {
            MENU_OPEN => activate(),
            MENU_EXIT => {
                EXIT_REQUESTED.store(true, Ordering::SeqCst);
                if let Some(ctx) = CTX.get() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            _ => {}
        }
    }

    /// 解码内置 PNG 并缩放到 32×32（托盘常用尺寸）。
    fn decode_tray_icon() -> (Vec<u8>, i32, i32) {
        let rgba = image::load_from_memory(include_bytes!("../assets/icon-tray.png"))
            .expect("内置托盘图标必须是可解码的 PNG")
            .resize_exact(32, 32, image::imageops::FilterType::Triangle)
            .into_rgba8();
        let (width, height) = (rgba.width() as i32, rgba.height() as i32);
        (rgba.into_raw(), width, height)
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 写入以 0 结尾的 UTF-16 定长缓冲（截断且保证终止符）。
    fn fill_utf16(target: &mut [u16], text: &str) {
        let mut chars = text.encode_utf16().take(target.len() - 1);
        for slot in target.iter_mut() {
            *slot = chars.next().unwrap_or(0);
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use eframe::egui;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{
        ChangeWindowAttributesAux, ClientMessageData, ClientMessageEvent, ConnectionExt,
        CreateGCAux, CreateWindowAux, EventMask, ImageFormat, Pixmap, PropMode, Window,
        WindowClass,
    };
    use x11rb::rust_connection::RustConnection;

    /// 待重采样绘制的源图标：RGBA8 像素与其宽高。
    struct SourceIcon {
        rgba: Vec<u8>,
        width: u32,
        height: u32,
    }

    /// XEmbed 托盘线程；面板的托盘区未提供 StatusNotifier watcher，但保留了
    /// legacy XEmbed 托盘管理器：创建一个小窗口按系统托盘协议请求嵌入。
    pub fn spawn(ctx: egui::Context) -> Result<(), String> {
        let image = image::load_from_memory(include_bytes!("../assets/icon-tray.png"))
            .expect("内置托盘图标必须是可解码的 PNG")
            .into_rgba8();
        let icon = SourceIcon {
            width: image.width(),
            height: image.height(),
            rgba: image.into_vec(),
        };
        std::thread::Builder::new()
            .name("xembed-tray".into())
            .spawn(move || {
                if let Err(error) = run_tray(ctx, icon) {
                    eprintln!("托盘图标不可用：{error}");
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn run_tray(ctx: egui::Context, icon: SourceIcon) -> Result<(), String> {
        fn xerr<E: std::fmt::Display>(error: E) -> String {
            error.to_string()
        }
        let (conn, screen_num) = x11rb::connect(None).map_err(xerr)?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;

        // 托盘管理器可能尚未就绪（登录竞争）或暂时无响应，多次尝试后才放弃；
        // 任何一轮失败都必须销毁本轮窗口，绝不留下无名的游离窗口。
        let mut last_error = String::new();
        for _ in 0..5 {
            match dock_to_tray(&conn, screen, root, screen_num) {
                Ok(window) => {
                    return tray_event_loop(&conn, &ctx, root, screen.root_depth, window, &icon)
                }
                Err(error) => {
                    last_error = error;
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        }
        Err(last_error)
    }

    /// 创建托盘图标窗口并请求嵌入；返回“已被托盘收养”的窗口。
    fn dock_to_tray(
        conn: &RustConnection,
        screen: &x11rb::protocol::xproto::Screen,
        root: Window,
        screen_num: usize,
    ) -> Result<Window, String> {
        fn xerr<E: std::fmt::Display>(error: E) -> String {
            error.to_string()
        }
        // 找托盘管理器（面板的 legacy systray）
        let sel_atom = conn
            .intern_atom(false, format!("_NET_SYSTEM_TRAY_S{screen_num}").as_bytes())
            .map_err(xerr)?
            .reply()
            .map_err(xerr)?
            .atom;
        let manager = conn
            .get_selection_owner(sel_atom)
            .map_err(xerr)?
            .reply()
            .map_err(xerr)?
            .owner;
        if manager == x11rb::NONE {
            return Err("面板没有运行 XEmbed 托盘管理器".into());
        }

        let window: Window = conn.generate_id().map_err(xerr)?;
        let aux = CreateWindowAux::new()
            .background_pixel(screen.black_pixel)
            .event_mask(
                EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::STRUCTURE_NOTIFY,
            );
        conn.create_window(
            screen.root_depth,
            window,
            root,
            -1,
            -1,
            24,
            24,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &aux,
        )
        .map_err(xerr)?;

        // XEmbed 协议要求客户端声明 _XEMBED_INFO（版本 0 + XEMBED_MAPPED），
        // 否则托盘管理器会忽略 dock 请求
        let xembed_info = conn
            .intern_atom(false, b"_XEMBED_INFO")
            .map_err(xerr)?
            .reply()
            .map_err(xerr)?
            .atom;
        conn.change_property(
            PropMode::REPLACE,
            window,
            xembed_info,
            xembed_info,
            32,
            2,
            // 两个 32 位值（小端）：version=0、XEMBED_MAPPED=1
            &[0u8, 0, 0, 0, 1, 0, 0, 0],
        )
        .map_err(xerr)?;
        set_window_identity(conn, window)?;
        // 注意：dock 成功前绝不能映射窗口——窗口管理器会立即把已映射的
        // 顶层窗口当普通应用接管（加边框、进任务栏），一旦托盘没有收养
        // 就会残留为任务栏里的“无标题窗口”。收养后由托盘负责映射。
        conn.flush().map_err(xerr)?;

        // 请求嵌入托盘：按 System Tray 规范，消息类型必须是 _NET_SYSTEM_TRAY_OPCODE
        // （不是 selection 原子），data = [服务器时间戳, SYSTEM_TRAY_REQUEST_DOCK, 窗口]。
        // 实测 xfce 的托盘插件对消息类型不符的请求直接忽略。
        let opcode_atom = conn
            .intern_atom(false, b"_NET_SYSTEM_TRAY_OPCODE")
            .map_err(xerr)?
            .reply()
            .map_err(xerr)?
            .atom;
        let timestamp = server_timestamp(conn, root)?;
        let data = ClientMessageData::from([
            timestamp,
            SYSTEM_TRAY_REQUEST_DOCK as u32,
            window,
            0,
            0,
        ]);
        let event = ClientMessageEvent::new(32, manager, opcode_atom, data);
        conn.send_event(false, manager, EventMask::NO_EVENT, event)
            .map_err(xerr)?;
        conn.flush().map_err(xerr)?;

        // 轮询等待收养确认（ReparentNotify 且新父窗口不是根窗口）。
        // 管理器无响应（占着 selection 但不处理 dock，见过 libsystray 挂死的情况）
        // 时超时销毁窗口重试，避免残留无名窗口。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
        while std::time::Instant::now() < deadline {
            while let Some(event) = conn.poll_for_event().map_err(xerr)? {
                if let x11rb::protocol::Event::ReparentNotify(ev) = event {
                    if ev.window == window && ev.parent != root {
                        return Ok(window);
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        conn.destroy_window(window).map_err(xerr)?;
        conn.flush().map_err(xerr)?;
        Err("托盘管理器 4 秒内未收养图标（可能无响应）".into())
    }

    /// 设置 WM_CLASS 与窗口标题：托盘/任务栏据此显示应用名，而不是“无标题”。
    fn set_window_identity(conn: &RustConnection, window: Window) -> Result<(), String> {
        fn xerr<E: std::fmt::Display>(error: E) -> String {
            error.to_string()
        }
        fn atom(conn: &RustConnection, name: &[u8]) -> Result<u32, String> {
            Ok(conn
                .intern_atom(false, name)
                .map_err(|error| error.to_string())?
                .reply()
                .map_err(|error| error.to_string())?
                .atom)
        }
        let wm_class = atom(conn, b"WM_CLASS")?;
        let wm_name = atom(conn, b"WM_NAME")?;
        let net_wm_name = atom(conn, b"_NET_WM_NAME")?;
        let string_type = atom(conn, b"STRING")?;
        let utf8_string = atom(conn, b"UTF8_STRING")?;
        // WM_CLASS = "实例名\0类名\0"，8 位 STRING
        let class = b"zcode-account-manager\0ZCodeAccountManager\0";
        conn.change_property(
            PropMode::REPLACE,
            window,
            wm_class,
            string_type,
            8,
            class.len() as u32,
            class,
        )
        .map_err(xerr)?;
        // WM_NAME 用 ASCII 兜底（Latin-1），_NET_WM_NAME 用 UTF-8 显示中文标题
        let name = b"ZCode Account Manager\0";
        conn.change_property(
            PropMode::REPLACE,
            window,
            wm_name,
            string_type,
            8,
            name.len() as u32,
            name,
        )
        .map_err(xerr)?;
        let mut titled = "ZCode 账户管家".as_bytes().to_vec();
        titled.push(0);
        conn.change_property(
            PropMode::REPLACE,
            window,
            net_wm_name,
            utf8_string,
            8,
            titled.len() as u32,
            &titled,
        )
        .map_err(xerr)?;
        Ok(())
    }

    /// 获取一个有效的 X 服务器时间戳（System Tray 规范要求 dock 消息
    /// data.l[0] 为真实时间戳）。标准技巧：对根窗口监听属性变更并追加
    /// 一个空属性，PropertyNotify 事件里携带服务器时间。
    fn server_timestamp(conn: &RustConnection, root: Window) -> Result<u32, String> {
        fn xerr<E: std::fmt::Display>(error: E) -> String {
            error.to_string()
        }
        let scratch = conn
            .intern_atom(false, b"ZCODE_TRAY_TIMESTAMP")
            .map_err(xerr)?
            .reply()
            .map_err(xerr)?
            .atom;
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(xerr)?;
        // 追加 0 个元素：不改变任何属性内容，只为收到带时间戳的 PropertyNotify
        conn.change_property(PropMode::APPEND, root, scratch, scratch, 32, 0, &[])
            .map_err(xerr)?;
        conn.flush().map_err(xerr)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut timestamp = 0u32;
        while std::time::Instant::now() < deadline {
            while let Some(event) = conn.poll_for_event().map_err(xerr)? {
                if let x11rb::protocol::Event::PropertyNotify(ev) = event {
                    if ev.atom == scratch {
                        timestamp = ev.time;
                    }
                }
            }
            if timestamp != 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // 事件掩码是各客户端的并集，恢复本连接的选择不会影响其他客户端
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
        )
        .map_err(xerr)?;
        Ok(timestamp)
    }

    /// 图标被托盘收养后的事件循环：尺寸变化时重绘图标，点击唤起主窗口；
    /// 图标窗口被托盘销毁或退回根窗口时干净退出线程。
    fn tray_event_loop(
        conn: &RustConnection,
        ctx: &egui::Context,
        root: Window,
        depth: u8,
        window: Window,
        icon: &SourceIcon,
    ) -> Result<(), String> {
        fn xerr<E: std::fmt::Display>(error: E) -> String {
            error.to_string()
        }
        let gc: u32 = conn.generate_id().map_err(xerr)?;
        conn.create_gc(gc, window, &CreateGCAux::new())
            .map_err(xerr)?;
        let mut current_size = 0u32;
        // 收养后立即可按当前几何绘制一次，托盘不一定再发 Resize
        if let Ok(geometry) = conn.get_geometry(window).map_err(xerr)?.reply() {
            let size = geometry.width.min(geometry.height) as u32;
            if size > 0 {
                current_size = size;
                draw_icon(conn, depth, window, gc, icon, size).map_err(xerr)?;
            }
        }
        loop {
            let event = conn.wait_for_event().map_err(xerr)?;
            match event {
                x11rb::protocol::Event::ConfigureNotify(ev) if ev.window == window => {
                    let size = ev.width.min(ev.height) as u32;
                    if size > 0 && size != current_size {
                        current_size = size;
                        draw_icon(conn, depth, window, gc, icon, size).map_err(xerr)?;
                    }
                }
                x11rb::protocol::Event::ButtonPress(ev) if ev.event == window => {
                    crate::single_instance::bring_to_front(ctx);
                }
                // 托盘插件退出/被移除时图标窗口可能被退回根窗口：主动销毁并退出，
                // 避免再次游离成“无标题窗口”
                x11rb::protocol::Event::ReparentNotify(ev)
                    if ev.window == window && ev.parent == root =>
                {
                    let _ = conn.destroy_window(window);
                    let _ = conn.flush();
                    return Ok(());
                }
                // 托盘销毁了图标窗口（面板重启等）：直接结束线程
                x11rb::protocol::Event::DestroyNotify(ev) if ev.window == window => {
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    const SYSTEM_TRAY_REQUEST_DOCK: i32 = 0;

    /// 把图标按目标尺寸重采样并写入窗口背景（24bpp ZPixmap，透明处混合到黑色）。
    fn draw_icon(
        conn: &RustConnection,
        depth: u8,
        window: Window,
        gc: u32,
        icon: &SourceIcon,
        size: u32,
    ) -> Result<(), std::string::String> {
        let mut pixels = Vec::with_capacity((size * size * 4) as usize);
        for y in 0..size {
            let sy = (y * icon.height / size) as usize;
            for x in 0..size {
                let sx = (x * icon.width / size) as usize;
                let i = (sy * icon.width as usize + sx) * 4;
                let a = icon.rgba[i + 3] as u16;
                let r = (icon.rgba[i] as u16 * a / 255) as u8;
                let g = (icon.rgba[i + 1] as u16 * a / 255) as u8;
                let b = (icon.rgba[i + 2] as u16 * a / 255) as u8;
                pixels.push(b);
                pixels.push(g);
                pixels.push(r);
                pixels.push(0);
            }
        }
        let pixmap: Pixmap = conn.generate_id().map_err(|error| error.to_string())?;
        conn.create_pixmap(depth, pixmap, window, size as u16, size as u16)
            .map_err(|error| error.to_string())?;
        conn.put_image(
            ImageFormat::Z_PIXMAP,
            pixmap,
            gc,
            size as u16,
            size as u16,
            0,
            0,
            0,
            24,
            &pixels,
        )
        .map_err(|error| error.to_string())?;
        conn.change_window_attributes(
            window,
            &ChangeWindowAttributesAux::new().background_pixmap(pixmap),
        )
        .map_err(|error| error.to_string())?;
        conn.clear_area(false, window, 0, 0, size as u16, size as u16)
            .map_err(|error| error.to_string())?;
        conn.free_pixmap(pixmap)
            .map_err(|error| error.to_string())?;
        conn.flush().map_err(|error| error.to_string())?;
        Ok(())
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
mod imp {
    use eframe::egui;

    pub fn spawn(_ctx: egui::Context) -> Result<(), String> {
        Err("当前平台暂不支持系统托盘".into())
    }
}
