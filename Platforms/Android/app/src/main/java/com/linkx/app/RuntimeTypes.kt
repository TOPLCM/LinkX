/**
 * 运行时对外的数据模型：GATT 发送器、已转发通知、文件摘要、传输状态与传输行、身份变更提示。
 *
 * 由原先的 `LinkxRuntime.kt` 拆出——这几个类型被运行时、TCP 通道、UDP 发现、文件传输
 * 与界面同时用，放在运行时那份文件里只会让两边互相牵扯。
 */

package com.linkx.app

import android.net.Uri

/** GATT 发送器：由 BlePeripheralService 注册，把每个待发分片包经 notify 写回 Central。 */
fun interface PacketSink {
    fun send(packet: ByteArray)
}

/** 已转发通知记录（Android 侧展示"本机已转发"的通知，最多 20 条；keyHash=0 表示无稳定 key）。 */
data class ForwardedNotification(
    val pkg: String,
    val title: String,
    val text: String,
    val tsMs: Long,
    val keyHash: Int = 0,
)

/** 文件信息（SAF Uri 解析结果，供 UI 展示）。 */
data class FileBrief(val name: String, val size: Long)

/** 传输任务状态："最后一个分块交给本地引擎" ≠ 对端收到。`AwaitingPeer` 等对端 FILE_DONE 回执才落 `Done`，回执超时落 `SentUnconfirmed`（分块确实发完了，只是对端没回话）——既不骗用户说已完成，也不谎报失败。`Cancelled` 单独一档而不并入 `Failed`：取消是用户的主动动作，报成失败就是说系统坏了。 */
enum class TransferState { Running, AwaitingPeer, SentUnconfirmed, Done, Failed, Cancelled }

/** 单条文件传输记录（发送/接收共用；bytes = 已完成字节数）。 */
data class TransferItem(
    val fileId: Long,
    val name: String,
    val size: Long,
    val outgoing: Boolean,
    val bytes: Long,
    val state: TransferState,
    /** 原因文案（[TransferState.Failed] = 失败原因，[TransferState.Cancelled] = 取消原因）。空 = 引擎没给。 */
    val error: String = "",
    /** 接收文件的落盘位置：普通绝对路径，或 `content://…`（用户自选目录里的文档）。 */
    val path: String = "",
    /** 已完成但有一句话要说（例如"没能存进你选的目录，先留在应用私有目录"）。 */
    val note: String = "",
)

/** 对端新身份待决策的提示数据（UI 据此弹「重新配对确认」）。 */
data class IdentityChangeInfo(
    val name: String,
    val oldFingerprint: String,
    val newFingerprint: String,
)
