# LinkX

> 开源的跨端协同效率革命工具

让手机躺在口袋里，把事情在电脑上办完

官网：[linkx.chaoming.xyz](https://linkx.chaoming.xyz)

---

## 功能

- **文件互传** — 互拖即传，断线续传，逐块 CRC32 + 整文件 SHA-256 校验
- **一次发一批** — 电脑端多选文件、手机端多选相册图片，一次整批发出，逐条看得见进度
- **通知同步** — 手机通知直达电脑，断线补发，带回复框的可直接在电脑上回
- **剪贴板同步** — 电脑复制手机粘贴，反向需在手机通知上点一下（安卓系统限制）
- **相册浏览** — 电脑直接翻手机相册，多选导出原图，拖进微信/剪辑软件
- **媒体控制** — 电脑上看播放、切歌、调音量、快进快退
- **电量徽标 / 功能开关 / 冷启动自动重连**

## 上手

1. 手机电脑同局域网，双方开蓝牙
2. 电脑装 MSI，手机装 APK
3. 电脑扫到手机 → 点一下 → 两端核对 6 位数字 → 完成

安卓需授予通知使用权、附近设备、位置。**建议在系统安全中心允许自启动**，否则通知可能被系统静默杀掉。

## 下载

安装包挂在 [GitHub Releases](https://github.com/TOPLCM/LinkX/releases)；每个包的 SHA-256 记在 [`Release/README.md`](Release/README.md)，下载后比一下才算拿到同一个文件。

| 平台 | 要求 | 体积 |
|---|---|---|
| Windows | 10/11，x64 与 ARM64 各有原生包（ARM 电脑挑 arm64 那一个） | MSI 1.00 MB（x64）/ 0.96 MB（ARM64） |
| Android | 8.0+（arm64 与 32 位 armv7 都支持） | APK 2.86 MB |

MSI 未代码签名，首次运行有 SmartScreen 提示（更多信息 → 仍要运行）。

## 技术

一份 Rust 核心，两个壳：

- **Windows**：纯 Win32 + GDI 自绘，无框架无 WebView，空闲工作集约 27 MB（其中约 24 MB 是可换出的镜像页）
- **Android**：Kotlin + Jetpack Compose，经 JNI 调核心
- **加密**：Noise XX 握手 + ChaCha20-Poly1305，蓝牙与 TCP 全链路同强度
- **通道分工**：配对、通知转发、剪贴板走蓝牙；媒体状态与控制、通知回复、电量在局域网连通时走局域网、否则回退蓝牙；文件与相册原图只走局域网加密通道

核心各模块负责什么（想知道更多，看 [`DIRECTORY.md`](DIRECTORY.md)）：

| 模块 | 负责什么 |
|---|---|
| `protocol` | 线格式：13 字节帧头、加密封装边界、TLV 小载荷、蓝牙分片与重组 |
| `crypto` | RSA-2048 长期身份、Noise XX 握手、每帧 AEAD 加密、通道绑定 |
| `session` | 配对与 6 位码比对、通道选择、心跳与退避、重连、事件派发 |
| `lan` | 局域网数据面：UDP 发现、TCP 连接、加密流分帧、抗重放与背压 |
| `transfer` | 文件传输语义：分块、逐块 CRC32、断点续传、文件名净化 |
| `storage` | 本地库（自带 SQLite）：设备身份、信任库与配置 |
| `ffi` | 核心的对外门面：版本号单一真源 + 安卓 JNI 导出（Windows 端直接调 Rust，不走这一层） |

`debuglog` / `debugd` 是本机回环的调试控制面，只在 `agent-debug` 特性下编译，**不进交付产物**。

## 安全

1. 信任锚是 RSA-2048 指纹，首次配对人工核对 6 位 SAS
2. 每一帧都加密，带抗重放窗口
3. 零外联：无遥测、无更新检查、无服务器端组件
4. 对端输入全部当不可信处理：文件名净化、路径穿越防护、大小上限

完整版见 [`Docs/Security.md`](Docs/Security.md)。

## 版本

| 版本 | 主题 |
|---|---|
| v0.1.0 | 蓝牙跑通：通知同步、剪贴板 |
| v0.2.0 | 双栈起步：文件传输、Wi-Fi 验证 |
| v0.3.0 | 安全底座：设备身份、加密握手 |
| v0.4.0 | 功能完善：媒体控制、相册 |
| v0.5.0 | 大修与打磨：通知回复、BUG 修复 |
| v0.5.1 | 电脑端常驻与安全底座：开机自启动、托盘常驻、系统通知卡、中文安装向导、会话密钥改用握手秘密派生 |
| v0.5.2 | 安装向导改版与多选发送：安装位置页改成可编辑路径框、许可协议必须勾选才能继续；两端都能一次发多个文件 |
| v0.5.3 | 兼容性专版：ARM 电脑有原生包，安卓一个包里同时带 64 位与 32 位两份核心库 |

## 构建

```bash
bash Scripts/build-mingw-windows.sh && bash Scripts/build-msi-windows.sh
bash Scripts/build-android-core.sh && bash Scripts/build-android-apk.sh --release
```

详见 [`Docs/Build.md`](Docs/Build.md)。

## 关于作者

由 Chaoming（[@TOPLCM](https://github.com/TOPLCM)）维护。纯 Vibe Coding 开发，AI 写代码，作者定框架、做联调、验真机。更多故事见[官网关于页](https://linkx.chaoming.xyz/about.html)。

如果帮你省下了时间，欢迎[请作者喝杯咖啡](https://linkx.chaoming.xyz/about.html#donate)。

## 许可

GPL-3.0-or-later，见 [`LICENSE`](LICENSE)。第三方组件见 [`THIRD_PARTY_LICENSES.md`](THIRD_PARTY_LICENSES.md)。
