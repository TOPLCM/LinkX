# Release 归档台账

本目录存放**已退役版本**的交付登记。布局对每个版本都一样：

```
Archive/
  v<版本>/
    Windows/LinkX-<版本>-x64.msi.sha256
    Android/LinkX-<版本>-release.apk.sha256      （+ .idsig，APK v4 签名哈希）
```

**仓库里只跟踪校验值，不跟踪安装包本身。** 安装包二进制走 Releases 页面分发
（`.gitignore` 里 `Release/Archive/**/LinkX-*.msi|apk|idsig` 就是这条规则的执行者），
否则源码树会被几十 MB 的二进制压住，而 GitHub 对单文件也有大小限制。
上表的大小列取自本机仍保留的那份产物，与 `.sha256` 逐条核对过。

规则（与 [`../README.md`](../README.md) 一致）：

- 每次出新版，把上一版整体移入 `Archive/v<版本>/`，**保留原始文件名**，不重命名、不重打包；
- 每个版本在本文件登记一行：日期 / 大小 / SHA-256 / 为什么退役；
- 调试构建（`*-debug.apk`）不进归档，只留在本机 `Release/Debug/`（同样不入库）。

## 逐版台账

| 版本 | 平台 | 文件 | 大小 (bytes) | SHA-256 |
|---|---|---|---|---|
| 0.1.0 | Windows | `LinkX-0.1.0-x64.msi` | 457,216 | `bef55560ac250f89c9c9dc491378cdda015384fee34a28b4c9a098c425d2f41f` |
| 0.1.0 | Android | `LinkX-0.1.0-m1-release.apk` | 1,837,246 | `bb067c863fd5f488773b5e2db00ef10dc9ee27db538b252bfc62e807774d061a` |
| 0.2.0 | Windows | `LinkX-0.2.0-x64.msi` | 539,648 | `ce957cd08ba39adbf82917187ea593c505f0077b826af6beaa3e254ef608b262` |
| 0.2.0 | Android | `LinkX-0.2.0-release.apk` | 2,029,814 | `c060183ab00d55469312967fc56acd8a3db2e5d755baf7638d23386cc6459e49` |
| 0.3.0 | Windows | `LinkX-0.3.0-x64.msi` | 598,016 | `b7ecd5b9efd645f9824b5bfd2a9e5a011ae5a7048cf7e7b3bbc7a0f2fa75e05e` |
| 0.3.0 | Android | `LinkX-0.3.0-release.apk` | 2,300,150 | `a3194be837141be3aa5088dc39c2b163d9f7fa3fcdc1850ed0c4f5447a3406bc` |
| 0.4.0 | Windows | `LinkX-0.4.0-x64.msi` | 585,728 | `ddbaed0935b90ec26cf934cf5f64d4475e66a49db293442f1b200ef0d8b47a5b` |
| 0.4.0 | Android | `LinkX-0.4.0-release.apk` | 2,378,926 | `b328e63f0daf2c2e00f7877e80cc5dc4caea52875b950d0057ddc635ad1fb932` |
| 0.4.1 | Windows | `LinkX-0.4.1-x64.msi` | 593,920 | `b05e0c39e20e0b62bef13fd7084a4063cdb3419bc451614c4e6ec0130a739fb8` |
| 0.4.1 | Android | `LinkX-0.4.1-release.apk` | 2,391,214 | `7adb83afe49e5cf271c95d3b111e23c5b2e22dd7e52b5388d3c9250208c9c842` |
| 0.4.2 | Windows | `LinkX-0.4.2-x64.msi` | 618,496 | `c970b9e0310d845d2923698ae48e9ca478af02289daa00da3143ed48116336b4` |
| 0.4.2 | Android | `LinkX-0.4.2-release.apk` | 2,424,038 | `530f8ab114805da54666152a5dc45f405989989b19fc932ed4b2d4d9dc4ade2b` |
| 0.4.3 | Windows | `LinkX-0.4.3-x64.msi` | 618,496 | `af1167b5eec968d74dd8ab8c1c2a7bdd5a447412b5f55e4cfce73b7083b94c1f` |
| 0.4.3 | Android | `LinkX-0.4.3-release.apk` | 2,424,038 | `022cf3ddad78c2841e9824794d20c91b07ec2b68df25a5a869c3193fc092c04a` |
| 0.4.4 | Windows | `LinkX-0.4.4-x64.msi` | 626,688 | `96db5f96979e9a5a7c6e559dcb57f0c80f90f9da6f5ee134d5216e87f7dc4194` |
| 0.4.4 | Android | `LinkX-0.4.4-release.apk` | 2,424,038 | `d99561dfbdf726e9808346cf9a393b6bf77d3117d3bd3e6c33858bd626ed5ab2` |
| 0.4.5 | Windows | `LinkX-0.4.5-x64.msi` | 630,784 | `567ac6775dcdf4caef1e1e9222fda2dd7a6b5ebad9118bc359d5ac12307226af` |
| 0.4.5 | Android | `LinkX-0.4.5-release.apk` | 2,428,134 | `26462226f016ab9ff73acf9c1e81b20417e58338a4bc4e0aa747c0d0da056275` |

