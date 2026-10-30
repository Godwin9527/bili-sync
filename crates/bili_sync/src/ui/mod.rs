//! Windows 桌面壳：图形界面、系统托盘、开机自启。
//!
//! 该模块只在 Windows 下参与编译。它把原本「双击后只有控制台窗口」的启动方式
//! 改造成一个带界面的桌面程序，同时保留原有的后台服务逻辑不变：
//!
//! - 主线程：eframe/egui 事件循环，负责窗口与托盘
//! - 服务线程：独立 tokio 运行时，负责 HTTP 服务与定时下载
//!
//! 两者通过 [`crate::shell::Shell`] 共享状态，通过 [`Action`] 队列传递托盘指令。

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use eframe::egui;
use parking_lot::Mutex;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};
use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

use crate::config::{ARGS, version};
use crate::shell::Shell;

/// 开机自启写入的注册表位置与值名
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const AUTOSTART_VALUE: &str = "BiliSync";

/// 主窗口背景色（深色主题）
const BACKGROUND: egui::Color32 = egui::Color32::from_rgb(24, 24, 27);

/// 托盘菜单触发的动作，由托盘监听线程投递、主线程消费
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// 在默认浏览器中打开管理页
    Open,
    /// 唤回主窗口
    Show,
    /// 重启后台服务
    Restart,
    /// 切换开机自启
    ToggleAutostart,
    /// 退出程序
    Quit,
}

/// 托盘相关的句柄，必须与窗口同生命周期持有，否则图标会消失
struct TrayHandles {
    _tray: TrayIcon,
    autostart: CheckMenuItem,
}

