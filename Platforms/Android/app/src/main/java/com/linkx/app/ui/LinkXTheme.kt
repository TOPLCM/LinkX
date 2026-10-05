package com.linkx.app.ui

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import com.linkx.app.AppPrefs
import com.linkx.app.ThemeMode

/**
 * LinkX 主题。
 *
 * - **深浅色**：优先取 [AppPrefs.themeMode]；选「跟随系统」时用 [isSystemInDarkTheme]，
 *   即 Android 10+ 的系统级深色开关（用户可在设置页手动覆盖）。
 * - **字体**：不设置 fontFamily → Compose 默认 `FontFamily.Default`，即**系统默认字体**。
 * - 色板与 Windows 端同源（同一套品牌蓝与中性色），保证跨端观感一致。
 */
@Composable
fun LinkXTheme(content: @Composable () -> Unit) {
    val dark = when (AppPrefs.themeMode) {
        ThemeMode.Light -> false
        ThemeMode.Dark -> true
        ThemeMode.System -> isSystemInDarkTheme()
    }
    MaterialTheme(
        colorScheme = if (dark) DarkScheme else LightScheme,
        content = content,
    )
}

private val LightScheme = lightColorScheme(
    primary = Color(0xFF1A73D9),
    onPrimary = Color.White,
    primaryContainer = Color(0xFFDCEBFF),
    onPrimaryContainer = Color(0xFF0B3C73),
    secondary = Color(0xFF4A5568),
    onSecondary = Color.White,
    background = Color(0xFFF6F7F9),
    onBackground = Color(0xFF1A1C1F),
    surface = Color(0xFFFFFFFF),
    onSurface = Color(0xFF1A1C1F),
    surfaceVariant = Color(0xFFEDF1F6),
    onSurfaceVariant = Color(0xFF5B6270),
    outline = Color(0xFFD7DCE3),
    outlineVariant = Color(0xFFE3E7EC),
    error = Color(0xFFD03030),
    onError = Color.White,
)

private val DarkScheme = darkColorScheme(
    primary = Color(0xFF4AA8FF),
    onPrimary = Color(0xFF06243F),
    primaryContainer = Color(0xFF1E3A5C),
    onPrimaryContainer = Color(0xFFCFE4FF),
    secondary = Color(0xFF9AA3B0),
    onSecondary = Color(0xFF1B1F25),
    background = Color(0xFF1B1F25),
    onBackground = Color(0xFFE9EDF3),
    surface = Color(0xFF232830),
    onSurface = Color(0xFFE9EDF3),
    surfaceVariant = Color(0xFF2C333D),
    onSurfaceVariant = Color(0xFF9BA4B1),
    outline = Color(0xFF3A424E),
    outlineVariant = Color(0xFF323844),
    error = Color(0xFFFF6B6B),
    onError = Color(0xFF3A0A0A),
)
