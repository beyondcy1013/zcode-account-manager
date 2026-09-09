use crate::{
    accounts::{
        active_account, delete_account, list_accounts, open_accounts_folder, save_current_account,
        set_alias, set_phone, switch_account, AccountProfile,
    },
    auto_send::{self, AutoSendRequest, SendSteps},
    clean, gemini, launch_zcode, single_instance, terminate_zcode, tray, zcode_running,
    CleanOptions, Roots,
};
use eframe::egui::{self, Color32, RichText};
use std::{
    fs,
    net::TcpListener,
    sync::{
        atomic::{AtomicBool, Ordering},
        {Arc, Mutex},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::identity::{self, AccountIdentity};

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Accounts,
    Gemini,
    Cleanup,
    AutoSend,
}

enum ConfirmAction {
    Switch(String),
    Save { name: Option<String> },
    Update(String),
    Delete(String),
    GeminiSwitch(String),
    GeminiDelete(String),
    Clean(bool),
    AutoSend {
        request: AutoSendRequest,
        /// Some((HH:MM, 每天重复)) 表示定时发送，None 表示立即发送。
        schedule: Option<(String, bool)>,
    },
}

/// 账户信息编辑窗口的状态：别名与手机号码一起编辑。
struct AccountEditor {
    id: String,
    alias: String,
    phone: String,
}

/// Gemini 账号编辑窗口的状态：仅别名。
struct GeminiEditor {
    id: String,
    alias: String,
}

/// 自动发送后台任务的状态：工作线程写入，UI 每帧读取展示。
#[derive(Default)]
struct AutoSendState {
    running: bool,
    log: Vec<String>,
    finished_ok: Option<bool>,
    /// 当前生效的定时任务描述（如「每天 09:30 → 置顶会话 1」），None 表示未定时。
    scheduled: Option<String>,
}

pub struct ZCodeApp {
    roots: Roots,
    accounts: Vec<AccountProfile>,
    active_id: Option<String>,
    current_identity: AccountIdentity,
    account_name: String,
    auto_restart: bool,
    info_editor: Option<AccountEditor>,
    status: String,
    status_error: bool,
    page: Page,
    confirm: Option<ConfirmAction>,
    auto_send_state: Arc<Mutex<AutoSendState>>,
    pinned_index: usize,
    auto_message: String,
    /// 定位测试的步骤多选项。
    test_click_session: bool,
    test_input: bool,
    test_send: bool,
    /// 定时发送设置。
    schedule_enabled: bool,
    schedule_time: String,
    schedule_daily: bool,
    schedule_cancel: Arc<Mutex<Option<Arc<std::sync::atomic::AtomicBool>>>>,
    /// Gemini 账号状态：备份列表、当前标识与编辑窗口。
    gemini_accounts: Vec<AccountProfile>,
    gemini_active_id: Option<String>,
    gemini_identity: gemini::GeminiIdentity,
    gemini_account_name: String,
    gemini_editor: Option<GeminiEditor>,
    /// 是否检测到正在运行的 Gemini CLI 会话（刷新时更新，不逐帧探测）。
    gemini_cli_running: bool,
}

impl ZCodeApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        roots: Roots,
        start_hidden: bool,
        activation_listener: Option<TcpListener>,
    ) -> Self {
        install_chinese_font(&cc.egui_ctx);
        if start_hidden {
            cc.egui_ctx
                .send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        if let Some(listener) = activation_listener {
            let ctx = cc.egui_ctx.clone();
            std::thread::spawn(move || single_instance::serve(listener, ctx));
        }
        if let Err(error) = tray::spawn(cc.egui_ctx.clone()) {
            eprintln!("{error}");
        }
        let current_identity = identity::detect(&roots);
        let mut app = Self {
            roots,
            accounts: Vec::new(),
            active_id: None,
            current_identity,
            account_name: String::new(),
            auto_restart: true,
            info_editor: None,
            status: "就绪".into(),
            status_error: false,
            page: Page::Accounts,
            confirm: None,
            auto_send_state: Arc::new(Mutex::new(AutoSendState::default())),
            pinned_index: 1,
            auto_message: String::new(),
            test_click_session: true,
            test_input: false,
            test_send: false,
            schedule_enabled: false,
            schedule_time: "09:00".into(),
            schedule_daily: true,
            schedule_cancel: Arc::new(Mutex::new(None)),
            gemini_accounts: Vec::new(),
            gemini_active_id: None,
            gemini_identity: gemini::GeminiIdentity::default(),
            gemini_account_name: String::new(),
            gemini_editor: None,
            gemini_cli_running: false,
        };
        app.refresh();
        if app.account_name.is_empty() {
            if let Some(default_name) = app.current_identity.default_name() {
                app.account_name = default_name;
            }
        }
        if app.gemini_account_name.is_empty() {
            if let Some(default_name) = app.gemini_identity.default_name() {
                app.gemini_account_name = default_name;
            }
        }
        app
    }

    fn refresh(&mut self) {
        match list_accounts(&self.roots) {
            Ok(accounts) => {
                self.accounts = accounts;
                self.active_id = active_account(&self.roots);
            }
            Err(error) => self.set_error(error),
        }
        match gemini::list_accounts(&self.roots) {
            Ok(accounts) => {
                self.gemini_accounts = accounts;
                self.gemini_active_id = gemini::active_account(&self.roots);
            }
            Err(error) => self.set_error(error),
        }
        // Gemini 识别只读本地文件，代价低，随列表一起刷新
        self.gemini_identity = gemini::detect(&self.roots);
        self.gemini_cli_running = gemini::cli_running();
    }

    fn reload_identity(&mut self) {
        self.current_identity = identity::detect(&self.roots);
        self.gemini_identity = gemini::detect(&self.roots);
    }

    /// 当前登录状态对应的既有备份（按凭据指纹匹配）。
    fn matched_profile(&self) -> Option<&AccountProfile> {
        let fingerprint = self.current_identity.fingerprint.as_deref()?;
        self.accounts
            .iter()
            .find(|profile| profile.manifest.fingerprint.as_deref() == Some(fingerprint))
    }

    fn set_ok(&mut self, message: impl Into<String>) {
        self.status = message.into();
        self.status_error = false;
    }

    fn set_error(&mut self, message: impl Into<String>) {
        self.status = message.into();
        self.status_error = true;
    }

    fn profile(&self, id: &str) -> Option<AccountProfile> {
        self.accounts
            .iter()
            .find(|profile| profile.manifest.id == id)
            .cloned()
    }

    /// 备份名：用户输入优先，否则回落到当前账号标识推导的默认名。
    fn pending_save_name(&self) -> Option<String> {
        let trimmed = self.account_name.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    fn save_new(&mut self) {
        let name = self.pending_save_name();
        if zcode_running() {
            self.confirm = Some(ConfirmAction::Save { name });
            return;
        }
        self.do_save(name);
    }

    fn do_save(&mut self, name: Option<String>) -> bool {
        match save_current_account(&self.roots, name.as_deref(), None) {
            Ok(profile) => {
                self.account_name.clear();
                self.reload_identity();
                if let Some(default_name) = self.current_identity.default_name() {
                    self.account_name = default_name;
                }
                self.set_ok(format!("已备份账户：{}", profile.manifest.display_name()));
                self.refresh();
                true
            }
            Err(error) => {
                self.set_error(error);
                false
            }
        }
    }

    fn update_profile(&mut self, id: &str) {
        if zcode_running() {
            self.confirm = Some(ConfirmAction::Update(id.to_string()));
            return;
        }
        self.do_update(id);
    }

    fn do_update(&mut self, id: &str) -> bool {
        let Some(profile) = self.profile(id) else {
            return false;
        };
        match save_current_account(&self.roots, Some(&profile.manifest.name), Some(&profile)) {
            Ok(_) => {
                self.set_ok(format!(
                    "已更新账户备份：{}",
                    profile.manifest.display_name()
                ));
                self.refresh();
                true
            }
            Err(error) => {
                self.set_error(error);
                false
            }
        }
    }

    fn append_status(&mut self, extra: &str) {
        self.status.push_str(extra);
    }

    fn gemini_profile(&self, id: &str) -> Option<AccountProfile> {
        self.gemini_accounts
            .iter()
            .find(|profile| profile.manifest.id == id)
            .cloned()
    }

    /// 当前 Gemini 登录状态对应的既有备份（按 refresh_token 指纹匹配）。
    fn gemini_matched_profile(&self) -> Option<&AccountProfile> {
        let fingerprint = self.gemini_identity.fingerprint.as_deref()?;
        self.gemini_accounts
            .iter()
            .find(|profile| profile.manifest.fingerprint.as_deref() == Some(fingerprint))
    }

    /// Gemini 备份名：用户输入优先，否则回落到当前账号标识推导的默认名。
    fn gemini_pending_save_name(&self) -> Option<String> {
        let trimmed = self.gemini_account_name.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    /// 保存当前 Gemini 登录状态为新备份。只读取本地文件，无需关闭任何程序。
    fn do_gemini_save(&mut self, name: Option<String>) -> bool {
        match gemini::save_current_account(&self.roots, name.as_deref(), None) {
            Ok(profile) => {
                self.gemini_account_name.clear();
                self.refresh();
                if let Some(default_name) = self.gemini_identity.default_name() {
                    self.gemini_account_name = default_name;
                }
                self.set_ok(format!(
                    "已备份 Gemini 账号：{}",
                    profile.manifest.display_name()
                ));
                true
            }
            Err(error) => {
                self.set_error(error);
                false
            }
        }
    }

    fn do_gemini_update(&mut self, id: &str) -> bool {
        let Some(profile) = self.gemini_profile(id) else {
            return false;
        };
        match gemini::save_current_account(
            &self.roots,
            Some(&profile.manifest.name),
            Some(&profile),
        ) {
            Ok(_) => {
                self.set_ok(format!(
                    "已更新 Gemini 账号备份：{}",
                    profile.manifest.display_name()
                ));
                self.refresh();
                true
            }
            Err(error) => {
                self.set_error(error);
                false
            }
        }
    }

    /// 若 ZCode 正在运行则先自动关闭，再执行操作；完成后按勾选自动重启。
    fn run_with_zcode_closed(&mut self, verb: &str, action: impl FnOnce(&mut Self) -> bool) {
        let was_running = zcode_running();
        if was_running {
            if let Err(error) = terminate_zcode() {
                self.set_error(format!("自动关闭 ZCode 失败，已取消{verb}：{error}"));
                return;
            }
        }
        let success = action(self);
        if was_running {
            if success && self.auto_restart {
                match launch_zcode() {
                    Ok(()) => self.append_status("，ZCode 已重新启动"),
                    Err(error) => self.append_status(&format!("（ZCode 未自动启动：{error}）")),
                }
            } else if success {
                self.append_status("，ZCode 已关闭，请手动启动");
            } else {
                self.append_status("；ZCode 已被关闭，需手动重新启动");
            }
        }
    }

    fn perform_confirmed(&mut self, ctx: &egui::Context) {
        let Some(action) = self.confirm.take() else {
            return;
        };
        match action {
            ConfirmAction::Switch(id) => {
                let Some(profile) = self.profile(&id) else {
                    self.set_error("目标账户不存在");
                    return;
                };
                self.run_with_zcode_closed("切换", |me| {
                    match switch_account(&me.roots, &profile) {
                        Ok(()) => {
                            me.refresh();
                            me.reload_identity();
                            me.set_ok(format!("已切换到 {}", profile.manifest.display_name()));
                            true
                        }
                        Err(error) => {
                            me.set_error(error);
                            false
                        }
                    }
                });
            }
            ConfirmAction::Save { name } => {
                self.run_with_zcode_closed("保存", |me| me.do_save(name));
            }
            ConfirmAction::Update(id) => {
                self.run_with_zcode_closed("更新备份", |me| me.do_update(&id));
            }
            ConfirmAction::Delete(id) => {
                let Some(profile) = self.profile(&id) else {
                    return;
                };
                match delete_account(&self.roots, &profile) {
                    Ok(()) => {
                        self.set_ok(format!("已删除备份：{}", profile.manifest.display_name()));
                        self.refresh();
                    }
                    Err(error) => self.set_error(error),
                }
            }
            ConfirmAction::GeminiSwitch(id) => {
                let Some(profile) = self.gemini_profile(&id) else {
                    self.set_error("目标 Gemini 账号不存在");
                    return;
                };
                match gemini::switch_account(&self.roots, &profile) {
                    Ok(()) => {
                        self.refresh();
                        self.set_ok(format!(
                            "已切换到 Gemini 账号 {}；正在运行的 gemini 会话请重启后使用",
                            profile.manifest.display_name()
                        ));
                    }
                    Err(error) => self.set_error(error),
                }
            }
            ConfirmAction::GeminiDelete(id) => {
                let Some(profile) = self.gemini_profile(&id) else {
                    return;
                };
                match gemini::delete_account(&self.roots, &profile) {
                    Ok(()) => {
                        self.set_ok(format!(
                            "已删除 Gemini 备份：{}",
                            profile.manifest.display_name()
                        ));
                        self.refresh();
                    }
                    Err(error) => self.set_error(error),
                }
            }
            ConfirmAction::Clean(safe) => {
                if zcode_running() {
                    self.set_error("请先完全退出 ZCode，再执行清理");
                    return;
                }
                match clean(
                    &self.roots,
                    CleanOptions {
                        safe,
                        ..CleanOptions::default()
                    },
                ) {
                    Ok(()) => self.set_ok(if safe {
                        "安全清理完成，登录凭据已保留"
                    } else {
                        "完整清理完成，操作前备份已保存"
                    }),
                    Err(error) => self.set_error(error),
                }
            }
            ConfirmAction::AutoSend { request, schedule } => match schedule {
                Some((time, daily)) => self.start_schedule(ctx, request, time, daily),
                None => {
                    self.set_ok("自动发送已开始，执行期间请勿操作鼠标和键盘");
                    self.start_auto_send(ctx, request);
                }
            },
        }
    }

    /// 立即在后台线程执行一次发送流程。
    fn start_auto_send(&mut self, ctx: &egui::Context, request: AutoSendRequest) {
        {
            let mut state = self.auto_send_state.lock().unwrap();
            state.running = true;
            state.finished_ok = None;
            state.log.clear();
        }
        let shared = self.auto_send_state.clone();
        let context = ctx.clone();
        spawn_send_worker(shared, context, request);
    }

    /// 设置定时发送：替换已有定时任务，到点后执行完整发送流程。
    fn start_schedule(
        &mut self,
        ctx: &egui::Context,
        request: AutoSendRequest,
        time: String,
        daily: bool,
    ) {
        let first_wait = match auto_send::seconds_until(&time, daily) {
            Ok(wait) => wait,
            Err(error) => {
                self.set_error(error);
                return;
            }
        };
        self.cancel_schedule_task();
        let cancel = Arc::new(AtomicBool::new(false));
        *self.schedule_cancel.lock().unwrap() = Some(cancel.clone());
        let desc = format!(
            "{} {} → 置顶会话 {}",
            if daily { "每天" } else { "单次" },
            time,
            request.pinned_index.max(1)
        );
        {
            let mut state = self.auto_send_state.lock().unwrap();
            state.scheduled = Some(desc.clone());
            state.log.clear();
            state.log.push(format!("定时已设置：{desc}"));
            state.finished_ok = None;
        }
        self.set_ok(format!(
            "定时已设置：{desc}，到点自动发送（期间请勿退出程序）"
        ));
        let shared = self.auto_send_state.clone();
        let context = ctx.clone();
        std::thread::spawn(move || {
            let mut wait = first_wait;
            loop {
                // 小步 sleep 便于及时响应取消
                let mut remaining = wait;
                while remaining > 0 {
                    if cancel.load(Ordering::SeqCst) {
                        return;
                    }
                    let tick = remaining.min(2);
                    std::thread::sleep(Duration::from_secs(tick));
                    remaining -= tick;
                }
                if cancel.load(Ordering::SeqCst) {
                    return;
                }
                {
                    let mut state = shared.lock().unwrap();
                    state.log.clear();
                    state.log.push("定时触发，开始发送...".into());
                    state.running = true;
                    state.finished_ok = None;
                }
                context.request_repaint();
                let result = {
                    let log_shared = shared.clone();
                    let log_context = context.clone();
                    let mut progress = move |text: &str| {
                        log_shared.lock().unwrap().log.push(text.to_string());
                        log_context.request_repaint();
                    };
                    auto_send::run(&request, &mut progress)
                };
                let ok = result.is_ok();
                {
                    let mut state = shared.lock().unwrap();
                    state.log.push(match result {
                        Ok(()) => "完成。".to_string(),
                        Err(error) => format!("失败：{error}"),
                    });
                    state.running = false;
                    state.finished_ok = Some(ok);
                }
                context.request_repaint();
                if !daily {
                    shared.lock().unwrap().scheduled = None;
                    return;
                }
                wait = auto_send::seconds_until(&time, true).unwrap_or(86_400);
            }
        });
    }

    /// 停止当前定时任务的后台线程（不更新界面状态）。
    fn cancel_schedule_task(&mut self) {
        if let Some(flag) = self.schedule_cancel.lock().unwrap().take() {
            flag.store(true, Ordering::SeqCst);
        }
    }

    /// 取消定时发送并更新界面状态。
    fn cancel_schedule(&mut self) {
        self.cancel_schedule_task();
        self.auto_send_state.lock().unwrap().scheduled = None;
        self.set_ok("已取消定时发送");
    }

    fn identity_banner(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("当前账号").strong());
                let has_state = self.current_identity.is_present();
                ui.colored_label(
                    if has_state {
                        Color32::from_rgb(32, 132, 88)
                    } else {
                        Color32::from_rgb(190, 112, 28)
                    },
                    self.current_identity.describe(),
                );
                if let Some(matched) = self.matched_profile() {
                    ui.label(format!(
                        "（与备份「{}」一致）",
                        matched.manifest.display_name()
                    ));
                }
            });
        });
    }

    fn accounts_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("账户备份与切换");
        ui.label(
            "自动识别当前登录账号并命名备份，可在多个账户间任意恢复，切换时自动关闭并重启 ZCode。",
        );
        ui.add_space(8.0);
        self.identity_banner(ui);
        ui.add_space(8.0);

        let default_name = self.current_identity.default_name();
        let can_save = self.pending_save_name().is_some() || default_name.is_some();
        ui.horizontal(|ui| {
            ui.label("账户名称");
            let input = ui.add_sized(
                [260.0, 30.0],
                egui::TextEdit::singleline(&mut self.account_name).hint_text(match &default_name {
                    Some(name) => format!("默认：{name}"),
                    None => "例如：工作账号".into(),
                }),
            );
            let save = ui.add_enabled(can_save, egui::Button::new("保存当前账户"));
            if save.clicked()
                || (input.lost_focus()
                    && ui.input(|input| input.key_pressed(egui::Key::Enter))
                    && can_save)
            {
                self.save_new();
            }
            if ui.button("刷新").clicked() {
                self.refresh();
                self.reload_identity();
                self.set_ok("已刷新账户列表与当前账号");
            }
            if ui.button("打开备份目录").clicked() {
                match open_accounts_folder(&self.roots) {
                    Ok(()) => self.set_ok("已打开账户备份目录"),
                    Err(error) => self.set_error(error),
                }
            }
        });
        ui.add_space(14.0);

        if self.accounts.is_empty() {
            ui.group(|ui| {
                ui.set_min_height(110.0);
                ui.vertical_centered(|ui| {
                    ui.add_space(18.0);
                    ui.label(RichText::new("还没有账户备份").strong());
                    ui.label("点击上方「保存当前账户」创建第一个备份（名称留空会自动使用当前账号标识命名）。");
                    ui.label("创建后列表每一行都会出现「切换」按钮，随时一键换号。");
                });
            });
            return;
        }

        egui::ScrollArea::both().show(ui, |ui| {
            // 账号标识等文本可能很长：限制各文本列宽度并保持单行截断，避免把「操作」按钮挤出可视区
            let identity_max_width = (ui.available_width() * 0.14).max(120.0);
            egui::Grid::new("accounts_table")
                .striped(true)
                .min_col_width(40.0)
                .spacing([10.0, 8.0])
                .show(ui, |ui| {
                    ui.strong("状态");
                    ui.strong("账户名称");
                    ui.strong("自动名称");
                    ui.strong("手机号码");
                    ui.strong("账号标识");
                    ui.strong("保存时间");
                    ui.strong("项目");
                    ui.strong("操作");
                    ui.end_row();

                    let rows = self.accounts.clone();
                    for profile in rows {
                        let id = profile.manifest.id.clone();
                        let is_active = self.active_id.as_deref() == Some(id.as_str());
                        if is_active {
                            ui.colored_label(Color32::from_rgb(32, 132, 88), "当前");
                        } else {
                            ui.label("已备份");
                        }
                        // 每列都只占一行：别名作为账户名称，自动名称拆成独立列
                        let display_name = profile.manifest.display_name().to_string();
                        cell_label(
                            ui,
                            &display_name,
                            125.0,
                            RichText::new(&display_name).strong(),
                        );
                        // 自动名称列始终显示保存时提取的自动名称，与编辑窗口一致
                        let auto_name = profile.manifest.name.clone();
                        cell_label(ui, &auto_name, 105.0, RichText::new(&auto_name));
                        // 手机号码：只读展示，通过「编辑」窗口随别名一起设置
                        let stored_phone = profile.manifest.phone.clone().unwrap_or_default();
                        if stored_phone.is_empty() {
                            cell_label(ui, "—", 88.0, RichText::new("—").weak())
                                .on_hover_text("通过「编辑」设置手机号码");
                        } else {
                            cell_label(
                                ui,
                                &stored_phone,
                                88.0,
                                RichText::new(&stored_phone),
                            );
                        }
                        let identity_text = profile
                            .manifest
                            .identity
                            .clone()
                            .unwrap_or_else(|| "—".into());
                        cell_label(
                            ui,
                            &identity_text,
                            identity_max_width,
                            RichText::new(&identity_text),
                        );
                        let updated = format_timestamp(profile.manifest.updated_at);
                        cell_label(ui, &updated, 90.0, RichText::new(&updated));
                        ui.label(profile.manifest.item_count.to_string());
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(
                                    !is_active,
                                    egui::Button::new(RichText::new("切换").strong()),
                                )
                                .on_hover_text(
                                    "切换到此账号；如 ZCode 正在运行会先自动关闭，完成后可自动重启",
                                )
                                .clicked()
                            {
                                self.confirm = Some(ConfirmAction::Switch(id.clone()));
                            }
                            if ui
                                .button("编辑")
                                .on_hover_text("设置别名与手机号码，便于识别账户")
                                .clicked()
                            {
                                self.info_editor = Some(AccountEditor {
                                    id: id.clone(),
                                    alias: profile.manifest.alias.clone().unwrap_or_default(),
                                    phone: profile.manifest.phone.clone().unwrap_or_default(),
                                });
                            }
                            if ui
                                .add_enabled(is_active, egui::Button::new("更新"))
                                .on_hover_text(
                                    "用当前登录状态更新此备份；仅当前正在使用的账户可以更新",
                                )
                                .clicked()
                            {
                                self.update_profile(&id);
                            }
                            if ui
                                .button(RichText::new("删除").color(Color32::from_rgb(180, 48, 48)))
                                .clicked()
                            {
                                self.confirm = Some(ConfirmAction::Delete(id));
                            }
                        });
                        ui.end_row();
                    }
                });
        });
    }

    fn gemini_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("Gemini 账号备份与切换");
        ui.label(
            "管理 Google Gemini CLI（~/.gemini）的登录账号：备份当前账号，在多个 Gemini 账号间一键切换。切换会整体替换 OAuth 凭据与账号缓存。",
        );
        ui.add_space(8.0);
        if self.gemini_cli_running {
            ui.colored_label(
                Color32::from_rgb(190, 112, 28),
                "检测到 Gemini CLI 正在运行：切换或更新备份前请先退出相关会话，否则旧会话可能把登录凭据回写覆盖。",
            );
        }
        self.gemini_identity_banner(ui);
        ui.add_space(8.0);

        let default_name = self.gemini_identity.default_name();
        let can_save = self.gemini_pending_save_name().is_some() || default_name.is_some();
        ui.horizontal(|ui| {
            ui.label("账户名称");
            let input = ui.add_sized(
                [260.0, 30.0],
                egui::TextEdit::singleline(&mut self.gemini_account_name).hint_text(match &default_name {
                    Some(name) => format!("默认：{name}"),
                    None => "例如：主力 Gmail".into(),
                }),
            );
            let save = ui.add_enabled(can_save, egui::Button::new("保存当前账号"));
            if save.clicked()
                || (input.lost_focus()
                    && ui.input(|input| input.key_pressed(egui::Key::Enter))
                    && can_save)
            {
                let name = self.gemini_pending_save_name();
                self.do_gemini_save(name);
            }
            if ui.button("刷新").clicked() {
                self.refresh();
                self.set_ok("已刷新 Gemini 账号列表与当前账号");
            }
            if ui.button("打开备份目录").clicked() {
                match gemini::open_accounts_folder(&self.roots) {
                    Ok(()) => self.set_ok("已打开 Gemini 备份目录"),
                    Err(error) => self.set_error(error),
                }
            }
        });
        ui.add_space(14.0);

        if self.gemini_accounts.is_empty() {
            ui.group(|ui| {
                ui.set_min_height(110.0);
                ui.vertical_centered(|ui| {
                    ui.add_space(18.0);
                    ui.label(RichText::new("还没有 Gemini 账号备份").strong());
                    ui.label("先用 Gemini CLI 登录一个 Google 账号，再点击上方「保存当前账号」创建备份。");
                    ui.label("创建后列表每一行都会出现「切换」按钮，随时一键换号。");
                });
            });
            return;
        }

        egui::ScrollArea::both().show(ui, |ui| {
            // 邮箱等账号标识可能很长：限制文本列宽度并保持单行截断，避免把「操作」按钮挤出可视区
            let identity_max_width = (ui.available_width() * 0.22).max(140.0);
            egui::Grid::new("gemini_accounts_table")
                .striped(true)
                .min_col_width(40.0)
                .spacing([10.0, 8.0])
                .show(ui, |ui| {
                    ui.strong("状态");
                    ui.strong("账户名称");
                    ui.strong("账号标识");
                    ui.strong("保存时间");
                    ui.strong("项目");
                    ui.strong("操作");
                    ui.end_row();

                    let rows = self.gemini_accounts.clone();
                    for profile in rows {
                        let id = profile.manifest.id.clone();
                        let is_active = self.gemini_active_id.as_deref() == Some(id.as_str());
                        if is_active {
                            ui.colored_label(Color32::from_rgb(32, 132, 88), "当前");
                        } else {
                            ui.label("已备份");
                        }
                        let display_name = profile.manifest.display_name().to_string();
                        cell_label(
                            ui,
                            &display_name,
                            125.0,
                            RichText::new(&display_name).strong(),
                        );
                        let identity_text = profile
                            .manifest
                            .identity
                            .clone()
                            .unwrap_or_else(|| "—".into());
                        // cell_label 自带悬停显示完整内容（邮箱 · 认证方式 · 凭据指纹）
                        cell_label(
                            ui,
                            &identity_text,
                            identity_max_width,
                            RichText::new(&identity_text),
                        );
                        let updated = format_timestamp(profile.manifest.updated_at);
                        cell_label(ui, &updated, 90.0, RichText::new(&updated));
                        ui.label(profile.manifest.item_count.to_string());
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(
                                    !is_active,
                                    egui::Button::new(RichText::new("切换").strong()),
                                )
                                .on_hover_text(
                                    "切换到此 Gemini 账号；当前状态会先自动备份，恢复失败会自动回滚",
                                )
                                .clicked()
                            {
                                self.confirm = Some(ConfirmAction::GeminiSwitch(id.clone()));
                            }
                            if ui
                                .button("编辑")
                                .on_hover_text("设置别名，便于识别账号")
                                .clicked()
                            {
                                self.gemini_editor = Some(GeminiEditor {
                                    id: id.clone(),
                                    alias: profile.manifest.alias.clone().unwrap_or_default(),
                                });
                            }
                            if ui
                                .add_enabled(is_active, egui::Button::new("更新"))
                                .on_hover_text(
                                    "用当前登录状态更新此备份；仅当前正在使用的账号可以更新",
                                )
                                .clicked()
                            {
                                self.do_gemini_update(&id);
                            }
                            if ui
                                .button(RichText::new("删除").color(Color32::from_rgb(180, 48, 48)))
                                .clicked()
                            {
                                self.confirm = Some(ConfirmAction::GeminiDelete(id));
                            }
                        });
                        ui.end_row();
                    }
                });
        });
    }

    fn gemini_identity_banner(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("当前 Gemini 账号").strong());
                let has_state = self.gemini_identity.is_present();
                ui.colored_label(
                    if has_state {
                        Color32::from_rgb(32, 132, 88)
                    } else {
                        Color32::from_rgb(190, 112, 28)
                    },
                    self.gemini_identity.describe(),
                );
                if let Some(matched) = self.gemini_matched_profile() {
                    ui.label(format!(
                        "（与备份「{}」一致）",
                        matched.manifest.display_name()
                    ));
                }
            });
        });
    }

    fn cleanup_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("本地状态清理");
        ui.label("清理前会自动备份。执行操作前请完全退出 ZCode。账户权益仍以服务端校验为准。");
        ui.add_space(16.0);
        ui.group(|ui| {
            ui.set_min_width(520.0);
            ui.heading("安全清理");
            ui.label("保留登录凭据，仅清理套餐缓存、Cookie 和遥测状态。");
            if ui.button("执行安全清理").clicked() {
                self.confirm = Some(ConfirmAction::Clean(true));
            }
        });
        ui.add_space(10.0);
        ui.group(|ui| {
            ui.set_min_width(520.0);
            ui.heading("完整重置");
            ui.label("备份后清除登录凭据、会话、缓存和本地设备状态。");
            if ui
                .button(RichText::new("执行完整重置").color(Color32::from_rgb(180, 48, 48)))
                .clicked()
            {
                self.confirm = Some(ConfirmAction::Clean(false));
            }
        });
    }

    fn confirmation_window(&mut self, ctx: &egui::Context) {
        let Some(action) = self.confirm.as_ref() else {
            return;
        };
        let (title, message, confirm_label) = match action {
            ConfirmAction::Switch(id) => {
                let name = self
                    .profile(id)
                    .map(|profile| profile.manifest.display_name().to_string())
                    .unwrap_or_default();
                let running_hint = if zcode_running() {
                    "检测到 ZCode 正在运行，确认后将自动关闭它再切换。"
                } else {
                    ""
                };
                (
                    "确认切换账户",
                    format!(
                        "将把本地状态切换为「{name}」。当前状态会先自动备份到该账户。{running_hint}"
                    ),
                    "开始切换",
                )
            }
            ConfirmAction::Save { name } => {
                let name = name
                    .as_deref()
                    .map(|name| format!("「{name}」"))
                    .unwrap_or_else(|| {
                        self.current_identity
                            .default_name()
                            .map(|default| format!("「{default}」（自动命名）"))
                            .unwrap_or_else(|| "（自动命名）".into())
                    });
                let running_hint = if zcode_running() {
                    "检测到 ZCode 正在运行，确认后将自动关闭它再保存。"
                } else {
                    ""
                };
                (
                    "确认保存当前账户",
                    format!("将把当前登录状态保存为新备份，名称：{name}。{running_hint}"),
                    "开始保存",
                )
            }
            ConfirmAction::Update(id) => {
                let name = self
                    .profile(id)
                    .map(|profile| profile.manifest.display_name().to_string())
                    .unwrap_or_default();
                let running_hint = if zcode_running() {
                    "检测到 ZCode 正在运行，确认后将自动关闭它再更新。"
                } else {
                    ""
                };
                (
                    "确认更新备份",
                    format!("将用当前登录状态覆盖更新备份「{name}」。{running_hint}"),
                    "开始更新",
                )
            }
            ConfirmAction::Delete(id) => {
                let name = self
                    .profile(id)
                    .map(|profile| profile.manifest.display_name().to_string())
                    .unwrap_or_default();
                (
                    "删除账户备份",
                    format!("确定删除“{name}”的本地备份？此操作不会删除 ZCode 云端账户。"),
                    "确认删除",
                )
            }
            ConfirmAction::GeminiSwitch(id) => {
                let name = self
                    .gemini_profile(id)
                    .map(|profile| profile.manifest.display_name().to_string())
                    .unwrap_or_default();
                let running_hint = if self.gemini_cli_running {
                    "检测到 Gemini CLI 正在运行，切换后请重启相关 gemini 会话，避免旧会话回写登录凭据。"
                } else {
                    ""
                };
                (
                    "确认切换 Gemini 账号",
                    format!("将把 Gemini CLI 本地登录状态切换为「{name}」。当前状态会先自动备份到该账号。{running_hint}"),
                    "开始切换",
                )
            }
            ConfirmAction::GeminiDelete(id) => {
                let name = self
                    .gemini_profile(id)
                    .map(|profile| profile.manifest.display_name().to_string())
                    .unwrap_or_default();
                (
                    "删除 Gemini 账号备份",
                    format!("确定删除“{name}”的 Gemini 本地备份？此操作不会删除 Google 云端账号。"),
                    "确认删除",
                )
            }
            ConfirmAction::Clean(true) => (
                "确认安全清理",
                "请确认 ZCode 已完全退出。系统将先创建备份，再清理缓存。".into(),
                "开始清理",
            ),
            ConfirmAction::Clean(false) => (
                "确认完整重置",
                "请确认 ZCode 已完全退出。系统将先创建备份，再清除本地账户与会话状态。".into(),
                "开始重置",
            ),
            ConfirmAction::AutoSend { request, schedule } => {
                let chars: Vec<char> = request.message.chars().collect();
                let preview: String = chars.iter().take(80).collect();
                let ellipsis = if chars.len() > 80 { "…" } else { "" };
                let timing = match &schedule {
                    Some((time, true)) => format!("将于每天 {time} 自动"),
                    Some((time, false)) => format!("将于今天 {time} 自动"),
                    None => "将立即".to_string(),
                };
                (
                    "确认自动发送",
                    format!(
                        "将自动定位 ZCode 窗口，{timing}点击侧栏「已置顶」区第 {} 个会话，并发送消息「{preview}{ellipsis}」。\n\n执行期间（约数秒）请不要操作鼠标和键盘。",
                        request.pinned_index
                    ),
                    "开始发送",
                )
            }
        };
        let needs_restart_choice = matches!(
            action,
            ConfirmAction::Switch(_) | ConfirmAction::Save { .. } | ConfirmAction::Update(_)
        );
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(460.0);
                ui.label(message);
                if needs_restart_choice {
                    ui.add_space(8.0);
                    ui.checkbox(
                        &mut self.auto_restart,
                        "完成后自动启动 ZCode（无需手动打开）",
                    );
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("取消").clicked() {
                        self.confirm = None;
                    }
                    if ui.button(confirm_label).clicked() {
                        self.perform_confirmed(ctx);
                    }
                });
            });
    }

    fn account_editor_window(&mut self, ctx: &egui::Context) {
        let Some(mut editor) = self.info_editor.take() else {
            return;
        };
        let default_name = self
            .profile(&editor.id)
            .map(|profile| profile.manifest.name.clone())
            .unwrap_or_default();
        let mut confirmed = false;
        let mut cancelled = false;
        egui::Window::new("编辑账户信息")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(380.0);
                ui.label(format!("自动名称：{default_name}"));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label("别名");
                    let input = ui.add_sized(
                        [270.0, 28.0],
                        egui::TextEdit::singleline(&mut editor.alias)
                            .hint_text("例如：工作主号（留空表示清除别名）"),
                    );
                    confirmed =
                        confirmed || (input.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)));
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("手机号码");
                    let input = ui.add_sized(
                        [270.0, 28.0],
                        egui::TextEdit::singleline(&mut editor.phone)
                            .hint_text("选填，留空表示清除手机号码"),
                    );
                    confirmed =
                        confirmed || (input.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)));
                });
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("保存").clicked() {
                        confirmed = true;
                    }
                    if ui.button("取消").clicked() {
                        cancelled = true;
                    }
                });
            });
        if confirmed {
            self.save_account_editor(editor);
        } else if cancelled {
            // 丢弃编辑状态
        } else {
            self.info_editor = Some(editor);
        }
    }

    /// 保存编辑窗口中的别名与手机号码；仅写有变化的字段，留空即清除。
    fn save_account_editor(&mut self, editor: AccountEditor) {
        let alias = editor.alias.trim();
        let phone = editor.phone.trim();
        let mut profile = match self.profile(&editor.id) {
            Some(profile) => profile,
            None => return,
        };
        let mut changed: Vec<String> = Vec::new();
        if alias != profile.manifest.alias.as_deref().unwrap_or_default() {
            match set_alias(&self.roots, &profile, Some(alias)) {
                Ok(updated) => {
                    changed.push(match updated.manifest.alias.as_deref() {
                        Some(value) => format!("别名「{value}」"),
                        None => "清除别名".into(),
                    });
                    profile = updated;
                }
                Err(error) => return self.set_error(error),
            }
        }
        if phone != profile.manifest.phone.as_deref().unwrap_or_default() {
            match set_phone(&self.roots, &profile, Some(phone)) {
                Ok(updated) => {
                    changed.push(match updated.manifest.phone.as_deref() {
                        Some(value) => format!("手机号码 {value}"),
                        None => "清除手机号码".into(),
                    });
                }
                Err(error) => return self.set_error(error),
            }
        }
        if changed.is_empty() {
            self.set_ok("账户信息未变化");
        } else {
            self.set_ok(format!("已保存{}", changed.join("，")));
            self.refresh();
        }
    }

    fn gemini_editor_window(&mut self, ctx: &egui::Context) {
        let Some(mut editor) = self.gemini_editor.take() else {
            return;
        };
        let default_name = self
            .gemini_profile(&editor.id)
            .map(|profile| profile.manifest.name.clone())
            .unwrap_or_default();
        let mut confirmed = false;
        let mut cancelled = false;
        egui::Window::new("编辑 Gemini 账号信息")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(380.0);
                ui.label(format!("自动名称：{default_name}"));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label("别名");
                    let input = ui.add_sized(
                        [270.0, 28.0],
                        egui::TextEdit::singleline(&mut editor.alias)
                            .hint_text("例如：主力 Gmail（留空表示清除别名）"),
                    );
                    confirmed = confirmed
                        || (input.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter)));
                });
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("保存").clicked() {
                        confirmed = true;
                    }
                    if ui.button("取消").clicked() {
                        cancelled = true;
                    }
                });
            });
        if confirmed {
            self.save_gemini_editor(editor);
        } else if cancelled {
            // 丢弃编辑状态
        } else {
            self.gemini_editor = Some(editor);
        }
    }

    /// 保存 Gemini 编辑窗口中的别名；留空即清除。
    fn save_gemini_editor(&mut self, editor: GeminiEditor) {
        let alias = editor.alias.trim();
        let Some(profile) = self.gemini_profile(&editor.id) else {
            return;
        };
        if alias == profile.manifest.alias.as_deref().unwrap_or_default() {
            self.set_ok("账户信息未变化");
            return;
        }
        match set_alias(&self.roots, &profile, Some(alias)) {
            Ok(updated) => {
                match updated.manifest.alias.as_deref() {
                    Some(value) => self.set_ok(format!("已保存别名「{value}」")),
                    None => self.set_ok("已清除别名"),
                };
                self.refresh();
            }
            Err(error) => self.set_error(error),
        }
    }

    fn auto_send_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("自动发送消息");
        ui.label(
            "自动定位 ZCode 窗口，滚动会话列表到顶部，点击「已置顶」区第 N 个会话，粘贴消息并发送。",
        );
        ui.add_space(8.0);

        let (running, scheduled) = {
            let state = self.auto_send_state.lock().unwrap();
            (state.running, state.scheduled.clone())
        };
        let ctx = ui.ctx().clone();

        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label("置顶会话序号");
                ui.add(
                    egui::DragValue::new(&mut self.pinned_index)
                        .range(1..=30)
                        .suffix(" 号"),
                );
                ui.label(
                    RichText::new("（侧栏「已置顶」区域从上往下数，建议先用「执行测试」核对）")
                        .weak(),
                );
            });
            ui.add_space(6.0);
            ui.label("消息内容");
            ui.add(
                egui::TextEdit::multiline(&mut self.auto_message)
                    .hint_text("发送给该会话的消息；发送前会先粘贴到 ZCode 输入框")
                    .desired_rows(4)
                    .desired_width(ui.available_width().min(620.0)),
            );
            ui.add_space(6.0);
            ui.separator();
            ui.label(RichText::new("定位测试（始终包含：激活窗口 + 滚动到顶部 + 悬停目标行）").strong());
            ui.horizontal(|ui| {
                ui.label("测试项目");
                ui.checkbox(&mut self.test_click_session, "点击切换");
                ui.checkbox(&mut self.test_input, "粘贴输入");
                ui.checkbox(&mut self.test_send, "回车发送");
            });
            ui.horizontal(|ui| {
                let steps = SendSteps {
                    click_session: self.test_click_session,
                    input_message: self.test_input,
                    send_enter: self.test_send,
                };
                let can_test = !running && (!steps.input_message || !self.auto_message.trim().is_empty());
                if ui
                    .add_enabled(can_test, egui::Button::new("执行测试"))
                    .on_hover_text(
                        "按勾选的步骤组合执行；全部不勾选 = 仅定位悬停，用于核对序号与位置",
                    )
                    .on_disabled_hover_text(if steps.input_message && self.auto_message.trim().is_empty() {
                        "勾选了「粘贴输入」但消息内容为空"
                    } else {
                        "正在执行中"
                    })
                    .clicked()
                {
                    let request = AutoSendRequest {
                        pinned_index: self.pinned_index.max(1),
                        message: self.auto_message.trim().to_string(),
                        steps,
                    };
                    self.start_auto_send(&ctx, request);
                }
                ui.label(
                    RichText::new("勾选「回车发送」会真实发出消息，请先用测试消息核对").weak(),
                );
            });
            ui.add_space(8.0);
            ui.separator();
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.schedule_enabled, "定时发送");
                ui.label("时间");
                ui.add_sized(
                    [70.0, 24.0],
                    egui::TextEdit::singleline(&mut self.schedule_time).hint_text("HH:MM"),
                );
                ui.checkbox(&mut self.schedule_daily, "每天重复");
                ui.label(RichText::new("（到点自动执行完整发送，期间请勿操作鼠标键盘；退出程序即失效）").weak());
            });
            ui.horizontal(|ui| {
                let button_label = if scheduled.is_some() && self.schedule_enabled {
                    "重新预约..."
                } else if self.schedule_enabled {
                    "预约定时发送..."
                } else {
                    "立即发送..."
                };
                let can_send = !running && !self.auto_message.trim().is_empty();
                if ui
                    .add_enabled(can_send, egui::Button::new(RichText::new(button_label).strong()))
                    .on_hover_text("发送前会弹出确认")
                    .on_disabled_hover_text(if self.auto_message.trim().is_empty() {
                        "请先填写消息内容"
                    } else {
                        "正在执行中"
                    })
                    .clicked()
                {
                    self.confirm = Some(ConfirmAction::AutoSend {
                        request: AutoSendRequest {
                            pinned_index: self.pinned_index.max(1),
                            message: self.auto_message.trim().to_string(),
                            steps: SendSteps::full(),
                        },
                        schedule: self.schedule_enabled.then(|| {
                            (self.schedule_time.trim().to_string(), self.schedule_daily)
                        }),
                    });
                }
                if scheduled.is_some() && ui.button("取消定时").clicked() {
                    self.cancel_schedule();
                }
                if let Some(desc) = &scheduled {
                    ui.label(RichText::new(format!("已预约：{desc}")).color(Color32::from_rgb(32, 132, 88)));
                }
            });
        });

        ui.add_space(10.0);
        let state = self.auto_send_state.lock().unwrap();
        if !state.log.is_empty() {
            ui.group(|ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.strong("执行日志");
                    if running {
                        ui.spinner();
                        ui.label(
                            RichText::new("执行中，请不要移动鼠标或敲键盘...").weak(),
                        );
                    } else if state.finished_ok == Some(false) {
                        ui.colored_label(Color32::from_rgb(190, 48, 48), "失败");
                    }
                });
                egui::ScrollArea::vertical()
                    .max_height(150.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &state.log {
                            ui.label(line);
                        }
                    });
            });
        }
    }
}