/// 启动图形界面。若图形界面无法初始化，则退化为无界面的后台服务。
pub fn run() {
    let shell = Shell::new();
    let service_rx = crate::spawn_service(shell.clone());
    let start_in_tray = ARGS.tray;

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Bili-Sync")
            .with_inner_size([400.0, 476.0])
            .with_min_inner_size([360.0, 420.0])
            .with_app_id("bili-sync")
            .with_icon(window_icon()),
        centered: true,
        ..Default::default()
    };

    let app = ShellApp {
        shell: shell.clone(),
        actions: Arc::new(Mutex::new(Vec::new())),
        tray: None,
        service_rx: Arc::new(Mutex::new(Some(service_rx))),
        autostart: is_autostart_enabled(),
        start_in_tray,
        quitting: false,
        quit_deadline: None,
        bootstrapped: false,
    };

    let result = eframe::run_native(
        "Bili-Sync",
        options,
        Box::new(move |cc| {
            setup_style(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    );

    if let Err(e) = result {
        error!("图形界面启动失败：{e}，退化为无界面模式");
        wait_for_service(&shell);
    }
}

/// 图形界面无法启动时的兜底：等待后台服务自行结束
fn wait_for_service(shell: &Arc<Shell>) {
    while shell.is_running() {
        std::thread::sleep(Duration::from_millis(500));
    }
}

struct ShellApp {
    shell: Arc<Shell>,
    actions: Arc<Mutex<Vec<Action>>>,
    tray: Option<TrayHandles>,
    /// 后台服务的结束信号，重启时会被替换
    service_rx: Arc<Mutex<Option<mpsc::Receiver<()>>>>,
    autostart: bool,
    /// 启动时是否直接隐藏到托盘（由 `--tray` 决定）
    start_in_tray: bool,
    quitting: bool,
    quit_deadline: Option<Instant>,
    bootstrapped: bool,
}

impl eframe::App for ShellApp {
    /// 每帧都会执行，**窗口隐藏时同样执行**，因此托盘指令与退出流程都放在这里
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.bootstrapped {
            self.bootstrapped = true;
            match build_tray(self.actions.clone(), ctx.clone(), self.autostart) {
                Ok(tray) => self.tray = Some(tray),
                Err(e) => error!("创建系统托盘失败：{e:#}"),
            }
            if self.start_in_tray {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        }

        let pending: Vec<Action> = self.actions.lock().drain(..).collect();
        for action in pending {
            self.handle(action, ctx);
        }

        // 关闭按钮默认只隐藏到托盘，真正的退出走托盘菜单
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        if self.quitting && self.service_stopped() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // 保持事件循环活跃：窗口隐藏后仍需处理托盘指令
        ctx.request_repaint_after(Duration::from_millis(300));
    }

    /// 仅在窗口可见时执行，负责实际绘制
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        draw(self, ui);
    }
}

impl ShellApp {
    fn handle(&mut self, action: Action, ctx: &egui::Context) {
        match action {
            Action::Open => self.open_browser(),
            Action::Show => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
            Action::Restart => self.restart_service(),
            Action::ToggleAutostart => {
                self.autostart = !self.autostart;
                if let Err(e) = set_autostart(self.autostart) {
                    error!("设置开机自启失败：{e:#}");
                    self.autostart = !self.autostart;
                }
                if let Some(tray) = &self.tray {
                    tray.autostart.set_checked(self.autostart);
                }
            }
            Action::Quit => {
                info!("收到退出指令，正在停止后台服务..");
                self.quitting = true;
                self.quit_deadline = Some(Instant::now() + Duration::from_secs(10));
                self.shell.request_shutdown();
            }
        }
    }

    /// 后台服务是否已经彻底停止（含超时兜底）
    fn service_stopped(&self) -> bool {
        if self.quit_deadline.is_some_and(|deadline| Instant::now() > deadline) {
            warn!("等待后台服务退出超时，强制关闭窗口");
            return true;
        }
        match self.service_rx.lock().as_ref() {
            Some(rx) => rx.try_recv().is_ok(),
            None => true,
        }
    }

    fn restart_service(&mut self) {
        info!("收到重启指令，正在重启后台服务..");
        self.shell.request_shutdown();
        let shell = self.shell.clone();
        let slot = self.service_rx.clone();
        let ctx = self.actions.clone();
        std::thread::spawn(move || {
            // 等待旧服务退出，最多 10 秒
            let old = slot.lock().take();
            if let Some(rx) = old {
                let _ = rx.recv_timeout(Duration::from_secs(10));
            }
            shell.reset_shutdown_token();
            let rx = crate::spawn_service(shell);
            *slot.lock() = Some(rx);
            ctx.lock().push(Action::Show);
        });
    }

    fn open_browser(&self) {
        let address = self.shell.bind_address();
        if address.is_empty() {
            warn!("管理页地址尚未就绪，请稍候再试");
            return;
        }
        let url = format!("http://{}", normalize_address(&address));
        if let Err(e) = open_url(&url) {
            error!("打开浏览器失败：{e:#}");
        }
    }
}

/// 用默认浏览器打开链接，过程中不弹出控制台窗口
fn open_url(url: &str) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;
    Ok(())
}

/// `0.0.0.0` 在浏览器里不可直接访问，替换为本机回环地址
fn normalize_address(address: &str) -> String {
    address
        .strip_prefix("0.0.0.0")
        .map(|rest| format!("127.0.0.1{rest}"))
        .unwrap_or_else(|| address.to_owned())
}

fn build_tray(actions: Arc<Mutex<Vec<Action>>>, ctx: egui::Context, autostart: bool) -> anyhow::Result<TrayHandles> {
    let menu = Menu::new();
    let open = MenuItem::new("打开管理页", true, None);
    let restart = MenuItem::new("重启服务", true, None);
    let autostart_item = CheckMenuItem::new("开机自启", true, autostart, None);
    let separator = PredefinedMenuItem::separator();
    let quit = MenuItem::new("退出", true, None);
    menu.append_items(&[&open, &restart, &autostart_item, &separator, &quit])?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Bili-Sync")
        .with_icon(load_icon().unwrap_or_else(fallback_icon))
        .build()?;

    // 菜单与图标事件都在独立线程里监听，避免窗口隐藏后主线程收不到事件
    let (open_id, restart_id, autostart_id, quit_id) = (
        open.id().clone(),
        restart.id().clone(),
        autostart_item.id().clone(),
        quit.id().clone(),
    );
    spawn_tray_listener(actions, ctx, open_id, restart_id, autostart_id, quit_id);

    Ok(TrayHandles {
        _tray: tray,
        autostart: autostart_item,
    })
}

