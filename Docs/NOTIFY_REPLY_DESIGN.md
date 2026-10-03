# 通知回复（Windows → Android）设计与实测

> 落地于能力层分支 `feat/capability-layer`，**随 0.5.0 一起发布**（不发 0.5.1）。宿主 = Windows 自绘壳 + 小米 22041216C / HyperOS / Android 14。
> 一句话：**电脑看到手机通知时，如果那条通知的自家应用支持快速回复，就能在电脑上回一句话。**

---

## 1. 架构说明

整条链路复用既有的通知同步，不新建第二套通知系统：

```
手机通知栏
  │ NotificationListenerService.onNotificationPosted   （既有）
  ▼
NlsService.forward()                                    （既有，本轮加 5 个字段）
  │ 采集 pkg / tag / id + 回复入口（首个挂了 RemoteInput 的 action 及其 resultKey）
  ▼
LinkxRuntime.sendNotification → JNI → 引擎 → NOTIFY_PUSH 0x10   （既有消息，加字段）
  ▼
Windows：NotificationItem → 通知页；can_reply 为真才画「回复」入口
  │ 点「回复」→ 顶部回复条 → 输入 → 发送 / 回车
  ▼
NOTIFY_REPLY 0x11（电脑 → 手机）
  ▼
手机 LinkxRuntime.handleEvent（事件泵锁内**只排队**）
  ▼ NlsService.submitReply → linkx-reply 单线程
  │ 按 pkg+tag+id 在"仍在通知栏"的条目里现找
  │ RemoteInput.addResultsToIntent(...) → action.actionIntent.send(...)
  ▼
NOTIFY_REPLY_ACK 0x13（手机 → 电脑，无论成败必回一条）
  ▼
Windows：按 reply_id 对上号 → 右上角显示"已发送"或手机给的那句原因

（并行的一条）手机通知消失 → NOTIFY_DISMISS 0x12 → Windows 只撤掉那一行的「回复」入口
```

三条设计约束值得单独说：

**手机侧不缓存通知对象。** 收到回复请求时按 `pkg + tag + id` 现找。缓存下来的
`PendingIntent` 在通知被划掉后就是废的，拿它回复只会静默失败——而静默失败是本项目最反对的形态。

**跨进程调用一律出锁。** `activeNotifications` 是 Binder 调用，而 `handleEvent` 跑在事件泵的
`@Synchronized` 里；锁内执行会按住 BLE 分片、TCP 帧和所有 JNI。所以回复与媒体指令同构：只排队，
在 `linkx-reply` 自己的线程上执行。

**回执按 `reply_id` 对号，而不是按行号。** 新通知会插到列表最前，行号会漂；对不上号的回执
必须出声（"收到一条不认识的回执"），否则"手机回了但电脑没显示"是最难查的一类问题。

界面几何：回复条压在标题行与首行之间那条空档（`REPLY_BAR_Y + INPUT_H < NOTIFY_Y0`，
这条由 `const _: () = assert!` 在编译期拦住），列表几何一个字没改。行上的「回复」与
「复制验证码」共用一条"从右往左排"的算法（`notification_chips`），绘制、命中、悬停三处只调它。

**通知消失要出声。** 从通知里回完短信，短信应用会把那条通知自己清掉；电脑若不知道，
下次点「回复」只会得到一句"这条通知已经不在了"。所以手机对**当初以 `can_reply=true` 转发过**
的条目额外报一条 `NOTIFY_DISMISS`，电脑收到后只撤那一行的回复入口（连正在编辑的输入框一起收掉），
**行本身留着**——正文还能复制，电脑上的"最近通知"不是通知栏的镜像。上报范围只限这几条，
是为了不在用户每划掉一条通知时都往蓝牙上塞一帧。

## 2. 协议变化

| 类型 | 值 | 方向 | 说明 |
|---|---|---|---|
| `NOTIFY_PUSH` | `0x10` | 手机 → 电脑 | **加字段**（原有 6 个不动） |
| `NOTIFY_REPLY` | `0x11` | 电脑 → 手机 | 新启用（这个值一直是预留槽位） |
| `NOTIFY_DISMISS` | `0x12` | 手机 → 电脑 | 新增（`0x12` 原本被 config.proto 的一句错注释声称占用，实际 `CONFIG_SYNC` 单源在 Tlv 的 `0x08`，注释已改） |
| `NOTIFY_REPLY_ACK` | `0x13` | 手机 → 电脑 | 新增 |

`NotificationPush` 新增：`tag=7`、`notification_id=8`、`can_reply=9`、`reply_action_index=10`、
`reply_result_key=11`。

