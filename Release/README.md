# Release 目录约定

这里是交付物出口，不是源码。**安装包二进制不入库**：二进制走 Releases 页面分发，
仓库里跟踪的只有校验值——当前版本登记在本页，已退役版本逐版登记在 `Archive/`。
`.gitignore` 排除了 `Release/Windows/*`、`Release/Android/*`、`Release/Debug/*`
与 `Release/Archive/**/LinkX-*.{msi,apk,idsig}`。

```text
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

本表就是 **Releases 页面上那两个文件**的校验值，下载后与下表比对即可。它们是从标签 `v0.5.1` 那份源码构建的；`main` 上发布之后才进来的改动（例如零行为变化的精简）不在这些包里，会随下一个版本一起出。自己从源码构建出来的哈希必然与本表不同（WiX 把打包时间写进产物，安卓的 `.so` 还编进了源码行号），那不代表你构建错了。

| 平台 | 文件 | 大小 (bytes) | SHA-256 |
|---|---|---|---|
| Windows | `LinkX-0.5.1-x64.msi` | 638,976 | `0c8951d655289e58604fe9c047b77b4afb01ecbb44a4f93e580f3c424a21a106` |
| Android | `LinkX-0.5.1-release.apk` | 2,399,518 | `ac83608ee0fe5c2cff1580fe2c33b182cb56a8769a4f16149006ebef22e24609` |

这一版手机端没有新增可见功能，但**加密核心（会话密钥派生）改在共享的 Rust 层**，
所以两端必须同批升级：只升一端会在第一条加密消息上解密失败、配对停在未完成。
调试包（带 AI 控制面，只在本机自测用）不归档不分发，放在 `Release/Debug/`。
电脑端这份 MSI 的**安装向导是中文的**（此前是英文），装到一半看到的字应该和下表说的一致。
这一版起 MSI 会多注册一条 `linkx://`（HKCU，不需要管理员权限），它是电脑通知卡上那两颗按钮的
回话通道；卸载时一并回收。通知卡本身还要程序启动时给开始菜单快捷方式补一次应用身份，
所以**不勾「创建开始菜单快捷方式」就没有卡片，只有托盘气泡**（气泡上没有按钮）。

MSI 的哈希每次重打包都会变（WiX 把打包时间写进产物），它核对的是同一份文件，不是同一份代码；
APK 在同样输入下可复现（同一份源码重跑两次，字节一致，已实测）。但**共享 Rust 层哪怕只改注释，
release APK 的字节也会变** —— 源码行号会编进 `.so`，所以安卓哈希变了不代表手机端行为变了，
要看改动清单。APK 用测试密钥签名，正式分发需自行重签，重签后哈希会变。

## 校验一个包

安装包不在仓库里（`Release/Windows/`、`Release/Android/` 是出包机器上的目录，克隆下来是空的），
所以核对的对象是**你从 Releases 页面下载到的那个文件**：

```powershell
Get-FileHash -Algorithm SHA256 "$env:USERPROFILE\Downloads\LinkX-0.5.1-x64.msi"
```

输出的哈希与上面那张表一致，才算拿到同一个文件。历史版本同理：`.sha256` 校验值在本仓库
`Archive/v<版本>/` 里逐版跟踪，安装包本体在 Releases 页面。
