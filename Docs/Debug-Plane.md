# LinkX 的 AI 调试控制面（Debug / Agent Control Plane）

> 这份文档回答开源社区一定会问的三件事：**你的产品里有没有一个没人说的后门？**
> 答：有一个本机 HTTP 控制面，但**交付构建里没有它**，而且它**只能驱动生产路径本来就允许的动作**。
> 下面把它的边界、能力、构建门控与自证方法全部写清。

## 1. 它是什么、为什么存在

LinkX 的验收要求是「每一轮改动都有真机证据」：配对能不能成、221 MB 文件逐字节对不对、
断线续传救没救回来。这些都要**反复、可脚本化地**驱动两台真设备。靠人点鼠标做不到规模化，
所以做了一个 HTTP 控制面，让脚本（以及驱动脚本的 AI/自动化）能：

- 读运行期状态（连接、任务、相册、通知、剪贴板、计数器）；
- 触发**既有生产命令**（点连接、确认 SAS、发文件、取消、切功能开关……）；
- 注入故障（丢掉指定序号的分块），用来验证错误路径。

**硬规矩**：调试面**不得另造一套逻辑**。每个 `/action/*` 都只是把命令
投递到 GUI 点击所走的同一条路径上。否则「调试面跑绿而交付版跑不通」就是假验证——
这一条在 `Platforms/Windows/src/app.rs` 的动作分派处有对应注释，是代码约束不只是文档口号。

## 2. 边界：它能碰到谁

| 项 | 事实 |
|---|---|
| 监听地址 | **只绑 `127.0.0.1`**（`Crates/debugd/src/lib.rs`：`DEFAULT_PORT = 55699`）。绝不监听 `0.0.0.0`；安卓侧要透到电脑，用的是 `adb forward tcp:55700 tcp:55699`，即由 adb 在电脑侧开一个到设备的隧道，而不是让手机开放端口 |
| 浏览器发起的请求 | **一律拒绝（403）**：`Host` 不是回环名、或带 `Origin`/`Referer`、或 `Sec-Fetch-Site` 不是 `none`。<br>这一条是补上的：过去这里写的"只绑 127.0.0.1 所以无远程可达路径"**是错的** —— DNS 重绑能让恶意网页把域名解析到 `127.0.0.1`，于是浏览器发出的请求看起来就是本机发的：`GET /state` 能读走 SAS 与双端指纹，而 `POST /action/confirm-sas` 是 `text/plain` 简单请求（不触发 CORS 预检），等于**替用户点掉那道防中间人确认**。有两条对照测试守着（本机脚本必须放行 / 浏览器形状必须拒） |
| 鉴权 | **无鉴权**，靠"只绑回环 + 不接受浏览器发起"这两条边界成立。这意味着**同一台机器上的任意本地进程/用户**仍可驱动它——这正是它绝不能进入交付构建的原因之一。若要在多用户机器或不可信本地环境里用，先关掉它（不设 `agent-debug` 就没有这个监听） |
| 能做什么 | 只能做产品 UI 本来就能做的事 + 故障注入。**不能**：导出会话密钥、跳过 SAS 核对、伪造已配对身份、连到任意远端主机、执行任意命令 |
| 路径处理 | `/action/` 前缀之外的路由是固定枚举；`../../Windows/System32` 这类路径有单测断言返回 404（`Crates/debugd/src/lib.rs` 的测试） |
| 生命周期 | 只存在于开发/验收环节。日常装机用的是 release 构建，压根没有这个监听；交付面的自证见 `Scripts/check-release-clean.sh`（依赖图闸门 + `.so`/`dex` 字节扫描） |

## 3. 路由

| 路由 | 方法 | 作用 |
|---|---|---|
| `/` | GET | 纯文本索引，列出可用路由 |
| `/health` | GET | `{"ok":true}` 存活探针 |
| `/state` | GET | 运行期状态快照 JSON（链路、配对、文件任务、相册、通知、剪贴板、错误列表……） |
| `/logs` | GET | NDJSON 埋点流；`?tail=N` 取末尾 N 行 |
| `/debug` | POST | 打开/关闭落盘调试日志：`?on=1`。**注意 `debuglog` 的 `ENABLED` 默认是 false**，不开就是 `/logs` 空 |
| `/counters` | GET | 内部计数器（帧数、字节数、通知数等） |
| `/action/<name>?<query>` | 任意 | 执行一个动作，见下表 |

## 4. Windows 端动作清单

出处：`Platforms/Windows/src/app.rs` 的 `agent-debug` 分派块。