`NotificationReply`：`reply_id / package / tag / notification_id / action_index / result_key / text`。
`NotificationReplyAck`：`reply_id / package / ok / error`——`error` 是手机写好的、给用户看的一句原话，
电脑原样显示，不翻译不吞掉（相册那套口径）。
`NotificationDismiss`：`package / tag / notification_id / key_hash`，只有定位信息，**不带任何正文**。

**兼容性**：proto3 未知字段被忽略，未知消息类型只 `emit_error` 不断链。所以

- 新手机 + 老电脑：多出来的字段被忽略，通知照旧显示，只是没有回复入口。
- 老手机 + 新电脑：手机回 `未知业务消息类型 0x11` 并**不回执**，电脑靠 10 秒超时把这条回复判为
  "手机没有回应（请把手机端 LinkX 升到最新版）"——不会停在"点了没反应"。老手机也不会报 `0x12`，
  电脑上的入口就一直是旧的"点了才知道没了"形态，不会更坏。

帧版本 `PROTOCOL_VERSION` 与产品版本号都不需要动。

## 3. 支持范围

**只有应用自己挂了 `RemoteInput` 的通知可以回复。** 那是应用构建通知时的选择，不是我们的能力。
界面上"没有回复按钮"就等于真的不支持，不留一个点了才报错的按钮。

| 场景 | 结论 | 依据 |
|---|---|---|
| Telegram / WhatsApp / Slack / Gmail 等实现了 Android 官方快速回复的应用 | 支持 | 机制即 `RemoteInput` + `Action.actionIntent` |
| **系统短信**（`com.android.messaging`） | **支持** | B 组真机实测：一条验证码通知就挂了 `RemoteInput`，电脑发的字真的进了会话 |
| 微信 | 预期不支持 | 微信安卓端通知不自挂 `RemoteInput`；**尚未实测**，以真机普查为准 |
| 支付 / 游戏等不带回复动作的通知 | 不支持，也不显示入口 | `reply=false`（验证码类**不在此列**，见上一行） |
| 常驻通知（`isOngoing`） | 不转发，因此也谈不上回复 | 既有过滤口径 |
| 敏感通知（`android.isSensitive`） | 正文不转发 | 既有口径 |

"能不能回复"完全由**那一条通知自己**决定，同一条短信在"刚收到"和"打开会话后"两种状态下
可以一边有、一边没有。所以判据只能是应用挂没挂 `RemoteInput`，不能按应用类型猜。

## 4. 已知限制

1. **不做模拟点击、不用无障碍、不自动输入**——只用 `NotificationListenerService` + `RemoteInput`。
   这是硬边界：后台读剪贴板/代客输入的几条路都在真机上实测被判死（复现记录属维护者内部材料，
   不随仓库发布），结论是**没有任何第三方 App 能绕开系统的焦点判据**。
2. **通知必须在回复那一刻还在通知栏**。用户已划掉、或应用自己把通知清掉（回完短信就清是常态）
   → 回执"这条通知已经不在了"。手机现在会把消失上报（`NOTIFY_DISMISS`），电脑随即撤掉那个入口；
   但"通知消失"到"电脑收到上报"之间总有一个链路往返的窗口，落在窗口里点仍会拿到那句原因。
3. **一个 action 上多个输入框时只填点名的那一个**，其余留给应用自己处理。
4. **回复正文与通知正文一样不进日志**：只记 `pkg / id / tag / key / len`。
5. **配对是前提**。未配对时引擎直接拒发（这条会真的把文字送进对端某个应用，比播放指令更硬）。
6. 电脑侧回复框沿用自绘输入的既有限制：只应答 `WM_CHAR` 与 `Backspace`/`Enter`，没有光标定位、
   选区与粘贴。要改是整套输入栈的事，不属本轮。
7. 手机侧排队上限 8 条，超了回"回复太频繁"。

## 5. 测试结果

### 5.1 静态与单元（全绿）

| 项 | 数量 | 覆盖 |
|---|---|---|
| `cargo test --workspace` | 326 | 协议字段往返、老对端忽略新字段、引擎回路、Windows 状态机与界面几何 |
| `cargo test -p linkx-ffi --features jni` | 14 | kind 21 字段序契约、回执不进下行流、超长事件对齐 |
| 新增用例 | 11 | 见下 |

新增用例逐条：

