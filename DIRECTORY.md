# 仓库目录树与每个文件的用途

这份文件逐目录、逐文件解释仓库里有什么、为什么在那儿。
第一次接触本仓库建议的顺序：`README.md` → 本文件 → `Docs/Debug-Plane.md` → `Docs/Limitations.md`；
想知道某个能力是哪一版进来的、当时修了什么，看 [`CHANGELOG.md`](CHANGELOG.md)。

约定：**目录首字母大写**（`Crates/`、`Docs/`、`Scripts/`、`Proto/`、`Tests/`）；
文件名用小写加连字符；Rust crate 名带 `linkx-` 前缀（`debuglog` 除外，它是历史名）。

---

```
LinkX/
├── README.md                    产品宣传单：能做什么、怎么上手、与同类对比、技术框架、许可
├── CHANGELOG.md                 逐版变更（0.1.0 → 0.5.0）
├── DIRECTORY.md                 本文件：目录树与每个文件的用途
├── LICENSE                      GPL-3.0-or-later 的 FSF 官方全文
├── THIRD_PARTY_LICENSES.md      第三方代码/素材登记 + 随产物分发的义务 + 依赖许可核查结论
├── Cargo.toml                   workspace 根：成员、[workspace.package] 版本单一源、依赖版本统一表
├── Cargo.lock                   依赖锁定（全部来自 crates.io，无 git 源）
├── rust-toolchain.toml          锁 Rust 通道版本（与 CI、构建脚本三处一致，漏改会被门禁拦）
├── .gitattributes               全仓文本强制 LF（.sh 变 CRLF 会直接坏构建链）；bat 保 CRLF；二进制禁转换
├── .gitignore                   排除构建产物/工具链真身/签名密钥/交付物（见文末「不在仓库里的东西」）
├── .cargo/
│   └── config.toml              windows-gnu 交叉链接器、Android 链接器约定、build.target-dir = Target
├── .github/
│   └── workflows/
│       └── build.yaml           CI：fmt + clippy + 单测 + 六道门禁，三个 runner（Linux / Windows / Android）
│
├── Crates/                      共享 Rust 核心。两端都链它 —— 逻辑不分叉是本项目的结构约束
│   │                            （每个 crate 目录下都有一份 Cargo.toml，依赖版本一律走 workspace 统一表）
│   ├── protocol/                线格式层
│   │   ├── build.rs             调 prost 从 Proto/ 生成消息代码
│   │   └── src/
│   │       ├── lib.rs           crate 门面 + 消息类型常量（单源指向 Proto/linkx/v1/tlv.rs）
│   │       ├── frame.rs         13 字节帧头：msg_type / flags / seq / 长度；编解码与单测
│   │       ├── envelope.rs      帧的加密封装边界（哪些字段进 AAD）
│   │       ├── tlv_codec.rs     64 字节 TLV：配置项、绑定 nonce、SAS 等小载荷
│   │       └── ble_frag.rs      蓝牙分片与重组（MTU 上限、乱序/重复/丢失语义、内存上限）
│   ├── crypto/                  加密原语与身份（不含协议状态机）
│   │   └── src/
│   │       ├── lib.rs           crate 门面 + 指纹派生（SHA-256 截断为 16 位十六进制）
│   │       ├── identity.rs      RSA-2048 长期身份：生成、SPKI、绑定签名与验签（含模长/签名长校验）
│   │       ├── noise.rs         Noise XX 握手生命周期（snow 之上，握手态与错误映射）
│   │       ├── noise_resolver.rs 自定义 cipher/dh/hash 选择：ChaCha20Poly1305 + X25519 + SHA256；
│   │       │                    退化公钥（全零、低阶）在这里拒掉
│   │       ├── cipher.rs        每帧 AEAD：方向域分离的 nonce 派生、帧头作 AAD
│   │       ├── binding.rs       channel binding：把"这条 TCP 属于这次握手"绑上
│   │       └── replay.rs        接收侧滑动窗口（先认证、后提交；心跳另用一份窗口）
│   ├── session/                 会话状态机：配对、通道选择、心跳、重连、文件/相册/配置事件派发
│   │   └── src/
│   │       ├── lib.rs           crate 门面
│   │       ├── engine.rs        最厚的一层：事件驱动的状态机 + 出站队列 + 接收会话编排（无锁，锁在平台侧）
│   │       ├── state.rs         状态与迁移表（非法迁移显式列出，避免"看着能用"）
│   │       ├── pairing.rs       配对流程与 SAS 计算（6 位、绑定到本次握手）
│   │       ├── trust.rs         信任库读写（TSV；指纹是主键）
│   │       ├── binding.rs       通道绑定的会话侧编排（BLE 已认证通道绑定 TCP）
│   │       ├── heartbeat.rs     心跳参数与退避（重连 1/2/4/8/16/32s）
│   │       └── code_extract.rs  从通知正文里取验证码（必须有关键词 + 距离窗口）
│   ├── lan/                     局域网数据面
│   │   └── src/
│   │       ├── lib.rs           crate 门面
│   │       ├── discovery.rs     UDP 广播/扫描：设备表、TTL 过期、手动 IP 兜底
│   │       ├── transport.rs     TCP 连接与读写线程（socket 选项失败即拆链，不静默）
│   │       └── stream.rs        加密字节流：分帧、抗重放、背压窗口、心跳
│   ├── transfer/                文件传输语义
│   │   └── src/
│   │       ├── lib.rs           crate 门面
│   │       ├── protocol.rs      FILE_META / CHUNK / DONE / RESUME 的字段编解码
│   │       ├── chunk.rs         分块切分、逐块 CRC32、块数计算
│   │       ├── task.rs          发送/接收任务对象（续传轮次、暂缓收尾、摘要增量）
│   │       └── filename.rs      接收侧文件名净化：Apache-2.0 上游的移植版，义务见 Licenses/localsend/
│   ├── storage/                 本地库（SQLite bundled，无外部进程）
│   │   ├── schema.sql           **schema 单一源**：include_str! 嵌入，建库时执行
│   │   └── src/lib.rs           SQLite（bundled）DAO：device_identity / pair_records / sessions / config / message_log
│   ├── debuglog/
│   │   └── src/lib.rs           NDJSON 埋点核心：环形缓冲 + 滚动 + 导出；默认关闭，满会丢行
│   ├── debugd/
│   │   └── src/lib.rs           本机回环调试控制面（仅 agent-debug 特性；只绑 127.0.0.1、拒浏览器发起）
│   ├── ffi/                     给两端壳的接缝
│   │   └── src/
│   │       ├── lib.rs           C ABI 门面 + 版本号常量
│   │       ├── events.rs        事件队列（Core → UI 批量分派）
│   │       └── jni_bridge.rs    安卓 JNI 入口：48 个 external fun 的实现、panic 不穿 JNI
│   └── app/
│       └── src/main.rs          自检入口：cargo run -p linkx-app 打 N/N PASS（冒烟用）
│
├── Platforms/
│   ├── Windows/                 纯 Win32 + GDI 自绘壳：无控件库、无 Direct2D、无 WebView
│   │   ├── Cargo.toml           依赖与 agent-debug 特性（不在 default 里）
│   │   ├── build.rs             版本资源 + 图标嵌入；只对 *-gnu 目标编 .res（见 README「构建」）
│   │   ├── linkx.rc             资源脚本：版本信息 + 图标
│   │   ├── LinkX.ico            应用图标（由 svg_assets.py 从 svg/ 栅格化，16..256 多尺寸）
│   │   ├── installers/
│   │   │   ├── LinkX-v4.wxs     WiX v4/v5 安装器 —— **交付走这条**（含防火墙规则、许可页、文件关联）
│   │   │   └── LinkX.wxs        历史 wixl 链路，不带许可文件，勿用于公开分发
│   │   └── src/
│   │       ├── main.rs          入口：单实例、Per-Monitor V2 DPI 声明（必须在建窗前）、退出清扫
│   │       ├── app.rs           worker 线程 + 事件分派 + 收发编排 + 相册拖出的后台预取
│   │       ├── state.rs         界面状态单一结构（渲染与命中判定共用同一份数据）
│   │       ├── window.rs        窗口过程、鼠标键盘、OLE 拖出、系统对话框
│   │       ├── render.rs        全部自绘：布局常量、页签分派、绘制与命中同源
│   │       ├── theme.rs         明暗主题 token（与安卓 ui/LinkXTheme.kt 同一套口径）
│   │       ├── icons.rs         GDI 描边图标与画笔/画刷缓存
│   │       ├── icons_svg.rs     ⚠ 生成物：svg/ 的矢量几何，改图标请改 svg/ 再跑生成脚本
│   │       ├── ble_central.rs   BLE 中心角色（电脑是主机，手机是外设）
│   │       ├── network.rs       局域网收发与设备表投影
│   │       ├── transfer.rs      发送/接收会话的平台侧结构、唯一化落盘路径
│   │       ├── wic.rs           图片解码与缩放（先量尺寸再解码的闸门）
│   │       ├── identity.rs      RSA 身份与信任库落盘（DPAPI 加密）
│   │       ├── settings.rs      设置持久化（key=value 文本，布尔只认 1）
│   │       ├── features.rs      运行期功能开关（关掉即不加载模块、不开端口）
│   │       ├── clipboard.rs     剪贴板监听与写入
│   │       ├── tray.rs          托盘图标与气泡
│   │       ├── dialog.rs        原生文件/文件夹选择器（电脑→手机入口）
│   │       ├── ipc.rs           单实例参数转交（命名管道）
│   │       ├── debug.rs         调试变体接线（日志开关、导出）
│   │       └── about.rs         关于页文案的单一来源（三行字，无外链）
│   └── Android/                 Kotlin + Jetpack Compose 壳，经 JNI 调同一份核心
│       ├── settings.gradle.kts  模块与仓库声明
│       ├── build.gradle.kts     顶层构建配置
│       ├── gradle.properties    JVM/并行参数（出包脚本会额外关并行防 R8 OOM）
│       ├── gradlew / gradlew.bat / gradle/wrapper/   Gradle Wrapper（Apache-2.0，自带 SPDX）
│       ├── gradle/libs.versions.toml                版本与依赖集中声明
│       └── app/
│           ├── build.gradle.kts  应用模块：minSdk 26 / targetSdk 34 / versionName 取版本单一源
│           ├── proguard-rules.pro  R8 规则：JNI 入口必须 keep（否则 release 静默崩）
│           └── src/main/
│               ├── AndroidManifest.xml  权限、前台服务类型、开机广播、FileProvider、分享 intent-filter
│               ├── java/com/linkx/app/
│               │   ├── MainActivity.kt          Compose 各屏与底部导航（含「关于」）
│               │   ├── LinkxRuntime.kt          与 NativeCore 的接缝：pump、收发会话、续传、调试动作
│               │   ├── NativeCore.kt            JNI 声明集 + .so 加载
│               │   ├── BlePeripheralService.kt  BLE 外设角色 + 前台服务（广播**必须**带设备名：
│               │   │                            电脑侧按名字认领设备与自动重连，去掉就连不回来）
│               │   ├── NlsService.kt            通知监听（正文/标题不外泄到日志；只转 10 分钟内的，
│               │   │                            绑定瞬间系统会补投整栏，全转会打爆蓝牙发送队列）
│               │   ├── BootReceiver.kt          开机/覆盖安装后拉起链路服务（不拉起=电脑拨不回来）
│               │   ├── ClipboardSync.kt         剪贴板同步（后台被系统拒绝时如实降级）
│               │   ├── ClipboardSendActivity.kt 通知动作「发送剪贴板」的落地页：透明拿一次输入焦点读完即退（Android 10+ 只让持焦点的应用读剪贴板）
│               │   ├── NotifCapability.kt       一条通知能不能回复/关闭的判据（RemoteInput 由应用自己决定，缺就是不支持）
│               │   ├── MediaControl.kt          媒体播放状态采样与远程控制
│               │   ├── AlbumProvider.kt         相册清单/缩略图/原图（缩略图不落盘是产品口径）
│               │   ├── BatteryMonitor.kt        电量与充电态上报
│               │   ├── IdentityStore.kt         RSA 身份的应用私有持久化
│               │   ├── AppPrefs.kt              偏好存取
│               │   ├── Features.kt              运行期功能开关（与 Windows features.rs 同口径）
│               │   ├── LinkPrereqs.kt           前置条件检查（蓝牙、局域网）
│               │   ├── Tlv.kt                   TLV 常量与蓝牙载荷（对齐 Rust 侧单源）
│               │   └── ui/LinkXTheme.kt         新拟物组件与主题 token
│               └── res/
│                   ├── drawable/                25 个矢量：24 个导航/操作图标 + ic_launcher.xml，
│                   │                            ⚠ 全部由 svg_assets.py 从 svg/ 生成，与 Windows 同几何
│                   ├── values/colors.xml        颜色 token
│                   ├── values/themes.xml        主题
│                   ├── values/strings.xml       文案
│                   ├── values-night/colors.xml  暗色 token
│                   └── xml/
│                       ├── file_paths.xml       FileProvider 共享范围（最小化）
│                       └── data_extraction_rules.xml  备份/迁移规则（身份与信任库不参与云备份）
│
├── Licenses/                    第三方许可全文与出处记录（仓库内没有 vendored 代码）
│   └── localsend/
│       ├── LICENSE              Apache-2.0 全文（上游）
│       ├── NOTICE               上游署名
│       └── PORTING.md           移植范围与改动声明 —— 对应 Crates/transfer/src/filename.rs
│
├── svg/                         图标包源文件（25 个 SVG，含产品 Logo 几何）—— 唯一真源
│                                经 Scripts/svg_assets.py 生成两端矢量、LinkX.ico 与 ic_launcher.xml
│                                逐个是：LinkX Logo + 侧边栏九枚（连接/通知/剪贴板/文件/媒体控制/
│                                功能/设置/手机/关于）+ 媒体九枚（播放/暂停类：播放/停止/左播放/
│                                右播放/单曲循环/随机播放/音乐/音量大/音量小）+ 动作与状态（上传/
│                                下载/发送/复制/相册/电量）
│
├── Proto/                       协议单一来源
│   ├── buf.yaml / buf.gen.yaml  buf lint 与代码生成配置
│   └── linkx/v1/
│       ├── common.proto         公共字段
│       ├── device.proto         设备信息与能力
│       ├── heartbeat.proto      心跳/链路观测
│       ├── notify.proto         通知转发（稳定 key 哈希 + 回复定位三元组）、回复请求与回执
│       ├── clipboard.proto      剪贴板同步
│       ├── file.proto           文件传输（META/CHUNK/DONE/RESUME）
│       ├── media.proto          媒体控制与播放状态
│       ├── album.proto          相册清单、缩略图、原图请求
│       ├── config.proto         跨端配置同步
│       └── tlv.rs               TLV 消息类型与蓝牙载荷（手写，与 proto 同处对齐）
│
├── Tests/
│   └── fuzz/                    独立 workspace（nightly + sanitizer，不进主 workspace）
│       ├── Cargo.toml / Cargo.lock
│       ├── fuzz_targets/
│       │   ├── noise_handshake_parser.rs   握手字节解析（对端可控输入）
│       │   ├── parse_frame_header.rs       13 字节帧头
│       │   └── parse_payload_protobuf.rs   消息体解析
│       └── corpus/              语料种子（入库，作回归起点；崩溃样本 artifacts/ 不入库）
│           ├── noise_handshake_parser/   10 个种子：握手字节样本，一个文件一个样本
│           ├── parse_frame_header/        1 个种子：13 字节帧头样本
│           └── parse_payload_protobuf/  410 个种子：由 Proto/ 各消息生成的载荷样本（机器产出，不逐个解释）
│
├── Release/
│   ├── README.md                当前版本交付物清单与校验值（二进制本身走 Releases 分发）
│   ├── Debug/                   历史调试包：本机产物，不入库、不归档（.gitignore 排除）
│   └── Archive/                 已退役版本，**每版统一 v<版本>/{Windows,Android}/ 三层**
│       ├── 00-README.md         归档台账：逐版大小 / SHA-256 / 为什么退役
│       └── v0.1.0 … v0.4.5/     Windows/*.msi.sha256 + Android/*.apk.sha256(.idsig)
│                                安装包本体在 Releases 页面，仓库里只跟踪校验值
│
├── Docs/
│   ├── Debug-Plane.md           **先读这个**：AI 调试控制面的边界、路由、动作、门控与自证
│   ├── Security.md              安全与隐私口径：数据路径、信任模型、加密实现、不可信输入、披露渠道
│   ├── Build.md                 从源码构建：双端出包、gnu 目标的原因、六道门禁、真机验收脚本
│   ├── Limitations.md           已知限制完整版：平台/ROM 边界、容量上限、交互短板、签名现状
│   ├── NOTIFY_REPLY_DESIGN.md   电脑回复手机通知：架构、协议变化、支持范围、实测结果
│   └── Design/
│       └── neumorphism.md         新拟物设计规格与 token（两端共同遵循）
│
└── Scripts/                     构建、门禁、真机验收与测量（Python 只用标准库）
    ├── build-mingw-windows.sh / build-msi-windows.sh      Windows：交叉编译与 MSI 出包
    ├── build-android-core.sh / build-android-apk.sh       安卓：.so 与 APK 出包（陈旧判定 + 页对齐）
    ├── check-version-sync.sh          版本单一源一致性（Cargo / Android / README / 安装器模板）
    ├── check-protocol-sync.sh         Proto ↔ 两端实现一致性
    ├── check-crypto-audit.sh          依赖黑名单（AES 系实现）+ 加密用法自查
    ├── check-compose-kotlin-pair.sh   Compose 与 Kotlin 版本配对，防"编译过但出包才炸"
    ├── check-release-clean.sh         交付产物零调试残留（字节 + 依赖图 + APK 的 .so 与 dex）
    ├── check-install-single.sh        本机注册份数与作用域（防同版本重出包变两份安装）
    ├── check-comment-hygiene.sh       注释卫生（不许内部编号、占比与连续块上限）
    ├── check-release-ledger.py        发布前核 `Release/README.md` 的三行哈希/大小是否与盘上产物一致
    │                                  （本地出包后跑，不进 CI：MSI 每次重打包哈希都会变）
    ├── check-repaint.py               看活窗口验证"数据变了不点也会刷新"
    ├── svg_assets.py + launcher_icon.tmpl + vector_icon.tmpl
    │                                  图标链：svg/ → 两端矢量 + LinkX.ico + ic_launcher.xml
    ├── linkx-ctl.py                   调试控制面 CLI（两端同一个入口）
    ├── ui-shot.py / nav_preview.py    Windows 界面截图与合成点击 / 导航图标预览
    ├── b3-file-transfer-check.py      电脑→手机传输验收（逐字节 sha256）
    ├── a7-phone-to-pc-check.py        手机→电脑传输验收（含"发送前必须不存在"前提）
    ├── b-bench-throughput.py          双向吞吐 + 电脑端工作集峰值（内存红线的实测口）
    ├── soak-transfer.py               传输浸泡：多轮双向，判据来自埋点 + 字节比对
    ├── verify-resume.py               断点续传用例（故障注入 + 反向对照）
    ├── stress-pair.py                 配对压力 / 重复配对
    ├── mem-map.py                     Windows 工作集构成探针（页类型 + 按模块归名）
    ├── measure-feature-memory.py      功能开关的内存收益实测
    ├── msi-walkthrough.ps1            安装向导走查
    └── export-public.sh               导出可公开发布的副本（排除清单 + 公开前自检）
```

