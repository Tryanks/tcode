# #409 换网恢复：复盘、修复与验证

本次检查基于 `a8321fdd`，追溯 [#409](https://github.com/Tryanks/tcode/issues/409)
及关闭它的 [#453](https://github.com/Tryanks/tcode/pull/453)。后者在
2026-09-17 04:00（UTC+8）合入，提交
[`f8dca71e`](https://github.com/Tryanks/tcode/commit/f8dca71e6f8a28689fe7511454a4c2172c17ebe1)。
这里的“复原”是还原原方案和失败链路，没有回滚用户工作。

## 结论与现场证据

原方案能重试**已经知道的地址**，却不能可靠找到普通 Wi-Fi 上**从未知道的新地址**。
在 `/24` 网络中，它主动探测的通常只是 `.1`，并不会寻找公司电脑的 `.161`。
新地址只能依赖 mDNS 等额外线索，而线索从发现、保存到消费又有多处丢失和竞争。
这足以解释“在家配对，到了公司双方能 ping，但始终恢复不了”的失败机制；
没有手机端现场日志，不能断言现场究竟是哪一环最先失败。

本机检查到 `en0 = 192.168.1.161/24`，另有
`bridge100 = 192.168.139.3/23`，桥成员包含 `vmenet0`。
这说明 `.139` 来自真实存在的虚拟桥接网段，并非 Tcode 凭空生成。
看到虚拟化软件运行不能单独证明是哪一个软件创建了该桥。

二维码问题与换网恢复问题相关，但不是同一个问题。旧二维码取所有本机地址
按字符串排序后的第一项；在家庭 `192.168.31.*` 环境中，`.139` 排在 `.31`
前，扫码端照抄该 origin。在公司，重新生成的列表中 `.1` 本应排在 `.139`
前；若仍出现 `.139`，还需要当时的二维码、生成时间或接口快照才能解释。
不能用家庭排序规则代替这部分现场证据。

本机安装的桌面版本为 `0.1.52`，已含 #453；保存的设备平台信息显示
`Android 16`，这不是手机端应用版本。检查时没有可用的 adb 设备。
没有读取或展示用户 token，没有改写用户的真实配对记录。

## 当时具体做了什么

| 原实现 | 能解决什么 | 为什么未覆盖这次场景 |
| --- | --- | --- |
| `PairedHost` 增加最多 16 个候选 origin | 记住旧成功地址、hello 返回地址、发现提示 | 缓存不能凭空知道另一地点的新 IP；满了还会拒绝新提示 |
| hello 返回 `host_id/addrs/port` | 已连上时学习机器其他地址 | 必须先连上，不能单独解决首次找到新地址 |
| 原址先单独尝试，失败后竞速最多 32 个地址，每个间隔 250ms、预算 5s | 旧地址失效时尝试已知替代地址 | 未知地址不在候选里就不会被尝试；原址黑洞还延迟启动发现 |
| 探测常见热点网关、私网 `.1`，`/28`～`/30` 扫全体主机 | 部分固定热点和很小的子网 | 普通 `/24` 里的 `.42/.161/.254` 不是网关；一般 `/16` 更不在覆盖范围 |
| 断线时 2.5s、在线时 10s 检查接口快照 | IP、前缀、接口变化可打断退避并检查旧 socket | 不等于监听 Wi-Fi 名称；换网但 IP/前缀相同仍靠心跳/前台唤醒识别 |
| mDNS 给 transport 发送候选提示 | 从局域网公告找到新地址 | 多地址被折叠、重试取消在途 browse，且公司网可能只允许单播 |
| hello 后按 `host_id` 判定是谁 | 避免普通陌生机器的拒绝误触发重新配对 | `host_id` 是公开声明；当时 bearer 已先发送，不能当作安全身份验证 |

mDNS 浏览、重连后的订阅重放原本就存在，并非 #453 新创。
当时的主要恢复测试提前放入了正确候选地址；另一次实际验证是同机变更监听端口。
它们能证明已知候选的尝试和 hello 恢复，不能证明手机跨 Wi-Fi 后能发现未知 IP、
接收会话快照并恢复 Preview。

## 本次完成的修复

| 问题 | 最终行为与代码归属 |
| --- | --- |
| 扫码选到虚拟桥 | `discovery.rs` 按接口用途和地址族排列，普通 LAN 优先，桥/VPN 保留为备选；QR 带多地址及主机公钥 |
| 发现只保留每台机器一个地址 | native mDNS 与 iOS 结果解析按 `(host_id, origin)` 去重，保留不同地址；Rust mDNS 用实际本地前缀排序，iOS 保留平台的同地址族顺序 |
| 普通 Wi-Fi 新 IP 不会被尝试 | 从客户端直连 RFC1918 IPv4 子网分页探测，包含普通主机地址；不依赖网关或固定热点地址表 |
| 大网段、虚拟接口、边界遗漏 | 每轮最多四个子网、每网最多一页；接口组轮转，页按本地页、上邻页、下邻页交错；真实掩码决定网络/广播地址，私网裁剪不能误删合法主机 |
| 候选满了拒收，超大重复提示反复翻页 | 候选仍上限 16；新提示可淘汰旧提示，同一超大批次重复到达不改变缓存、不重启尝试 |
| 在途发现被下一次重试取消 | UI 让 browse 完成；完成后允许下一轮，连接恢复或 attachment 结束才取消 |
| 新提示在发送半个 WebSocket 帧时打断连接 | 同一个发送 future 内吸收提示，不取消、不重复发送部分帧；其他重连/关闭唤醒保持原语义 |
| 原址黑洞拖延、前台重连只试旧址 | 每次建立连接都允许已知候选与 LAN 探测；健康原址优先启动，找到认证成功的机器即取消其余尝试 |
| 将 token 发给猜到的地址 | `identity.rs` 负责主机证明；主连接与 Preview 在同一 TCP/TLS 流先做有界身份检查，再发送 bearer |
| 单次配对码被候选竞争消耗 | 新 QR 只竞速身份验证，在首个通过者上发送一次 code；响应丢失不自动向其他地址重复提交 |
| 陈旧连接覆盖地址、pin、时间戳 | `update_hosts` 持有进程间文件锁，按字段更新、原子替换文件；token 不匹配不写，已建立的 pin 不允许被移除或替换 |
| 未认证提示污染已保存地址 | 提示先留在内存，认证成功后才由 transport 保存；不会因 mDNS 广告直接更换已保存的 primary |
| 同原址认证成功却未保存内存里的新提示 | 每次认证成功都经过字段事务；磁盘内容没变时事务不重写文件 |
| Preview 留在旧 IP | attachment 共享完整配对信息和 endpoint 代际；主连接恢复后更新入口，关闭旧代际连接，保留浏览器与本地 URL |
| 地址写盘失败导致 Preview 跟不上主连接 | transport 在发送 Syncing 事件前发布 `LiveHost` 快照，Preview 从当前 attachment 读取；磁盘用于重启恢复 |
| 取消后的旧配对结果覆盖新配对 | UI 先确认 generation 有效，再保存结果 |
| AuthStore 磁盘失败但内存已变 | token 发放、撤销、设备刷新先保存副本，成功才提交内存状态 |
| 恶意响应的大声明触发预分配 | HTTP 身份头/体均有界；QR WebSocket 在解析前限制帧/消息；已认证的主连接仍可接收大历史快照 |
| 合法大写公钥、缺半个身份字段、旧 QR 身份不符 | 公钥规范化；有 key 无 host 拒绝；旧 QR 返回的 host_id 必须匹配，不静默降级有 pin 的邀请 |
| 已固定公钥但 token 损坏时无限重试 | 身份挑战不先拒绝 token 格式；先验证主机，再接受它的终态拒绝；真实撤销与本地损坏均有测试 |
| 配对错误直接显示英文、403 误提示“必须六位” | 双语提示区分身份失败、坏邀请、结果未确认与拒绝，给出相应恢复操作 |
| 附带发现：旧 `block` 依赖的 extern 类型不可实例化 | 保持 0.1.6 API 的最小源代码补丁，保留许可证和上游互操作测试；详见 [补丁说明](../vendor/block/PATCH.md) |

行为合同同步更新在 [remote.md](remote.md) 和 [DESIGN.md](DESIGN.md)。
配对失败的新增提示同步提供中英文；没有增加新的网络设置开关。

## 白板说明：怎么工作，为什么这样做

恢复分为五步：**发现地址 → 验证机器 → 使用原 token 做 hello → 重放订阅 → 等待快照恢复工作区**。
“TCP 通了”“hello 成功”“用户数据已恢复”是不同阶段。Preview 随当前 attachment 发布的已认证入口更新，
不依赖地址是否成功写盘；写盘失败会记录错误，重启只能从上一次成功保存的记录重新发现。
其浏览器内正在执行的 HTTP/WebSocket 请求会被取消；不重放这些请求，以免 POST 等操作执行两次。
浏览器是否自行重试由浏览器负责。

**为什么不用一个正确 IP 永远解决？** DHCP 和地点都会改变地址，同机还会同时存在 Wi-Fi、
有线、VPN、虚拟桥。IP 是到达机器的线索，公钥和配对 token 才是身份/权限。
接口名排序只是初始优先级，不声称能精确识别所有厂商虚拟网卡，因而不能把低优先级地址删掉。

**为什么保留 mDNS 又加探测？** mDNS 快且能携带变更后的端口，但公司 Wi-Fi 可能不转发多播。
单播私网探测覆盖这条缺口，代价是有限的网络流量和等待。仅扫描附着的私网、已知监听端口，
不枚举整个公网或 IPv6 地址空间，也不引入外部发现服务器。

**为什么用这些数据结构？** 每台配对的候选 `Vec` 最多 16 项，简单且顺序本身就是尝试优先级；
生成本轮任务时用集合去重，避免同地址通过多个来源重复占预算。有限并发流最多执行 16 个
origin 任务；LAN 探测使用 IP，一个任务对应一个地址，域名 origin 内部还会竞速 DNS 解析结果，
因此总 socket 数不能一概称为 16。接口页和组随轮次推进，不让较低优先级接口永久饿死。
客户端 `hosts.json` 用文件锁保护 read-modify-write，原子 rename 保护读者；进程内 mutex 无法防另一
进程写同一客户端 profile。服务端 `remote.json` 仍由进程内 AuthStore 与原子替换管理，不应由多个
服务进程同时写同一个 profile。

`LiveHost` 是 attachment 的当前配对快照，由拥有连接的 transport 在认证成功后更新。
Preview 读取这个快照，自己的代际负责关闭旧连接；两者分别管理“当前认证入口”和“浏览器连接生命周期”。
这份内存状态有明确更新点，避免把用于重启的 JSON 文件兼作实时通知机制。

**如何证明机器，攻击者能做什么？** 主机持久保存随机 Ed25519 seed。客户端每次产生新的
32 字节随机 nonce；签名消息包含固定版本域、host_id、nonce 和公钥。新 QR 带公钥，扫描端
先核对签名再交出六位码。旧配对没有 pin 时，使用主机已保存的 `SHA256(token)` 作为 HMAC-SHA256
密钥，证明同一消息，再固定公钥；请求只带该 hash 的再次哈希，不能作为 bearer 登录。
实现复用 `ring` 和 `hmac`，有独立 OpenSSL/Python 生成的固定协议向量。
手输地址与无公钥的旧 QR 仍走单址首次信任；它们没有新 QR 在交出 code 前核对公钥的保证。

在本次要求身份检查的原生 LAN 路径中，伪造 mDNS、占用旧 IP、声明相同 host_id、自签另一个公钥，
都不能单凭这些声明获得原 token。旧 HTTPS/loopback 无 pin 的兼容路径和浏览器页面 origin 路径
另有信任前提，见下面的兼容边界。
已经固定公钥后，即使 token 被撤销仍能验证机器并接受它的明确拒绝。
**这不是链路加密**：HTTP 上的被动监听者仍能读到随后发送的 token，主动透明转发也不被这套
应用层证明阻断。需要保密性仍使用 HTTPS 或可信加密覆盖网络。
`remote.json` 的 token hash 现在也是旧配对迁移的认证材料，必须与签名 seed 一样保护。
伪造提示仍可能消耗有限的尝试预算；这不是抵御局域网拒绝服务的保证。

身份检查与 bearer 使用同一连接，避免“先证明 A，再另开连接却连到 B”的间隙。
需要身份检查的主连接先做有界 HTTP 交换，证明后才交给 WebSocket 解码器；仅检查解码后的字符串长度不够，
因为 WebSocket 库可能先按恶意帧头声明分配内存。QR 专用 WebSocket 则始终限制小消息。

## 明确的失败边界

- 电脑端与原生客户端都需更新。旧 LAN 服务端不会回答新身份请求，不能靠重输 code 补上能力。
  未固定 key 的旧 HTTPS/loopback 主址保留旧兼容路径；浏览器仍使用页面 origin。
- 升级前 token 已被撤销、客户端又没有 pin 时，不能安全地自动建立身份，需重新配对。
  已固定公钥的客户端遇到主机 key 更换也需明确重新配对，不能自动接受新 key。
- ICMP ping 通不代表监听端口可达，也不代表 mDNS 可达。AP 隔离、防火墙、绑定错误接口、监听关闭
  都不会被客户端重试修好。
- `/24` 覆盖完整主机范围；大子网每轮只扫一页。全黑洞时，一个 `/24` 的 16 并发、每次 1.5s
  预算本身就约需 24s，另有已知地址、调度和退避；远处 `/16` 页可能需要很久，极大网段完整周期
  更不适合作为快速发现保证。不是“换任何网络，几秒内必定恢复”。
- 探测使用已保存端口。完全未知的新端口、只有 IPv6 的未知地址、公网/CGNAT、非附着网段依赖有效
  地址提示或稳定入口；不会盲扫。自动扫描要求已知的 `/8`～`/30` 前缀，缺前缀或 `/31`、`/32`
  也需要提示或稳定入口。HTTPS 配对不降级到明文 LAN。
- 换 Wi-Fi 但接口地址/前缀未变时，主要靠原有心跳与前台唤醒。App 被操作系统挂起时不承诺后台恢复时限。
- Preview 保留 URL/history，但取消的在途业务请求不自动重放、不强制刷新页面。

## 回归与验收证据

关键自动化测试在 [client_recovery_tests.rs](../crates/remote/src/client_recovery_tests.rs)：
测试保留原 profile/token，在家庭与公司两套接口快照间切换，**不预置公司电脑 IP**。
注入点仅将生产算法选中的地址映射到本地测试 TCP server；地址生成、竞速、密码学身份证明、
connection_loop、持久化、Index/SessionEvents 订阅和真实协议快照都走生产路径。
测试不访问开发者实际 LAN，证明的是逻辑链路，不是 Android 网络栈或公司 Wi-Fi 的现场验收。

另有真实 TCP 配对与 Preview 测试，以及 UI 发现生命周期、过期配对结果、并发持久化测试。
候选已满、超量重复批次、虚拟桥排序、发现多地址、探测覆盖、宽掩码边界、UI browse 被取消、
旧结果覆盖新 token、AuthStore 写失败、Preview 旧连接不退出等缺陷均取得了红→绿证据。
恶意服务端测试断言没有收到 code/token，并覆盖只有巨大长度声明、没有实际 payload 的输入。
发送中途到达的提示也有真实 WebSocket 回归：先写出部分帧再阻塞，注入提示后继续原帧，
断言连接没有被取消、没有丢提示或重复发送。同原址重新认证后的提示保存另有生产连接循环测试。

最终验证结果如下；与本次代码无关的既有 ignored 仍保留其明确原因。

| 检查 | 最终结果 |
| --- | --- |
| `cargo fmt --all --check` | 通过 |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | 通过 |
| `cargo build --workspace --locked` | 通过；开发构建链接器提示见下文 |
| `cargo test --workspace --locked` | 通过，无失败项 |
| `cargo machete`（0.9.2） | 通过 |
| iOS simulator / Android arm64 / Web，按 CI 并启用 `-D warnings` | 三项通过 |
| `cargo test -p tcode-remote --lib discovery::tests::mdns_loopback_round_trip --locked -- --ignored` | 通过；仅证明本机 loopback 多播，不代替公司 Wi-Fi 验收 |
| `RUSTFLAGS='-D warnings' cargo test --manifest-path vendor/block/Cargo.toml --locked` | 上游 6 个互操作测试、2 个文档测试通过，无警告 |
| `cargo build --release -p tcode --locked` | 通过，编译与链接零警告 |

Workspace 验证运行于 macOS；移动/Web 为编译检查，未在本机运行 Windows/Linux CI。
三项跨平台命令如下，Android 使用 NDK `27.1.12297006`、cargo-ndk `4.1.2`：

```sh
RUSTFLAGS='-D warnings' IPHONEOS_DEPLOYMENT_TARGET=26.0 cargo check -p tcode-ios --target aarch64-apple-ios-sim --locked
RUSTFLAGS='-D warnings' cargo check -p tcode-web --target wasm32-unknown-unknown --locked
ANDROID_HOME=/opt/homebrew/share/android-commandlinetools ANDROID_NDK_HOME=/opt/homebrew/share/android-commandlinetools/ndk/27.1.12297006 CARGO_NDK_PLATFORM=26 RUSTFLAGS='-D warnings' cargo ndk -t arm64-v8a check -p tcode-android --locked
```

开发构建仍会触发 macOS 链接器的 `__eh_frame` 超过 16 MiB 提示。
这是未优化产物的 Mach-O compact-unwind 偏移容量边界，影响异常展开/回溯查找性能，
没有用关闭 unwind、修改 panic 语义或 `allow` 掩盖它。
最终发布配置实测 `__eh_frame = 2,699,496` 字节（约 2.57 MiB），不触发该警告。
容量边界及回退查找可对照 [LLVM Mach-O 链接器](https://github.com/llvm/llvm-project/blob/main/lld/MachO/UnwindInfoSection.cpp)
和 [Apple libunwind](https://github.com/apple-oss-distributions/libunwind/blob/main/libunwind/src/UnwindCursor.hpp)。

还需实机验收：两端安装本次构建后，在家保留配对并开启线程和 Preview，移动到公司同 Wi-Fi，
不扫码、不改地址，观察原线程快照恢复、hosts.json 的 origin 更新、Preview 原 URL 能重新访问。
再覆盖手机热点、同 IP 换 SSID、锁屏后返回前台、关闭多播但放行 TCP 的环境。
记录两端应用版本、接口/前缀、监听端口及恢复时间；日志不得包含 token、code 或完整身份响应。
当前没有连接的手机，不能将上述实机条目写成已通过。
