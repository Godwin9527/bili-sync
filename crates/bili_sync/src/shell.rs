//! 桌面壳与后台服务之间共享的状态。
//!
//! 该模块本身与平台无关：非 Windows 平台下由 `main` 直接驱动后台服务，
//! Windows 下则由 [`crate::ui`] 中的图形界面同时驱动界面与后台服务。

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use crate::api::MAX_HISTORY_LOGS;

/// 图形界面与后台服务共享的运行时状态。
///
/// 所有字段都做了内部可变性封装，因此可以安全地在 GUI 线程与服务线程之间共享。
pub struct Shell {
    /// 触发后台服务优雅退出的令牌，重启服务时整体替换
    shutdown: Mutex<CancellationToken>,
    /// HTTP 服务实际监听的地址，由 `http_server` 在绑定成功后写入
    bind_address: Mutex<String>,
    /// 后台服务是否处于运行状态
    running: AtomicBool,
    /// 日志环形缓冲，直接复用 `LogHelper` 的历史记录，避免重复实现
    logs: Arc<RwLock<VecDeque<String>>>,
}

impl Shell {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            shutdown: Mutex::new(CancellationToken::new()),
            bind_address: Mutex::new(String::new()),
            running: AtomicBool::new(false),
            logs: Arc::new(RwLock::new(VecDeque::with_capacity(MAX_HISTORY_LOGS + 1))),
        })
    }

    /// 取得日志缓冲区的共享句柄，交给 `LogHelper` 作为写入目标
    pub fn logs(&self) -> Arc<RwLock<VecDeque<String>>> {
        self.logs.clone()
    }

    /// 取得当前的退出令牌
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.lock().clone()
    }

    /// 替换为一个全新的退出令牌，用于重启服务
    pub fn reset_shutdown_token(&self) -> CancellationToken {
        let token = CancellationToken::new();
        *self.shutdown.lock() = token.clone();
        token
    }

    /// 请求后台服务优雅退出
    pub fn request_shutdown(&self) {
        self.shutdown.lock().cancel();
    }

    pub fn set_bind_address(&self, address: impl Into<String>) {
        *self.bind_address.lock() = address.into();
    }

    pub fn bind_address(&self) -> String {
        self.bind_address.lock().clone()
    }

    pub fn set_running(&self, running: bool) {
        self.running.store(running, Ordering::SeqCst);
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}
