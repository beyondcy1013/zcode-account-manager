use crate::{
    accounts::{
        active_account, delete_account, list_accounts, open_accounts_folder, save_current_account,
        set_alias, set_phone, switch_account, AccountProfile,
    },
    auto_send::{self, AutoSendRequest, SendSteps},
    candidate_by_tag, claude, clean, cli_accounts, clipboard, codebuddy, codex, gemini,
    launch_zcode, single_instance, terminate_zcode, transfer, tray, zcode_running, CleanOptions,
    Roots, FULL_TAGS,
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

use crate::identity::AccountIdentity;

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Accounts,
    GeminiAccounts,
    CodexAccounts,
    ClaudeAccounts,
    CodeBuddyAccounts,
    Cleanup,
    AutoSend,
}

/// CLI 工具账号页展示的工具顺序。
const TOOL_STORES: [&cli_accounts::ToolStore; 4] = [
    &gemini::STORE,
    &codex::STORE,
    &claude::STORE,
    &codebuddy::STORE,
];

enum ConfirmAction {
    Switch(String),
    Save {
        name: Option<String>,
    },
    Update(String),
    Delete(String),
    /// 按 TOOL_STORES 下标定位工具。
    ToolSwitch(usize, String),
    ToolDelete(usize, String),
    /// 清空该工具的登录状态，恢复未登录原始状态。
    ToolClear(usize),
    /// 切换完成后询问是否立即在新终端中启动 CLI 会话。
    ToolRelaunch(usize, String),
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

/// Gemini / Antigravity 登录窗口状态。
#[derive(Default)]
struct GeminiLoginState {
    auth_code: String,
    submitting: bool,
    error: Option<String>,
}

/// CLI 工具账号编辑窗口的状态：仅别名。
struct ToolEditor {
    id: String,
    alias: String,
}

/// 导入导出的目标：ZCode 主账号页或某个 CLI 工具页（按 TOOL_STORES 下标）。
#[derive(Clone, Copy, PartialEq)]
enum TransferTarget {
    ZCode,
    Tool(usize),
}

/// 导入窗口状态：文件路径与粘贴文本二选一。
struct TransferImportState {
    target: TransferTarget,
    path: String,
    text: String,
}

/// 导出窗口状态：打开时即生成移植文件内容。
struct TransferExportState {
    target: TransferTarget,
    /// None = 导出全部账号。
    account_id: Option<String>,
    json: String,
    error: Option<String>,
    save_path: String,
}

/// 后台慢速刷新的结果（进程探测、ZCode CLI 账号识别、CLI 会话探测），
/// 完成后由 UI 线程取回应用，避免点击刷新时界面冻结且无反馈。
struct RefreshOutcome {
    /// 丢弃过期结果：begin_refresh 每次递增，不匹配则不应用。
    tag: u64,
    zcode_running: bool,
    current_identity: AccountIdentity,
    /// 与 TOOL_STORES 顺序一致：(账号标识, 是否有会话运行)。
    tools: Vec<(cli_accounts::CliIdentity, bool)>,
}

/// 一个 CLI 工具在界面中的账号状态：列表、当前标识与编辑窗口。
struct ToolAccounts {
    store: &'static cli_accounts::ToolStore,
    accounts: Vec<AccountProfile>,
    active_id: Option<String>,
    identity: cli_accounts::CliIdentity,
    /// 记录最近一次据以填充默认名称的账号标识；账号未变时保留用户手动修改，仅当检测到新账号才自动覆盖更新。
    last_detected_identity: Option<cli_accounts::CliIdentity>,
    account_name: String,
    editor: Option<ToolEditor>,
    /// 是否检测到正在运行的 CLI 会话（刷新时更新，不逐帧探测）。
    cli_running: bool,
}

impl ToolAccounts {
    fn new(store: &'static cli_accounts::ToolStore) -> Self {
        Self {
            store,
            accounts: Vec::new(),
            active_id: None,
            identity: cli_accounts::CliIdentity::default(),
            last_detected_identity: None,
            account_name: String::new(),
            editor: None,
            cli_running: false,
        }
    }

    /// 当检测到新账号时动态更新账户名称；若账号未变，则保留用户手动修改的内容。
    fn sync_account_name_on_identity_change(&mut self) {
        if !self.identity.is_present() {
            if self.last_detected_identity.is_some() {
                self.last_detected_identity = None;
                self.account_name.clear();
            }
            return;
        }
        if self.last_detected_identity.as_ref() != Some(&self.identity) {
            if let Some(default_name) = self.identity.default_name(self.store.id_prefix) {
                self.account_name = default_name;
            }
            self.last_detected_identity = Some(self.identity.clone());
        }
    }

    fn profile(&self, id: &str) -> Option<AccountProfile> {
        self.accounts
            .iter()
            .find(|profile| profile.manifest.id == id)
            .cloned()
    }

    /// 当前登录状态对应的既有备份（按凭据指纹匹配）。
    fn matched_profile(&self) -> Option<&AccountProfile> {
        let fingerprint = self.identity.fingerprint.as_deref()?;
        self.accounts
            .iter()
            .find(|profile| profile.manifest.fingerprint.as_deref() == Some(fingerprint))
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
    /// 记录最近一次据以填充默认名称的账号标识；账号未变时保留用户手动修改，仅当检测到新账号才自动覆盖更新。
    last_detected_identity: Option<AccountIdentity>,
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
    /// CLI 工具（Gemini / Codex / Claude / CodeBuddy）账号状态，与 TOOL_STORES 顺序一致。
    tools: Vec<ToolAccounts>,
    /// CLI 账号页当前选中的工具下标。
    tool_page: usize,
    /// ZCode 运行状态缓存：由后台线程每 2 秒探测，界面帧只读缓存。
    /// 逐帧同步调用 tasklist 在 Windows 上会反复弹黑框并阻塞 UI。
    zcode_running: Arc<std::sync::atomic::AtomicBool>,
    /// 后台慢速刷新的结果槽与序号；refreshing 为 true 表示结果未回。
    refresh_shared: Arc<Mutex<Option<RefreshOutcome>>>,
    refresh_seq: u64,
    refreshing: bool,
    gemini_login: Option<GeminiLoginState>,
    /// 账号导入窗口（所有账号类型共用）。
    transfer_import: Option<TransferImportState>,
    /// 账号导出窗口（所有账号类型共用）。
    transfer_export: Option<TransferExportState>,
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
        let mut app = Self {
            roots,
            accounts: Vec::new(),
            active_id: None,
            // 当前账号标识由启动后的首次后台刷新填充，避免窗口出现前卡顿数秒
            current_identity: AccountIdentity::default(),
            last_detected_identity: None,
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
            tools: TOOL_STORES
                .iter()
                .map(|store| ToolAccounts::new(store))
                .collect(),
            tool_page: 0,
            zcode_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            refresh_shared: Arc::new(Mutex::new(None)),
            refresh_seq: 0,
            refreshing: false,
            gemini_login: None,
            transfer_import: None,
            transfer_export: None,
        };
        app.refresh();
        // 启动后立即后台补齐慢速信息（ZCode 进程状态、当前账号标识）
        app.begin_refresh(&cc.egui_ctx);
        // 后台轮询 ZCode 运行状态，界面帧只读缓存值
        let shared_running = app.zcode_running.clone();
        let poll_ctx = cc.egui_ctx.clone();
        std::thread::spawn(move || loop {
            let running = crate::zcode_running();
            shared_running.store(running, Ordering::SeqCst);
            poll_ctx.request_repaint();
            std::thread::sleep(Duration::from_secs(2));
        });
        app
    }

