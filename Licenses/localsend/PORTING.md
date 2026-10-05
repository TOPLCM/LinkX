# 第三方许可与出处记录：LocalSend（本目录，不是 vendored 代码）

**现状（2026-09-30 起）**：LinkX 仓库里**已经没有 vendored LocalSend 代码**——
`Crates/vendor/localsend/`（60 个文件）与从未接线的互操作层 `Crates/transfer/src/ls_interop.rs`
一起删除（真实开发没用到它，却把 hyper/reqwest/rustls/tokio 整套异步栈挂在构建图上；
删除后 `Cargo.lock` 从 287 个唯一 crate 降到 160 个，tokio/hyper/reqwest/rustls 全部归零）。
需要从 git 历史取回时，先做完旧文件头部写的三件事：TLS、证书指纹绑定到已 TOFU 确认的
RSA 身份、逐次接收确认。

**但许可义务没有随删除消失**：生产路径里的 `Crates/transfer/src/filename.rs`
（接收侧文件名净化的唯一真源）是上游 `packages/core/src/util/filename.rs` 的**移植版**，
属 Apache-2.0 的衍生作品。所以本目录保留许可全文（`LICENSE`）、署名（`NOTICE`）
与下面这份改动/出处记录，并随 MSI 一起分发。

## 来源

- 上游仓库：https://github.com/localsend/localsend
- 上游 commit：`e768240d1ad95f0f162b852b5ff37bec71cde1ef`
- 抽取路径：`packages/core`
- License：Apache-2.0（见同目录 `LICENSE`，来自上游仓库根 `LICENSE`）

## 上游版权与许可

上游代码版权归 LocalSend 项目及其贡献者所有，按 Apache License 2.0 授权。
Copyright notice 与许可全文见 `LICENSE`。再次分发须保留该文件。

## 历史：当时那份 vendored 副本的改动清单（相对上游 `packages/core`）

以下记录描述的是**已删除**的 `Crates/vendor/localsend/`，保留是为了让"我们改过什么"这件事
在删除之后依然可查（Apache-2.0 §4(b) 的改动声明义务对移植版仍然有效）。

除下列改动外，源码文件保持上游原样：

1. **删除 webrtc 面源码**：删除整个 `src/webrtc/` 目录
   （`mod.rs`、`signaling.rs`、`webrtc.rs`）。
2. **`src/lib.rs`**：删除 `pub mod webrtc;`（webrtc 模块入口）。
3. **`Cargo.toml` — 依赖删除**：
   - `flate2`（仅 webrtc.rs 使用）
   - `tokio-tungstenite`（仅 signaling.rs 使用）
   - `tungstenite`（仅 signaling.rs 使用）
   - `webrtc`（仅 webrtc.rs 使用）
4. **`Cargo.toml` — feature 删除**：
   - `webrtc-signaling = ["tokio-tungstenite"]`
   - `webrtc = ["crypto", "flate2", "dep:webrtc", "webrtc-signaling", "x509-parser"]`
   - `full` 改为 `["crypto", "discovery", "http", "multicast"]`（不再含 `webrtc`）。
5. **新增 `LICENSE`**：从上游仓库根目录复制而来（原本位于 `packages/core` 之外）。
6. **新增 `VENDORING.md`**：本文件（后更名为 `PORTING.md`）。
7. **`src/model/transfer.rs` — 测试时间戳改为 100 ns 对齐**：
   `formats_nanosecond_timestamp` 中 `Duration::from_nanos(123_456_789)` → `123_456_700`，
   期望串 `.123456789Z` → `.1234567Z`。
   原因：Windows 的 `SystemTime` 以 FILETIME（100 ns）为单位存储，非对齐值在构造时
   即被静默截断，该断言只在 Linux 上成立（宿主迁 Windows 后暴露）。
   **只动 `#[cfg(test)]` 用例，不参与任何编译产物，上游运行时行为未改**；
   与同文件上游自写的 `reads_file_timestamps`（其注释已要求 100 ns 对齐）口径一致。

## 刻意保留项

- `x509-parser` **保留**：`crypto/cert.rs`（证书/公钥解析）与
  `http/client/server_cert_verifier.rs`、`http/server/common/client_cert_verifier.rs`
  （HTTPS 双向 TLS 校验）均依赖它，属 `crypto` / `http` feature 的必需依赖，与 webrtc 无关。
- `examples/stress_send.rs` **保留**：仅依赖 `http` feature 与 `rand`（未被删除），
  且是 `LsHttpClientV2` 纯 HTTP 上传的真实用法示例。

## 删除原因（Y4 门禁）

LocalSend 上游 `webrtc` feature 会引入 `webrtc` / DTLS 相关依赖（含 `aes-gcm` 系），
而 LinkX 的 CI 门禁 `Scripts/check-crypto-audit.sh` 要求 `Cargo.lock` 中不得出现
`aes-gcm`、`dtls`、`webrtc-srtp` 等 AES 载体（Y4：唯一 ChaCha20-Poly1305）。
因此 vendored 副本只保留 hyper 1.11 路线的 `packages/core` 协议/传输能力，
彻底删除 webrtc 源码、依赖与 feature。
## 0.4.0 收尾：真正接进生产链路的是"文件名规则"

- `ls-interop`（HTTP 互操作层）从默认 feature 降级为**显式开启**
  （`Crates/transfer/Cargo.toml`）：全仓搜索确认它没有任何生产调用点，
  却把 hyper / reqwest / rustls / tokio 整套异步栈编进交付二进制。
  需要与真实 LocalSend 互通时：`cargo build --features ls-interop`。
- 上游 `src/util/filename.rs` 的规则被移植为 `Crates/transfer/src/filename.rs`，
  成为接收侧文件名的唯一真源（改名式、永不拒绝）。与上游的差异记在该文件头：
  去掉 Hfs/Posix 两档、去掉 Options、保留设备名改为加前缀而不是整段替换。
- vendored 目录新增 `[lints.rust] dead_code = allow`：部分 vendoring 下，上游条目在我们
  这边必然有"没人用"的，不该让 `clippy -D warnings` 因为上游的完整度而红。
