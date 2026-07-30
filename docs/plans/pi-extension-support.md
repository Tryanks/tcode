# pi extension 支持 — 实施计划

目标：用户安装的 pi extension 在 tcode 的 pi 会话中真正可用 —— 注册的工具能执行（经 tcode 原生审批）、UI 对话框能交互、注入的消息可见、项目级 `.pi/extensions` 可选择信任加载。

前置事实（已核对到 file:line，两份调查报告为准）：

- pi 的 `tool_call` 事件只有 `toolName`/`toolCallId`/`input`，无来源信息；但门 extension 可调 `pi.getAllTools()`，每条记录带 `sourceInfo { path, source, scope, origin }`，内置工具为 `source: "builtin"` / `path: "<builtin:name>"`（agent-session.js:613、source-info.d.ts:4）。注册表按名字只保留一个生效条目（agent-session.js:1954、1990），extension 覆盖内置后查询结果即实际执行的实现 —— 判定依据可靠。
- 对话框协议（rpc-types.d.ts:384）：`select {title, options: string[], timeout?}`、`input {title, placeholder?, timeout?}`、`editor {title, prefill?}`（无 timeout）；响应 `{value}` / `{cancelled:true}`；confirm 用 `{confirmed:bool}`。pi 侧自行执行 timeout（rpc-mode.js:45），超时后迟到响应被忽略。
- custom 消息：`message_end` 携带 `{role:"custom", customType, content: string|Array<Text|Image>, display, details?, timestamp}`（agent-session.js:1065）。
- tcode 契约：`UserInputQuestion { id, header, question, options, multi_select }`（lib.rs:419）+ `RespondUserInput { request_id, answers }`（lib.rs:571）；composer 常驻自由文本输入框（composer.rs:2422），答案为 string 或 string[]。`ApprovalKind::ToolUse { name, input, detail }`（lib.rs:1105）；pi.rs 的 confirm 流对未知名工具已映射 ToolUse（pi.rs:1197），approve-for-session 按工具名缓存（pi.rs:654）。`ItemContent::Other` 渲染为 Work Log 单行（session.rs:1271、chat.rs:1916）。`message_end` 的 role 过滤在 pi.rs:993，`custom` 落 `_ => Vec::new()`。

## Module A — 工具放行 + 内置名保护

文件：`crates/agent/assets/pi/tcode-permissions.ts`、`crates/agent/src/pi.rs`

- 门在 `tool_call` 时查 `pi.getAllTools().find(t => t.name === event.toolName)?.sourceInfo`：
  - `source === "builtin"` → 现行策略不变（safe 集免确认、bash/edit/write 按模式）。
  - 非 builtin（extension/sdk）→ `read_only`: block（`--tools` 白名单已在注册表层过滤，此为纵深防御）；`full_access`: 放行；其余模式: `ctx.ui.confirm("tcode:"+toolName, payload)`，payload 增加 `source: "extension"`、`extensionPath`、`shadowsBuiltin: bool`（name ∈ 内置七名时 true）。
  - 查不到 sourceInfo → fail-closed block。
- pi.rs `approval_kind`（pi.rs:1160）：payload 带 `source: "extension"` 时，即便 toolName 是 bash/edit/write 也映射为 `ToolUse`，detail 注明来源 extension 与是否覆盖内置 —— 防止 extension 代码以"普通 shell 命令"的样子出现在审批卡上。
- approve-for-session 现有按名缓存直接适用。

验收：`cargo test -p agent` 新增 approval_kind 测试（extension source → ToolUse，含 shadowed bash 用例）；脚本化 e2e（pi RPC + 最小 extension）：default 模式 hello_world → 收到 confirm 且 payload 带 source，回 confirmed:true 后工具真实执行；read_only 仍拦截；覆盖 `read` 的 extension 需审批而非免确认放行。

## Module B — select/input/editor → 原生 UserInput

文件：`crates/agent/src/pi.rs`、`crates/agent/src/lib.rs`、`crates/ui/src/composer.rs`

