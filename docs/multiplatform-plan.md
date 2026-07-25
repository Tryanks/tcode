# tcode 多端预案：移动端、Web 端与远程协同

本文是移动 app（Android/iOS）、wasm 网页版、多端同步三项工作的**总预案**。
它记录已验证的事实、架构决策、分期与闸门、风险与退出条件。

结论先行：**这三件事不是三个并列的功能，而是一条强制串行的依赖链。**
同步层是另外两者的地基，不是收尾。

侦察基准：zed rev `1a246efd7e1b83ab568ec5e3e6c1a43a42e1abba`（2026-07-16，zed
v1.13.0，gpui 0.2.2），gpui-component rev `0315556`。本文所有事实性断言均在
该基准上实测得出；与代码不符时，**明确地**修正其中之一。

---

## 1. 决定一切的那条事实

**手机和浏览器上永远跑不了 agent。**

tcode 的核心机制是 spawn 本地 CLI 子进程并用 stdio 管道对话。五个 provider
全部如此：

| provider | 机制 | 位置 |
| --- | --- | --- |
| Claude | `claude --print --input-format stream-json` + 三条管道 | `crates/agent/src/claude.rs:119` |
| Codex | `codex app-server` + NDJSON 管道 | `crates/agent/src/codex.rs:642` |
| pi | `pi --mode rpc` + LF 分帧 JSON | `crates/agent/src/pi.rs:224` |
| OpenCode | spawn `opencode serve` + 回环 TCP + SSE | `crates/agent/src/opencode.rs:1247` |
| ACP | 子进程 stdio 上的 JSON-RPC，且**代其执行命令** | `crates/agent/src/acp.rs:137`、`:2560` |

iOS 禁止 `fork`/`exec` 且无第三方 CLI 安装模型；wasm 根本没有进程概念。
`crates/term` 的 PTY + 登录 shell + `tcgetpgrp`（`term/src/lib.rs:322`）、
`services/git.rs:63` 的 shell-out git、`acp_registry.rs:291` 的
下载→解压→chmod→执行，同理全部不可移植。

所以移动端与 Web 端**只能是远程客户端**，桌面机（或用户自己的服务器）当 host。

```
多端同步（host/client 拆分）
   ├──► 移动端 app（Android / iOS）──► 移动端 UI 完善
   └──► wasm 网页版
```

任何"先做移动端、同步以后再补"的排期都会在第一周撞墙：没有 host，移动端
app 打开后是一个什么都做不了的空壳。

---

## 2. 好消息：三个前提条件都已经成立

侦察推翻了三个原本以为要从零做的假设。

### 2.1 上游已有 wasm 后端

`crates/gpui_web/` 是一个 3010 行的可运行 `WebPlatform`：异步初始化 WebGPU、
创建 canvas 与隐藏 input、跟踪 DPR/resize、`requestAnimationFrame` 驱动帧、
翻译键鼠/滚轮/拖放事件、经 `gpui_wgpu` 渲染
（`gpui_web/src/platform.rs:60`、`window.rs:73`）。基于隐藏 `<input>` +
DOM composition 事件的 **IME 是真的**（`window.rs:119`、`events.rs:465`）。
`gpui_platform` 已在 `cfg(target_family = "wasm")` 下分发到它。
上游 CI 会对 wasm 做编译检查（`.github/workflows/run_tests.yml:663`）。

**但它是一个有分量的 spike，不是生产级。** 已确认的缺口：

- `Platform` 桩：`quit`/`restart`/`activate`/`hide`/`reveal_path`/`set_menus`
  等为空操作；文件对话框与可执行路径返回错误（`platform.rs:138`、`:209`）。
- 剪贴板读取恒为 `None`；凭据读 `None`、写/删返回错误（`platform.rs:341`、`:355`）。
- URL/reopen/menu/thermal/layout 回调**存了但无人触发**（`platform.rs:48`）。
- `PlatformWindow` 桩：`prompt`、最小化、缩放、IME 位置、窗口装饰（`window.rs:566`）。
- **触摸被无条件转成鼠标事件**，不检查 `pointerType`（`events.rs:133`）。
- dispatcher 有 5 处未决的 `TODO-Wasm`，含邮箱同步与优先级调度（`dispatcher.rs:31`）。
- **`crates/gpui_web` 下没有任何 `#[test]` 或浏览器测试。**

