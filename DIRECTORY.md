# 仓库目录与文件用途

这份文件说明仓库里有什么、每一部分负责什么，方便你直接找到要读的那一处。

第一次接触本仓库，建议的顺序是：[`README.md`](README.md) 看能做什么与怎么上手，
[`Docs/Limitations.md`](Docs/Limitations.md) 看有什么限制，本文件看东西在哪里，
[`Docs/Build.md`](Docs/Build.md) 看怎么从源码构建。想再深入某一面，按需要读
[`Docs/Security.md`](Docs/Security.md)（安全与隐私口径）或
[`Docs/Debug-Plane.md`](Docs/Debug-Plane.md)（调试控制面）。
某一能力是哪一版进来的、当时改了什么，看 [`CHANGELOG.md`](CHANGELOG.md)。

命名约定：目录首字母大写（`Crates/`、`Docs/`、`Scripts/`）；文件名小写加连字符；
Rust crate 名带 `linkx-` 前缀（`debuglog` 除外，它是历史名）。

## 一级目录

```text
LinkX/
├── Crates/          共享 Rust 核心，电脑端与手机端链接的是同一份
├── Platforms/       两个平台的壳：Windows 与 Android
├── Docs/            面向使用者的文档
├── Proto/           协议定义的单一来源
├── Tests/           独立的 fuzz workspace
├── Licenses/        第三方许可全文与出处记录
├── svg/             图标源文件，两端图标与产品 Logo 的唯一真源
├── Release/         交付物清单与逐版校验值（安装包本体不入库）
├── Scripts/         构建、门禁与验收脚本
├── .cargo/          交叉链接器约定与构建目录位置
├── .github/         CI：格式检查、clippy、单测与门禁脚本
└── 根文件            简介、变更、许可与 workspace 配置
```

## 根文件

- `README.md` 产品简介：能做什么、怎么上手、技术框架、许可。
- `CHANGELOG.md` 逐版变更（0.1.0 → 0.5.1）。
- `DIRECTORY.md` 本文件。
- `LICENSE` GPL-3.0-or-later 的 FSF 官方全文。
- `THIRD_PARTY_LICENSES.md` 第三方代码与素材登记，以及随产物分发的义务。
- `Cargo.toml` workspace 根：成员列表、`[workspace.package]` 版本单一源、依赖版本统一表。
- `Cargo.lock` 依赖锁定，全部来自 crates.io，没有 git 源。
- `rust-toolchain.toml` 锁定 Rust 通道版本，与 CI 和构建脚本保持一致。
- `.gitattributes` 全仓文本强制 LF（`.sh` 一旦变成 CRLF 会直接坏构建链），二进制禁止转换。
- `.gitignore` 排除构建产物与安装包，见文末「不在仓库里的东西」。

## Crates/

两端都链接这十个 crate，逻辑不分叉是这个仓库的结构约束。每个 crate 目录下都有自己的
`Cargo.toml`，依赖版本一律走 workspace 统一表。

`protocol` 是线格式层：13 字节帧头、帧的加密封装边界、64 字节 TLV 小载荷、蓝牙分片与重组
（MTU 上限、乱序与重复的语义、内存上限），消息代码由 `build.rs` 从 `Proto/` 生成。
`crypto` 是加密原语与身份：RSA-2048 长期身份、Noise XX 握手、每帧 AEAD
（ChaCha20-Poly1305 + X25519 + SHA-256）、通道绑定、接收侧滑动窗口，退化公钥在这里拒掉。

`session` 是会话状态机：配对与 6 位 SAS 计算、通道选择、心跳与退避、重连、
文件 / 相册 / 配置的事件派发，信任库以指纹为主键。`lan` 是局域网数据面：UDP 广播与扫描、
TCP 连接与读写线程、加密字节流的分帧、抗重放与背压窗口。`transfer` 是文件传输语义：
`FILE_META` / `CHUNK` / `DONE` / `RESUME` 的字段编解码、分块切分与逐块 CRC32、发送与接收
任务对象，以及接收侧文件名净化（`src/filename.rs` 是 Apache-2.0 上游的移植件，义务见
`Licenses/localsend/`）。

