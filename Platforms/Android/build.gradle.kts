// 根工程：仅声明插件版本（K3 单一版本源见 gradle/libs.versions.toml）
plugins {
    alias(libs.plugins.android.application) apply false
    alias(libs.plugins.kotlin.android) apply false
    alias(libs.plugins.kotlin.compose) apply false
}