- `notify_reply_fields_round_trip_and_old_peers_ignore_them`（协议）
- `notify_reply_round_trip` / `notify_reply_is_not_sent_while_unpaired`（引擎回路 + 未配对闸门）
- `notify_dismiss_reaches_the_pc`（消失上报回路）
- `notification_merge_also_refreshes_the_reply_handle`（同 key 更新要连回复入口一起换）
- `reply_ack_lands_only_on_the_request_it_belongs_to`（对不上号不写提示；失败无原因时显示"回复失败"而不是空白）
- `a_reply_that_never_gets_an_ack_expires_instead_of_hanging`（等不到回执要超时出声）
- `a_dismissed_notification_loses_its_reply_entry_but_stays_listed`（撤入口不删行；三元组差一项不许误伤）
- `notification_text_yields_to_its_buttons`（两行文字必须让位给按钮）
- `notify_reply_request_event_layout_matches_contract`、`notify_reply_ack_is_not_encoded_into_the_event_stream`（FFI）

`clippy -D warnings`（含 `agent-debug` 与 `jni` 两种组合）、注释卫生、协议同步、版本同步、
Compose/Kotlin 配对全过。协议同步脚本新加 6 道断言：三个消息类型常量、proto 里三个新消息体、
Kotlin 侧必须解析事件 21 且必须有 `nativeSendNotifyDismiss` 声明。

### 5.2 真机（小米 22041216C / HyperOS / Android 14，BLE 已配对）

| 用例 | 结果 | 证据 |
|---|---|---|
| 回复定位三元组上行 | ✅ | 两条 `id=2020` 的通知靠 `tag=LinkXTEST2` / `654321` 在电脑侧区分开 |
| 不可回复通知不显示回复入口 | ✅ | 通知页截图：只有标题/包名/正文，无「回复」按钮，排版无叠字 |
| 电脑侧闸门拒绝向不可回复通知发请求 | ✅ | `POST /action/reply-notify` → `这条通知没有回复入口：应用没挂 RemoteInput` |
| 手机侧"找到通知但没有回复按钮" | ✅ | `reply.ack id=9001 ok=false reason=这条通知没有可用的回复按钮` |
| 手机侧"通知已经不在了" | ✅ | `reply.ack id=9002 ok=false reason=这条通知已经不在了` |
| 回执 0x13 手机 → 电脑送达并对号 | ✅ | 电脑 `/state.errors`：`收到一条不认识的回执（第 9001 次回复）`（探针发的号电脑没在等，判为不匹配是正确行为） |
| 在栏通知可回复面普查 | ✅ | `nls-probe`：`在栏 11 条｜可回复 0 条`，逐条 `pkg/id/tag/reply/action/key` 进 logcat |

### 5.3 真机正向验收（B 组：另一台电脑 + 同一台小米，0.5.1 双端包）

短信验证码通知（`com.android.messaging`，标题"阿里云"）实测挂了 `RemoteInput`：

| 用例 | 结果 | 证据 |
|---|---|---|
| 电脑输入 → 短信应用真的把这句话发出去 | ✅ | 手机短信会话里出现电脑发的"123"、"123088"、"111111"，与对方号码同一条线程 |
| 电脑显示"已发送" | ❌ 已修，**待复验** | 第二轮实测：字发出去了、电脑却报"回复时出错"。根因是手机侧把"成功"表示成 `null`，取值却用 `runCatching{…}.getOrNull()`——成功与异常塌成同一个 `null`。**上一轮我在这张表里把"已发送"记成 ✅，那是错的**：当时真机跑到的只有"字进了会话"。改 `fold` 分两路后需 B 组再确认一次 |
| 有 `RemoteInput` 的通知露出「回复」入口 | ✅ | 通知页那一行出现「回复」按钮 |
| 回复后再次点同一条 | ❌ 已修 | 第二次报"这条通知已经不在了"——回完短信应用自己把通知清掉了，电脑却不知道。**修法见 `NOTIFY_DISMISS`** |
| 通知行排版 | ❌ 已修 | 「回复」「复制验证码」两个按钮压在标题与正文上，且会压到行末时间；窗口拉大后滚动条浮在半空。**根因是这一页并存两个右界，已归一** |

这两项都是**真人用一遍才暴露**的：前者在回路测试里表现为"手机如实回了失败"，后者在截图前
无人可见。它们也正是"只有正向用例才能撞出来"的那一类——5.2 那批负向用例全绿时，两个问题都在。

**仍未跑的一项：微信**。用户测试矩阵里要求确认微信通知到底有没有 `RemoteInput`；本轮 B 组只用了短信。
在栏普查（5.2 最后一行）里微信当时没有通知在栏，所以"预期不支持"目前仍是**预期**，不是实测。