`storage` 是本地库，用 bundled SQLite，不起外部进程；`schema.sql` 是结构单一源，编译时嵌入。
`ffi` 是给两端壳的接缝：C ABI 门面、事件队列、安卓 JNI 入口。`debuglog` 与 `debugd` 是埋点
和本机回环调试控制面，只在 `agent-debug` 特性下编译，默认关闭。`app` 是自检入口，
`cargo run -p linkx-app` 打印 N/N PASS。

## Platforms/

电脑端是纯 Win32 + GDI 自绘：不用控件库、不用 Direct2D、也不用 WebView。
`src/main.rs` 负责入口、单实例与每显示器 DPI 声明，`src/app.rs` 是工作线程与收发编排，
`src/state.rs` 一份界面状态同时供渲染和命中判定，`src/render.rs` 与 `src/window.rs` 是绘制
和窗口过程，`src/theme.rs`、`src/icons.rs` 是配色与描边图标。协议与传输相关的平台侧在
`src/ble_central.rs`（电脑端是蓝牙主机）、`src/network.rs`、`src/transfer.rs`，
落盘与开关在 `src/identity.rs`、`src/settings.rs`、`src/features.rs`。
`src/icons_svg.rs` 是生成物，改图标请改 `svg/` 再跑生成脚本。
系统消息走 `src/toast.rs`：弹 Windows 自己的通知卡（卡上两颗按钮靠 `linkx://` 回到主进程，
口令一次性），弹不出去才回落 `src/tray.rs` 的托盘气泡；`src/ipc.rs` 是"第二次启动只转交、
不再开一个窗口"的那条命名管道，也是按钮激活参数的入口。
两份安装器源各喂一种打包引擎，内容要一起改：`installers/LinkX-v4.wxs` 给 Windows 上的 WiX v5
（正式交付包由它出，含中文向导、防火墙规则、许可页、`linkx://` 注册），
`installers/LinkX.wxs` 给 Linux 上的 wixl（交叉出包链路，向导是英文）。

手机端是 Kotlin + Jetpack Compose，经 JNI 调同一份核心。`MainActivity.kt` 是各屏与底部导航，
`LinkxRuntime.kt` 与 `NativeCore.kt` 是和 Native 核心的接缝，`BlePeripheralService.kt` 是蓝牙
外设角色与前台服务（广播必须带设备名，电脑端按名字认领设备并自动重连），`NlsService.kt` 是
通知监听（正文与标题不进日志，只转发 10 分钟内的条目），`BootReceiver.kt` 负责开机与覆盖安装后
拉起链路服务。其余按能力分文件：剪贴板同步、通知回复能力判定、媒体控制、相册、电量、
身份与偏好存取、功能开关、前置条件检查。`res/drawable/` 的矢量全部由 `svg/` 生成，
与电脑端同几何；`app/proguard-rules.pro` 必须 keep 住 JNI 入口。

## Docs/

四份文档各自独立，按需读，不必顺序读。`Limitations.md` 是已知限制完整版：平台与 ROM 边界、
容量上限、交互短板、签名现状。`Security.md` 是安全与隐私口径：数据路径、信任模型、加密实现、
不可信输入、披露渠道。`Build.md` 是从源码构建：两端出包、GNU 目标的原因、门禁脚本、真机验收。
`Debug-Plane.md` 是可选的调试控制面：边界、路由、动作、门控与自证方法，
只在带 `agent-debug` 特性的构建里存在。

## Proto/

协议单一来源，两端实现都由它对齐。`buf.yaml` 与 `buf.gen.yaml` 是 lint 和代码生成配置；
`linkx/v1/` 下按能力分文件：公共字段、设备信息与能力、心跳、通知转发与回复、剪贴板、
文件传输、媒体控制、相册、跨端配置。同目录的 `tlv.rs` 是手写的蓝牙载荷与消息类型常量，
与 proto 放在一处便于核对。

