# Bili-Sync Fork 维护文档

> 本文件是本 fork 的**唯一权威维护记录**。任何对代码的改动都必须同步更新本文档，
> 尤其是「变更记录」与「给后续 AI 的操作规范」两节。
>
> 最后更新：2026-10-30

---

## 1. 这个 fork 要解决什么问题

上游 [amtoaer/bili-sync](https://github.com/amtoaer/bili-sync) 是一个**纯服务端**程序：

- 双击 `bili-sync-rs.exe` 只会弹出一个控制台窗口
- 所有交互都要在浏览器里访问 `http://127.0.0.1:12345`
- 关掉控制台窗口 = 程序停止
- 没有系统托盘、没有开机自启

本 fork 的目标是把它变成**普通 Windows 桌面软件**的使用体验：

| 需求 | 状态 |
|---|---|
| 图形界面（替代控制台窗口） | 已完成 |
| 界面内按钮跳转浏览器管理页 | 已完成 |
| 最小化 / 关闭窗口时隐藏到系统托盘 | 已完成 |
| 托盘右键菜单可彻底退出并结束所有进程 | 已完成 |
| 开机自启（启动后直接进托盘） | 已完成 |
| 深色主题 | 已完成 |

**核心约束：后台同步逻辑一行都不能改。** 所有改动都是"外壳"层面的，目的是让上游代码可以持续合并。

---

## 2. 架构：线程模型

这是理解整个改造的关键。Windows 的窗口消息循环**必须占用主线程**，而 tokio 的 `#[tokio::main]` 也想占主线程，两者冲突。解决办法是把服务挪到子线程：

```
┌──────────────────────── 主线程 ────────────────────────┐
│  eframe / egui 事件循环                                 │
│  ├─ 主窗口（状态、按钮、勾选框、日志）                    │
│  ├─ 系统托盘（TrayIcon + 右键菜单）                      │
│  └─ 每 300ms request_repaint，保证窗口隐藏后仍在跑        │
└───────────────────────┬────────────────────────────────┘
                        │ 共享 Arc<Shell>
┌───────────────────────┴────────────────────────────────┐
│  服务线程 "bili-sync-service"                           │
│  └─ 独立 tokio Runtime::block_on(run_service())         │
│     ├─ HTTP 服务（axum，内嵌前端）                       │
│     └─ 定时下载任务（video_downloader）                  │
└────────────────────────────────────────────────────────┘

        ┌─────────────────────────────┐
        │ 托盘监听线程 "bili-sync-tray" │
        │ 轮询 MenuEvent / TrayIconEvent│
        │ → 投递 Action + request_repaint│
        └─────────────────────────────┘
```

三个线程通过 `Arc<Shell>` 交换状态，通过 `Arc<Mutex<Vec<Action>>>` 传递指令。

### 为什么托盘要单独起线程

主窗口被 `ViewportCommand::Visible(false)` 隐藏后，winit 可能不再派发重绘事件。
如果托盘事件只靠 `update()` 里的轮询处理，窗口一隐藏就可能"点了没反应"。
所以托盘事件用独立线程阻塞轮询，收到后主动 `ctx.request_repaint()` 唤醒主线程。

---

## 3. 文件改动清单

### 3.1 新增文件

| 文件 | 作用 |
|---|---|
| `crates/bili_sync/src/shell.rs` | 平台无关的共享状态（退出令牌、监听地址、运行标志、日志缓冲） |
| `crates/bili_sync/src/ui/mod.rs` | **仅 Windows 编译**。图形界面、托盘、开机自启、深色主题、中文字体 |

### 3.2 修改文件

#### `crates/bili_sync/src/main.rs`（改动最大）

| 改动 | 原因 |
|---|---|
| 顶部加 `#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]` | release 构建时不弹控制台；debug 构建保留控制台便于看 panic |
| `#[tokio::main] async fn main()` → 普通 `fn main()` + `async fn run_service()` | 主线程要让给 GUI |
| 新增 `pub fn spawn_service(shell) -> Receiver<()>` | 在子线程建 tokio runtime 跑服务，结束时通过 channel 通知主线程 |
| `init()` 增加 `shell: &Shell` 参数 | 日志缓冲区改为由 `Shell` 持有，GUI 直接读同一份数据 |
| 新增 `static LOGGER_READY: AtomicBool` | **关键**：`init_logger` 内部是 `try_init().expect()`，重复调用会 panic。重启服务时必须跳过 |
| `handle_shutdown()` 增加 `token` 参数与 `token.cancelled()` 分支 | 原本只监听 `ctrl_c`，GUI 模式下没有控制台就永远收不到退出信号，会变成硬杀进程 |
| 新增 `mod shell;` 和 `#[cfg(target_os = "windows")] mod ui;` | 模块注册 |

#### `crates/bili_sync/src/task/http_server.rs`

- 函数签名增加 `shell: Arc<Shell>` 参数
- 监听成功后写入 `shell.set_bind_address(&bind_address)` 与 `shell.set_running(true)`
- **原因**：GUI 需要知道实际监听地址（配置里可能是 `0.0.0.0`，且绑定时会自动回退到默认地址），也要知道服务是否真的起来了

#### `crates/bili_sync/src/config/args.rs`

- 新增 `--tray` 参数（环境变量 `BILI_SYNC_TRAY`）
- **原因**：开机自启时用 `--tray` 启动，直接进托盘不弹窗

#### `Cargo.toml`（workspace）

新增 4 个依赖声明：`eframe 0.37`、`tray-icon 0.26`、`image 0.25`（仅 png）、`winreg 0.56`。

#### `crates/bili_sync/Cargo.toml`

```toml
[target.'cfg(target_os = "windows")'.dependencies]
eframe = { workspace = true }
tray-icon = { workspace = true }
image = { workspace = true }
winreg = { workspace = true }
```

**必须保持 `cfg(target_os = "windows")` 限定**，否则 Linux/macOS 和 Docker 构建会被 eframe 的系统依赖拖垮。

#### `web/package.json`

新增 `d3-shape`、`d3-scale` 到 devDependencies。
**原因**：上游用 bun 安装，bun 会把传递依赖提升到顶层；pnpm 严格模式下 `src/routes/+page.svelte` 里 `import ... from "d3-shape"` 解析失败，前端构建直接中断。这是**构建环境差异**，不是上游 bug。

---

## 4. 关键设计决策与理由

### 4.1 为什么内嵌 GUI 而不是做独立启动器

独立启动器（两个 exe）的好处是上游零冲突、崩溃隔离；但用户明确要求"点开就出界面"的单文件体验，且这是个人 fork，`main.rs` 冲突可以手工解决。因此选择内嵌。

### 4.2 为什么用 eframe/egui 而不是 Win32 原生或 Tauri

| 方案 | 结论 |
|---|---|
| Win32 手写窗口 | 代码量 500+ 行，UI 观感差，不值得 |
| Tauri | 需要 Node + 前端构建链，为一个按钮引入整个 webview，过重 |
| **eframe/egui** | 纯 Rust、单个 exe、编译进二进制、无需运行时，体积增加约 5MB |

### 4.3 关闭按钮的行为

- 点窗口的 `X` → `CancelClose` + `Visible(false)`，**隐藏到托盘**
- 真正退出只走托盘菜单的「退出」

这是用户明确要求的行为。

### 4.4 退出流程（不要改成 `process::exit`）

```
托盘「退出」
  → shell.request_shutdown()（取消 CancellationToken）
  → 各任务收到取消信号，停止下载、退出 HTTP 服务
  → handle_shutdown 关闭数据库连接（sqlite 需要正常关闭）
  → 服务线程结束，通过 mpsc 通知主线程
  → 主线程收到后 ViewportCommand::Close
  → 兜底：10 秒超时强制关窗口
```

**绝对不能直接 `std::process::exit(0)`**，否则 sqlite 可能留下未落盘的 WAL，长期会损坏数据库。

### 4.5 中文字体

egui 内置字体不含中文，必须手动加载系统字体。按优先级尝试：

```
%WINDIR%\Fonts\Deng.ttf     (等线，Win10/11 必有)
%WINDIR%\Fonts\simhei.ttf   (黑体)
%WINDIR%\Fonts\simsun.ttc
%WINDIR%\Fonts\msyh.ttc
```

**注意 `.ttc` 是字体集合**，ab_glyph 只支持单字体，`.ttc` 可能加载失败。所以 `.ttf` 排在前面。

### 4.6 深色主题

`egui::Visuals::dark()` + 自定义 `panel_fill = #18181b`、`extreme_bg_color = #101012`、`faint_bg_color = #202024`。

### 4.7 开机自启

写 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`，值名 `BiliSync`，内容为 `"<exe 绝对路径>" --tray`。

- 用 HKCU 而不是 HKLM：不需要管理员权限
- 用注册表而不是启动文件夹：用户可在任务管理器「启动」页统一管理
- 勾选框状态直接读注册表，不额外存配置文件

### 4.8 日志显示

`LogHelper` 本来就维护了一个 200 条的环形缓冲（给 Web UI 的 WebSocket 用）。
GUI 直接读这个缓冲，**不额外写日志文件**。

注意：缓冲区里存的是 **JSON 字符串**（`{"timestamp":...,"level":"INFO","message":"..."}`），
`ui::parse_log()` 负责解析出 `level` 与 `message`，解析失败则原样显示。

读取时先 clone 成 `Vec<String>` 再释放锁，避免持有读锁期间阻塞服务线程写日志。

### 4.9 eframe 0.37 的 API 与旧版本差异（重要）

这个版本的 eframe **不是**常见的 `App::update(ctx, frame)` 模型，踩过坑：

| 旧版本（≤0.32） | 0.37 实际写法 |
|---|---|
| `fn update(&mut self, ctx, frame)` | 拆成 `fn logic(&mut self, ctx, frame)` + `fn ui(&mut self, ui, frame)` |
| `ctx.style()` | 已移除，改用 `ctx.style_of(Theme::Dark)` 或 `ctx.all_styles_mut(...)` |
| `ctx.set_style(...)` | 已移除，改用 `ctx.all_styles_mut(\|style\| ...)` |
| `ctx.set_visuals(...)` | 仍在，但推荐 `ctx.set_theme(ThemePreference::Dark)` + `all_styles_mut` |
| `egui::Frame::new()` | 不存在，用 `egui::Frame::NONE` |
| `App::ui` 拿到的 `Ui` | **没有 margin、没有背景色**，必须自己铺底色 |

**`logic` 与 `ui` 的分工是这个方案能成立的关键**：

> While the window is hidden, eframe runs no egui pass at all, and calls `Self::logic` instead.

也就是说窗口隐藏到托盘后，`ui()` 不再被调用，但 `logic()` 仍会被调用。
所以**所有状态机逻辑（托盘指令、退出流程、关闭拦截）都必须写在 `logic()` 里**，
只有纯绘制放 `ui()`。如果误把逻辑放进 `ui()`，隐藏到托盘后程序就会"失去响应"。

---

## 5. 构建

### 5.1 前置环境

| 依赖 | 要求 | 备注 |
|---|---|---|
| Rust | **1.98.0 + MSVC 工具链** | 见下方「工具链陷阱」 |
| VS Build Tools | 2022，含 C++ 桌面开发 | 提供 link.exe |
| Node / pnpm | Node 22+ / pnpm 10+ | 构建前端 |
| FFmpeg | 需在 PATH 中 | 程序启动时会检查 |

### 5.2 完整构建命令

```bash
# 1. 构建前端（必须先做，web/build 为空会导致内嵌前端 404）
cd web
pnpm install
pnpm build          # 产物在 web/build/

# 2. 编译 Rust（release）
cd ..
cargo +1.98.0-x86_64-pc-windows-msvc build --release
# 产物：target/release/bili-sync-rs.exe
```

### 5.3 工具链陷阱（踩过的坑，务必注意）

`rust-toolchain.toml` 里只写了 `channel = "1.98.0"`，rustup 解析这个模糊版本时，
如果本机同时装过 `1.98.0-x86_64-pc-windows-gnu`，**会优先选中 GNU 版本**，
然后因为缺少 `dlltool.exe` 报错：

```
error: error calling dlltool 'dlltool.exe': program not found
```

**解决办法**：构建时显式指定工具链

```bash
cargo +1.98.0-x86_64-pc-windows-msvc build --release
```

或者卸载 GNU 版：`rustup toolchain uninstall 1.98.0-x86_64-pc-windows-gnu`。

**不要**把 `rust-toolchain.toml` 改成写死 msvc 全名，那会破坏 Linux/macOS 用户的构建。

### 5.4 产物位置

编译完成后把 `target/release/bili-sync-rs.exe` 复制到仓库根目录，用户可直接双击运行。

---

## 6. 运行行为

| 操作 | 结果 |
|---|---|
| 双击 exe | 弹出主窗口（深色），后台自动启动服务 |
| 点窗口 `X` | 隐藏到托盘，服务继续运行 |
| 点「打开管理页」 | 默认浏览器打开 `http://127.0.0.1:12345`（自动把 `0.0.0.0` 换成 `127.0.0.1`） |
| 托盘双击 | 唤回主窗口 |
| 托盘「重启服务」 | 优雅停止 → 等待退出 → 重新拉起 |
| 托盘「开机自启」 | 切换注册表项，勾选状态同步 |
| 托盘「退出」 | 停止服务 → 关数据库 → 退出进程 |
| `bili-sync-rs.exe --tray` | 启动后直接隐藏到托盘（开机自启用） |

**配置文件位置**：`%APPDATA%\bili-sync\data.sqlite`（配置也在数据库里，没有 config.toml）

---

## 7. 已知限制

1. **仅支持 Windows x86_64**。非 Windows 平台会走原来的控制台逻辑，功能不受影响，但没有 GUI。
2. **窗口隐藏后 `update()` 是否持续运行依赖 eframe 行为**。当前用 `request_repaint_after(300ms)` 保活；如果未来升级 eframe 后发现隐藏后界面不响应，检查这一处。
3. **托盘图标来自 `web/static/favicon.png`**，通过 `include_bytes!` 编译进二进制。换图标需要重新编译。
4. **单实例未做限制**。重复双击会启动第二个实例，第二个会因为端口占用而绑定失败（程序会回退到默认地址，仍可能失败）。如果要做单实例，需要引入命名互斥量。
5. **没有自动更新机制**。升级需要重新编译并替换 exe。
6. **日志不落盘**。只有内存中最近 200 条，重启后清空。排查长期问题需要临时加文件日志。

---

## 8. 上游同步流程

```bash
git remote add upstream https://github.com/amtoaer/bili-sync.git   # 首次
git fetch upstream
git merge upstream/main
```

**预期冲突文件**：`crates/bili_sync/src/main.rs`、`crates/bili_sync/src/task/http_server.rs`、
`crates/bili_sync/src/config/args.rs`、两个 `Cargo.toml`。

冲突解决原则：
1. `main.rs`：保留上游的新逻辑，把 GUI 相关的 `mod`、`spawn_service`、`run_service` 包裹层重新套上
2. `http_server.rs`：保留 `shell` 参数与两行状态写入
3. `args.rs`：保留 `--tray`
4. `Cargo.toml`：保留 4 个 GUI 依赖与 `cfg(windows)` 段落
5. 合并后**必须重新构建前端**，再编译验证

`shell.rs` 和 `ui/mod.rs` 是新增文件，不会冲突。

---

## 9. 给后续 AI 的操作规范

> 这一节是硬性约定，任何接手此 fork 的 AI 都必须遵守。

### 必须做

1. **每次改动后更新本文档的「变更记录」**，写清：改了什么、为什么改、影响了哪些文件。
2. **改动前先读本文档第 2 节的线程模型**，任何涉及主线程/服务线程职责的改动都要先评估影响。
3. **保持非 Windows 平台可构建**。新增 Windows 专属代码必须放在 `#[cfg(target_os = "windows")]` 内。
4. **构建必须用 `cargo +1.98.0-x86_64-pc-windows-msvc build --release`**，不要用裸 `cargo build`。
5. **改了前端必须重新 `pnpm build`**，否则二进制里的页面是旧的。
6. **验证退出流程**：改完退出相关代码后，手动测一次托盘「退出」，确认进程真的没了（任务管理器检查 `bili-sync-rs.exe`）。

### 禁止做

1. **禁止**在退出流程里用 `std::process::exit()` 或 `TerminateProcess` 跳过数据库关闭。
2. **禁止**移除 `LOGGER_READY` 这个一次性初始化保护，否则重启服务会 panic。
3. **禁止**把 GUI 依赖加到全局 `[dependencies]`，必须留在 `[target.'cfg(target_os = "windows")'.dependencies]`。
4. **禁止**修改 `crates/bili_sync/src/adapter/`、`downloader.rs`、`task/video_downloader.rs` 里的同步逻辑——那是上游核心，改了会大幅增加合并成本。
5. **禁止**在 UI 线程持有日志读锁期间做耗时操作（会阻塞服务线程写日志）。

### 代码风格

- 与上游保持一致：中文注释、`tracing` 宏打日志、`anyhow::Result` 做错误传播
- UI 文案用中文，不引入 emoji
- 新增的 `unsafe` 代码需在注释里说明安全性依据（当前实现**没有** `unsafe`）

---

## 10. 变更记录

### 2026-10-30 首次桌面化改造

**环境准备**
- 安装 Rust `1.98.0-x86_64-pc-windows-msvc` 工具链（`rust-toolchain.toml` 要求 1.98.0）
- 卸载冗余的 `1.98.0-x86_64-pc-windows-gnu`（缺少 dlltool 导致编译失败）
- `web/` 下执行 `pnpm install`（44s）与 `pnpm build`
- 补装 `d3-shape`、`d3-scale` 到 devDependencies（pnpm 严格模式解析失败）
- 生成了 `web/pnpm-lock.yaml`（上游用 bun，本 fork 改用 pnpm）

**代码改动**
- 新增 `crates/bili_sync/src/shell.rs`
- 新增 `crates/bili_sync/src/ui/mod.rs`
- 改造 `main.rs`：主线程让给 GUI，服务移入子线程，退出信号改用 `CancellationToken`
- 改造 `task/http_server.rs`：回写监听地址与运行状态
- 改造 `config/args.rs`：新增 `--tray`
- 改造两个 `Cargo.toml`：新增 4 个 Windows 专属依赖

**产物**
- `bili-sync-rs.exe`（release，25 MB，含 GUI 与内嵌前端）放在仓库根目录，双击即用

**实测验证结果**

| 项目 | 结果 | 验证方式 |
|---|---|---|
| 编译通过 | ✅ | `cargo +1.98.0-x86_64-pc-windows-msvc build --release`，耗时 2m32s |
| 双击出图形界面（非控制台） | ✅ | 进程 `MainWindowTitle = "Bili-Sync"`，截图确认深色界面 |
| 中文显示正常 | ✅ | 截图无方块/乱码，使用 `Deng.ttf` |
| 状态与日志实时显示 | ✅ | 界面显示「服务运行中 127.0.0.1:12345」与启动日志 |
| 托盘图标注册成功 | ✅ | `HKCU\Control Panel\NotifyIconSettings` 中存在本程序项 |
| 关闭窗口 → 隐藏到托盘 | ✅ | `taskkill`（不带 /F）发送 WM_CLOSE 后，进程仍存活且端口仍在监听，窗口已不可见 |
| `--tray` 启动直接进托盘 | ✅ | `MainWindowTitle` 为空、端口监听正常、无可见窗口 |
| 托盘菜单点击响应 | ⚠️ 未实测 | 无法脚本模拟托盘右键，需人工点一次确认 |
| 开机自启写注册表 | ⚠️ 未实测 | 需在界面勾选后检查 `HKCU\...\Run` 下的 `BiliSync` |

**踩坑记录**

1. **首次 `cargo build --release` 进程僵死**：跑到 8 分钟后 rustc 的 CPU 时间停止增长、19 分钟无新产物。杀掉进程重新构建后 2m32s 完成。怀疑与杀毒软件/文件锁有关，**不是代码问题**。若再次遇到，直接杀 `cargo`/`rustc` 重跑即可。
2. **GNU 工具链误选**：见 5.3。
3. **eframe 0.37 API 大改**：见 4.9，`update` 已被拆成 `logic` + `ui`。
