use crate::{
    accounts::{
        active_account, delete_account, list_accounts, open_accounts_folder, save_current_account,
        switch_account, AccountProfile,
    },
    clean, zcode_running, CleanOptions, Roots,
};
use eframe::egui::{self, Color32, RichText};
use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Accounts,
    Cleanup,
}

enum ConfirmAction {
    Switch(String),
    Delete(String),
    Clean(bool),
}

pub struct ZCodeApp {
    roots: Roots,
    accounts: Vec<AccountProfile>,
    active_id: Option<String>,
    account_name: String,
    status: String,
    status_error: bool,
    page: Page,
    confirm: Option<ConfirmAction>,
}

impl ZCodeApp {
    fn new(cc: &eframe::CreationContext<'_>, roots: Roots) -> Self {
        install_chinese_font(&cc.egui_ctx);
        let mut app = Self {
            roots,
            accounts: Vec::new(),
            active_id: None,
            account_name: String::new(),
            status: "就绪".into(),
            status_error: false,
            page: Page::Accounts,
            confirm: None,
        };
        app.refresh();
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

    fn save_new(&mut self) {
        if zcode_running() {
            self.set_error("请先完全退出 ZCode，再备份当前账户");
            return;
        }
        match save_current_account(&self.roots, &self.account_name, None) {
            Ok(profile) => {
                self.account_name.clear();
                self.set_ok(format!("已备份账户：{}", profile.manifest.name));
                self.refresh();
            }
            Err(error) => self.set_error(error),
        }
    }

    fn update_profile(&mut self, id: &str) {
        if zcode_running() {
            self.set_error("请先完全退出 ZCode，再更新账户备份");
            return;
        }
        let Some(profile) = self.profile(id) else {
            return;
        };
        match save_current_account(&self.roots, &profile.manifest.name, Some(&profile)) {
            Ok(_) => {
                self.set_ok(format!("已更新账户备份：{}", profile.manifest.name));
                self.refresh();
            }
            Err(error) => self.set_error(error),
        }
    }

    fn perform_confirmed(&mut self) {
        let Some(action) = self.confirm.take() else {
            return;
        };
        match action {
            ConfirmAction::Switch(id) => {
                let Some(profile) = self.profile(&id) else {
                    self.set_error("目标账户不存在");
                    return;
                };
                if zcode_running() {
                    self.set_error("请先完全退出 ZCode，再切换账户");
                    return;
                }
                match switch_account(&self.roots, &profile) {
                    Ok(()) => {
                        self.set_ok(format!(
                            "已切换到 {}，现在可以启动 ZCode",
                            profile.manifest.name
                        ));
                        self.refresh();
                    }
                    Err(error) => self.set_error(error),
                }
            }
            ConfirmAction::Delete(id) => {
                let Some(profile) = self.profile(&id) else {
                    return;
                };
                match delete_account(&self.roots, &profile) {
                    Ok(()) => {
                        self.set_ok(format!("已删除备份：{}", profile.manifest.name));
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
        }
    }

    fn accounts_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("账户备份与切换");
        ui.label("退出 ZCode 后保存每个已登录账户的本地状态，之后无需重复登录即可快速切换。");
        ui.add_space(12.0);

        ui.horizontal(|ui| {
            ui.label("账户名称");
            let input = ui.add_sized(
                [260.0, 30.0],
                egui::TextEdit::singleline(&mut self.account_name).hint_text("例如：工作账号"),
            );
            let save = ui.add_enabled(
                !self.account_name.trim().is_empty(),
                egui::Button::new("保存当前账户"),
            );
            if save.clicked()
                || (input.lost_focus()
                    && ui.input(|input| input.key_pressed(egui::Key::Enter))
                    && !self.account_name.trim().is_empty())
            {
                self.save_new();
            }
            if ui.button("刷新").clicked() {
                self.refresh();
                self.set_ok("账户列表已刷新");
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
                ui.set_min_height(90.0);
                ui.vertical_centered(|ui| {
                    ui.add_space(18.0);
                    ui.label(RichText::new("还没有账户备份").strong());
                    ui.label("登录 ZCode 后，在上方输入名称并保存当前账户。");
                });
            });
            return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("accounts_table")
                .striped(true)
                .min_col_width(80.0)
                .spacing([18.0, 10.0])
                .show(ui, |ui| {
                    ui.strong("状态");
                    ui.strong("账户名称");
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
                        ui.label(RichText::new(&profile.manifest.name).strong());
                        ui.label(format_timestamp(profile.manifest.updated_at));
                        ui.label(profile.manifest.item_count.to_string());
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(!is_active, egui::Button::new("切换"))
                                .on_hover_text("切换前会自动保留当前状态")
                                .clicked()
                            {
                                self.confirm = Some(ConfirmAction::Switch(id.clone()));
                            }
                            if ui
                                .add_enabled(is_active, egui::Button::new("更新备份"))
                                .on_hover_text("仅当前正在使用的账户可以更新")
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
                    .map(|profile| profile.manifest.name)
                    .unwrap_or_default();
                (
                    "确认切换账户",
                    format!("将切换到“{name}”。请确认 ZCode 已完全退出。当前状态会先自动备份。"),
                    "确认切换",
                )
            }
            ConfirmAction::Delete(id) => {
                let name = self
                    .profile(id)
                    .map(|profile| profile.manifest.name)
                    .unwrap_or_default();
                (
                    "删除账户备份",
                    format!("确定删除“{name}”的本地备份？此操作不会删除 ZCode 云端账户。"),
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
        };
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(430.0);
                ui.label(message);
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("取消").clicked() {
                        self.confirm = None;
                    }
                    if ui.button(confirm_label).clicked() {
                        self.perform_confirmed();
                    }
                });
            });
    }
}

impl eframe::App for ZCodeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
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
                    .selectable_label(self.page == Page::Cleanup, "清理")
                    .clicked()
                {
                    self.page = Page::Cleanup;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
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
                Page::Cleanup => self.cleanup_page(ui),
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

pub fn launch() -> Result<(), String> {
    let roots = Roots::detect()?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ZCode 账户管家")
            .with_inner_size([900.0, 580.0])
            .with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "ZCode 账户管家",
        options,
        Box::new(move |cc| Ok(Box::new(ZCodeApp::new(cc, roots)))),
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