    /// 快速刷新：只读本地文件（账号列表、active 标记、工具账号识别），
    /// 立即让界面符合当前登录状态；进程与 CLI 级探测走 begin_refresh 后台。
    fn refresh(&mut self) {
        match list_accounts(&self.roots) {
            Ok(accounts) => {
                self.accounts = accounts;
                self.active_id = active_account(&self.roots);
            }
            Err(error) => self.set_error(error),
        }
        let mut errors = Vec::new();
        for tool in self.tools.iter_mut() {
            let store = tool.store;
            match store.list_accounts(&self.roots) {
                Ok(accounts) => {
                    tool.accounts = accounts;
                    tool.active_id = store.active_account(&self.roots);
                }
                Err(error) => errors.push(error),
            }
            // 工具账号识别只读本地文件，代价低，随列表一起刷新
            tool.identity = (store.detect)(&self.roots);
            tool.sync_account_name_on_identity_change();
        }
        for error in errors {
            self.set_error(error);
        }
    }

    /// 当检测到新账号时动态更新账户名称；若账号未变，则保留用户手动修改的内容。
    fn sync_account_name_on_identity_change(&mut self) {
        if !self.current_identity.is_present() {
            if self.last_detected_identity.is_some() {
                self.last_detected_identity = None;
                self.account_name.clear();
            }
            return;
        }
        if self.last_detected_identity.as_ref() != Some(&self.current_identity) {
            if let Some(default_name) = self.current_identity.default_name() {
                self.account_name = default_name;
            }
            self.last_detected_identity = Some(self.current_identity.clone());
        }
    }

    /// 完整刷新：先同步完成文件级刷新（界面立即更新），再在后台探测慢速
    /// 项目（ZCode 进程状态、ZCode CLI 账号识别、各 CLI 会话探测）。
    /// 点击后状态栏立即提示"正在刷新"，完成后结果回填并提示"已刷新"，
    /// 期间界面保持响应、刷新按钮呈禁用态。
    fn begin_refresh(&mut self, ctx: &egui::Context) {
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        self.refresh_seq += 1;
        let tag = self.refresh_seq;
        self.set_ok("正在刷新：读取账号列表、当前账号与 CLI 运行状态……");
        self.refresh();

        let shared = self.refresh_shared.clone();
        let roots = self.roots.clone();
        let context = ctx.clone();
        std::thread::spawn(move || {
            let zcode_running = crate::zcode_running();
            let current_identity = crate::identity::detect(&roots);
            let tools = TOOL_STORES
                .iter()
                .map(|store| ((store.detect)(&roots), store.cli_running()))
                .collect();
            *shared.lock().unwrap() = Some(RefreshOutcome {
                tag,
                zcode_running,
                current_identity,
                tools,
            });
            context.request_repaint();
        });
    }

