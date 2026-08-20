# 语音输入与语音模式调研报告

调研日期：2026-08-21。所有外部主张均来自一手来源（官方文档 / 仓库源码 / release notes），无法验证的均标注 **未验证**。三份底层调研（代码库集成图、ASR 技术全景、Codex CLI 语音专项）的完整引用见文末来源清单。

## 结论速览

1. **Codex 官方语音输入不可复用**：该功能（0.105.0 按住空格听写）已于 0.118.0 被官方整体删除，且它本来就不是本地识别——是 cpal 录音 + 整段 WAV 上传 OpenAI 云端（`gpt-4o-mini-transcribe`）。代码是 Apache-2.0 可以抄思路，但深度耦合 TUI、不是可依赖的 crate。真正值得抄的是它的 **UX 模式**（插入锚点、按住即说）和 **架构形状**（录完再传的一次性转写）。
2. **本地方案首选 sherpa-onnx**：Apache-2.0、官方 Rust 绑定 + 50+ 示例、唯一有真流式中英双语模型（Zipformer/Paraformer）的引擎，还内置 Silero VAD 和 TTS——一个依赖同时覆盖语音输入和未来的语音模式。
3. **MVP 建议"录完即转"而非流式**：按住说话 → 松开 → SenseVoice-small（int8 仅 228MB，中英日韩粤 + 自动标点）离线转写 → 插入光标处。实现量最小，延迟可接受（短句亚秒级），流式 partial 留到二期。
4. **语音模式（对话）技术上可行但应后置**：sherpa-onnx TTS（Kokoro 中英模型）+ 已有采集/VAD 层即可搭半双工原型；全双工的打断/回声消除/延迟才是难点。

---

## 一、Codex 官方语音输入：能不能复用？

### 事实（源码级确认）

| 事项 | 结论 |
|---|---|
| 存在过吗 | 是。0.105.0（2026-02-25）实验性上线"按住空格听写"，需 `features.voice_transcription = true` |
| 现在还在吗 | **不在**。PR #16114 于 0.118.0（2026-03-31）整体删除；本机 0.148.0 无此功能。另一个"realtime voice" TUI 实验也已在 2026-06 删除 |
| 是本地识别吗 | **不是**。cpal 采集 → 24kHz 单声道 16-bit WAV（hound 编码，手写重采样）→ 松开后整段上传。API key 走公开 `POST /v1/audio/transcriptions`（`gpt-4o-mini-transcribe`），ChatGPT 登录走未公开的 `chatgpt.com/backend-api/transcribe` |
| 无流式 | 录音中无 partial、无增量解码，只有松开后的一次性结果 |
| 可作为 crate 依赖吗 | **不可**。私有 TUI 模块，耦合 Codex 鉴权/配置/事件类型；无 `codex-voice` crate。0.148 的 `codex-utils-audio` 只是 data-URL/token 工具，不含采集或转写 |
| 许可证 | Apache-2.0，抄代码合法（需保留声明）；但许可证不覆盖服务——`/backend-api/transcribe` 是未公开端点，第三方调用是否被允许 **未验证**，不应依赖 |

### 0.148 仍存在的可编程接口：`thread/realtime/*`

Codex app-server 有一套**实验性** JSON-RPC 实时接口，且暴露流式转写：

- 方法：`thread/realtime/start | appendAudio | appendText | appendSpeech | stop | listVoices`
- 通知：`thread/realtime/transcript/delta`、`transcript/done`、`outputAudio/delta`、`error`、`closed` 等
- 配置 `[realtime] version = "v2"`、`type = "transcription"` 时为纯转写会话：24kHz PCM 输入、`gpt-4o-mini-transcribe`、无 VAD/无自动应答
- 门槛：客户端须声明 `capabilities.experimentalApi: true`（tcode 已声明，见 `crates/agent/src/codex.rs:95`）+ Codex 侧 `features.realtime_conversation = true`（0.148 默认关）
- **限制：WebSocket 路径要求 API key 鉴权**，纯 ChatGPT OAuth 登录用不了；且整个接口标注 UnderDevelopment，版本间可能随意变动