fn spawn_tray_listener(
    actions: Arc<Mutex<Vec<Action>>>,
    ctx: egui::Context,
    open_id: MenuId,
    restart_id: MenuId,
    autostart_id: MenuId,
    quit_id: MenuId,
) {
    std::thread::Builder::new()
        .name("bili-sync-tray".to_owned())
        .spawn(move || {
            let menu_rx = MenuEvent::receiver();
            let tray_rx = TrayIconEvent::receiver();
            loop {
                let mut fired = Vec::new();
                while let Ok(event) = menu_rx.try_recv() {
                    let action = if event.id == open_id {
                        Some(Action::Open)
                    } else if event.id == restart_id {
                        Some(Action::Restart)
                    } else if event.id == autostart_id {
                        Some(Action::ToggleAutostart)
                    } else if event.id == quit_id {
                        Some(Action::Quit)
                    } else {
                        None
                    };
                    fired.extend(action);
                }
                while let Ok(event) = tray_rx.try_recv() {
                    if matches!(event, TrayIconEvent::DoubleClick { .. }) {
                        fired.push(Action::Show);
                    }
                }
                if !fired.is_empty() {
                    actions.lock().extend(fired);
                    ctx.request_repaint();
                }
                std::thread::sleep(Duration::from_millis(120));
            }
        })
        .expect("启动托盘监听线程失败");
}

fn load_icon() -> Option<Icon> {
    let bytes = include_bytes!("../../../../web/static/favicon.png");
    let image = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height).ok()
}

fn fallback_icon() -> Icon {
    let size = 32u32;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let edge = x < 3 || y < 3 || x > size - 4 || y > size - 4;
            rgba.extend_from_slice(if edge { &[30, 30, 30, 255] } else { &[0, 161, 214, 255] });
        }
    }
    Icon::from_rgba(rgba, size, size).expect("构造默认托盘图标失败")
}

fn window_icon() -> Arc<egui::IconData> {
    let bytes = include_bytes!("../../../../web/static/favicon.png");
    match image::load_from_memory(bytes) {
        Ok(image) => {
            let image = image.to_rgba8();
            let (width, height) = image.dimensions();
            Arc::new(egui::IconData {
                rgba: image.into_raw(),
                width,
                height,
            })
        }
        Err(_) => Arc::new(egui::IconData {
            rgba: vec![0, 161, 214, 255],
            width: 1,
            height: 1,
        }),
    }
}

/// 深色主题与中文字体
fn setup_style(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.all_styles_mut(|style| {
        style.visuals.panel_fill = BACKGROUND;
        style.visuals.window_fill = BACKGROUND;
        style.visuals.extreme_bg_color = egui::Color32::from_rgb(16, 16, 18);
        style.visuals.faint_bg_color = egui::Color32::from_rgb(32, 32, 36);
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
    });

    setup_fonts(ctx);
}

fn setup_fonts(ctx: &egui::Context) {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".to_owned());
    let fonts_dir = std::path::Path::new(&windir).join("Fonts");
    let mut fonts = egui::FontDefinitions::default();
    // 依次尝试系统中可用的中文字体，`Deng.ttf`（等线）在 Win10/11 上必然存在
    for candidate in ["Deng.ttf", "simhei.ttf", "simsun.ttc", "msyh.ttc"] {
        let Ok(data) = std::fs::read(fonts_dir.join(candidate)) else {
            continue;
        };
        fonts
            .font_data
            .insert("cjk".to_owned(), Arc::new(egui::FontData::from_owned(data)));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(family).or_default().insert(0, "cjk".to_owned());
        }
        break;
    }
    ctx.set_fonts(fonts);
}

fn draw(app: &mut ShellApp, ui: &mut egui::Ui) {
    ui.painter().rect_filled(ui.max_rect(), 0.0, BACKGROUND);
    egui::Frame::NONE
        .inner_margin(egui::Margin::same(16))
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Bili-Sync").size(18.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(format!("v{}", version())).size(11.0).weak());
                });
            });
            ui.label(
                egui::RichText::new("B 站收藏夹自动同步")
                    .size(12.0)
                    .color(ui.visuals().weak_text_color()),
            );
            ui.add_space(14.0);

            draw_status(ui, app);
            ui.add_space(12.0);
            draw_open_button(ui, app);
            ui.add_space(14.0);
            draw_options(ui, app);
            ui.add_space(14.0);
            draw_logs(ui, app);
        });
}

