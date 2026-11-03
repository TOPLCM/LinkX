# LinkX

> 开源的局域网跨端协同工具

让手机躺在口袋里，把事情在电脑上办完。

官网：[linkx.chaoming.xyz](https://linkx.chaoming.xyz)

---

## 它能做什么

- **文件互传** — 拖过去就传，断线能续，两端各自校验到字节
- **一次发一批** — 电脑多选文件、手机多选相册图，整批发出，逐条看进度
- **通知同步** — 手机通知直达电脑；带回复框的通知可以直接在电脑上回
- **剪贴板同步** — 电脑复制、手机粘贴；反方向在手机通知上点一下（安卓系统限制）
- **相册浏览** — 在电脑上翻手机相册，选几张导出原图，或直接拖进微信、剪辑软件
- **媒体控制** — 电脑上播放/暂停、切歌、调音量、快进快退
- 另有：电量徽标、功能开关、开机后自动重连已绑定设备

所有数据只在你自己的局域网和蓝牙范围内走，**无服务器、无遥测、无更新检查**。

## 怎么用起来

1. 手机和电脑连同一个路由器，两边都打开蓝牙
2. 电脑装 MSI，手机装 APK
3. 电脑扫到手机 → 点一下 → 两边各显示 6 位数字，一致就确认

安卓需要授予「通知使用权」「附近设备」「位置」。**建议在系统安全中心允许自启动**，否则通知可能被系统静默杀掉。

## 下载

安装包在 [GitHub Releases](https://github.com/TOPLCM/LinkX/releases)。每个包的 SHA-256 记在 [`Release/README.md`](Release/README.md)，下载后比一下才算拿到同一个文件。

| 平台 | 要求 | 安装包 |
|---|---|---|
| Windows | 10 / 11，x64 与 ARM64 各有原生包（ARM 电脑挑 arm64 那一个） | MSI 1.00 MB（x64）/ 0.96 MB（ARM64） |
| Android | 8.0 以上，64 位与 32 位机型都支持（一个 APK 内含两套架构） | APK 2.86 MB |

MSI 未做代码签名，首次运行会有 SmartScreen 提示（更多信息 → 仍要运行）。

## 怎么做的

一份 Rust 核心，两个平台的壳——Windows 用纯 Win32 + GDI 自绘（不用框架、不用 WebView），Android 用 Kotlin + Jetpack Compose 经 JNI 调同一份核心。配对与每一帧通信都是加密的，细节见 [`Docs/Security.md`](Docs/Security.md)，模块划分见 [`DIRECTORY.md`](DIRECTORY.md)。

安全上的四条底线：

1. 信任锚是设备自己的 RSA-2048 指纹，首次配对要人工核对那 6 位数字
2. 每一帧都加密，并带抗重放窗口
3. 零外联：没有遥测、没有更新检查、没有服务端组件
4. 对端发来的一律当不可信处理：文件名净化、路径穿越防护、大小上限

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

改动明细见 [`CHANGELOG.md`](CHANGELOG.md)，已知限制见 [`Docs/Limitations.md`](Docs/Limitations.md)。

## 自己构建

```bash
bash Scripts/build-mingw-windows.sh && bash Scripts/build-msi-windows.sh
bash Scripts/build-android-core.sh && bash Scripts/build-android-apk.sh --release
```

前置条件与逐步说明见 [`Docs/Build.md`](Docs/Build.md)。

## 关于作者

由 Chaoming（[@TOPLCM](https://github.com/TOPLCM)）维护。纯 Vibe Coding 开发，AI 写代码，作者定框架、做联调、验真机。更多故事见[官网关于页](https://linkx.chaoming.xyz/about.html)。

如果帮你省下了时间，欢迎[请作者喝杯咖啡](https://linkx.chaoming.xyz/about.html#donate)。

## 许可

GPL-3.0-or-later，见 [`LICENSE`](LICENSE)。第三方组件见 [`THIRD_PARTY_LICENSES.md`](THIRD_PARTY_LICENSES.md)。
