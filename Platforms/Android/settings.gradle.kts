// LinkX Android（4.3.2：Jetpack Compose + Material3，单 Activity）
// Gradle 8.7 / AGP 8.5.2 / Kotlin 2.0.0 / Compose Compiler(plugin) 2.0.0（K3：Compose 编译器版本与 Kotlin 一一对应）
pluginManagement {
    repositories {
        // 沙箱代理优先 Aliyun 镜像（见 android-package Skill）
        maven { url = uri("https://maven.aliyun.com/repository/google") }
        maven { url = uri("https://maven.aliyun.com/repository/central") }
        maven { url = uri("https://maven.aliyun.com/repository/gradle-plugin") }
        gradlePluginPortal()
        google()
        mavenCentral()
    }
}
dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        maven { url = uri("https://maven.aliyun.com/repository/google") }
        maven { url = uri("https://maven.aliyun.com/repository/central") }
        google()
        mavenCentral()
    }
}

rootProject.name = "LinkX"
include(":app")