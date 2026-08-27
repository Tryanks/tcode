# macOS Computer Use 权限 UX 调研

调研日期：2026-08-27。范围仅限 tcode 的 Accessibility 与 Screen Recording
授权引导；实现仍使用现有 Core Graphics / AX API。本结论核对了 Apple 官方文档、
Apple Support、Apple DTS 回复，以及本机 Xcode 26.5 所带 macOS 26.5 SDK 头文件。

## 结论

1. **首次请求不要同时主动打开 System Settings。** 用户点击授权后，只调用对应的
   系统请求 API，让系统弹窗完成这一轮交互。Apple 对 Accessibility 的用户指南明确
   要求用户在系统弹窗中选择 **Open System Settings**；应用同时跳转会抢在用户选择前
   打开同一页面，重复了系统交互。[Apple Support：Accessibility 授权流程][ax-support]
2. **把“请求权限”和“打开系统设置”拆成两个动作。** 只有请求后仍检测为未授权，或
   用户稍后重试时，才显示独立的 **Open System Settings** 兜底按钮。Apple HIG 要求在
   功能确实需要资源时请求，并依赖系统标准权限弹窗让用户作决定。[Apple HIG：Privacy][hig-privacy]
3. **请求 API 的返回值不能承担完整状态机。** Accessibility 请求的提示是异步的，
   明确“不影响返回值”；Screen Recording 的公开文档与 SDK 头文件没有定义 request
   返回的 `Bool` 如何区分首次、拒绝或待重启。实际授权状态应分别以
   `AXIsProcessTrusted()` 和 `CGPreflightScreenCaptureAccess()` 的后续结果为准。
4. **只有确认 Screen Recording 从未授权变为已授权，才提示重启。** Apple 的
   ScreenCaptureKit 官方示例明确要求首次授权后重启应用才能捕获；仅仅点击 Grant、
   弹出窗口或返回 `false` 都不代表需要重启。[Apple ScreenCaptureKit 示例][sc-sample]

## API 能知道什么

| 权限 | 无提示检查 | 请求 | 可可靠表达 | 不能表达 |
| --- | --- | --- | --- | --- |
| Accessibility | `AXIsProcessTrusted()` | `AXIsProcessTrustedWithOptions({ prompt: true })` | 当前进程此刻是否为 trusted client | 弹窗是否出现、用户是否正在选择、`false` 是首次还是拒绝 |
| Screen Recording | `CGPreflightScreenCaptureAccess()` | `CGRequestScreenCaptureAccess()` | 当前进程是否已有 screen capture access | `false` 是首次/拒绝/等待设置/待重启，系统是否会再次弹窗 |

### Accessibility

Apple 对 `AXIsProcessTrustedWithOptions` 的契约很明确：返回值只表示当前进程是否已被
信任；`kAXTrustedCheckOptionPrompt` 触发的提示是异步的，且不影响返回值。因此首次调用
时返回 `false` 是预期行为，不能被解释成用户刚刚拒绝，也没有 completion callback。
[Apple Developer Documentation][ax-options]

本机 SDK 的
`ApplicationServices.framework/.../HIServices.framework/.../AXUIElement.h:55-74`
与线上文档一致；无提示复检可继续使用 `AXIsProcessTrusted()`。

### Screen Recording

本机 SDK 的 `CoreGraphics.framework/.../Headers/CGWindow.h:294-298` 只承诺：

- `CGPreflightScreenCaptureAccess()` 检查当前进程是否已经有捕获权限；
- `CGRequestScreenCaptureAccess()` 在缺少权限时发起请求，并且“potentially prompting”。

Apple 的公开 API 页面只给出 `CGRequestScreenCaptureAccess() -> Bool` 声明，没有
Return Value 语义。[Apple Developer Documentation：request][cg-request]
Apple DTS 也将职责拆成“request 用于触发对话框，preflight 用于检测是否已授权”。
[Apple Developer Forums（DTS 回复）][cg-dts]

所以稳健做法是：忽略 request 返回值的产品含义，调用后再以 preflight 为事实来源；
不要据此制造 `denied`、`prompt_shown` 或 `restart_required` 等系统并未提供的状态。

## 推荐产品流程

```text
页面出现 / 应用重新激活
  └─ 无提示复检
       ├─ 已授权 → Granted
       └─ 未授权
            ├─ 本轮尚未请求 → Request Access
            │    └─ 只调用系统 request，不主动跳设置
            └─ 已请求仍未授权 → Open System Settings + Recheck

Screen Recording 在同一进程观察到 false → true
  └─ Granted · Restart required → 用户主动 Relaunch
```