/// 在后台线程执行一次发送流程，逐步把进度写入共享状态；立即任务与定时触发共用。
fn spawn_send_worker(
    shared: Arc<Mutex<AutoSendState>>,
    context: egui::Context,
    request: AutoSendRequest,
) {
    let log_shared = shared.clone();
    let log_context = context.clone();
    std::thread::spawn(move || {
        let mut progress = move |text: &str| {
            log_shared.lock().unwrap().log.push(text.to_string());
            log_context.request_repaint();
        };
        let result = auto_send::run(&request, &mut progress);
        let ok = result.is_ok();
        {
            let mut state = shared.lock().unwrap();
            state.log.push(match result {
                Ok(()) => "完成。".to_string(),
                Err(error) => format!("失败：{error}"),
            });
            state.running = false;
            state.finished_ok = Some(ok);
        }
        context.request_repaint();
    });
}

impl eframe::App for ZCodeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // 点击窗口关闭按钮 = 隐藏到系统托盘；仅托盘菜单「退出」会真正退出
        if ctx.input(|input| input.viewport().close_requested())
            && !tray::EXIT_REQUESTED.load(std::sync::atomic::Ordering::SeqCst)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.set_ok("已最小化到系统托盘；点击托盘图标或再次启动程序即可打开");
        }
        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.heading("ZCode 账户管家");
                ui.label(format!("v{}", env!("CARGO_PKG_VERSION")));
                ui.separator();
                if ui
                    .selectable_label(self.page == Page::Accounts, "账户")
                    .clicked()
                {
                    self.page = Page::Accounts;
                }
                if ui
                    .selectable_label(self.page == Page::Gemini, "Gemini 账号")
                    .clicked()
                {
                    self.page = Page::Gemini;
                }
                if ui
                    .selectable_label(self.page == Page::Cleanup, "清理")
                    .clicked()
                {
                    self.page = Page::Cleanup;
                }
                if ui
                    .selectable_label(self.page == Page::AutoSend, "自动发送")
                    .clicked()
                {
                    self.page = Page::AutoSend;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button("退出")
                        .on_hover_text("退出程序（点击窗口 × 只是隐藏到系统托盘）")
                        .clicked()
                    {
                        tray::EXIT_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    let running = zcode_running();
                    ui.colored_label(
                        if running {
                            Color32::from_rgb(190, 112, 28)
                        } else {
                            Color32::from_rgb(32, 132, 88)
                        },
                        if running {
                            "ZCode 运行中"
                        } else {
                            "ZCode 已退出"
                        },
                    );
                });
            });
            ui.add_space(8.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(10.0);
            match self.page {
                Page::Accounts => self.accounts_page(ui),
                Page::Gemini => self.gemini_page(ui),
                Page::Cleanup => self.cleanup_page(ui),
                Page::AutoSend => self.auto_send_page(ui),
            }
        });

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.add_space(5.0);
            ui.colored_label(
                if self.status_error {
                    Color32::from_rgb(190, 48, 48)
                } else {
                    Color32::from_rgb(42, 112, 78)
                },
                &self.status,
            );
            ui.add_space(5.0);
        });
        self.confirmation_window(&ctx);
        self.account_editor_window(&ctx);
        self.gemini_editor_window(&ctx);
    }
}