    /// 每帧取回已完成的后台刷新结果并应用到界面。
    fn apply_refresh_outcome(&mut self) {
        let Some(outcome) = self.refresh_shared.lock().unwrap().take() else {
            return;
        };
        // 无论结果是否过期都复位刷新标记，避免界面永久停留在"刷新中"
        self.refreshing = false;
        if outcome.tag != self.refresh_seq {
            return;
        }
        self.zcode_running
            .store(outcome.zcode_running, Ordering::SeqCst);
        self.current_identity = outcome.current_identity;
        self.sync_account_name_on_identity_change();
        for (tool, (identity, cli_running)) in self.tools.iter_mut().zip(outcome.tools) {
            tool.identity = identity;
            tool.cli_running = cli_running;
            tool.sync_account_name_on_identity_change();
        }
        self.set_ok("已刷新：账户列表与当前账号为最新状态");
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

    fn save_new(&mut self, ctx: &egui::Context) {
        let name = self.pending_save_name();
        if zcode_running() {
            self.confirm = Some(ConfirmAction::Save { name });
            return;
        }
        self.do_save(name, ctx);
    }

    fn do_save(&mut self, name: Option<String>, ctx: &egui::Context) -> bool {
        match save_current_account(&self.roots, name.as_deref(), None) {
            Ok(profile) => {
                if let Some(default_name) = self.current_identity.default_name() {
                    self.account_name = default_name;
                } else {
                    self.account_name.clear();
                }
                self.refresh();
                // 当前账号标识较慢（需调用 zcode CLI），放后台刷新
                self.begin_refresh(ctx);
                self.set_ok(format!(
                    "已备份账户：{}；正在后台刷新当前账号标识……",
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

    /// 保存某 CLI 工具的当前登录状态为新备份。只读取本地文件，无需关闭任何程序。
    fn do_tool_save(&mut self, tool_index: usize, name: Option<String>) -> bool {
        let Some(store) = self.tools.get(tool_index).map(|tool| tool.store) else {
            return false;
        };
        match store.save_current_account(&self.roots, name.as_deref(), None) {
            Ok(profile) => {
                self.refresh();
                let tool = &mut self.tools[tool_index];
                if let Some(default_name) = tool.identity.default_name(store.id_prefix) {
                    tool.account_name = default_name;
                } else {
                    tool.account_name.clear();
                }
                self.set_ok(format!(
                    "已备份 {} 账号：{}",
                    store.display,
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

    fn do_tool_update(&mut self, tool_index: usize, id: &str) -> bool {
        let Some(store) = self.tools.get(tool_index).map(|tool| tool.store) else {
            return false;
        };
        let Some(profile) = self.tools[tool_index].profile(id) else {
            return false;
        };
        match store.save_current_account(&self.roots, Some(&profile.manifest.name), Some(&profile))
        {
            Ok(_) => {
                self.set_ok(format!(
                    "已更新 {} 账号备份：{}",
                    store.display,
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
                            // 当前账号标识较慢（需调用 zcode CLI），放后台刷新
                            me.begin_refresh(&ctx);
                            me.set_ok(format!(
                                "已切换到 {}；正在后台刷新当前账号标识……",
                                profile.manifest.display_name()
                            ));
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
                self.run_with_zcode_closed("保存", |me| me.do_save(name, &ctx));
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
            ConfirmAction::ToolSwitch(tool_index, id) => {
                let Some(store) = self.tools.get(tool_index).map(|tool| tool.store) else {
                    return;
                };
                let Some(profile) = self.tools[tool_index].profile(&id) else {
                    self.set_error("目标账号不存在");
                    return;
                };
                let name = profile.manifest.display_name().to_string();
                match store.switch_account(&self.roots, &profile) {
                    Ok(()) => {
                        self.refresh();
                        self.set_ok(format!(
                            "已切换到 {} 账号 {}；正在运行的会话请重启后使用",
                            store.display, name
                        ));
                        // 切换完成后询问是否立即启动新会话
                        self.confirm = Some(ConfirmAction::ToolRelaunch(tool_index, name));
                    }
                    Err(error) => self.set_error(error),
                }
            }
            ConfirmAction::ToolDelete(tool_index, id) => {
                let Some(store) = self.tools.get(tool_index).map(|tool| tool.store) else {
                    return;
                };
                let Some(profile) = self.tools[tool_index].profile(&id) else {
                    return;
                };
                match store.delete_account(&self.roots, &profile) {
                    Ok(()) => {
                        self.set_ok(format!(
                            "已删除 {} 备份：{}",
                            store.display,
                            profile.manifest.display_name()
                        ));
                        self.refresh();
                    }
                    Err(error) => self.set_error(error),
                }
            }
            ConfirmAction::ToolClear(tool_index) => {
                let Some(store) = self.tools.get(tool_index).map(|tool| tool.store) else {
                    return;
                };
                match store.clear_account(&self.roots) {
                    Ok(Some(name)) => {
                        self.refresh();
                        self.set_ok(format!(
                            "已清空 {} 登录状态（已备份为「{name}」），可重新登录或随时切换回来",
                            store.display
                        ));
                    }
                    Ok(None) => {
                        self.set_ok(format!("{} 已是未登录状态，无需清空", store.display));
                    }
                    Err(error) => self.set_error(error),
                }
            }
            ConfirmAction::ToolRelaunch(tool_index, name) => {
                let Some(store) = self.tools.get(tool_index).map(|tool| tool.store) else {
                    return;
                };
                match cli_accounts::launch_cli_session(store) {
                    Ok(()) => self.set_ok(format!(
                        "已在终端窗口启动新的 {} 会话，使用账号「{name}」",
                        store.display
                    )),
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
                self.save_new(ui.ctx());
            }
            let refresh_label = if self.refreshing {
                "刷新中…"
            } else {
                "刷新"
            };
            if ui
                .add_enabled(!self.refreshing, egui::Button::new(refresh_label))
                .on_hover_text("立即更新账号列表；当前账号与运行状态在后台探测，完成后自动更新显示")
                .clicked()
            {
                self.begin_refresh(ui.ctx());
            }
            if ui.button("打开备份目录").clicked() {
                match open_accounts_folder(&self.roots) {
                    Ok(()) => self.set_ok("已打开账户备份目录"),
                    Err(error) => self.set_error(error),
                }
            }
            if ui
                .button(RichText::new("导入账号").color(Color32::from_rgb(32, 132, 88)))
                .on_hover_text("从本工具导出的移植文件（zam-zcode-accounts.json）导入账号；ZCode 快照包含大量文件，仅支持文件导入")
                .clicked()
            {
                self.transfer_import = Some(TransferImportState {
                    target: TransferTarget::ZCode,
                    path: String::new(),
                    text: String::new(),
                });
            }
            if ui
                .add_enabled(!self.accounts.is_empty(), egui::Button::new("导出全部"))
                .on_hover_text("把全部 ZCode 账号备份导出为单个 JSON 移植文件，可在另一台电脑导入")
                .clicked()
            {
                self.open_export_window(TransferTarget::ZCode, None);
            }
        });
        egui::CollapsingHeader::new("备份的配置文件路径")
            .id_salt("zcode_backup_paths")
            .default_open(false)
            .show(ui, |ui| {
                ui.add_space(4.0);
                for tag in FULL_TAGS {
                    let candidate = candidate_by_tag(tag);
                    let path = self.roots.resolve(candidate);
                    let path_str = path.to_string_lossy();
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("• {tag}")).weak().monospace());
                        ui.label(RichText::new(path_str.as_ref()).monospace());
                    });
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
                            cell_label(ui, &stored_phone, 88.0, RichText::new(&stored_phone));
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
                                .button("导出")
                                .on_hover_text("导出为单个 JSON 移植文件，可在另一台电脑导入")
                                .clicked()
                            {
                                self.open_export_window(TransferTarget::ZCode, Some(id.clone()));
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

    fn tool_accounts_page(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);

        let tool_index = self.tool_page;
        let store = self.tools[tool_index].store;
        ui.heading(format!("{} 账号备份与切换", store.tab));
        ui.label(format!(
            "管理 {} 的登录账号：备份当前账号，在多个账号间一键切换。切换会整体替换登录凭据与账号缓存。",
            store.display
        ));
        ui.add_space(8.0);
        if self.tools[tool_index].cli_running {
            ui.colored_label(
                Color32::from_rgb(190, 112, 28),
                format!(
                    "检测到 {} 正在运行：切换或更新备份前请先退出相关会话，否则旧会话可能把登录凭据回写覆盖。",
                    store.cli_names
                ),
            );
        }
        self.tool_identity_banner(tool_index, ui);
        ui.add_space(8.0);

        let default_name = self.tools[tool_index]
            .identity
            .default_name(store.id_prefix);
        let can_save =
            self.tools[tool_index].pending_save_name().is_some() || default_name.is_some();
        ui.horizontal(|ui| {
            ui.label("账户名称");
            let input = ui.add_sized(
                [260.0, 30.0],
                egui::TextEdit::singleline(&mut self.tools[tool_index].account_name).hint_text(
                    match &default_name {
                        Some(name) => format!("默认：{name}"),
                        None => format!("例如：我的{}账号", store.tab),
                    },
                ),
            );
            let save = ui.add_enabled(can_save, egui::Button::new("保存当前账号"));
            if save.clicked()
                || (input.lost_focus()
                    && ui.input(|input| input.key_pressed(egui::Key::Enter))
                    && can_save)
            {
                let name = self.tools[tool_index].pending_save_name();
                self.do_tool_save(tool_index, name);
            }
            let refresh_label = if self.refreshing { "刷新中…" } else { "刷新" };
            if ui
                .add_enabled(!self.refreshing, egui::Button::new(refresh_label))
                .on_hover_text("立即更新账号列表；CLI 会话探测在后台进行，完成后自动更新显示")
                .clicked()
            {
                self.begin_refresh(ui.ctx());
            }
            if ui.button("打开备份目录").clicked() {
                match store.open_accounts_folder(&self.roots) {
                    Ok(()) => self.set_ok(format!("已打开 {} 备份目录", store.display)),
                    Err(error) => self.set_error(error),
                }
            }
            if store.key == "gemini"
                && ui
                    .button(RichText::new("登录账号").color(Color32::from_rgb(32, 132, 88)))
                    .on_hover_text("使用 Google 官方 OAuth 登录新的 Gemini / Antigravity (agy) 账号")
                    .clicked()
            {
                self.gemini_login = Some(GeminiLoginState::default());
            }
            if ui
                .button(RichText::new("导入账号").color(Color32::from_rgb(32, 132, 88)))
                .on_hover_text(match store.key {
                    "codebuddy" => "从本工具导出的移植文件、WorkBuddy JSON 数组或单个 settings.json 导入；导入只创建备份，不切换当前账号",
                    "claude" => "从本工具导出的移植文件、settings.json 或 .credentials.json 导入；导入只创建备份，不切换当前账号",
                    "codex" => "从本工具导出的移植文件或 auth.json 导入；导入只创建备份，不切换当前账号",
                    _ => "从本工具导出的移植文件或另一台机器的 OAuth 凭据 JSON 导入；导入只创建备份，不切换当前账号",
                })
                .clicked()
            {
                self.transfer_import = Some(TransferImportState {
                    target: TransferTarget::Tool(tool_index),
                    path: String::new(),
                    text: String::new(),
                });
            }
            if ui
                .add_enabled(
                    !self.tools[tool_index].accounts.is_empty(),
                    egui::Button::new("导出全部"),
                )
                .on_hover_text("把全部账号备份导出为单个 JSON 移植文件（也可复制到剪贴板），可在另一台电脑导入")
                .clicked()
            {
                self.open_export_window(TransferTarget::Tool(tool_index), None);
            }
            if ui
                .button(RichText::new("清空账号").color(Color32::from_rgb(180, 48, 48)))
                .on_hover_text("自动备份当前登录状态后清除凭据，恢复到未登录的原始状态，方便重新登录其他账号")
                .clicked()
            {
                self.confirm = Some(ConfirmAction::ToolClear(tool_index));
            }
        });
        egui::CollapsingHeader::new("备份的配置文件路径")
            .id_salt(format!("tool_backup_paths_{}", store.key))
            .default_open(false)
            .show(ui, |ui| {
                ui.add_space(4.0);
                let base = store.base(&self.roots);
                for path_entry in store.paths {
                    let path = base.join(path_entry.relative);
                    let path_str = path.to_string_lossy();
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("• {}", path_entry.tag))
                                .weak()
                                .monospace(),
                        );
                        ui.label(RichText::new(path_str.as_ref()).monospace());
                    });
                }
            });
        ui.add_space(14.0);

        if self.tools[tool_index].accounts.is_empty() {
            ui.group(|ui| {
                ui.set_min_height(110.0);
                ui.vertical_centered(|ui| {
                    ui.add_space(18.0);
                    ui.label(RichText::new(format!("还没有 {} 账号备份", store.tab)).strong());
                    ui.label(format!(
                        "先用 {} 登录一个账号，再点击上方「保存当前账号」创建备份。",
                        store.display
                    ));
                    ui.label("创建后列表每一行都会出现「切换」按钮，随时一键换号。");
                });
            });
            return;
        }

        let rows = self.tools[tool_index].accounts.clone();
        egui::ScrollArea::both().show(ui, |ui| {
            // 邮箱等账号标识可能很长：限制文本列宽度并保持单行截断，避免把「操作」按钮挤出可视区
            let identity_max_width = (ui.available_width() * 0.22).max(140.0);
            egui::Grid::new("tool_accounts_table")
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

                    for profile in rows {
                        let id = profile.manifest.id.clone();
                        let is_active =
                            self.tools[tool_index].active_id.as_deref() == Some(id.as_str());
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
                                    "切换到此账号；当前状态会先自动备份到该账号，恢复失败会自动回滚",
                                )
                                .clicked()
                            {
                                self.confirm =
                                    Some(ConfirmAction::ToolSwitch(tool_index, id.clone()));
                            }
                            if ui
                                .button("编辑")
                                .on_hover_text("设置别名，便于识别账号")
                                .clicked()
                            {
                                self.tools[tool_index].editor = Some(ToolEditor {
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
                                self.do_tool_update(tool_index, &id);
                            }
                            if ui
                                .button("导出")
                                .on_hover_text(
                                    "导出为单个 JSON 移植文件（也可复制到剪贴板），可在另一台电脑导入",
                                )
                                .clicked()
                            {
                                self.open_export_window(
                                    TransferTarget::Tool(tool_index),
                                    Some(id.clone()),
                                );
                            }
                            if ui
                                .button(RichText::new("删除").color(Color32::from_rgb(180, 48, 48)))
                                .clicked()
                            {
                                self.confirm = Some(ConfirmAction::ToolDelete(tool_index, id));
                            }
                        });
                        ui.end_row();
                    }
                });
        });
    }

    fn tool_identity_banner(&mut self, tool_index: usize, ui: &mut egui::Ui) {
        let tool = &self.tools[tool_index];
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("当前 {} 账号", tool.store.tab)).strong());
                let has_state = tool.identity.is_present();
                ui.colored_label(
                    if has_state {
                        Color32::from_rgb(32, 132, 88)
                    } else {
                        Color32::from_rgb(190, 112, 28)
                    },
                    tool.identity.describe(),
                );
                if let Some(matched) = tool.matched_profile() {
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
            ConfirmAction::ToolSwitch(tool_index, id) => {
                let tool = &self.tools[*tool_index];
                let name = tool
                    .profile(id)
                    .map(|profile| profile.manifest.display_name().to_string())
                    .unwrap_or_default();
                let running_hint = if tool.cli_running {
                    format!(
                        "检测到 {} 正在运行，切换后请重启相关会话，避免旧会话回写登录凭据。",
                        tool.store.cli_names
                    )
                } else {
                    String::new()
                };
                (
                    "确认切换 CLI 账号",
                    format!(
                        "将把 {} 本地登录状态切换为「{name}」。当前状态会先自动备份到该账号。{running_hint}",
                        tool.store.display
                    ),
                    "开始切换",
                )
            }
            ConfirmAction::ToolDelete(tool_index, id) => {
                let tool = &self.tools[*tool_index];
                let name = tool
                    .profile(id)
                    .map(|profile| profile.manifest.display_name().to_string())
                    .unwrap_or_default();
                (
                    "删除 CLI 账号备份",
                    format!(
                        "确定删除“{name}”的 {} 本地备份？此操作不会删除云端账号。",
                        tool.store.display
                    ),
                    "确认删除",
                )
            }
            ConfirmAction::ToolClear(tool_index) => {
                let tool = &self.tools[*tool_index];
                let running_hint = if tool.cli_running {
                    format!(
                        "检测到 {} 正在运行，请先退出会话再清空，否则旧会话可能把凭据回写。",
                        tool.store.cli_names
                    )
                } else {
                    String::new()
                };
                (
                    "清空账号（退出登录）",
                    format!(
                        "将先自动备份当前 {} 登录状态（与已有备份是同一账号时更新它），然后清除登录凭据，恢复到未登录的原始状态。之后可重新登录，也可随时从列表切换回该账号。{running_hint}",
                        tool.store.display
                    ),
                    "清空并退出登录",
                )
            }
            ConfirmAction::ToolRelaunch(tool_index, name) => {
                let tool = &self.tools[*tool_index];
                (
                    "切换完成",
                    format!(
                        "已切换到 {} 账号「{name}」。是否立即打开一个终端窗口，启动新的 {} 会话以使用新账号？\n\n正在运行的旧会话不会受影响，退出后重新打开也会使用新账号。",
                        tool.store.display, tool.store.cli_names
                    ),
                    "启动终端会话",
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
                    confirmed = confirmed
                        || (input.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter)));
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("手机号码");
                    let input = ui.add_sized(
                        [270.0, 28.0],
                        egui::TextEdit::singleline(&mut editor.phone)
                            .hint_text("选填，留空表示清除手机号码"),
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

    fn tool_editor_window(&mut self, ctx: &egui::Context) {
        let tool_index = self.tool_page;
        let Some(mut editor) = self.tools[tool_index].editor.take() else {
            return;
        };
        let store = self.tools[tool_index].store;
        let default_name = self.tools[tool_index]
            .profile(&editor.id)
            .map(|profile| profile.manifest.name.clone())
            .unwrap_or_default();
        let mut confirmed = false;
        let mut cancelled = false;
        egui::Window::new(format!("编辑 {} 账号信息", store.tab))
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
            self.save_tool_editor(tool_index, editor);
        } else if cancelled {
            // 丢弃编辑状态
        } else {
            self.tools[tool_index].editor = Some(editor);
        }
    }

    /// 保存编辑窗口中的别名；留空即清除。
    fn save_tool_editor(&mut self, tool_index: usize, editor: ToolEditor) {
        let alias = editor.alias.trim();
        let Some(profile) = self.tools[tool_index].profile(&editor.id) else {
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

    fn gemini_login_window(&mut self, ctx: &egui::Context) {
        let Some(mut login_state) = self.gemini_login.take() else {
            return;
        };

        let mut close_requested = false;
        let mut do_exchange = false;

        egui::Window::new("登录 Gemini / Antigravity (agy) 账号")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(520.0);
                ui.label(RichText::new("使用 Google 官方 OAuth 授权登录新的 Gemini / agy 账号。").strong());
                ui.add_space(6.0);

                ui.label("步骤 1：点击下方按钮在浏览器中打开 Google 授权页面并完成登录；");
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("打开浏览器授权").strong()).clicked() {
                        let auth_url = gemini::build_agy_auth_url();
                        if let Err(e) = cli_accounts::open_url(&auth_url) {
                            login_state.error = Some(e);
                        } else {
                            login_state.error = None;
                        }
                    }
                    if ui.button("复制授权链接").clicked() {
                        let auth_url = gemini::build_agy_auth_url();
                        ui.ctx().output_mut(|o| {
                            o.commands.push(egui::OutputCommand::CopyText(auth_url));
                        });
                    }
                });

                ui.add_space(8.0);
                ui.label("步骤 2：授权完成后，浏览器会重定向到 localhost:8085。复制地址栏中 code= 后面（到 & 符号之前）的授权码，或在页面直接复制授权码，粘贴在下方：");

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("授权码：");
                    let input = ui.add_sized(
                        [380.0, 28.0],
                        egui::TextEdit::singleline(&mut login_state.auth_code)
                            .hint_text("例如：4/0A... 粘贴完整授权码或重定向 URL"),
                    );
                    if input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        do_exchange = true;
                    }
                });

                if let Some(err) = &login_state.error {
                    ui.add_space(6.0);
                    ui.colored_label(Color32::from_rgb(190, 48, 48), err);
                }

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let can_submit = !login_state.auth_code.trim().is_empty() && !login_state.submitting;
                    if ui.add_enabled(can_submit, egui::Button::new(RichText::new("完成登录").strong())).clicked() {
                        do_exchange = true;
                    }
                    if ui.button("取消").clicked() {
                        close_requested = true;
                    }
                });
            });