## 不在仓库里的东西

| 路径 | 是什么 | 为什么不在 |
|---|---|---|
| `Target/` | cargo 构建目录（`.cargo/config.toml` 指定的 target-dir） | 产物 |
| `Release/Windows/`、`Release/Android/`、`Release/Debug/` | 当前版本安装包与历史调试包 | 二进制走 Releases 页面分发，仓库里只留 `.sha256` 校验值 |
| `Release/Archive/**/LinkX-*.msi|apk|idsig` | 归档目录里的安装包本体 | 同上；这条 `.gitignore` 规则也防止 `git add Release/Archive` 把几十 MB 拉进树 |
| `Tests/fuzz/artifacts/` | fuzz 崩溃样本 | 样本不是回归资产，语料种子才是 |
| `Tools/` | 本机持久化的工具链真身与**签名密钥** | 体积；密钥绝不入库 |
| `Temp/` | 会话临时产物 | 垃圾 |
| `Skills/`、`Docs/Handover/`、`Docs/Diag/`、`Docs/Audit/`、`Docs/Review/` | 维护者侧的工作方法、环境交接、走查证据、内部审计 | 绑定本机路径与内部过程叙述，对外读者不需要 |
| `Docs/Spec/` | 立项与逐版修订记录（1880 行开发过程文档） | 引用了一堆不随仓库发布的内部台账编号，公开出去只剩死链 |
| `Web/`、`.zcode/`、`.zcodeignore` | 官网工程（由另一个工具与作者维护） | 不属于本仓库，也不由本仓库改动 |
| `Scripts/restore-toolchain.sh`、`git-snapshot.sh`、`wine-ui-review.sh`、`repro-bug047.py` | 绑定本机工具链或内部台账的维护者脚本 | 外部读者跑不通，且引用内部编号 |

