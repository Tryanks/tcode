# tcode 视觉重塑规范 —「霜面仪表」Frosted Instrument

状态：进行中。本文件是唯一权威规范；所有视觉改动以此为准。
不变量：**布局结构零改动、交互零改动、蓝色主轴保留、DM Sans + SF Mono 保留。**
（细节允许微调：分隔线、圆角、间距、配色全部按本规范重新决策。）

## 核心理念

玻璃机壳 + 纸面阅读区 + 一条蓝色墨线贯穿。
侧栏与窗口边缘是系统材质"机壳"（macOS vibrancy / Windows 11 Acrylic），
正文坐在一块近实的"纸面"上；
蓝色从单一按钮色升级为贯穿选中、聚焦、强调的墨线体系。
观感目标：一台精密仪器，而不是一个网页。用户气泡保持中性灰（用户已拍板）。

## 0. 技术底座（已验证）

- gpui `WindowBackgroundAppearance::Blurred` 在 macOS Metal 层下垫
  `NSVisualEffectView`；合成后 alpha < 1 的区域透出桌面毛玻璃。
- Windows 11 的持久主窗口也刻意使用
  `WindowBackgroundAppearance::Blurred`；锁定的 GPUI Windows 后端将其映射为
  Acrylic Accent state 4，使暴露的 T0 侧栏／窗口边缘能感知窗口后方实时变化并保持模糊。
  Mica 因无法达到本产品所需的可感知实时 background-through blur 而被否决。
- 全窗口仅一层原生 blur；应用内 popover/menu/dialog/drawer/toast 保持全不透明，
  不会各自获得原生 Acrylic。
- gpui-component `Root` 用 `colors.background` 涂画布，支持 8 位 hex alpha。
  **AppShell 不得重复涂 `background`**（已移除）——全屏例外：全屏 Space 的
  vibrancy 背景是纯黑，AppShell 根节点仅在全屏时垫一层不透明基色
  （`material::opaque_canvas`，把画布 alpha 提到 1），窗口模式仍由 Root 独自涂玻璃。
  与 Windows 材质不可用时的回退、macOS「降低透明度」同一降级策略。
- macOS vibrancy 与 Windows Acrylic 保留画布 alpha，使系统材质可见；Linux
  及其他平台使用 Opaque，main.rs 的 `flatten_canvas_for_opaque_window`
  把画布色压平为实色（与 JSON 字面量保持同步）。`TCODE_NO_VIBRANCY=1`
  仍只在 macOS 上切换为 Opaque 并压平画布。

## 1. 材质分层

| 层 | 用途 | 谁来涂 | 策略 |
|---|---|---|---|
| T0 玻璃机壳 | 侧栏、窗口边缘 | Root（`background`） | 唯一刻意透明层 ~78%；全屏时压平为实色 |
| T1 纸面 | 聊天区、右面板、设置页 | shell.rs（`material::content_surface`） | 近实 94-95% |
| T2 浮起 | 气泡、卡片、输入域 | 组件（muted/secondary 叠色） | 半透明墨色叠加 |
| T3 悬浮 | popover/menu/dialog/drawer/toast | `popover.background` | 100% 实底 + 发丝边 + 大软阴影 |

- 侧栏不涂底（`sidebar.background` 全透明），直接露玻璃。
- 正文文字永远坐在 ≥94% 的表面上；侧栏短标签除外。

## 2. 配色体系（themes/tcode.json）

### tcode Light —「纸面微暖，机壳冷调」
| token | 值 | 说明 |
|---|---|---|
| background | `#F2F4F7C7` | T0 冷灰蓝玻璃，78% |
| foreground | `#1F2328` | 墨色正文（弃纯黑灰） |
| muted.foreground | `#5E6B7A` | 蓝灰次要文字 |
| border | `#1F232814` | 墨色 8% 发丝线 |
| input.border | `#1F23281F` | |
| accent/secondary/muted .background | `#24344D0F` | 墨蓝 6% 叠色（弃纯黑叠色） |
| accent/secondary .foreground | `#1F2328` | |
| popover.background | `#FFFFFFFF` | T3，全不透明 |
| list.background | `#FFFFFF00` | 列表不自带底，坐在所在层上 |
| list.hover.background | `#24344D0A` | |
| list.active.background | `#1447E614` | 主色 8% 着色胶囊 |
| sidebar.background | `#FFFFFF00` | 全透明 |
| sidebar.foreground | `#3A4453` | 机壳文字：蓝灰 |
| sidebar.border | `#00000000` | **无线**，材质对比成缝 |
| sidebar.accent.background | `#24344D0F` | hover |
| title_bar.background / border | `#FFFFFF00` / `#00000000` | |
| primary.background | `#1447E6` | 保留 |
| ring | `#1447E6` | |
| danger | bg `#FF3B30` fg `#D70015` | 对齐 macOS 系统色盘 |
| success | bg `#34C759` fg `#248A3D` | |
| warning | bg `#FF9500` fg `#C93400` | |
| info | bg `#007AFF` fg `#0056CC` | |

