plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.compose)
}

android {
    namespace = "com.linkx.app"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.linkx.app"
        minSdk = 26          // Android 8.0（4.3.2）
        targetSdk = 34
        versionCode = 13
        versionName = "0.5.0"
        ndk { abiFilters += "arm64-v8a" }   // J5：arm64 only
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
    // buildConfig：让 Kotlin 侧的版本号直接取 versionName，
    // 不再在源码里抄一份字面量（那份抄本曾经停在 0.3.0，电脑看到的手机版本从此失真）
    buildFeatures { compose = true; buildConfig = true }
    // Rust Core .so（cargo-ndk 输出 LinkX/Target/android-jni；Gradle 工程根=Platforms/Android，
    // 故用模块 projectDir 上溯 3 级定位 LinkX 根，避免相对/rootDir 解析错位）
    sourceSets["main"].jniLibs.srcDir(project.projectDir.resolve("../../../Target/android-jni"))
}

dependencies {
    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.lifecycle.runtime.ktx)
    implementation(libs.androidx.activity.compose)
    implementation(libs.androidx.compose.ui)
    implementation(libs.androidx.compose.ui.graphics)
    implementation(libs.androidx.compose.ui.tooling.preview)
    implementation(libs.androidx.compose.material3)
    implementation(libs.androidx.compose.animation)
    debugImplementation(libs.androidx.compose.ui.tooling)
}