fn draw_status(ui: &mut egui::Ui, app: &ShellApp) {
    let running = app.shell.is_running();
    let address = normalize_address(&app.shell.bind_address());
    egui::Frame::NONE
        .fill(ui.visuals().faint_bg_color)
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let color = if running {
                    egui::Color32::from_rgb(88, 190, 120)
                } else {
                    egui::Color32::from_rgb(220, 96, 96)
                };
                let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 4.5, color);
                ui.label(if running { "服务运行中" } else { "服务未运行" });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !address.is_empty() {
                        ui.label(egui::RichText::new(address).size(11.5).weak().monospace());
                    }
                });
            });
        });
}

fn draw_open_button(ui: &mut egui::Ui, app: &ShellApp) {
    let width = ui.available_width();
    let button = egui::Button::new(egui::RichText::new("打开管理页").size(15.0))
        .min_size(egui::vec2(width, 42.0))
        .fill(ui.visuals().selection.bg_fill);
    if ui.add(button).clicked() {
        app.open_browser();
    }
}

fn draw_options(ui: &mut egui::Ui, app: &mut ShellApp) {
    let mut autostart = app.autostart;
    if ui.checkbox(&mut autostart, "开机自动启动").changed() {
        app.actions.lock().push(Action::ToggleAutostart);
    }
    ui.label(
        egui::RichText::new("开启后开机时直接进入系统托盘，不弹出窗口")
            .size(11.0)
            .color(ui.visuals().weak_text_color()),
    );
}

fn draw_logs(ui: &mut egui::Ui, app: &ShellApp) {
    ui.label(
        egui::RichText::new("最近日志")
            .size(11.0)
            .color(ui.visuals().weak_text_color()),
    );
    let snapshot: Vec<String> = {
        let logs = app.shell.logs();
        let guard = logs.read();
        guard.iter().rev().take(80).rev().cloned().collect()
    };
    egui::Frame::NONE
        .fill(ui.visuals().extreme_bg_color)
        .corner_radius(6)
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(120.0)
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    if snapshot.is_empty() {
                        ui.label(
                            egui::RichText::new("暂无日志")
                                .size(11.0)
                                .color(ui.visuals().weak_text_color()),
                        );
                        return;
                    }
                    for raw in snapshot {
                        let (level, message) = parse_log(&raw);
                        ui.label(
                            egui::RichText::new(message)
                                .size(11.0)
                                .monospace()
                                .color(level_color(&level, ui.visuals())),
                        );
                    }
                });
        });
}

/// 日志写入缓冲区时是 JSON 结构（见 `utils::init_logger`），这里取出可读的文本与级别
fn parse_log(raw: &str) -> (String, String) {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) {
        let level = value
            .get("level")
            .and_then(|v| v.as_str())
            .unwrap_or("INFO")
            .to_owned();
        let message = value
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or(raw)
            .to_owned();
        return (level, message);
    }
    ("INFO".to_owned(), raw.trim().to_owned())
}

fn level_color(level: &str, visuals: &egui::Visuals) -> egui::Color32 {
    match level {
        "ERROR" => egui::Color32::from_rgb(226, 106, 106),
        "WARN" => egui::Color32::from_rgb(226, 178, 90),
        "DEBUG" | "TRACE" => visuals.weak_text_color(),
        _ => visuals.text_color(),
    }
}

fn is_autostart_enabled() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(RUN_KEY)
        .and_then(|key| key.get_value::<String, _>(AUTOSTART_VALUE))
        .is_ok()
}

fn set_autostart(enable: bool) -> anyhow::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(RUN_KEY)?;
    if enable {
        let exe = std::env::current_exe()?;
        let command = format!("\"{}\" --tray", exe.display());
        key.set_value(AUTOSTART_VALUE, &command)?;
    } else if key.get_value::<String, _>(AUTOSTART_VALUE).is_ok() {
        key.delete_value(AUTOSTART_VALUE)?;
    }
    Ok(())
}
