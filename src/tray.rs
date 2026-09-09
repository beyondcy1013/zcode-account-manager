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
        DispatchMessageW, DestroyMenu, GetMessageW, GetCursorPos, PostMessageW, PostQuitMessage,
        RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, TrackPopupMenu,
        TranslateMessage, HWND_MESSAGE, ICONINFO, MF_STRING, MSG, TPM_BOTTOMALIGN, TPM_RETURNCMD,
        TPM_RIGHTBUTTON, WNDCLASSW, WM_APP, WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL,
        WM_RBUTTONUP,
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
            AppendMenuW(menu, MF_STRING, MENU_OPEN as usize, wide("打开主窗口").as_ptr());
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
    use x11rb::protocol::xproto::{ChangeWindowAttributesAux, ClientMessageEvent, ClientMessageData, ConnectionExt, CreateGCAux, CreateWindowAux, EventMask, ImageFormat, Pixmap, PropMode, Window, WindowClass};
    use x11rb::rust_connection::RustConnection;

    /// XEmbed 托盘线程；面板的托盘区未提供 StatusNotifier watcher，但保留了
    /// legacy XEmbed 托盘管理器：创建一个小窗口按系统托盘协议请求嵌入。
    pub fn spawn(ctx: egui::Context) -> Result<(), String> {
        let icon = image::load_from_memory(include_bytes!("../assets/icon-tray.png"))
            .expect("内置托盘图标必须是可解码的 PNG")
            .into_rgba8();
        let (icon_w, icon_h) = (icon.width(), icon.height());
        std::thread::Builder::new()
            .name("xembed-tray".into())
            .spawn(move || {
                if let Err(error) = run_tray(ctx, icon.into_vec(), icon_w, icon_h) {
                    eprintln!("托盘图标不可用：{error}");
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn run_tray(ctx: egui::Context, icon: Vec<u8>, icon_w: u32, icon_h: u32) -> Result<(), String> {
        fn xerr<E: std::fmt::Display>(error: E) -> String {
            error.to_string()
        }
        let (conn, screen_num) = x11rb::connect(None).map_err(xerr)?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;

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
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::STRUCTURE_NOTIFY);
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
        conn.map_window(window).map_err(xerr)?;
        conn.flush().map_err(xerr)?;

        // 请求嵌入托盘：向管理器发送 SYSTEM_TRAY_REQUEST_DOCK
        // data[0] 时间戳用 0（面板接受），data[2] 为要嵌入的窗口
        let data = ClientMessageData::from([0u32, SYSTEM_TRAY_REQUEST_DOCK as u32, window, 0, 0]);
        let event = ClientMessageEvent::new(32, manager, sel_atom, data);
        conn.send_event(false, manager, EventMask::NO_EVENT, event)
            .map_err(xerr)?;
        conn.flush().map_err(xerr)?;

        // 事件循环：尺寸变化时重绘图标，点击时唤起主窗口
        let gc: u32 = conn.generate_id().map_err(xerr)?;
        conn.create_gc(gc, window, &CreateGCAux::new()).map_err(xerr)?;
        let mut current_size = 0u32;
        loop {
            let event = conn.wait_for_event().map_err(xerr)?;
            match event {
                x11rb::protocol::Event::ConfigureNotify(ev) if ev.window == window => {
                    let size = ev.width.min(ev.height) as u32;
                    if size > 0 && size != current_size {
                        current_size = size;
                        draw_icon(&conn, screen.root_depth, window, gc, &icon, icon_w, icon_h, size)
                            .map_err(xerr)?;
                    }
                }
                x11rb::protocol::Event::ButtonPress(ev) if ev.event == window => {
                    crate::single_instance::bring_to_front(&ctx);
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
        icon: &[u8],
        icon_w: u32,
        icon_h: u32,
        size: u32,
    ) -> Result<(), std::string::String> {
        let mut pixels = Vec::with_capacity((size * size * 4) as usize);
        for y in 0..size {
            let sy = (y * icon_h / size) as usize;
            for x in 0..size {
                let sx = (x * icon_w / size) as usize;
                let i = (sy * icon_w as usize + sx) * 4;
                let a = icon[i + 3] as u16;
                let r = (icon[i] as u16 * a / 255) as u8;
                let g = (icon[i + 1] as u16 * a / 255) as u8;
                let b = (icon[i + 2] as u16 * a / 255) as u8;
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
        conn.change_window_attributes(window, &ChangeWindowAttributesAux::new().background_pixmap(pixmap))
            .map_err(|error| error.to_string())?;
        conn.clear_area(false, window, 0, 0, size as u16, size as u16)
            .map_err(|error| error.to_string())?;
        conn.free_pixmap(pixmap).map_err(|error| error.to_string())?;
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