        if do_exchange {
            let mut raw_code = login_state.auth_code.trim().to_string();
            // 如果用户直接粘贴了完整的重定向 URL，自动提取 code 参数
            if raw_code.contains("code=") {
                if let Some(pos) = raw_code.find("code=") {
                    let after = &raw_code[pos + 5..];
                    let end_pos = after.find('&').unwrap_or(after.len());
                    raw_code = after[..end_pos].to_string();
                }
            }
            raw_code = url_decode_simple(&raw_code);

            match gemini::exchange_and_save_token(&self.roots, &raw_code) {
                Ok(account_desc) => {
                    self.set_ok(format!(
                        "登录成功：{account_desc}，已更新本地 Antigravity 凭据"
                    ));
                    self.refresh();
                    self.begin_refresh(ctx);
                    return;
                }
                Err(e) => {
                    login_state.error = Some(format!("登录失败: {e}"));
                    self.gemini_login = Some(login_state);
                }
            }
        } else if !close_requested {
            self.gemini_login = Some(login_state);
        }
    }

    /// 打开导出窗口并立即生成移植文件内容。
    fn open_export_window(&mut self, target: TransferTarget, account_id: Option<String>) {
        let tool_key = match target {
            TransferTarget::ZCode => transfer::ZCODE_TOOL_KEY,
            TransferTarget::Tool(index) => self.tools[index].store.key,
        };
        let ids: Vec<String> = account_id.iter().cloned().collect();
        let (json, error) = match target {
            TransferTarget::ZCode => match transfer::export_zcode_accounts(&self.roots, &ids) {
                Ok(json) => (json, None),
                Err(error) => (String::new(), Some(error)),
            },
            TransferTarget::Tool(index) => {
                let store = self.tools[index].store;
                match transfer::export_tool_accounts(store, &self.roots, &ids) {
                    Ok(json) => (json, None),
                    Err(error) => (String::new(), Some(error)),
                }
            }
        };
        let save_path = self
            .roots
            .user_profile
            .join(transfer::default_file_name(tool_key))
            .to_string_lossy()
            .into_owned();
        self.transfer_export = Some(TransferExportState {
            target,
            account_id,
            json,
            error,
            save_path,
        });
    }