### 2.2 上游已有可移植渲染器与文字系统

`crates/gpui_wgpu/`（4081 行）提供 `WgpuContext`/`WgpuRenderer`/`WgpuAtlas` 与
`CosmicTextSystem`（cosmic-text 分词整形 + swash 度量光栅）。Linux 的 X11/Wayland
后端已经在用它（`gpui_linux/src/linux/x11/window.rs:1703`）。

GPUI **没有渲染器 trait**；抽象边界就是 `PlatformWindow::draw(&Scene)` 加
`sprite_atlas()`（`gpui/src/platform.rs:750`）。原生构造函数接受任意
`HasWindowHandle + HasDisplayHandle`（`wgpu_renderer.rs:176`）。

**关键信号**：`WgpuRenderer` 里已经有 `unconfigure_surface`/`replace_surface`，
注释明写是为 Android 原生窗口销毁/重建准备的（`wgpu_renderer.rs:1690`）。

也就是说 **Android 后端不需要重写渲染，也不需要重写文字排版**。这正是
`gpui-mobile` 当年手搓的那套（wgpu + cosmic-text + swash）——上游现在自带了。

### 2.3 上游正在主动为移动端做准备

`gpui::Platform` / `PlatformWindow` 里已有成体系的移动端词汇：

- `AppLifecyclePhase { Active, Inactive, Background, Foreground }`，文档里带
  **iOS/Android 回调对照表**（`platform.rs:647`）。
- `WindowInsets { safe_area, ime }`，注释直接点名 iOS `safeAreaInsets` 与
  Android `WindowInsets.Type.ime()`（`platform.rs:672`）。
- `on_memory_warning`，点名 `didReceiveMemoryWarning` / `onTrimMemory`（`platform.rs:211`）。
- `TouchEvent { id, phase, position, force }`、`TouchId`、`TouchPhase`、
  `PlatformGestures`（`interactive.rs:85`）。
- Android 返回键、软键盘显隐、文本状态通知（`platform.rs:822`）。

**这些绝大多数带默认实现，且核心尚未接线**：`App::new_app` 注册了键盘布局、
热状态、退出回调，但**没有注册生命周期与内存回调**（`app.rs:858`）；
`TouchEvent` 自己的文档写着分发"实现待定"，窗口层记录了触摸模态却不派发
（`window.rs:4721`）；`insets`/`on_insets_changed` 在声明之外找不到消费者。

**判读**：Zed 正在把 GPUI 往移动端推，词汇先落地、接线在后。这对我们是**顺风**，
但也意味着**移动端 trait 表面是移动靶**。战略含义见 §7。

### 2.4 gpui-component 官方支持 WASM

就在 tcode 当前 pin 的 rev 上：`crates/story-web` 存在，README 支持矩阵写
`Web | Yes (WASM)`，非 wasm 依赖已做 target 门控。代价：**tree-sitter 在 wasm
下被整体去掉**——但 tcode 用的是 syntect，不受影响。

---

## 3. 目标架构

### 3.1 拆分接缝

侦察确认接缝**已经天然存在**，不需要发明：

```rust
// crates/agent/src/lib.rs:615
pub struct SessionHandle {
    pub provider: ProviderKind,
    pub commands: async_channel::Sender<SessionCommand>,
    pub events:   async_channel::Receiver<AgentEvent>,
}
```

五个 provider 全部归一化进同一个 `AgentEvent` 流（32 个变体，
`lib.rs:652`），且**已经是 `Serialize + Deserialize`、`#[serde(tag="type")]`**。
`SessionCommand`（`lib.rs:548`）只差两个 derive，payload 类型均可序列化。
`Timeline::fold_events`（`core/src/session.rs:558`）是纯 reducer，客户端可以
原样重放出与本地完全一致的 Timeline。

**线协议基本是白送的。**

### 3.2 crate 布局

区分两层，这一点不能混：**GPUI 后端是通用的、可上游化的；app 外壳是 tcode 专属的。**