| 动作 | 参数 | 说明 |
|---|---|---|
| `connect` | `?addr=<16 进制>` | 对设备列表里扫到的地址下发「点连接」，走的是 UI 同一条请求 |
| `confirm-sas` / `reject-sas` | — | 模拟两端核对 SAS 后的「一致 / 不一致」按钮。**它只是按下按钮，不会替系统跳过核对** |
| `accept-identity` / `reject-identity` | — | 对端身份变化提示的接受 / 拒绝 |
| `manual-ip` | — | 手动 IP 连接 |
| `unbind` | — | 解绑当前设备 |
| `send-clip` | `?text=` | 发送剪贴板 |
| `send-file` | `?path=<绝对路径>` | 电脑→手机发送（只回执「已下发」，是否受理看 `/state` 的 `errors` 与 `file_tasks`） |
| `cancel-file` | `?name=&dir=send\|recv` | 取消传输 |
| `album-list` | `?page=<**从 0 起算**>&per=` | 请求相册清单（条目 id/名字/大小/类型看 `/state` 的 `ui_album_items`，几何看 `ui_album_scroll`）。页码是 0 起算：传 `page=1` 拿的是**第二页**，据此判断"相册少了东西"会得出假结论（0.5.0 就是这么误登记过一条 P1，当天翻案）。 |
| `album-thumb` | `?id=` | 请求单张缩略图 |
| `album-export` | `?id=<可逗号分隔>&dir=<绝对路径>` | 手机→电脑导出原图/视频 |
| `feature` | `?module=notifications\|clipboard\|file_transfer&on=0\|1` | 功能开关（参数写错一律报错，不给默认值） |
| `media` | `?action=0..8&volume=&delta_ms=` | 多媒体控制指令 |
| `restart` | — | 按正常退出流程重启实例 |
| `drop-chunk` | `?at=<分块序号>` | **故障注入**：发送侧第 N 块不交给引擎（一次性） |
| `drop-recv-chunk` | `?at=<分块序号>` | **故障注入**：入站第 N 块当作没收到（一次性） |

## 5. Android 端动作清单

出处：`Platforms/Android/app/src/main/java/com/linkx/app/LinkxRuntime.kt`（调试面由
`NativeCore.nativeDebugdStart()` 在核心 `.so` 里拉起，端口同为 55699）。

`connect`、`confirm-sas`、`reject-sas`、`accept-fingerprint`、`unbind`、`send-clip`、
`toggle-clip`、`set-theme?mode=light|dark|system`、`manual-ip`、`media-cmd`、
`send-file`、`cancel-file`、`album-list`、`album-thumb`、`album-full`、`drop-chunk`。

从电脑访问手机上的它：

```bash
adb forward tcp:55700 tcp:55699
curl -s http://127.0.0.1:55700/state
```

## 6. 怎么构建「带」与「不带」的两种包

| | Windows | Android |
|---|---|---|
| 门控机制 | cargo feature `agent-debug = ["dep:linkx-debugd", "dep:serde_json"]`（`Platforms/Windows/Cargo.toml`）；所有相关代码在 `#[cfg(feature = "agent-debug")]` 内 | 核心 `.so` 的 cargo feature 同名；由 `Scripts/build-android-core.sh` 的 `DEBUGD=1` 环境变量决定是否加上 |
| 不带（交付） | `bash Scripts/build-msi-windows.sh`（默认特性，不含 `agent-debug`） | `bash Scripts/build-android-core.sh && bash Scripts/build-android-apk.sh --release` |
| 带（调试变体） | `cargo build --features agent-debug`（直接产出带控制面的 `linkx.exe`，调试用，**不打 MSI**） | `DEBUGD=1 bash Scripts/build-android-core.sh` 后 `bash Scripts/build-android-apk.sh`（**不带 `--release` 就是 debug**，脚本默认值即 debug） |

**坑（已踩过，写在这省你两小时）**：安卓的 `DEBUGD=1` 必须**同时**给
`build-android-core.sh` 和 `build-android-apk.sh`。APK 脚本会校验 `.so` 里的
agent-debug 状态并强制重建，只给前者会产出一个「名字叫 debug、其实没有控制面」的包。

调试包与交付包**用同一把签名**（`Tools/Keys/linkx-test.jks`，由脚本缺失时自动生成），
所以覆盖安装不会丢配对——不需要为了装调试包而重新配对两次。

## 7. 怎么自证交付产物里没有它

```bash
bash Scripts/check-release-clean.sh                     # 扫 linkx.exe
bash Scripts/check-release-clean.sh --apk-so <apk 路径>  # 解出 APK 内 lib/arm64-v8a/liblinkx_core.so 再扫
```

做法：对产物做 `strings`（缺失时退化为 `grep -a` 扫字节），命中下列任一**即失败**——

```
linkx-debugd  nativeDebugdStart  nativeDebugTakeRequest  nativeDebugSetField
nativeDebugCounter  nativeDebugAddRequestLimit  /state.host  55699
GET /counters  action/confirm-sas  serde_json
```

同时**必须**命中一个产品符号（exe 里是 `LinkX Core`，`.so` 里是
`Java_com_linkx_app_NativeCore_nativeSendMediaState`），否则判定为「扫错了文件或产物是旧的」——
这条反向断言防的是「扫了个空文件所以全绿」。

## 8. 用它做验收时，判据必须来自埋点

一个真实的教训：任务行按 `(方向, 文件名)` 复用，界面「有文件了」「显示成功」都可能造假通过。
所以 `Scripts/verify-resume.py`、`Scripts/soak-transfer.py` 这类脚本的结论**只从
`/logs` 的 ndjson 事件 + 落盘字节数比对得出**，不读界面文案。
自己写自动化时请沿用同一口径。

## 9. 我不想用它，能不能彻底去掉

能。`agent-debug` 是 optional feature，交付路径默认不启用；
`Scripts/check-release-clean.sh` 保证交付产物里连字符串都不存在。
从源码 `cargo build --release` 出来的东西就是这个状态——你可以自行跑第 7 节的命令验证。