**判断**：tcode 今天技术上就能驱动它（我们本来就是 app-server 客户端），可以作为"Codex 用户的云端流式转写"实验后端加个 feature flag 试验，但不能当主路径——不稳定、要 API key、只覆盖 Codex 一家 agent。

### 值得抄的 UX（比代码更有价值）

- **插入锚点**：录音开始时在光标处放一个占位符（内联电平动画 → 转写中 spinner → 替换为最终文本），用户可以在已有文字中间听写，异步完成也不会插错位置——这是最值得抄的一条。
- 按住即说（push-to-talk）为主交互；但空格键 hold 在桌面编辑器里与正常打字冲突，tcode 应改为**麦克风按钮 + 可配置快捷键**，Esc 取消。
- API-key 分支会把 composer 已有文本作为转写 `prompt` 传入，帮助识别项目名/技术词——本地引擎虽不支持 prompt，云端 BYOK 路径应保留此设计。
- 不值得抄：失败时静默移除占位符（无重试、无持久错误提示）、Linux 直接编译为 no-op、无权限引导。

---

## 二、本地 ASR 引擎对比（中英混合是硬需求）

| 引擎 | 许可 | 流式 | 中英适配 | 体积 | Rust 成熟度 | 加速 |
|---|---|---|---|---|---|---|
| **sherpa-onnx** | Apache-2.0（模型另查） | ✅ 真流式（在线 Zipformer/Paraformer，解码器保态） | ✅ 官方中英双语流式模型（2023-02 起多个） | 双语 Zipformer ~340MB fp32，有 int8 | ★ 官方 in-tree crate，50+ 示例（含麦克风流式、VAD、TTS） | CPU 全平台；Linux CUDA；**macOS Metal 未验证** |
| **SenseVoice-small**（经 sherpa-onnx） | 引擎 Apache-2.0；转换权重再分发条款**未验证** | ❌ 离线整句（配 Silero VAD 可模拟流式） | ✅ 单模型覆盖中/英/粤/日/韩 + 标点/ITN | **int8 228MB** / fp32 894MB | ★ 官方 Rust 示例 | ONNX Runtime，全平台 CPU |
| **whisper.cpp**（whisper-rs 0.16.0） | MIT / 权重 MIT | ❌ 架构非流式（官方 stream 例自称 naive 滑窗，partial 会回改） | ✅ 99 语多语基线最强；code-switch 无基准（未验证） | turbo Q5 547MB / fp16 1.5GB | ★ API 完善；注意 GitHub 仓库已归档迁至 Codeberg，需锁版本 | ✅ Metal/CoreML/CUDA/Vulkan 最全 |
| Vosk | Apache-2.0 | ✅ | ❌ 中英模型分开，无双语 | 小模型 40–50MB | 社区 wrapper（0.3.1，旧） | CPU |
| Moonshine | 代码 MIT；**非英语模型非商用许可** | 流式仅英语 | ❌ 中文单语离线 | Base 58M 参数 | 走 sherpa | — |
| Parakeet/Canary | CC-BY-4.0 | — | ❌ 无中文 | — | 走 sherpa（离线英文） | — |

**要点**：

- 想要**打字机式实时 partial** → 只有 sherpa-onnx 的在线双语模型能做（Whisper 的"流式"是每 500ms 重转滑窗，partial 会回改，UI 必须按"可替换临时区间"处理）。
- 想要**最高一次性质量** → whisper large-v3-turbo（Metal 加速最好）或 SenseVoice-small（体积/中文/标点最平衡）。
- 所有引擎在"把 `Arc<Mutex<T>>` 改成 `tokio::sync::RwLock`"这类中英夹杂开发者语料上的表现都**无一手基准**——选型定案前必须用自建语料做 bake-off（中文、英文、混合句、crate 名、路径、shell 命令、标识符）。

## 三、系统原生与云端

**macOS**：