### tcode Dark —「夜间仪表，蓝碳」
| token | 值 | 说明 |
|---|---|---|
| background | `#15171CC7` | T0 蓝碳玻璃，78% |
| foreground | `#ECEEF1` | |
| muted.foreground | `#8A94A6` | 蓝灰 |
| border | `#C9D4E80F` | 冷光发丝线 |
| input.border | `#C9D4E817` | |
| accent/secondary/muted .background | `#C9D8F00D` | 冷光 5% 叠色 |
| popover.background | `#22262EFF` | T3，全不透明，比纸面抬一档 |
| list.background | `#FFFFFF00` | |
| list.hover.background | `#C9D8F00A` | |
| list.active.background | `#155DFC26` | 主色 15% 胶囊 |
| sidebar.background | `#16161600` | 全透明 |
| sidebar.foreground | `#B7C0CE` | |
| sidebar.border | `#FFFFFF00` | 无线 |
| sidebar.accent.background | `#C9D8F00D` | |
| title_bar.background / border | `#16161600` / `#FFFFFF00` | |
| primary.background | `#155DFC` | 保留 |
| danger | bg `#FF453A` fg `#FF7B72` | |
| success | bg `#30D158` fg `#4ADE80` | |
| warning | bg `#FF9F0A` fg `#FFB340` | |
| info | bg `#0A84FF` fg `#64A9FF` | |

### T1 纸面（material.rs 常量，JSON 无槽位）
- light: `#FDFDFB` @ 95%（微暖纸白）
- dark: `#1B1E24` @ 94%（蓝碳纸面）

## 3. 分隔线革命（去 t3 味的第一刀）

1. 侧栏↔内容：**没有线**。材质对比 + 纸面组件自身的 `shadow_sm` 边缘光影成缝。
2. 内容区内部通栏 1px 线全部废除：优先留白分隔；确需线时用
   `material::faded_hairline`（两端渐隐的渐变发丝线，非通栏）。
3. 表格/列表行分隔：行 hover 叠色代替行间线。

## 4. 圆角语言（按角色分配）

material.rs 常量，禁止再写魔法数字：
| 常量 | px | 用途 |
|---|---|---|
| RADIUS_OVERLAY | 14 | popover/menu/dialog/toast |
| RADIUS_CARD | 12 | 卡片、事件卡、diff 块 |
| RADIUS_INPUT | 10 | 普通输入框、按钮组容器 |
| RADIUS_BUTTON | 8 | 按钮 |
| RADIUS_COMPOSER | 16 | composer 输入域（主角元素） |
| 胶囊 | rounded_full | chips、状态标签、侧栏选中项 |
| 气泡 | rounded_xl 保留 | 用户气泡（中性灰不变） |

## 5. 深度与光

- T3 悬浮层：`shadow_xl` + 发丝边，双轮廓缺一不可。
- T1 纸面列（shell.rs 已涂底的容器）：`shadow_sm`，让纸面对机壳有 1px 级浮起。
- Composer 输入域（主角）：常态 内发丝线 + `shadow_md`；聚焦时 ring 主色
  + 主色 25% 外光晕（`material::focus_glow`）。
- 主按钮：极微顶部高光（linear-gradient，上 +4% 亮度），macOS 按压钮质感。
- 纸面内部其它元素不用阴影；层次靠 T2 叠色。

## 5.5 修订版组件准则（2026-07 用户验收后追加，优先级高于下文冲突条款）

- **设置类界面 = 对话的浮起卡片语法**（2026-07 第三轮用户反馈修订,取代此前
  System Settings 条款）：设置分组不再是扁平无阴影的 System-Settings 盒子——
  用户验收认为那仍读作"另一套设计语言"。改为与 chat 的 composer 控制台／卡片
  同一套 surface/depth 词汇：每个分组是一张**浮起卡片**（`material::floating_card`：
  `popover` 填充 + 发丝边 `border` + `radius_card` 12px 圆角 + `shadow_md` 柔和阴影），
  像 chat 的卡片一样真正浮在 T1 纸面上。行与行**优先用留白分隔、不画线**（对话即
  以间距而非导轨分隔内容）；只有确实密集的列表（Providers、Orchestrate 可编辑行）
  才用**最淡的内缩发丝线**（`border` 60%）。组间 20-24px 留白 + 组外 11px 大写标题。
  ❌ 仍禁止成片的灰色 T2 平板卡无间隔堆叠（AI slop）——浮起卡片是白／popover 实底、
  有真实景深，不是灰平板。
