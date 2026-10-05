# LinkX

> 开源的跨端协同效率革命工具

让手机躺在口袋里,把事情在电脑上办完

官网：[linkx.chaoming.xyz](https://linkx.chaoming.xyz)

---

## 功能

- **文件互传** — 互拖即传，断线续传，逐块 CRC32 + 整文件 SHA-256 校验
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

[Releases](releases) 页面取安装包，校验值见 [`Release/README.md`](Release/README.md)。

| 平台 | 要求 | 体积 |
|---|---|---|
| Windows | 10/11 x64 | MSI 0.61 MB |
| Android | 8.0+ arm64 | APK 2.32 MB |

MSI 未代码签名，首次运行有 SmartScreen 提示（更多信息 → 仍要运行）。

## 技术

一份 Rust 核心，两个壳：

- **核心**：protocol / crypto / session / lan / transfer / storage，两端共用同一份
- **Windows**：纯 Win32 + GDI 自绘，无框架无 WebView，空闲内存约 27 MB
- **Android**：Kotlin + Jetpack Compose，经 JNI 调核心
- **加密**：Noise XX 握手 + ChaCha20-Poly1305，蓝牙与 TCP 全链路同强度
- **通道分工**：通知/剪贴板/媒体走蓝牙，文件走局域网加密通道

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