fn install_chinese_font(ctx: &egui::Context) {
    #[cfg(windows)]
    let candidates = [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\simhei.ttf",
    ];
    #[cfg(not(windows))]
    let candidates = [
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
    ];
    let Some(bytes) = candidates.iter().find_map(|path| fs::read(path).ok()) else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "system_cjk".into(),
        egui::FontData::from_owned(bytes).into(),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, "system_cjk".into());
    }
    ctx.set_fonts(fonts);
}

fn format_timestamp(timestamp: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let elapsed = now.saturating_sub(timestamp);
    match elapsed {
        0..=59 => "刚刚".into(),
        60..=3_599 => format!("{} 分钟前", elapsed / 60),
        3_600..=86_399 => format!("{} 小时前", elapsed / 3_600),
        _ => format!("{} 天前", elapsed / 86_400),
    }
}

/// 表格单元格：始终单行显示，超出列宽截断，悬停可查看完整内容。
fn cell_label(ui: &mut egui::Ui, text: &str, max_width: f32, styled: RichText) -> egui::Response {
    ui.add_sized([max_width, 20.0], egui::Label::new(styled).truncate()).on_hover_text(text)
}

/// 应用图标：基于 ZCode 图标加账户切换徽章，嵌入二进制供窗口/任务栏使用。
fn load_app_icon() -> egui::IconData {
    const ICON_BYTES: &[u8] = include_bytes!("../assets/icon-512.png");
    let rgba = image::load_from_memory(ICON_BYTES)
        .expect("内置图标必须是可解码的 PNG")
        .into_rgba8();
    let (width, height) = rgba.dimensions();
    egui::IconData {
        width,
        height,
        rgba: rgba.into_raw(),
    }
}

pub fn launch(start_hidden: bool) -> Result<(), String> {
    let roots = Roots::detect()?;
    let listener = match single_instance::acquire() {
        single_instance::Instance::AlreadyRunning => {
            // 已有实例在运行：激活请求已发送，本次直接退出
            println!("ZCode 账户管家已在运行，已请求显示已有窗口。");
            return Ok(());
        }
        single_instance::Instance::First(listener) => listener,
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ZCode 账户管家")
            .with_inner_size([900.0, 580.0])
            .with_min_inner_size([760.0, 480.0])
            .with_icon(load_app_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "ZCode 账户管家",
        options,
        Box::new(move |cc| Ok(Box::new(ZCodeApp::new(cc, roots, start_hidden, listener)))),
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_timestamp_is_readable() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(format_timestamp(now), "刚刚");
        assert_eq!(format_timestamp(now - 120), "2 分钟前");
    }
}