```
crates/
  # ── 现有，需要拆分 ──
  agent/          → agent-protocol（纯类型，全平台） + agent-local（spawn，桌面）
  services/       → services-core（纯逻辑） + services-desktop（fs/git/进程）
  runtime/        → runtime-core（状态与 fold） + runtime-host（进程/PTY/存储属主）
  ui/             → ui-core（领域渲染，复用） + ui-desktop + ui-mobile
  term/           → 不动，永远桌面专属
  {preview,orchestrate,computer-use}-mcp/ → 逻辑与 HTTP 服务器分离

  # ── 新增：同步层 ──
  sync-protocol/  纯类型 + 编解码，全平台
  sync-host/      桌面侧服务端
  sync-client/    客户端，全平台（含 wasm）

  # ── 新增：GPUI 后端（上游候选，与 tcode 无关）──
  gpui-android/   Platform 实现 · NDK/NativeActivity · JNI
  gpui-ios/       Platform 实现 · UIKit · Metal

  # ── 新增：app 外壳 ──
  tcode-android/  cdylib + Gradle 工程
  tcode-ios/      staticlib + Xcode 工程
  tcode-web/      cdylib + Trunk
```

把 `gpui-android`/`gpui-ios` 和 `tcode-android`/`tcode-ios` 分开，是为了让后端
能独立于 tcode 演进、独立于 tcode 被上游接纳。合在一起会永久绑死。

### 3.3 同步模型：单写者，不做 CRDT

**host 是唯一权威。客户端是瘦的。**

- host → client：带**单调 `seq`** 的 `AgentEvent` 流（游标续传）。
- client → host：`SessionCommand` + 新增的 `RuntimeCommand`（app 级操作：开线程、
  归档、git、设置……候选面在 `AppState` 的 209 个 public 方法里，需筛选）。
- client 本地只保留纯展示态（滚动位置、面板开合、草稿文本）。

**明确不做 CRDT、不做离线编辑合并。** 客户端离线时排队用户输入并在重连后重放，
冲突就报错让用户重试。做 CRDT 会让工程量翻倍且收益存疑——agent 会话本质上是
单写者的。

### 3.4 必须补的两个洞

1. **没有序列号。** 事件信封只有 Unix 毫秒 `ts`，且代码明确容忍时间戳相等或
   回退（`services/src/store.rs:25`）——**时间戳不构成严格序**。必须加持久化的
   per-session 单调 `seq`，否则断线续传无从谈起。这是对现有 JSONL 格式的
   向后兼容扩展（旧行 `seq` 缺省按文件位置补齐）。

2. **机器局部状态泄漏进持久化结构。** 需要引入 host 相对路径 + workspace 标识：

   | 字段 | 位置 |
   | --- | --- |
   | `Project.root` | `core/src/project.rs:14` |
   | `SessionMeta.cwd` | `core/src/project.rs:47` |
   | `WorktreeInfo.root_project_path` | `core/src/project.rs:70` |
   | provider `binary_path` / `home_path` | `core/src/settings.rs:84` |
   | `AcpLaunch::Binary.command` | `agent/src/lib.rs:80` |
   | `Attachment.source_path` | `agent/src/lib.rs:435` |
   | `ApprovalKind::ExecCommand.cwd` | `agent/src/lib.rs:965` |
   | `ResumeCursor`（provider 私有 JSON，绑定 host 安装/账号） | `agent/src/lib.rs:110` |

---

## 4. 明确的非目标（范围围栏）

写下来是为了以后有人想加时，必须显式推翻它。

- **移动端/Web 端不跑本地 agent。** 永远。
- **手机上没有可交互终端。** 最多只读回滚屏。原因：无键盘时的原始按键透传 +
  h/v 分屏是死路（`terminal_drawer.rs:402`、`:1503`）。
- **手机上没有内嵌 WebView 预览。** 它是 GPUI 不合成的原生 WKWebView，Linux 上
  本来就已经是占位符；手机上交给系统浏览器。
- **不做离线编辑与冲突合并。**
- **移动端不做 computer-use。** 后端只有 macOS 有实现，其余平台是恒返回
  unsupported 的桩（`computer-use-mcp/src/backend.rs:9`）。
- **不 vendor / fork GPUI。** 见 §7。
- **手机不做代码编辑器。** tcode 是 agent 客户端，不是移动 IDE。

---

## 5. 分期与闸门

每一期都以**可运行的东西**结束，而不是以"重构完成"结束。每期入口有 gate，
不过 gate 不进下一期。

### 期 0 — 地基与止血（无用户可见变化）