    /// 执行导入：优先文件路径，其次粘贴文本。
    fn run_transfer_import(&mut self, path: &str, text: &str) {
        let target = self
            .transfer_import
            .as_ref()
            .map(|state| state.target)
            .expect("import window open");
        let result = match target {
            TransferTarget::ZCode => {
                if !path.trim().is_empty() {
                    transfer::import_zcode_file(&self.roots, std::path::Path::new(path.trim()))
                } else {
                    transfer::import_zcode_text(&self.roots, text)
                }
            }
            TransferTarget::Tool(index) => {
                let store = self.tools[index].store;
                if !path.trim().is_empty() {
                    transfer::import_tool_file(store, &self.roots, std::path::Path::new(path.trim()))
                } else {
                    transfer::import_tool_text(store, &self.roots, text)
                }
            }
        };
        self.transfer_import = None;
        match result {
            Ok(result) => {
                self.refresh();
                self.set_ok(format!(
                    "导入完成：成功 {} 个，跳过 {} 个。可在列表中切换使用。",
                    result.imported, result.skipped
                ));
            }
            Err(error) => self.set_error(error),
        }
    }

    /// 通用导入窗口：文件路径 + 粘贴文本（CLI 工具支持剪贴板；ZCode 快照
    /// 包含大量文件，只走文件通道）。
    fn transfer_import_window(&mut self, ctx: &egui::Context) {
        let Some(state) = self.transfer_import.as_mut() else {
            return;
        };
        let target = state.target;
        let (title, tab) = match target {
            TransferTarget::ZCode => ("导入 ZCode 账号", "ZCode"),
            TransferTarget::Tool(index) => (
                "导入账号",
                self.tools[index].store.tab,
            ),
        };
        let is_tool = matches!(target, TransferTarget::Tool(_));
        let mut do_import = false;
        let mut do_paste = false;
        let mut close_requested = false;

        egui::Window::new(format!("{title}（{tab}）"))
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                match target {
                    TransferTarget::ZCode => {
                        ui.label("选择本工具导出的移植文件（zam-zcode-accounts.json）导入 ZCode 账号。");
                        ui.label("ZCode 快照包含大量文件（会话、本地存储等），仅支持文件导入。");
                    }
                    TransferTarget::Tool(index) => {
                        let key = self.tools[index].store.key;
                        ui.label(match key {
                            "codebuddy" => "支持：本工具导出的移植文件、WorkBuddy / wb-switch JSON 数组、单个 ~/.codebuddy/settings.json。",
                            "claude" => "支持：本工具导出的移植文件、~/.claude/settings.json（端点 Token）、.credentials.json（OAuth 凭据）。",
                            "codex" => "支持：本工具导出的移植文件、~/.codex/auth.json（ChatGPT OAuth 或 API Key）。",
                            _ => "支持：本工具导出的移植文件、另一台机器的 oauth_creds.json 或 antigravity-oauth-token。",
                        });
                        ui.label("导入只创建备份，不改变当前登录状态。");
                    }
                }
                ui.label(
                    RichText::new("⚠ 导入内容包含登录凭据，请确认来源可信，不要导入来路不明的文件。")
                        .color(Color32::from_rgb(190, 112, 28)),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label("JSON 文件");
                    ui.add_sized(
                        [420.0, 26.0],
                        egui::TextEdit::singleline(&mut state.path)
                            .hint_text("例如 /home/user/Downloads/zam-gemini-accounts.json"),
                    );
                });
                if is_tool {
                    ui.add_space(6.0);
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("或粘贴 JSON").strong());
                        if ui
                            .button("从剪贴板粘贴")
                            .on_hover_text("读取系统剪贴板中的移植文件 / 凭据 JSON 文本")
                            .clicked()
                        {
                            do_paste = true;
                        }
                    });
                    ui.add_sized(
                        [520.0, 140.0],
                        egui::TextEdit::multiline(&mut state.text)
                            .code_editor()
                            .hint_text("粘贴导出的 JSON（也可直接在此输入框按 Ctrl+V 粘贴）"),
                    );
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("取消").clicked() {
                        close_requested = true;
                    }
                    let can_import =
                        !state.path.trim().is_empty() || (is_tool && !state.text.trim().is_empty());
                    if ui
                        .add_enabled(
                            can_import,
                            egui::Button::new(RichText::new("导入").color(Color32::WHITE)),
                        )
                        .clicked()
                    {
                        do_import = true;
                    }
                });
            });

        if do_paste {
            let text = self.transfer_import.as_mut().map(|state| &mut state.text);
            if let Some(text) = text {
                match clipboard::read_text() {
                    Ok(clipboard_text) if !clipboard_text.trim().is_empty() => {
                        *text = clipboard_text;
                    }
                    Ok(_) => self.set_error("剪贴板为空"),
                    Err(error) => self.set_error(error),
                }
            }
        }
        if close_requested {
            self.transfer_import = None;
        }
        if do_import {
            let (path, text) = self
                .transfer_import
                .as_ref()
                .map(|state| (state.path.clone(), state.text.clone()))
                .expect("import window open");
            self.run_transfer_import(&path, &text);
        }
    }

    /// 通用导出窗口：预览移植文件 + 复制到剪贴板（CLI 工具）+ 保存到文件。
    fn transfer_export_window(&mut self, ctx: &egui::Context) {
        if self.transfer_export.is_none() {
            return;
        }
        let (target, account_id) = {
            let state = self.transfer_export.as_ref().expect("checked above");
            (state.target, state.account_id.clone())
        };
        let is_tool = matches!(target, TransferTarget::Tool(_));
        let (tab, account_name) = match target {
            TransferTarget::ZCode => ("ZCode", self.account_display_name(account_id.as_deref())),
            TransferTarget::Tool(index) => (
                self.tools[index].store.tab,
                self.tool_account_display_name(index, account_id.as_deref()),
            ),
        };
        let scope = account_name
            .map(|name| format!("账号「{name}」"))
            .unwrap_or_else(|| "全部账号".into());
        let mut do_copy = false;
        let mut do_save = false;
        let mut close_requested = false;

        egui::Window::new(format!("导出账号（{tab} · {scope}）"))
            .collapsible(false)
            .resizable(true)
            .default_size([560.0, 420.0])
            .show(ctx, |ui| {
                let state = self.transfer_export.as_mut().expect("checked above");
                if let Some(error) = &state.error {
                    ui.colored_label(Color32::from_rgb(190, 48, 48), error);
                } else {
                    ui.label(
                        RichText::new("⚠ 导出文件包含登录凭据，请像密码一样保管，不要上传或分享。")
                            .color(Color32::from_rgb(190, 112, 28)),
                    );
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical()
                        .max_height(240.0)
                        .show(ui, |ui| {
                            ui.add_sized(
                                [520.0, 200.0],
                                egui::Label::new(
                                    RichText::new(state.json.as_str()).monospace().weak(),
                                )
                                .wrap(),
                            );
                        });
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if is_tool && state.error.is_none() && ui
                        .button("复制到剪贴板")
                        .on_hover_text("复制整个移植文件 JSON，到另一台电脑的导入窗口粘贴")
                        .clicked()
                    {
                        do_copy = true;
                    }
                    if state.error.is_none() {
                        ui.add_sized(
                            [330.0, 26.0],
                            egui::TextEdit::singleline(&mut state.save_path),
                        );
                        if ui.button("保存到文件").clicked() {
                            do_save = true;
                        }
                    }
                    if ui.button("关闭").clicked() {
                        close_requested = true;
                    }
                });
            });

        if do_copy {
            if let Some(json) = self.transfer_export.as_ref().map(|state| state.json.clone()) {
                match clipboard::write_text(&json) {
                    Ok(()) => self.set_ok("已复制到剪贴板，可在另一台电脑的导入窗口粘贴"),
                    Err(error) => self.set_error(error),
                }
            }
        }
        if do_save {
            let path = self
                .transfer_export
                .as_ref()
                .map(|state| state.save_path.trim().to_string())
                .expect("export window open");
            let json = self
                .transfer_export
                .as_ref()
                .map(|state| state.json.clone())
                .expect("export window open");
            match fs::write(&path, json.as_bytes()) {
                Ok(()) => self.set_ok(format!("已导出到 {path}")),
                Err(error) => self.set_error(format!("写入 {path} 失败: {error}")),
            }
        }
        if close_requested {
            self.transfer_export = None;
        }
    }

    fn account_display_name(&self, id: Option<&str>) -> Option<String> {
        let id = id?;
        self.accounts
            .iter()
            .find(|profile| profile.manifest.id == id)
            .map(|profile| profile.manifest.display_name().to_string())
    }

    fn tool_account_display_name(&self, tool_index: usize, id: Option<&str>) -> Option<String> {
        let id = id?;
        self.tools[tool_index]
            .accounts
            .iter()
            .find(|profile| profile.manifest.id == id)
            .map(|profile| profile.manifest.display_name().to_string())
    }
}