具体约束：

- 页面 mount、用户点 **Recheck**、以及从系统弹窗或 System Settings 回到 tcode 时复检。
  AppKit 的 `NSApplication.didBecomeActiveNotification` 会在应用重新 active 后发出，适合
  触发这次复检。[Apple Developer Documentation：didBecomeActive][app-active]
- 首次 **Request Access** 只调用 request。若仍是 `false`，界面保持“未授权/等待用户
  完成系统操作”，不要立即宣告拒绝。
- 后续 **Open System Settings** 必须是用户明确点击的兜底动作，不能与 request 在同一
  点击处理器中无条件连用。精确 pane deep-link 只是导航便利，不是授权状态信号；若
  系统版本不接受链接，应保留可读的手动路径说明。
- 重复调用 request 不应承诺一定再出现弹窗：AX 提示是异步通知，CG 头文件只写
  “potentially prompting”。一旦本轮请求后仍未授权，主行动应变为打开设置并复检。
- Screen Recording 由 `false` 变为 `true` 后再显示 **Relaunch tcode**；Accessibility
  不因一次请求显示重启提示。Apple 当前的 screen recording 用户指南也允许用户随时
  在 Privacy & Security 中更改选择，因此每次使用前的事实检查仍有价值。
  [Apple Support：Screen & System Audio Recording][screen-support]

### Relaunch marker

授权请求不等于授权成功，因此 marker 不应同时充当“已需要重启”的证据：

- 普通的 app-controlled relaunch：在用户点击 **Relaunch tcode** 后、真正 relaunch 前
  写 marker。
- 如果 tcode 要在权限设置期间发生任何 quit/relaunch 后继续恢复原页面，可在发起
  Screen Recording 请求前写一个**临时 flow marker**，但应用重新 active 且 preflight
  仍为 `false` 时必须清除；它只能用于恢复页面/会话，不能驱动 restart banner。
- 启动后消费 marker 并重新检查；最终 UI 始终以两个官方检查 API 为准。

## 对当前实现的直接含义

修复前的 `SettingsPage::grant_permission` 在一次点击里依次执行 request 和
`open_settings_pane`，并且对 Screen Recording 无条件设置 restart hint。应改为：

1. 删除 request 后的自动 `open_settings_pane`；
2. 为仍未授权的行保留独立 **Open System Settings** 兜底；
3. 在 app 重新 active 时自动复检，同时保留手动 **Recheck**；
4. 只在观察到 Screen Recording `false -> true` 时设置 restart hint；
5. Accessibility 请求不写 relaunch marker；Screen Recording 的临时 marker 在未授权
   返回时清理。

## 验证矩阵

- 全新 TCC 状态：请求后只出现系统弹窗，不由 tcode 同时打开设置。
- 系统弹窗中拒绝：仍显示未授权和 Open System Settings，不显示重启，不遗留 marker。
- 从弹窗或设置中授权 Accessibility：回到 tcode 后自动变为 Granted，无重启提示。
- 从弹窗或设置中授权 Screen Recording：复检确认后才显示 Restart required；重启后
  仍为 Granted，marker 被消费。
- 已拒绝后再次点击：即使系统不再弹窗，Open System Settings 仍可完成流程。
- 在系统设置中撤销任一权限：下次激活/使用前复检回到未授权。
- Debug 与 release 使用稳定签名身份；Apple DTS 确认改变 code-signing identity 会让
  系统把构建视为不同应用，导致已有 screen capture 权限不再匹配。
  [Apple Developer Forums（DTS 回复）][signing-dts]

[ax-support]: https://support.apple.com/en-gb/guide/mac-help/mh43185/mac
[hig-privacy]: https://developer.apple.com/design/human-interface-guidelines/privacy
[ax-options]: https://developer.apple.com/documentation/applicationservices/1459186-axisprocesstrustedwithoptions
[cg-request]: https://developer.apple.com/documentation/coregraphics/cgrequestscreencaptureaccess()
[cg-dts]: https://developer.apple.com/forums/thread/683860?answerId=684400022
[sc-sample]: https://developer.apple.com/documentation/screencapturekit/capturing-screen-content-in-macos
[app-active]: https://developer.apple.com/documentation/appkit/nsapplication/didbecomeactivenotification
[screen-support]: https://support.apple.com/en-ie/guide/mac-help/mchld6aa7d23/mac
[signing-dts]: https://developer.apple.com/forums/thread/819406