1. **锁死 gpui rev。** 现在 `crates/{app,runtime,ui}/Cargo.toml` 的 gpui git
   依赖**没有任何 `rev`**，只有 Cargo.lock 兜着——一次 `cargo update` 就会把
   GPUI 静默跳到 master。移植期间这是不可接受的。显式 pin，并定一个升级节奏
   （建议双周，单独 PR，单独跑全量 gate）。
2. **修 `cfg(not(target_os = "linux"))`。** `crates/ui/Cargo.toml:29` 用它门控
   `gpui-wry`/`wry`，这个条件把 **Android/iOS/wasm 全算成了桌面**。改成显式
   列举 macos/windows。移植第一天就会被它绊倒。
3. 建立 `cargo check --target wasm32-unknown-unknown` 与
   `aarch64-linux-android` 的 CI 任务（**允许失败**，只作为进度计分板）。
4. **注意 wasm 工具链约束**：上游 CI 用的是
   `cargo -Zbuild-std=std,panic_abort check --target wasm32-unknown-unknown`
   加 `-C target-feature=+atomics,+bulk-memory,+mutable-globals`，即
   **需要 nightly + build-std**，且多线程模式依赖 SharedArrayBuffer
   → **部署页面必须带 COOP/COEP 响应头**。这条要在期 0 就确认能接受。

**Gate 0**：pin 生效；wasm/android 的 check 任务能跑起来并输出一份错误清单
（此时必然大量失败，这是基线）。

### 期 1 — 协议与 host（最大的一块）

1. `sync-protocol`：给 `SessionCommand` 加 derive；定义带 `seq` 的事件信封；
   定义 `RuntimeCommand`；定义握手/鉴权/游标续传。
2. 存储加 `seq`（向后兼容）。
3. 拆 `agent` → protocol / local。`start_session` 增加 `Remote` 分支。
4. `sync-host`：**独立 crate，不依赖 GPUI**（这样期 1.5 的无头 `tcode-server`
   只需换一个 `main` 即可复用）。桌面进程内起服务端，鉴权（配对码 + 长期
   token），WebSocket 传输（axum 0.8 自带 `ws`，无新增重依赖）。
5. 路径与机器身份的规范化（§3.4 第 2 条）。
6. **协议版本号 + 握手兼容性协商**——这是多端版本漂移的正解（见 §8.1）。

**Gate 1**：两个**桌面** tcode 实例，一个当 host、一个当 client，
client 能实时看到 host 上跑的会话并成功发送一个 turn、批准一次权限请求。
这一步完全不碰 GPUI，用桌面验证协议——**这是整个计划里最重要的一个 gate**。

### 期 1.5 — 无头 tcode-server（可与期 2 并行）

复用期 1 的 `sync-host` crate，只换一个 `main` + 配置/日志/进程管理。
需要额外解决的：provider CLI 在服务器上的安装与鉴权路径、无 GUI 环境下的
`shell_env::import_login_shell_environment` 等价物、升级策略。

**Gate 1.5**：`tcode-server` 跑在一台远程机器上，桌面 client 连上去完成
读/发/批准。用户笔记本可以合盖。

### 期 2 — Web 客户端（第一个真正的新端）

1. `services`/`runtime` 拆分，把 fs/进程/git 关到 `-desktop` 后面。
2. `tcode-web` + Trunk 构建。
3. `ui` 拆出 `ui-core`。**注意**：`chat.rs`(5156 行) 与 `composer.rs`(5139 行)
   把领域渲染和外壳布局**交织在同一批 `render_*` 函数里**——`markdown/*` 和
   `diff/model.rs` 已经分得很干净，这两个没有。**拆这两个文件是整个 UI 移植的
   前置闸门**，也是"40% 可复用"是真数字还是纸面数字的分水岭。
4. 补 `gpui_web` 的缺口（剪贴板、文件、触摸类型判别），能上游的上游。

**Gate 2**：浏览器里打开一个 host 上的活跃会话，能读、能发 turn、能批准。
桌面布局，不做响应式。

### 期 3 — Android

1. `gpui-android`：`Platform`/`PlatformWindow`/`PlatformDispatcher`/`PlatformDisplay`
   实现，复用 `gpui_wgpu` 的 `WgpuRenderer` + `CosmicTextSystem`。
   参考骨架用 `gpui_web`（架构最接近），不用 Linux headless。
2. Activity 生命周期 ↔ `AppLifecyclePhase`；surface 销毁/重建 ↔
   `unconfigure_surface`/`replace_surface`。