## 每一版为什么退役

- **0.1.0（2026-09-25 归档）** 最小可用切片：BLE 配对 + 通知同步 + 剪贴板同步，真机联通。
  0.2.0 在其上加了文件传输、局域网 TCP 通道、设备管理与跨端配置同步。
  Android `versionName` 当时还带里程碑后缀（`0.1.0-m1`），自 0.2.0 起与 MSI 版本号统一为纯版本号。
- **0.2.0（2026-09-26 归档）** 真机联调暴露三个问题（幽灵设备 / 重配对不弹窗 / 配对卡死），
  根因是设备身份没有持久化；0.3.0 用 RSA-2048 长期身份 + TOFU 根治。
- **0.3.0（2026-09-28 归档）** 完成持久身份、双端调试日志、MSI 体验重做与 LocalSend 移植；
  退役原因是 0.4.0 结案了手机→电脑大文件丢块（两端各说各话的"成功"）。
- **0.4.0（2026-10-01 归档）** 修掉丢块并改成背压 + 乱序容忍。退役原因：安装作用域分叉
  （per-machine 与 per-user 混装会出现两份注册），0.4.1 统一为 per-user 并升版本。
- **0.4.1（2026-10-01 归档）** 作用域根治 + 媒体回显与蓝牙地址漂移修复。退役原因：图片互传
  （相册浏览/缩略图/多选导出）在 0.4.2 落地。
- **0.4.2（2026-10-01 归档）** 相册互传落地。退役原因：缩略图缓存按张数封顶导致大图场景
  内存不可控，0.4.3 改成按字节封顶并加了视频互传。
- **0.4.3（2026-10-01 归档）** 相册内存红线根治 + 视频互传 + 安卓接收目录可选。退役原因：
  沉浸式标题栏与深色模式白条问题由 0.4.4 处理。
- **0.4.4（2026-10-01 归档）** 深色标题栏跟随主题；接收大文件不再按文件大小吃内存
  （221 MB 整片视频，接收期工作集峰值 30 MB）。退役原因：迟到的续传请求救不回来，
  0.4.5 补上"收端暂缓收尾 + 发端回执窗口内补发"。
- **0.4.5（2026-10-01 归档）** 断点续传闭环 + 调试包与交付包同一把签名（覆盖安装不再丢配对）。
  退役原因：0.5.0 加入「关于」页与冷启动自动重连。

## 当前版本

0.5.0 的产物与校验值在 `Release/Windows`、`Release/Android` 与 Releases 页面，
逐版改了什么见 [`../../CHANGELOG.md`](../../CHANGELOG.md)。

> 说明：0.3.0 的调试包（10,716,334 B，SHA-256
> `aa91313750c7849d42c86d11f8fcced72fba402cecf4a1ed52f4b1846b7b7c4d`）曾一并归档，
> 后按"调试包不入库"的口径移出工作树，只在这里登记校验值；需要时从 git 历史取回。