- **窗口材质跨路由不变**：chat↔settings 导航必须保持同一套 T0 玻璃模糊 + 半透纸面
  合成，仅内容变化。settings 路由不得在导航栏／内容之下铺任何不透明整窗层
  （历史 bug：shell 在设置页整窗涂 `content_surface`，导致左侧 nav 坐在实底纸上而非
  玻璃画布，材质在导航瞬间翻转）。settings nav 用与主侧栏相同的半透 `sidebar` token
  直接坐在画布上；内容列用 `content_surface` + `shadow_sm` 浮起，与 chat 列一致。
- **changed-files/diff 清单 = 安静清单**：与正文列对齐、无容器无导轨，
  小型大写标题行 + hover 着色的文件行。导轨语言只属于**状态性**事件
  （错误/成功/工具日志）与 toast。
- **弹窗（Dialog/Alert）**：面板必须 `popover` 实底（库默认涂玻璃画布 token，
  调用侧必须覆盖）+ `overlay` 遮罩 token（亮 #1F232852 / 暗 #00000080）。
  gpui 无元素级 backdrop blur，靠"深遮罩 + 近实面板"达成等效可读性隔离。
- **composer 填充 = popover 白控制台**，禁止用玻璃 `background` token 涂纸面内组件。
- **侧栏收起按钮在侧栏头部最右**，不得与红绿灯相邻。
- **选中态 = 6px 小圆角矩形着色条**（macOS 原生侧栏语法）。
  ❌ 禁止 rounded_full 胶囊选中态；胶囊只属于徽标、计数、状态点。

## 6. 组件个性

- **工具调用/事件卡**（runtime_event.rs）：废除四边框盒子 → 左侧 2px 圆头
  彩色导轨（按事件语义着色：工具=info、成功=success、错误=danger、
  普通=border）+ 无边框 T2 填充，RADIUS_CARD。
- **状态 chips**：灰底改语义色 12% 着色底 + 同语义色文字（danger/success/
  warning/info），中性信息才用 muted。
- **次要按钮**："玻璃片"——常态透明，hover 浮现半透明填充+发丝边。
- **侧栏**：分组标题小型大写 + 字距（text_xs + 字距 + muted）；选中项
  `list.active.background` 蓝色胶囊 rounded_full…（若现为方角，改胶囊）。
- **菜单/palette**：T3 双轮廓 + RADIUS_OVERLAY；选中行主色胶囊。
- **Toast**：T3 规则 + 左侧语义色导轨（与事件卡同语言）。

## 7. 状态

- hover：T2 叠色浮现；禁止 hover 改变布局尺寸。
- 选中：主色着色（token 表）。
- focus：仅键盘可聚焦元素 ring；composer 仅加深中性描边（蓝色 ring + glow 已回退，磨砂画布上太刺眼）。
- disabled：50% opacity。

## 8. 排版

字体不动。归一化：说明 11px / 次要 13px / 正文 15px；分组标签 11px 大写加字距。
不引入新字号。

## 9. 实施地图

1. ✅ main.rs：Blurred 窗口（macOS vibrancy / Windows Acrylic）+ flatten fallback
2. ✅ shell.rs：去重复涂底；chat/right/settings 涂纸面
3. ✅ token 层：theme JSON 全表 + material.rs（常量与 helper）
4. 组件清扫（并行，文件禁区严格隔离）：
   - A: sidebar.rs + shell.rs
   - B1: chat.rs + runtime_event.rs
   - B2: composer.rs + composer_trigger.rs + context_meter.rs + attachments.rs
   - C: diff_panel.rs + terminal_drawer.rs + preview_panel.rs + plan_panel.rs + acp_panel.rs
   - D: settings_page.rs + settings.rs + provider_card.rs + provider_model_picker.rs + provider_models.rs + provider_status.rs + orchestrate_settings.rs
   - E: add_project_dialog.rs + commit_dialog.rs + toast.rs + palette.rs + git.rs
5. 集成验收：cargo check/test + tools/vm-screenshot.sh 双模式截图逐面板对照。

## 10. 验收红线（每批必须满足）

- `cargo check -p tcode-ui -p tcode` 通过；`cargo test -p tcode-ui` 不新增失败。
- 无布局改动：不改 flex 结构/尺寸/间距语义（分隔线厚度、圆角、颜色、阴影除外）。
- 无越界文件改动；不改 gpui-component 依赖。
- 不新增硬编码 hex：颜色一律 `cx.theme().*` 或 material.rs。
- 正文文字不落在 alpha < 0.9 表面。
- 每处通栏分隔线的去留必须在报告中逐条列出（改成了什么）。