3. 触摸合成为鼠标/滚轮/pinch（因为 `TouchEvent` 的核心分发上游尚未实现）。
4. 软键盘 + `WindowInsets`；IME 桥接 `PlatformInputHandler`。
5. `tcode-android` cdylib + Gradle。

**Gate 3a（渲染证明）**：Android 设备上开出窗口并渲染一帧。
最小可行后端所需的非桩方法：执行器、text system、`run`、display 发现、
`open_window`、raw handle、bounds/scale、input handler 存储、
`request_frame`/input/resize/close 回调存储、`draw`、`sprite_atlas`、
以及**由原生 vsync 回调驱动 `on_request_frame`**——最后这条漏了会开出窗口但
永不绘制。

**Gate 3b**：手机上跑通期 2 的同一条端到端路径（读/发/批准）。

### 期 4 — iOS

同期 3，外加一个**已确认的额外阻塞**：
`gpui_wgpu` 的原生 instance 只启用 `Backends::VULKAN | GL`，**没有 METAL**
（`wgpu_context.rs:183`）。iOS 无法靠"只提供 raw handle"复用它。
这是一个小改动（加 Metal 后端选择），**优先走上游 PR**；上游不接则本地补丁。

**Gate 4**：iOS 设备上跑通同一条端到端路径。

### 期 5 — 移动端 UI

现在才动 IA。已量化的现状：

- **161 处 hover/tooltip**（59 + 102），集中在 `composer.rs`(18+21) 与
  `chat.rs`(11+10)。触摸端全部失效。
- **⌘K 命令面板在手机上完全无法触达**：全 crate 只有 8 个键绑定，其余动作都有
  指针入口，唯独 palette 只能靠 `on_action`（`shell.rs:318`、`main.rs:296`），
  没有任何按钮能打开它。它还兼任线程搜索/切换——这是导航黑洞。
- **侧栏折叠态是 hover 触发的浮层**（`shell.rs:533`，12px 左边缘条），触摸无对应手势。
- **`main.rs:442` 写死 `window_min_size 900×600`**。
- 大量写死宽度：`palette.rs:492` 640px、`add_project_dialog.rs:48` 680px、
  `settings_page.rs:341` 620px、`shell.rs:126` 右栏 560px、侧栏 255px
  （单它就吃掉 390pt 手机宽度的 65%）、`diff/view.rs:981` 分栏最小 190+180px。

**IA 决策：**

- 手机：**底部 tab（Threads · Chat · Changes · More）+ 每 tab 内导航栈 +
  bottom sheet 承载决策**。理由：本 app 恰好只有约 3 个同级目的地和 1 个主导
  目的地（chat），tab 让主导面永远一次拇指点击可达，抽屉做不到；
  批准/提交/选 provider 这类瞬时决策形状天然适配 bottom sheet。
- chat + composer 是首屏，app 启动即进。
- diff 改成**全屏 unified**，杀掉 2-up（380pt 分栏塞不进 390pt）。
- 命令面板 → **Search tab**，从隐藏模态升为一等导航。
- 批准面板 → bottom sheet，Approve/Reject 拇指区大按钮。这是最时间敏感的交互。
- hover 类可供性 → 每行常驻 `⋯` 溢出按钮 + 长按菜单。**不允许任何 hover-only。**
- **平板：适配桌面布局，不是放大手机布局。** 1024pt 放得下 sidebar+chat，
  diff 并排在平板上是真有用。三栏保留，可拖拽把手换成 2–3 个断点。
  **已定为手机之后**（§8）——它是这三条里最便宜的一条，留作手机 IA 定型后的
  快速追加。

**手机独有的新能力**（桌面不需要）：需批准/turn 完成的**推送通知**（桌面假设
你在看，手机假设你不在）、后台挂起与重连、拇指可达的发送/批准、语音输入、
连接状态横幅。

**复用估算**（基于实测 `crates/ui/src` = **36,929** 行）：

| 桶 | 约 LOC | 占比 |
| --- | ---: | ---: |
| 可直接复用（markdown 引擎 6153 + diff model/parse/algorithm + 高亮/格式化等纯逻辑） | ~15,000 | ~40% |
| 外壳需重写（shell/sidebar/settings/各类 dialog + chat/composer/diff-view 的布局半边） | ~17,000 | ~46% |
| 直接砍掉（terminal_drawer 2239 + preview_panel 1004 + window_caption 283 + 胶水） | ~4,000 | ~11% |