- lib.rs：`UserInputQuestion` 增加 `#[serde(default)] pub prefill: Option<String>`（additive，codex/序列化不受影响）；composer 用它预填自由文本框（composer.rs:2422 一带）。
- pi.rs `handle_extension_ui`：
  - `select` → `UserInputRequested { request_id: id, questions: [{ id, header: "pi", question: title, options: options→{label, description:""}, multi_select: false }] }`
  - `input` → 同上但无 options（placeholder 丢弃或并入 question 文案）
  - `editor` → 无 options + prefill
- `RespondUserInput` → `{"type":"extension_ui_response","id":…,"value":<string>}`；用户取消/无答案 → `{"cancelled":true}`。关联键就是顶层 `id`，照 confirm 现有 pending 模式管理；turn 结束时清理未决请求。
- 启动握手期（`wait_response`）到达的对话框维持自动取消（彼时无 UI 可挂）。
- 已知行为记录：select 的自由文本回答会原样传给 extension（协议 value 本就是 string）；pi 侧 timeout 过期后 tcode 的迟到回答被 pi 忽略。

验收：pi.rs 单测覆盖三种方法的请求映射与响应回包（value / cancelled / turn 清理）；合入后真实 pi 手动 e2e。

## Module C — custom 消息显示

文件：`crates/agent/src/pi.rs`（pi.rs:993 的 role match）

- `role == "custom"` 且 `display == true` → `ItemContent::Other { provider_kind: customType（缺省 "pi-extension"）, summary: 文本 content 拼接 }`；`display` false/缺省 → 丢弃；image 部分忽略（记录限制）。

验收：单测 display true/false、content 为 string 与数组两种形态。

## Module D — 项目信任开关（`--approve`）

文件：`crates/core/src/settings.rs`、`crates/ui/src/provider_dialog.rs`、`crates/runtime/src/app.rs`、`crates/services/src/settings.rs`（测试字面量 settings.rs:153）、`locales/en.yml`、`locales/zh-CN.yml`、`docs/DESIGN.md`

- `ProviderSettings` 增加 `#[serde(default)] pi_trust_project_extensions: bool`，`ProfileConfigurationPatch` 同步；provider_dialog 的 connection 区（provider_dialog.rs:536）渲染 pi-only 开关；app.rs `session_options` 为 true 时向 pi `extra_args` 追加 `"--approve"`（app.rs:8416 一带），扩展 app.rs:9946 launch-args 测试；双语 locale 同 key（parity 测试 i18n/src/lib.rs:111）；DESIGN.md 补 UI 变更。
- 已知残留（不修，记录）：模型发现走 `pi --mode rpc --no-session` 且不带 launch args（pi.rs:65），项目 extension 注册的 provider 不会出现在模型列表。
- 设计约束：信任决策发生在启动握手期，运行时提问会死锁，故只能做设置项。

验收：settings 回环/默认值测试、launch-args 测试、locale parity、全量门禁。

## 排期与派工

前置：先提交当前未提交的 pi.rs 修复（MCP 警告措辞 + notify warning/error 冒泡），保证 clean tree。

- Phase 1（并行，文件不相交）：Worker-A = Module A；Worker-D = Module D。均 codex/gpt-5.6-sol medium。
- Phase 2（串行，均改 pi.rs）：Worker-BC = 先 Module C（小）再 Module B（最大块）。codex medium。
- 每件工作：clean tree 派发 → 本人验 diff + 跑门禁（`cargo fmt --check`、`clippy --workspace --all-targets -D warnings`、`cargo test --workspace`）→ 通过即 commit。
- 收尾：全 workspace 门禁一次 + 真实 pi e2e：一个示例 extension 走通四条链路（工具审批、select/input/editor、sendMessage 显示、项目目录信任加载）。

## 范围外（记录备查）

- `setStatus` / `setWidget` / `setTitle` 的 UI 落点；`notify` info 级。
- extension 自发 turn（`sendUserMessage`/`triggerTurn`）产生的无主 `agent_start`，以及 `newSession`/`fork`/`switchSession` 造成的 pi sessionId 与 tcode resume cursor 失配 —— 真实一致性风险，实现 B 时若被触发再评估。
