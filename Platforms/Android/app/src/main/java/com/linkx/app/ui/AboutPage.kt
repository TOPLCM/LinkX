/**
 * 关于页：应用名 / 一句话介绍 / 作者，外加第三方许可清单的入口。
 *
 * 关于页文案放在 [AboutCopy] 里当**单一来源**（对应 Windows 侧 `Platforms/Windows/src/about.rs`），
 * 这一页只排版——改一个字不该动界面代码。
 *
 * 由原先的 `MainActivity.kt` 拆出（一页一个文件）。
 */

package com.linkx.app.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

// ---------- 页：关于 ----------

/**
 * 关于页文案的**单一来源**（对应 Windows 侧 `Platforms/Windows/src/about.rs`）。
 * 这一页不放任何外链（「获取最新版本」「打赏」都不上 UI），所以只有三行字，也就不再是"Links"。
 * 两端文案要改一起改（门禁不校验这一对）。
 */
internal object AboutCopy {
    const val APP_NAME = "LinkX"
    const val TAGLINE = "开源的跨端协同效率革命工具"
    const val AUTHOR = "开发者：Chaoming"
}

/** APK 内许可清单的目录（打包时由 `Scripts/build-android-apk.sh` 复制进来，见仓库根 `THIRD_PARTY_LICENSES.md`「二」）。 */
private const val LICENSE_DIR = "licenses"

/** 清单里的文件：assets 里的文件名 + 给用户看的名字。 */
private val LICENSE_FILES = listOf(
    "THIRD_PARTY_LICENSES.md" to "第三方组件许可登记",
    "LICENSE" to "LinkX 本体许可（GPL-3.0-or-later）",
    "LICENSE-localsend.txt" to "LocalSend 移植件许可（Apache-2.0）",
    "NOTICE-localsend.txt" to "LocalSend 上游署名",
    "PORTING-localsend.md" to "LocalSend 移植范围与改动声明",
)

/**
 * 关于页：三行字、没有外链。文案取自 [AboutCopy]，这里只排版——改一个字不该动界面代码。
 */
@Composable
internal fun AboutPage() {
    var showLicenses by remember { mutableStateOf(false) }
    Column(
        modifier = Modifier.fillMaxWidth(),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Text(
            AboutCopy.APP_NAME,
            style = MaterialTheme.typography.displaySmall,
            fontWeight = FontWeight.Bold,
        )
        Text(AboutCopy.TAGLINE, style = MaterialTheme.typography.titleMedium)
        Text(
            AboutCopy.AUTHOR,
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        // 许可入口不能只写在仓库里：APK 也是一种"分发形式"，随包附许可全文是硬要求。
        TextButton(onClick = { showLicenses = true }) { Text("第三方许可") }
    }
    if (showLicenses) {
        LicenseDialog(onClose = { showLicenses = false })
    }
}

/**
 * 许可清单弹窗：先列文件，点一份看全文。
 *
 * 清单文本是打包时复制进 assets 的固定文件，读它要走 IO 线程；读不到就如实说"这个包里没有"，
 * 不能装成有——那等于让用户以为义务已履行。
 */
@Composable
private fun LicenseDialog(onClose: () -> Unit) {
    val ctx = LocalContext.current
    var selected by remember { mutableStateOf<String?>(null) }
    var text by remember { mutableStateOf("") }
    var missing by remember { mutableStateOf(false) }

    LaunchedEffect(selected) {
        val name = selected ?: return@LaunchedEffect
        val body = withContext(Dispatchers.IO) {
            runCatching {
                ctx.assets.open("$LICENSE_DIR/$name").bufferedReader().use { it.readText() }
            }.getOrNull()
        }
        text = body.orEmpty()
        missing = body == null
    }

    AlertDialog(
        onDismissRequest = onClose,
        title = {
            val name = selected
            Text(if (name == null) "第三方许可" else LICENSE_FILES.find { it.first == name }?.second.orEmpty())
        },
        text = {
            Column(modifier = Modifier.verticalScroll(rememberScrollState())) {
                val name = selected
                if (name == null) {
                    LICENSE_FILES.forEach { (file, label) ->
                        TextButton(onClick = { selected = file }) { Text(label) }
                    }
                    Text(
                        "这些许可全文随本 APK 一起分发（assets/licenses/）。",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                } else {
                    TextButton(onClick = { selected = null }) { Text("返回清单") }
                    if (missing) {
                        Text(
                            "这个安装包里没有 $name（构建时没把许可清单复制进来）。",
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.error,
                        )
                    } else {
                        Text(text, style = MaterialTheme.typography.bodySmall)
                    }
                }
            }
        },
        confirmButton = { TextButton(onClick = onClose) { Text("关闭") } },
    )
}