> 此表按模块行数分桶得出，`chat.rs`/`composer.rs`/`diff/view.rs` 三个混合文件
> 按 50/50 拆分——这是全表**最不可靠的一个数字**，且它成立的前提正是期 2 第 3 项
> 的拆分重构。

**Gate 5**：手机上完成一次真实工作流：收到批准推送 → 打开 → 读 diff → 批准 →
发下一个 turn。

---

## 6. 风险台账与退出条件

| 风险 | 判据 | 预案 |
| --- | --- | --- |
| **GPUI 移动端表面漂移**（上游正在活跃改动） | 双周升级 PR 的修复成本 | 若单次升级修复 > 2 天，改为季度升级并接受落后 |
| **`gpui_web` 不够生产级**（0 测试、5 个 TODO-Wasm、回调空转） | 期 2 中缺陷密度 | 补齐并上游；上游不接则维护薄补丁层（**不 fork 整个 GPUI**） |
| **iOS Metal 未启用**（已确认） | 上游 PR 是否被接纳 | 本地补丁 `gpui_wgpu`，成本可控 |
| **`AppState` 上帝对象**（69 字段、impl 块 7,416 行、209 个 public 方法） | 期 1 中 `RuntimeCommand` 面的抽取难度 | 只抽取移动端**真正需要**的子集，桌面路径保持直调不变 |
| **`chat.rs`/`composer.rs` 拆不开** | 期 2 第 3 项耗时 | **这是硬闸门。** 若拆分失败，手机端 UI 只能整体重写，复用率从 40% 掉到 ~15%，期 5 工作量翻倍——此时应重新评估是否继续 |
| **wasm 需 nightly + build-std + COOP/COEP** | 期 0 确认 | 若部署环境给不了 COOP/COEP，退到 `single_threaded_web()` 单线程模式，接受性能损失 |
| **wasm 包体积** | **已实测，见 §6.1** | **已排除"降级为只读"这条退路**（§8 定为完整客户端）。预案：`wasm-opt -Oz` + brotli 预压缩 + 剥离 syntect 语法集与 `image` 多余解码器 + 代码分割/懒加载。若仍不可接受，退到"首屏骨架 + 渐进加载"，而非砍功能 |
| **触摸分发上游未实现** | 期 3 手感 | 合成鼠标事件（`gpui_web` 已是此路线）；滚动手感不达标则自研惯性层 |
| **移动端 IME/软键盘不可用** | 期 3a 之后立刻验证 | **这是移动端最大的未知。**必须在期 3 早期用一个"能打字的输入框"独立验证，不要等到期 5 |

### 6.1 wasm 体积：已实测

在本 rev 上实际构建两个代理产物测得（非估算）。tcode 自身尚不能编译到 wasm，
故取上下界：`hello_web` 是 GPUI + `gpui_wgpu` + cosmic-text 的**地板**，
`story-web` 是**最接近 tcode 依赖面的代理**。

| 产物 | 裸 wasm | `wasm-opt -Oz` | gzip -9 | **brotli -q 11** | 首屏总载荷（含 JS 胶水与资源） |
| --- | ---: | ---: | ---: | ---: | ---: |
| `hello_web`（地板） | 9.60 MB | 7.24 MB | 2.84 MB | **1.93 MB** | 1.94 MB (br) |
| `story-web`（代理） | 31.55 MB | 20.97 MB | 7.55 MB | **5.18 MB** | 5.22 MB (br) |

工具链：rustc 1.99.0-nightly、trunk 0.21.14、wasm-bindgen 0.2.126/0.2.121、
binaryen 131、brotli 1.2.0。两个 `-Oz` 产物均通过 `wasm-dis` 复核可解析。

**对"能否嵌入桌面 app"的结论：取决于嵌未压缩还是嵌预压缩。**

- 嵌 `-Oz` 未压缩：**21 MB 起，超出 10MB 预算**，且 tcode 只会更大。
- 嵌 **brotli 预压缩产物并以 `Content-Encoding: br` 直接吐出**：**5.2 MB**，
  tcode 预计落在 6–8 MB，**在 10MB 预算之内**。浏览器本来就要解压，服务端
  不需要持有原始字节。

