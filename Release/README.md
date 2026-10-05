# Release 目录约定

这里是交付物出口，不是源码。**安装包二进制不入库**：二进制走 Releases 页面分发，
仓库里跟踪的只有校验值——当前版本登记在本页，已退役版本逐版登记在 `Archive/`。
`.gitignore` 排除了 `Release/Windows/*`、`Release/Android/*`、`Release/Debug/*`
与 `Release/Archive/**/LinkX-*.{msi,apk,idsig}`。

```
Release/                          （除 Archive 里的 .sha256 之外，下面都是本机产物、不入库）
  Windows/LinkX-<版本>-x64.msi              当前版本：电脑端安装包（+ .sha256）
  Android/LinkX-<版本>-release.apk          当前版本：手机端交付 APK（+ .sha256）
  Debug/                                    调试包：不入库、不归档
  Archive/v<版本>/                          已退役版本，每版一层
    Windows/LinkX-<版本>-x64.msi.sha256
    Android/LinkX-<版本>-release.apk.sha256
```

## 三条规则

1. **当前版本永远在 `Windows/` 与 `Android/`，且必须晚于最后一次代码改动**。
   产物比代码旧就不算交付。
2. **换版本时把上一版整体移入 `Archive/v<版本>/{Windows,Android}/`**，保留原始文件名与
   `.sha256`，不重命名、不重打包、不删，同时在 `Archive/00-README.md` 补登记一行。
   所有版本都用同一套分层。
3. **调试构建（`*-debug.apk`）不归档、不分发**，移到 `Release/Debug/`。

## 当前版本 0.5.1 的校验值

本表是当前发布产物的校验值；早于本表的任何登记都不再适用。
从 Releases 页面下载后与下表比对。

| 平台 | 文件 | 大小 (bytes) | SHA-256 |
|---|---|---|---|
| Windows | `LinkX-0.5.1-x64.msi` | 630,784 | `62380562aa99c3ec764bb820beaa5a433da97e5d733736bddd3ba45a4c1e02cb` |
| Android | `LinkX-0.5.1-release.apk` | 2,407,710 | `92d3b6bcdee182ae205f8d9825e333c0eb3e92a49f164453ea94b1c78b6e2be8` |

这一版手机端没有功能改动（只是版本号与前向兼容的 `versionCode` 一起前进），
因此不另出调试包；需要调试面时按 `Scripts/build-android-apk.sh` 的默认（debug）方式自己构建。

MSI 的哈希每次重打包都会变（WiX 把打包时间写进产物），它核对的是同一份文件，不是同一份代码；
APK 在同样输入下可复现。APK 用测试密钥签名，正式分发需自行重签，重签后哈希会变。

## 校验一个包

安装包不在仓库里（`Release/Windows/`、`Release/Android/` 是出包机器上的目录，克隆下来是空的），
所以核对的对象是**你从 Releases 页面下载到的那个文件**：

```powershell
Get-FileHash -Algorithm SHA256 "$env:USERPROFILE\Downloads\LinkX-0.5.0-x64.msi"
```

输出的哈希与上面那张表一致，才算拿到同一个文件。历史版本同理：`.sha256` 校验值在本仓库
`Archive/v<版本>/` 里逐版跟踪，安装包本体在 Releases 页面。