维护者本地另有 `Docs/Memory/`（会话与决策台账）与 `Docs/Plan/`（迭代计划）两类过程文档，
**不随仓库发布**：它们写的是"内部怎么走的"，含只对维护者有意义的编号、本机路径与
缺陷叙述；对外需要的事实由 `CHANGELOG.md`、提交历史与上面的文档承担。

`Scripts/export-public.sh` 是这条边界的执行者：它按 `git ls-files` 取当前工作树、套用排除规则、
整目录重建副本，然后在公开前做一轮自检（密钥文件、私钥 PEM、本机用户名/路径、
内部台账、必备许可文件）。排除规则一旦写错，最典型的表现是"清单空了但脚本照样说复制完成"，
所以它对清单规模也设了断言。

## 写作与流程约定（改这个仓库时请遵守）

- 注释解释**为什么**，不复述代码在做什么；不写内部缺陷编号（读者没有那份台账）——
  由 `Scripts/check-comment-hygiene.sh` 守。
- 归档类文档一经归档不再改写；口径变化用带日期的更正提示或新条目覆盖。
- 任何"已验证"的表述都要能被别人复跑：结论来自埋点事件与字节比对，不来自界面文案；
  产物哈希必须晚于最后一次代码改动。
- UI 改动一律截图看效果（`Scripts/ui-shot.py` 或真机 `adb exec-out screencap`）：
  编译通过、单测全绿都抓不到"字叠字""弧画反了"这类错。
