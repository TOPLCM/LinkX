package com.linkx.app

import android.content.Context
import android.content.SharedPreferences
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue

/** 外观偏好（默认跟随系统）。 */
enum class ThemeMode { System, Light, Dark }

/**
 * 应用设置（与剪贴板开关共用 SharedPreferences 文件 "linkx"）。
 *
 * `themeMode` 用 Compose `mutableStateOf` 持有：设置页一改就触发整棵 UI 重组（深浅色立即生效），
 * 无需重启 Activity。
 */
object AppPrefs {
    private const val PREFS = "linkx"
    private const val KEY_THEME = "theme_mode"

    private var prefs: SharedPreferences? = null

    var themeMode by mutableStateOf(ThemeMode.System)
        private set

    fun init(context: Context) {
        val p = context.applicationContext.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        prefs = p
        themeMode = parse(p.getString(KEY_THEME, null))
    }

    fun setTheme(mode: ThemeMode) {
        themeMode = mode
        prefs?.edit()?.putString(KEY_THEME, name(mode))?.apply()
    }

    private fun parse(raw: String?): ThemeMode = when (raw) {
        "light" -> ThemeMode.Light
        "dark" -> ThemeMode.Dark
        else -> ThemeMode.System
    }

    private fun name(mode: ThemeMode): String = when (mode) {
        ThemeMode.System -> "system"
        ThemeMode.Light -> "light"
        ThemeMode.Dark -> "dark"
    }
}