fn url_decode_simple(input: &str) -> String {
    let mut result = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(val) = u8::from_str_radix(&input[i + 1..i + 3], 16) {
                result.push(val);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            result.push(b' ');
            i += 1;
            continue;
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
}

impl ZCodeApp {
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
            ui.label(
                RichText::new("定位测试（始终包含：激活窗口 + 滚动到顶部 + 悬停目标行）").strong(),
            );
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
                let can_test =
                    !running && (!steps.input_message || !self.auto_message.trim().is_empty());
                if ui
                    .add_enabled(can_test, egui::Button::new("执行测试"))
                    .on_hover_text(
                        "按勾选的步骤组合执行；全部不勾选 = 仅定位悬停，用于核对序号与位置",
                    )
                    .on_disabled_hover_text(
                        if steps.input_message && self.auto_message.trim().is_empty() {
                            "勾选了「粘贴输入」但消息内容为空"
                        } else {
                            "正在执行中"
                        },
                    )
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
                ui.label(
                    RichText::new("（到点自动执行完整发送，期间请勿操作鼠标键盘；退出程序即失效）")
                        .weak(),
                );
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
                    .add_enabled(
                        can_send,
                        egui::Button::new(RichText::new(button_label).strong()),
                    )
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
                        schedule: self
                            .schedule_enabled
                            .then(|| (self.schedule_time.trim().to_string(), self.schedule_daily)),
                    });
                }
                if scheduled.is_some() && ui.button("取消定时").clicked() {
                    self.cancel_schedule();
                }
                if let Some(desc) = &scheduled {
                    ui.label(
                        RichText::new(format!("已预约：{desc}"))
                            .color(Color32::from_rgb(32, 132, 88)),
                    );
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
                        ui.label(RichText::new("执行中，请不要移动鼠标或敲键盘...").weak());
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
        // 后台慢速刷新完成时取回结果，保证展示的账号状态为最新
        self.apply_refresh_outcome();
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
                for (index, store) in TOOL_STORES.iter().enumerate() {
                    let page = match index {
                        0 => Page::GeminiAccounts,
                        1 => Page::CodexAccounts,
                        2 => Page::ClaudeAccounts,
                        _ => Page::CodeBuddyAccounts,
                    };
                    if ui
                        .selectable_label(self.page == page, store.tab)
                        .on_hover_text(store.display)
                        .clicked()
                    {
                        self.page = page;
                        self.tool_page = index;
                    }
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
                    if ui
                        .button("Web 管理端")
                        .on_hover_text("在浏览器中打开 Web 管理页面 (http://127.0.0.1:14596)")
                        .clicked()
                    {
                        let _ = cli_accounts::open_url(&format!(
                            "http://127.0.0.1:{}",
                            crate::web::DEFAULT_WEB_PORT
                        ));
                    }
                    let running = self.zcode_running.load(Ordering::SeqCst);
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
                Page::GeminiAccounts
                | Page::CodexAccounts
                | Page::ClaudeAccounts
                | Page::CodeBuddyAccounts => self.tool_accounts_page(ui),
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
        self.tool_editor_window(&ctx);
        self.gemini_login_window(&ctx);
        self.transfer_import_window(&ctx);
        self.transfer_export_window(&ctx);
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
    ui.add_sized([max_width, 20.0], egui::Label::new(styled).truncate())
        .on_hover_text(text)
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
    use std::path::PathBuf;

    #[test]
    fn recent_timestamp_is_readable() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(format_timestamp(now), "刚刚");
        assert_eq!(format_timestamp(now - 120), "2 分钟前");
    }

    #[test]
    fn tool_account_name_updates_on_identity_change_and_preserves_manual_edit() {
        let mut tool = ToolAccounts::new(&crate::gemini::STORE);
        assert_eq!(tool.account_name, "");
        assert_eq!(tool.last_detected_identity, None);

        // 1. 初次检测到账号：自动填入默认名称
        tool.identity = cli_accounts::CliIdentity {
            email: Some("alice@example.com".into()),
            auth_type: Some("Google".into()),
            fingerprint: Some("fp_alice_123456".into()),
        };
        tool.sync_account_name_on_identity_change();
        assert_eq!(tool.account_name, "alice");
        assert_eq!(tool.last_detected_identity, Some(tool.identity.clone()));

        // 2. 用户手动修改名称：同一账号刷新不覆盖修改
        tool.account_name = "我的主力号".into();
        tool.sync_account_name_on_identity_change();
        assert_eq!(tool.account_name, "我的主力号");

        // 3. 检测到新账号（换号）：自动覆盖更新为新账号默认名称
        tool.identity = cli_accounts::CliIdentity {
            email: Some("bob@example.com".into()),
            auth_type: Some("Google".into()),
            fingerprint: Some("fp_bob_789012".into()),
        };
        tool.sync_account_name_on_identity_change();
        assert_eq!(tool.account_name, "bob");
        assert_eq!(tool.last_detected_identity, Some(tool.identity.clone()));

        // 4. 用户再次手动修改：同一账号保持
        tool.account_name = "Bob临时号".into();
        tool.sync_account_name_on_identity_change();
        assert_eq!(tool.account_name, "Bob临时号");

        // 5. 账号退出（未登录）：清空输入框与缓存
        tool.identity = cli_accounts::CliIdentity::default();
        tool.sync_account_name_on_identity_change();
        assert_eq!(tool.account_name, "");
        assert_eq!(tool.last_detected_identity, None);

        // 6. 重新登录账号：再次自动填充
        tool.identity = cli_accounts::CliIdentity {
            email: Some("charlie@example.com".into()),
            auth_type: None,
            fingerprint: Some("fp_charlie_999".into()),
        };
        tool.sync_account_name_on_identity_change();
        assert_eq!(tool.account_name, "charlie");
    }

    #[test]
    fn zcode_account_name_updates_on_identity_change_and_preserves_manual_edit() {
        let mut app = ZCodeApp {
            roots: Roots {
                user_profile: PathBuf::new(),
                app_data: PathBuf::new(),
            },
            accounts: Vec::new(),
            active_id: None,
            current_identity: AccountIdentity::default(),
            last_detected_identity: None,
            account_name: String::new(),
            auto_restart: true,
            info_editor: None,
            status: String::new(),
            status_error: false,
            page: Page::Accounts,
            confirm: None,
            auto_send_state: Arc::new(Mutex::new(AutoSendState::default())),
            pinned_index: 1,
            auto_message: String::new(),
            test_click_session: false,
            test_input: false,
            test_send: false,
            schedule_enabled: false,
            schedule_time: String::new(),
            schedule_daily: false,
            schedule_cancel: Arc::new(Mutex::new(None)),
            tools: Vec::new(),
            tool_page: 0,
            zcode_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            refresh_shared: Arc::new(Mutex::new(None)),
            refresh_seq: 0,
            refreshing: false,
            gemini_login: None,
            transfer_import: None,
            transfer_export: None,
        };

        // 1. 初次检测到 ZCode 账号：自动填入默认名称
        app.current_identity = AccountIdentity {
            username: Some("dev_user".into()),
            user_id: Some("1001".into()),
            provider: Some("corp".into()),
            fingerprint: Some("fp_dev_1111".into()),
        };
        app.sync_account_name_on_identity_change();
        assert_eq!(app.account_name, "dev_user");
        assert_eq!(
            app.last_detected_identity,
            Some(app.current_identity.clone())
        );

        // 2. 用户手动修改账户名称：账号未变时刷新保留用户输入
        app.account_name = "我的开发账户".into();
        app.sync_account_name_on_identity_change();
        assert_eq!(app.account_name, "我的开发账户");

        // 3. 切换检测到新账号：自动覆盖更新为新账号名称
        app.current_identity = AccountIdentity {
            username: Some("prod_user".into()),
            user_id: Some("2002".into()),
            provider: Some("corp".into()),
            fingerprint: Some("fp_prod_2222".into()),
        };
        app.sync_account_name_on_identity_change();
        assert_eq!(app.account_name, "prod_user");
        assert_eq!(
            app.last_detected_identity,
            Some(app.current_identity.clone())
        );
    }
}