- **系统听写可能白嫖**：Apple 听写走 NSTextInputClient 文本输入体系，GPUI 实现了该协议，理论上 tcode 输入框已支持系统听写键——**先冒烟测试**，这是零成本的第一档（但无 UI 控制、无模型选择、不跨平台，不能替代自建方案）。
- macOS 26 的 **SpeechAnalyzer/SpeechTranscriber** 是理想的免下载后端：系统管理模型资产、本地推理、原生 volatile/final 语义。但 objc2 无现成绑定（`objc2-speech` 只覆盖旧的 SFSpeechRecognizer），需要一小段 Swift shim（swift-bridge）；中文资产可用性需实机枚举 `supportedLocales` 验证。适合作为后续增量后端。
- 权限：直接开麦需要 `NSMicrophoneUsageDescription`——**当前 release.yml 内联生成的 Info.plist 没有这个 key，必须补**，否则 macOS 上一开麦即崩。已有 TCC 权限层可平行扩展（`crates/computer-use-mcp/src/permissions.rs`，现有 Accessibility/录屏两个变体）。

**Windows**：WinRT `Windows.Media.SpeechRecognition` 的自由听写语法**实际走网络**、基础路径限 10 秒、还要求 MSIX 打包身份——不可用作本地默认。Windows 上走"cpal + 本地模型"。

**Linux**：无原生听写 API，唯一路径就是 cpal（ALSA/PipeWire）+ 本地模型。

**云端 BYOK**（可选后端，tcode 不代理 key，桌面端直连）：

| 供应商 | 价格/分钟 | 流式 | 中文 |
|---|---|---|---|
| OpenAI gpt-4o-mini/4o-transcribe | ~$0.003–0.006 | 文件流式 delta；实时走 Realtime（$0.017） | 多语（code-switch 未验证） |
| ElevenLabs Scribe v2 Realtime | $0.0065 | ✅ WebSocket partial/commit | ✅ 明确列出，WER 5–10% 档 |
| Deepgram Nova-3 | ~$0.005–0.006 | ✅ <300ms | 单语中文 ✅；`multi` 模型**不含中文** |
| Groq whisper turbo | $0.00067 | ❌ 仅文件上传 | 多语 |

## 四、音频采集与 VAD

- **cpal 0.18.1**（2026-06）：macOS CoreAudio / Windows WASAPI / Linux ALSA(+PipeWire/Pulse 可选)。注意：0.18 流不再自动 start（需 `play()`）；**CoreAudio 后端文档基线升到 macOS 14.2**（issue #1241），确认 tcode 最低系统版本后再定 0.17/0.18。回调在实时音频线程执行——只做拷贝进有界队列，重采样/推理放 worker。Zed 自己也用 cpal（0.17）；不要为采集引入 LiveKit 全家桶。
- **VAD**：用 sherpa-onnx 内置 Silero VAD（同一个依赖，官方 Rust 示例齐全）。MVP 里 VAD 只做"说话指示灯 + 可选自动停止"，永远保留手动停止；静音尾巴 800–1500ms 起配，保留 pre-roll 防吞首字。

## 五、先例：VS Code 是交互蓝本

VS Code Insiders 1.132（2026-08）内置本地听写：首次使用下载 `nemotron-3.5-asr-streaming-0.6b`，之后完全离线。值得照抄的交互清单：输入框旁麦克风按钮、interim 文本展示、快捷键短按切换/长按 push-to-talk、Esc 取消并删除本次听写、停止保留文本但不提交、可选麦克风设置、可配静音超时、模型与应用分离安装。Zed 的听写仍停留在 issue #16410（planned 未实现）——tcode 有先发机会。

---

## 六、推荐方案与分阶段计划

### 架构：引擎中立的 trait

```text
cpal 回调 → 有界 PCM 队列 → 降混+重采样(16k/24k) → [VAD] → ASR worker
  → partial { text, range, stability } / final { text, range }
  → GPUI 前台 executor → TextareaState 插入
```

引擎接口：`start(locale_hint, vocab_context)` / `push_audio` / `partial` / `final` / `stop` / `cancel`。本地 sherpa、macOS SpeechAnalyzer、云端 BYOK、（实验性）Codex realtime 都实现同一 trait。

### Phase 0 — 冒烟验证（几乎零成本）

1. macOS 系统听写键能否直接输入 GPUI 输入框（能 → 文档里告诉用户，立即可用）。
2. 用 sherpa-onnx 官方 Rust 示例在真机跑通 SenseVoice-small 与流式双语 Zipformer，录一组中英混合开发语料做 bake-off，定引擎与模型档位。

