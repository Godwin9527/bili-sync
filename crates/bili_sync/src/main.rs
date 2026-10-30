#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

#[macro_use]
extern crate tracing;

mod adapter;
mod api;
mod bilibili;
mod config;
mod database;
mod downloader;
mod error;
mod notifier;
mod shell;
mod task;
#[cfg(target_os = "windows")]
mod ui;
mod utils;
mod workflow;

use std::fmt::Debug;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use bilibili::BiliClient;
use sea_orm::DatabaseConnection;
use task::{http_server, video_downloader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::api::LogHelper;
use crate::config::{ARGS, CONFIG_DIR, VersionedConfig};
use crate::database::setup_database;
use crate::shell::Shell;
use crate::utils::init_logger;
use crate::utils::signal::terminate;

/// 日志订阅者只能初始化一次，重启后台服务时不能重复注册
static LOGGER_READY: AtomicBool = AtomicBool::new(false);

fn main() {
    // Windows 下交给图形界面接管，界面会在独立线程中启动后台服务
    #[cfg(target_os = "windows")]
    {
        ui::run();
    }

    // 其它平台保持原有行为：直接以控制台方式运行服务
    #[cfg(not(target_os = "windows"))]
    {
        let runtime = tokio::runtime::Runtime::new().expect("创建 tokio 运行时失败");
        runtime.block_on(run_service(Shell::new()));
    }
}

/// 在独立线程中启动后台服务，返回服务彻底结束时会收到信号的接收端。
///
/// 之所以新开线程，是因为 Windows 的窗口消息循环必须占用主线程。
pub fn spawn_service(shell: Arc<Shell>) -> std::sync::mpsc::Receiver<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("bili-sync-service".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(e) => {
                    error!("创建 tokio 运行时失败：{e}");
                    let _ = tx.send(());
                    return;
                }
            };
            runtime.block_on(run_service(shell));
            let _ = tx.send(());
        })
        .expect("启动后台服务线程失败");
    rx
}

/// 后台服务的完整生命周期：初始化 → 启动任务 → 等待退出 → 关闭数据库
async fn run_service(shell: Arc<Shell>) {
    let (bili_client, connection, log_writer) = match init(&shell).await {
        Ok(res) => res,
        Err(e) => {
            error!("初始化失败：{:#}", e);
            return;
        }
    };

    let token = shell.shutdown_token();
    let tracker = TaskTracker::new();

    spawn_task(
        "HTTP 服务",
        http_server(connection.clone(), bili_client.clone(), log_writer, shell.clone()),
        &tracker,
        token.clone(),
    );

    spawn_task(
        "定时下载",
        video_downloader(connection.clone(), bili_client),
        &tracker,
        token.clone(),
    );

    tracker.close();
    handle_shutdown(connection, tracker, token).await;
    shell.set_running(false);
}

fn spawn_task(
    task_name: &'static str,
    task: impl Future<Output = impl Debug> + Send + 'static,
    tracker: &TaskTracker,
    token: CancellationToken,
) {
    tracker.spawn(async move {
        tokio::select! {
            res = task => {
                error!("「{}」异常结束，返回结果为：「{:?}」，取消其它仍在执行的任务..", task_name, res);
                token.cancel();
            },
            _ = token.cancelled() => {
                info!("「{}」接收到取消信号，终止运行..", task_name);
            }
        }
    });
}

/// 初始化日志系统、打印欢迎信息，初始化数据库连接和全局配置
async fn init(shell: &Shell) -> Result<(Arc<BiliClient>, DatabaseConnection, LogHelper)> {
    let (tx, _rx) = tokio::sync::broadcast::channel(30);
    // 日志缓冲区由 Shell 持有，图形界面直接读取同一份数据
    let log_writer = LogHelper::new(tx, shell.logs());

    if !LOGGER_READY.swap(true, Ordering::SeqCst) {
        init_logger(&ARGS.log_level, Some(log_writer.clone()));
    }
    info!("欢迎使用 Bili-Sync，当前程序版本：{}", config::version());
    info!("项目地址：https://github.com/amtoaer/bili-sync");

    let ffmpeg_path = ARGS.ffmpeg_path.as_deref().unwrap_or("ffmpeg");
    let ffmpeg_exists = Command::new(ffmpeg_path)
        .arg("-version")
        .output()
        .await
        .map(|output| output.status.success())
        .unwrap_or(false);
    if !ffmpeg_exists {
        bail!("ffmpeg 不存在或无法执行，请确保已正确安装 ffmpeg，并且 {ffmpeg_path} 命令可用");
    }

    let connection = setup_database(&CONFIG_DIR.join("data.sqlite"))
        .await
        .context("数据库初始化失败")?;
    info!("数据库初始化完成");
    VersionedConfig::init(&connection).await.context("配置初始化失败")?;
    info!("配置初始化完成");

    Ok((Arc::new(BiliClient::new()), connection, log_writer))
}

async fn handle_shutdown(connection: DatabaseConnection, tracker: TaskTracker, token: CancellationToken) {
    tokio::select! {
        _ = tracker.wait() => {
            error!("所有任务均已终止..")
        }
        _ = token.cancelled() => {
            // 图形界面（托盘菜单 / 重启服务）触发的退出路径
            info!("接收到退出信号，开始终止任务..");
            tracker.wait().await;
            info!("所有任务均已终止..");
        }
        _ = terminate() => {
            // 控制台模式下的 Ctrl+C
            info!("接收到终止信号，开始终止任务..");
            token.cancel();
            tracker.wait().await;
            info!("所有任务均已终止..");
        }
    }
    info!("正在关闭数据库连接..");
    match connection.close().await {
        Ok(()) => info!("数据库连接已关闭，程序结束"),
        Err(e) => error!("关闭数据库连接时遇到错误：{:#}，程序异常结束", e),
    }
}