## Tests/

`fuzz/` 是一个独立 workspace，用 nightly 与 sanitizer，不进主 workspace。三个目标覆盖
来自对方设备的可控输入：握手字节解析、13 字节帧头、消息体解析。`corpus/` 里的种子入库，作为回归起点；
崩溃样本 `artifacts/` 不入库。

## Licenses/

`localsend/` 存上游的 Apache-2.0 全文、署名 NOTICE，以及 `PORTING.md`（移植范围与改动声明），
对应 `Crates/transfer/src/filename.rs`。仓库内没有 vendored 代码。

## svg/

25 个 SVG 是图标唯一真源，含产品 Logo 几何。`Scripts/svg_assets.py` 由它生成两端矢量资源、
`LinkX.ico` 与 `ic_launcher.xml`。分组是：Logo、侧边栏导航九枚、媒体控制九枚、
动作与状态类若干。

## Release/

`Release/README.md` 登记当前版本交付物的清单与校验值，`Archive/00-README.md` 是逐版校验值台账，
`Archive/v<版本>/{Windows,Android}/` 只跟踪 `.sha256` 文件。安装包二进制走 Releases 页面分发。
`Debug/` 是本机的调试包目录，不入库也不归档。

## Scripts/

Python 脚本只用标准库。构建出包：`build-mingw-windows.sh` 与 `build-msi-windows.sh` 出电脑端，
`build-android-core.sh` 与 `build-android-apk.sh` 出手机端。门禁：`check-version-sync.sh` 查版本
单一源，`check-protocol-sync.sh` 查协议与两端实现一致，`check-crypto-audit.sh` 查加密用法，
`check-compose-kotlin-pair.sh` 查 Compose 与 Kotlin 版本配对，`check-release-clean.sh` 查交付产物
无调试残留，`check-install-single.sh` 查安装份数与作用域，`check-comment-hygiene.sh` 查注释卫生。
`check-release-ledger.py` 核对 `Release/README.md` 登记的哈希与大小，本地出包后跑，不进 CI。
`check-repaint.py` 用来看活窗口的刷新是否跟得上数据变化。

图标链是 `svg_assets.py` 加两个模板 `launcher_icon.tmpl`、`vector_icon.tmpl`。
真机验收与测量：`check-file-transfer.py`（电脑端→手机端）、`check-phone-to-pc.py`（手机端→电脑端）、
`soak-transfer.py`（多轮双向传输）、`verify-resume.py`（断点续传）、`stress-pair.py`（重复配对）、
`bench-throughput.py`（吞吐与工作集峰值）、`mem-map.py`（工作集构成）、
`measure-feature-memory.py`（功能开关的内存收益）、`ui-shot.py` 与 `nav_preview.py`（界面截图与
图标预览）、`linkx-ctl.py`（调试控制面命令行）、`msi-walkthrough.ps1`（安装向导走查）。

## 不在仓库里的东西

| 路径 | 是什么 | 为什么不在 |
|---|---|---|
| `Target/` | cargo 构建目录 | 构建产物 |
| `Release/Windows/`、`Release/Android/` | 当前版本的安装包 | 二进制走 Releases 页面，校验值登记在 `Release/README.md` |
| `Release/Debug/` | 调试包 | 不交付、不归档 |
| `Release/Archive/` 里的安装包本体 | 历史版本的 `.msi`、`.apk`、`.idsig` | 同上，仓库只跟踪 `.sha256` |
| `Tests/fuzz/artifacts/` | fuzz 崩溃样本 | 语料种子才是回归资产 |
| `*.jks`、`*.keystore` | 签名密钥 | 密钥不入库 |

还有一类不逐个列路径：维护者本机在开发过程中产生的记录与私有脚本。它们写的是内部怎么走的，
对外读者按图索骥只会撞到不存在的东西。需要公开的事实由 `README.md`、`CHANGELOG.md`、
提交历史和上面这几份文档承担。
