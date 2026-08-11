# Beautiful UI 对话面重设计规范

本文件是对话界面（消息流页面）视觉与信息流重设计的**权威规范**，在对话面范围内取代
`visual-redesign.md`。参考材料在 `docs/beautiful-ui-ref/`：19 个组件源码（.tsx，不入库，
仅本地参考）、`tokens-light.json` / `tokens-dark.json`、`MOTION-SPEC.md`（动效事实清单）。

**范围**：`crates/ui/src/chat.rs`、`composer.rs`，以及全局 token 层（`themes/tcode.json`、
`material.rs`）。token 改动会重着色整个 app，这是预期效果；但**布局改动只允许发生在
chat.rs / composer.rs**。sidebar、terminal drawer、settings、diff 面板本轮不动布局。

## 一、信息流哲学（五条原则）

1. **生命周期驱动**：每个机器动作有显式状态：等待 → 进行中（shimmer/spinner）→
   已定格（灰勾）/ 失败（红 + 重试入口）。动效只编码状态转换，不做装饰。
2. **聚合优先，按需下钻，摘要必带量词**：折叠标题写明数量与种类
   （"4 次工具调用 · 2 处编辑"）；每行可独立展开看细节。
3. **两平面分离**：给人读的散文回答 vs 机器活动（胶囊/卡片），互不渗透。
4. **证据物化为 chip**：文件改动 `+74 -41` 等宽胶囊、来源 pill、内联引用。
5. **历史自压缩**：进行中自动展开，完成后自动折叠成一行；流里只有当前工作是展开的。

## 二、Token 映射（Phase 1）

Beautiful UI 语义 token → `themes/tcode.json`（gpui-component schema）。两套主题都改。

| BUI token | Light | Dark | 语义 |
|---|---|---|---|
| canvas | #f1f2f3 | #1c1d1f | app 背景 → `background` |
| surface | #ffffff | #232427 | 卡片/控件表面 → `card` / `popover` |
| field | #f2f2f3 | #2b2c2f | 输入框/选中中性 → `input`/`muted` 类 |
| inset | #f7f8f9 | #1f2022 | 内凹子面板 |
| hover / hover-2 | #f4f5f6 / #e7e9eb | #2a2b2e / #313236 | 交互填充 → `accent`(hover) |
| ink / ink-2 / ink-3 | #1f2124 / #62656b / #9a9da3 | #f2f3f4 / #a5a8ad / #6c6f75 | 三级文字 → `foreground` / `muted_foreground` 等 |
| line / line-strong | #ecedef / #e0e2e5 | #2e3033 / #3a3c40 | 发丝线/描边 → `border` |
| accent / accent-ink / accent-tint | #0285ff / #0170dd / #e9f3ff | #3d9aff / #7ec0ff / #3d9aff29 | 主色 → `primary` 系 |
| green(-tint) | #189a4d (#e8f5ed) | #3dbb72 (#3dbb7224) | success |
| red(-tint) | #e3474c (#fcecec) | #ee5c61 (#ee5c6124) | danger |
| orange(-tint) | #ef720c (#fdf1e5) | #f68f3c (#f68f3c24) | warning |

具体字段名以 tcode.json 现有 schema 为准做最合理映射；无对应字段的（inset、hover-2、
各 tint）在 material.rs 提供 helper（从主题色派生或常量表，跟随亮暗主题）。

**几何**：圆角体系 chip=6 / control=8 / card=10 / composer=14（px），落在
`material.rs` 的 radius helpers；`tcode.json` 的 radius=8、radius_lg=14。
**字体**：保持 DM Sans + SF Mono，不引入 Inter。字号惯例：正文/行 13px，
次级 12–12.5px，元数据/mono 11–11.5px，大写小节标题 10.5px tracking 0.08em。
计时、路径、命令、± 计数一律 mono。

## 三、动效政策（硬约束）

只做 GPUI 原生能力（`with_animation` 时间驱动插值）覆盖的效果：

**允许**：opacity 淡入入场；shimmer 扫光（渐变位置推移，"进行中"的核心信号）；
spinner 旋转 / 像素网格脉冲；pop-in（opacity+scale）；计时器文本刷新。

**禁止（一律瞬时切换/snap）**：展开收起的高度插值、圆角插值、逐词模糊(stream-in)、
菜单高亮滑块 glide、宽度 morph、任何需要"先测量再插值"或滤镜的效果。
宁可不做，不许硬凑。悬停态颜色直接 snap（GPUI hover 默认行为）。

## 四、信息流重组（Phase 2，具体改动）

### chat.rs — 工作日志（对应 tool-chips / thinking-state / loading-state）

- 活动段收进一个胶囊组（inset 表面，card 圆角，发丝描边）：头部为可点折叠行，
  文案带量词："N 次工具调用 · M 处编辑 · K 条命令"（i18n，复用 WorkLogCounts）。
- **活动行可下钻**：每行点击展开详情（inset 面板，snap）：命令行→完整命令 + 输出尾部
  （mono, 最多 ~20 行，超出截断）；工具行→入参摘要 + 输出摘要。数据已在
  `EntryContent` 中，仅渲染层改动。展开状态并入现有 `expanded` HashSet。