### Phase 1 — 语音输入 MVP（录完即转，Codex 同款形状）

- composer 控制行加麦克风按钮（`mod.rs:739-762`）+ 可配置快捷键；按住即说 / 短按切换；Esc 取消。
- 录音时在光标处放占位符（电平指示 → spinner → 替换为转写文本），用 `set_selected_range` + `TextareaState::replace`（`trigger_menu.rs:187-201` 现成模式）。
- 引擎：sherpa-onnx + SenseVoice-small int8（首次使用时下载到数据目录，非打包内置）。
- macOS：permissions.rs 加 Microphone 变体；release.yml 的 Info.plist 补 `NSMicrophoneUsageDescription`；设置页加权限行。
- 设置节 `voice`（引擎/模型/语言提示/快捷键/静音超时），按 Orchestrate 节的 13 个接缝照抄；en + zh-CN 文案。
- 明确错误状态：需授权 / 无设备 / 设备占用 / 录音中 / 转写中 / 失败（保留音频可重试，不静默丢弃）。

### Phase 1.5 — 流式 partial 与更多后端

- 在线双语 Zipformer 流式 partial（灰色临时区间，final 落定替换）。
- macOS 26 SpeechAnalyzer 后端（Swift shim）；云端 BYOK 后端（OpenAI 一次性 / ElevenLabs Realtime）。
- （可选实验）Codex `thread/realtime/*` 转写会话，flag 门控、锁 Codex 版本。

### Phase 2 — 语音模式（后置）

复用采集/VAD/ASR 层 + sherpa-onnx TTS（Kokoro 中英模型，~80–300MB）→ 半双工按键对话原型。全双工（打断、回声消除、延迟、双模型内存）等听写层稳定后再评估；云端捷径是 OpenAI Realtime（BYOK，$32/M 音频输入 token）。注意 Piper 后继项目已转 GPL-3.0，避免直接内嵌。

### 主要开放风险

1. 中英 code-switch 实际质量（所有引擎均未验证）→ Phase 0 bake-off 是硬前置。
2. SenseVoice 转换权重的再分发条款 → 用"运行时下载"而非打包内置可绕开分发问题，仍需核对。
3. sherpa-onnx 在 Apple Silicon 无 Metal 路径 → 流式小模型 CPU 大概率够用，bake-off 时实测 RTF。
4. cpal 0.18 的 macOS 最低版本要求 → 确认 tcode 支持矩阵。

---

## 来源清单（关键项）

- Codex 语音：openai/codex PR #3381（新增）、PR #16114（删除）、`rust-v0.105.0`/`rust-v0.118.0` release notes、历史 `codex-rs/tui/src/voice.rs`、0.148.0 `app-server-protocol/src/protocol/v2/realtime.rs` 与 `codex-api/.../realtime_websocket/methods_v2.rs`
- sherpa-onnx：github.com/k2-fsa/sherpa-onnx（rust-api-examples、在线双语模型目录、SenseVoice 文档）
- whisper.cpp：ggml-org/whisper.cpp（models README、stream 示例、releases）；whisper-rs 0.16.0（docs.rs，仓库已迁 Codeberg）
- Apple：SpeechAnalyzer/SpeechTranscriber 文档、WWDC25 session 277、NSMicrophoneUsageDescription、macOS 听写指南
- Microsoft：Windows.Media.SpeechRecognition 文档（在线语法 + 10 秒限制 + MSIX 要求，2026-07 更新）
- VS Code：code.visualstudio.com/docs/configure/accessibility/voice（1.132，2026-08）
- Zed：issue #16410（听写请求，open/planned）；Zed 主仓 Cargo.toml（cpal 0.17）
- cpal：RustAudio/cpal 0.18.1 changelog、issue #1241
- 云端定价：platform.openai.com/pricing、deepgram.com/pricing、elevenlabs.io/pricing/api、console.groq.com/docs/speech-to-text（均 2026-08-21 访问）
- tcode 集成点：`crates/ui/src/composer/mod.rs`、`composer/components/trigger_menu.rs`、`composer/components/images.rs`（异步模式）、`crates/computer-use-mcp/src/permissions.rs`、`crates/core/src/settings.rs`、`.github/workflows/release.yml`