tcode 相对 `story-web` 的增量：syntect 语法/主题 dump、`image` 的六个解码器
（`ui/Cargo.toml:17`）、markdown 引擎、打包字体与主题；同时会去掉 story 的
演示内容。**前两项是最肥也最容易砍的**——wasm 构建只需保留实际用到的语法集
与 png/jpeg。

两点必须记住的前提：

1. 以上均为**多线程构建**（`+atomics` + `--shared-memory`），部署需 COOP/COEP
   响应头。切到 `single_threaded_web()` 会更小且无需特殊响应头，代价是性能——
   **卡在预算边缘时这是第一个该拉的杠杆**。
2. **上游两个 wasm 完整构建目前都不是开箱即用的**，实测均需绕过：`hello_web`
   缺 `--export=__heap_base` 导致 Trunk 线程注入失败；`story-web` 需要
   `getrandom` 的 `js` feature（经 `rand 0.8` 传入），外加一个 `psm` 的 git
   patch 不可达问题。**注意 `getrandom` 这一类错误在本仓库的 wasm 基线检查中
   同样出现**（见 §6.2）——这是期 2 会反复遇到的一类工作，不是个例。

### 6.2 移植基线：已实测

期 0 就地测得的 `wasm32-unknown-unknown` 首个基线（`cargo check`，非 build-std）：

| crate | 错误数 | 备注 |
| --- | ---: | --- |
| `tcode-i18n` | **0** | 干净通过，验证了"直接可移植"的判定 |
| `tcode-core` | 2 | 经 `agent` 传递 |
| `agent` | 7 | `ring` 构建脚本、`getrandom` 缺 `js`、`errno`/`polling` 不支持该 OS |
| `tcode-services` | 7 | 同上量级 |

错误性质集中在**传递依赖的 target 支持**，而非 tcode 自身逻辑——这与 §2 的
判断一致，也说明这些数字会随依赖门控快速下降。
`.github/workflows/port-scoreboard.yml` 会持续跟踪这个数列。

**整体退出条件（要不要继续做）：**

- **Gate 1 拿不下**（两个桌面实例互联失败）→ 停。协议都跑不通，端就别谈了。
- **Gate 3a 拿不下**（Android 开不出窗口渲染不出帧）→ 移动端搁置，
  只交付 Web 端（Web 已被上游 CI 覆盖，风险低一个量级）。
- **两次分期计划都失败** → 停下来重新评估整体可行性，而不是加人。

---

## 7. 上游策略：跟随，不分叉

参考项目的教训：

- **`gpui-mobile`**（itsbalamurali）：pin 在另一个 zed rev，自己搓
  wgpu + cosmic-text + swash——**正是上游 `gpui_wgpu` 现在提供的东西**。
  它的参考价值在**平台胶水层**（NDK/UIKit 桥接、生命周期、触摸），
  **不在渲染层**。
- **`gpui-toolkit`**（pierreaubert）：pin 在 **zed v1.9.0**，且
  **vendor 了 19 个 zed crate（含 gpui 本体）**。整体采用等于把 tcode 从
  v1.13.0 倒退回 v1.9.0 并接管一个 fork。**代价不可接受。**
  只作为 `gpui-ios`/`gpui-android` 的实现参考。

**我们的策略**：显式 pin rev + 定期升级；把 `gpui-android`/`gpui-ios` 写成
**干净的、与 tcode 无关的**后端 crate，主动向上游投递。上游正在为移动端铺路
（§2.3），这是投递窗口期。被接纳则维护成本归零，不被接纳我们也只维护两个
独立 crate，而不是一个 GPUI 分叉。

依赖可达性已确认：`gpui_web`/`gpui_wgpu` 都是 workspace 包，有公开库根与公开
入口类型，下游可按 git 依赖。但两者继承 `publish = false`，**不会上 crates.io**；
且 `gpui_platform::current_platform` **没有 Android/iOS 分支**
（`gpui_platform.rs:35`），移动端 crate 应直接依赖 `gpui` + `gpui_wgpu`，
绕开该 facade。

---

## 8. 已定决策