- **思考一等公民**：Reasoning 进行中 shimmer 标签"正在思考…"；完成后"思考了 Ns"
  一行（无秒数数据则省略时长），展开见推理全文，左侧 5px 缩进 + 竖线 + 16px padding。
- **进行中指示**：替换 "••• Working for Ns" 为 shimmer 文字 + mono 计时
  （秒级即可，保留现有 1s 刷新）。
- 文件改动 chips：`file.ext +N -N` 胶囊（chip 圆角 6、高 22px、11.5px mono、
  绿/红 ± 数字），超过 3 个显示 "+K more" 展开。对齐现有 render_changed_files。

### chat.rs — 子代理（对应 task-rows）

- 子代理行升级为任务胶囊：44px 行高、圆角胶囊、状态徽标
  （运行=spinner 环、完成=绿 pill、失败=红 pill + 重试图标）、右侧元数据 + chevron。
- 运行中自动展开子行（现有 render_subagent_child），完成后自动收起；手动点击覆盖自动。

### chat.rs — 气泡与回答（对应 chat-composer / streaming-text）

- 用户气泡：右对齐、field 灰填充（前景色 8% 透明度派生，不用 surface 白）、
  12px 圆角、13px 字号、最大宽度约 76%。
- 回答完成后才显示操作行（淡入 400ms opacity；流式中不渲染）。
- 错误卡对齐 red-tint 卡片样式。

### composer.rs — 输入区（对应 prompt-bar / approval-card）

- composer 容器：14px 圆角、6px 内边距、focus 时描边 accent（snap）；
  控件统一 28px 高、control 圆角 8。
- @/斜杠/模型菜单：popover 卡片 10px 圆角、pop-in 入场（opacity+scale）、
  行高 28px、hover 填充 snap。
- 审批面板改造成审批卡模式：320px 宽卡片、单选/复选选项行（16px 圆点、选中
  accent 填充）、多问题时底部圆点翻页器、内联自定义输入行、发送后绿勾 + 摘要。
  保持现有键盘流。

## 四·五、比例与密度（Phase 3 —— 观感的决定层）

色板只是皮，Beautiful UI 的观感由紧凑的比例决定。以下数值为硬规范：

- **内容列宽**：消息流内容列 max-width 720px 水平居中（画布可以更宽，内容不拉伸）。
- **正文**：assistant/user 散文 13.5px / 行高 21px（替换现有 15/26）。markdown 标题
  相应降档（h1≈17px h2≈15px h3≈13.5px semibold）。
- **元数据**：时间/费用/统计行 11px mono muted；大写小节标签 10.5px tracking 0.08em。
- **工作日志胶囊**：头部行高 30px、文字 12.5px medium；活动行 min-height 28px、
  文字 12.5px、图标 13px；下钻详情 inset、11.5px、内边距 10–12px。
- **气泡**：13px / 行高 1.4、padding 10×6px。
- **间距**：turn 内元素 gap 8–10px（替换 gap_4=16px）；turn 之间 ≤24px。
- **chip**：高 22px、11.5px、mono（路径/数字）已定，全部落实为实际值而非近似。

判断标准：截图与参照站组件并排对比时，行密度和字号层级应当无法一眼区分。

## 四·六、有机取舍裁决（Phase 4 —— 图标 / 动画 / 用词）

**采纳 BUI，替换现状：**
- 工作指示器 = **像素矩阵三件套**：3×3 网格（格 4px、间距 1.5px、圆角 1px）逐格
  opacity 脉冲（pixel-on 650ms ease-in-out infinite，delay 按 MOTION-SPEC loading-state
  的索引表）+ shimmer 标签 + 0.1s 精度 mono 计时（100ms interval）。纯 opacity，
  符合动效政策。
- 图标语义：settled = **muted 灰勾**（成功不上绿色，颜色只留给异常）；active = spinner
  环；failed = 红色 + 重试符号。尺寸统一 12–15px。禁止装饰性图标、禁止语义错配
  （如失败态用 Loader 图标）。
- 计数摘要**只报非零项**："0 处编辑"这类零计数一律省略；条目顺序按重要性
  （工具 → 编辑 → 命令）。i18n 两语言同步。
- 时间/费用行压缩为单行安静 mono："1:28 PM · 3m50s · $23.79"；分项耗时
  （思考/工具）从常显移入该行的展开或悬停详情。

**保留 tcode，拒绝照搬 BUI：**
- 错误卡完整展示原则（永不截断、永不折叠进工作日志）。
- 成本/token 透明度（信息保留，呈现压缩）。
- 已更改文件的可点 diff 功能；≤3 个文件时扁平 chip 行，不渲染文件夹树。
- relay 分隔线、steering 队列、orchestrate 回调行：tcode 特有构件，按本规范的
  视觉词汇（安静居中行 / chip / 胶囊）重绘，不删除。
- 中英 i18n、DM Sans + SF Mono、720px 内容列。

## 五、验收

每个 piece：`cargo check --workspace` 与 `cargo clippy --workspace -- -D warnings`
零错误零新警告；`cargo fmt --all -- --check` 通过；i18n 新增 key 在全部语言文件
中补齐；不改动 store / core / session 数据层；不新增依赖；禁止 `#[allow]` 压警告。
整体完成后：应用可启动、两套主题下对话页人工截图验收。