| 议题 | 决定 | 影响 |
| --- | --- | --- |
| **host 形态** | **两者都做，内嵌优先。** 期 1 用桌面内嵌 host 拿下 Gate 1，但 `sync-host` 从第一天就写成**不依赖 GPUI 的独立 crate**；无头 `tcode-server` 作为期 1.5 复用同一套代码，只换一个 `main`。 | 内嵌的增量体积近乎为零——app 已内嵌三个 MCP 服务器，`axum` + `tokio` 早已链入（当前 release 二进制 30.3 MB），axum 0.8 自带 `ws` feature，无新增重依赖。 |
| **Web 端定位** | **完整客户端**（读 / 发 turn / 批准 / 看 diff），不是只读查看器。 | 包体积风险**不能**再靠"降级成只读"化解。期 2 必须实测首屏体积并准备代码分割/懒加载预案。见 §6。 |
| **手机 vs 平板** | **手机优先**，平板靠后。 | 与"先拿便宜胜利"的建议相反，但有真实好处：**把 `chat.rs`/`composer.rs` 的拆分硬闸门与触摸/IME 的真实手感提前引爆**。坏消息早来好过晚来。代价是更长时间没有可展示成果。 |
| **wasm 资产分发** | **独立静态托管**（GitHub Pages / CDN），桌面二进制不托管 wasm。 | 桌面二进制不受影响。纯内网场景需自行搭静态服务。 |
| **桌面 UI 是否也由 wasm 驱动** | **否。** 见 §8.1。 | UI 一致性改由源码共享 + 协议版本协商保证。 |

### 8.1 被否决的方案：桌面 UI 也加载同一份 wasm

提案：桌面 app 内嵌 wasm 运行时（不用 webview）执行与浏览器端相同的 UI wasm，
实现"一份 UI 产物跑所有端"。

**否决，理由如下（均已实测核实）：**

1. **浏览器那份 wasm 不可移植。** `gpui_web` 深度绑定 wasm-bindgen / js-sys /
   web-sys（仅 `window.rs` + `platform.rs` 两文件即 42 处引用），示例经 Trunk
   以 `data-bindgen-target="web"` 构建。该模块导入的是 wasm-bindgen 生成的
   JS shim 函数面。在 wasmtime 中运行它 = 用 Rust 实现一个无头浏览器
   （canvas / WebGPU / DOM 事件 / ResizeObserver / 剪贴板）。且 wasm-bindgen
   ABI 跨版本不稳定，生成的 JS 胶水不可省略。
2. **改走"自定义 UI 协议 + GPUI 留在宿主侧"也不成立。** 唯一抽象边界是
   `PlatformWindow::draw(&Scene)`，而 `Scene`（`gpui/src/scene.rs:41`）的
   `paint_operations` / `primitive_bounds` / `layer_stack` 为私有或 `pub(crate)`，
   整个类型**无 `Serialize`**，且 `monochrome_sprites` / `polychrome_sprites`
   引用的是**图集贴片句柄（GPU 常驻状态）**。跨边界传 Scene 需连纹理图集共享
   一并解决。该类型不是为跨进程设计的。
3. **反向放大体积问题。** 该方案要求桌面内嵌 wasm 运行时（wasmtime 数 MB），
   比托管一个 wasm blob 更重——而提案的出发点正是控制桌面二进制体积。
4. **iOS 大概率过不了审。** App Store 审核指南 2.5.2 限制下载并执行可执行代码；
   自带运行时执行下载来的模块历史上属高风险被拒区。

**它识别的真问题是有效的**：多端 UI 版本漂移。改用便宜解法——
**协议版本号 + 握手时兼容性协商**，不兼容即明确拒绝并提示升级（成本约几百行）。
而"一份 UI 代码跑所有端"**已经免费拥有**：tcode 全栈 Rust，`ui-core` 的**源码**
同时编译到 macOS / Windows / Linux / Android / iOS / wasm，源码级复用已提供
100% 的代码共享收益。二进制级共享额外只买到 UI 热更新，而那在 iOS 上本就是禁区。

若日后仍要推进，作为**独立探索性专题**排期，不进主线关键路径，且须先完成
最小可行性验证。

---

## 9. 仍待拍板

1. **鉴权与网络模型？** 局域网直连 + 配对码最简单；广域网需要中继或打洞。
   先做局域网、后加中继，还是一开始就上中继？
2. **iOS 分发？** App Store 会不会因"远程执行代码"条款有麻烦（注意：即使按
   §8.1 否决了 wasm 方案，"手机遥控桌面执行命令"本身也可能触碰审核红线），
   还是只走 TestFlight/企业签名？**这个法务问题应在期 4 之前问清楚**，
   别等做完才发现。
