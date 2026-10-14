import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("rust")
}

val tauriProperties = Properties().apply {
    val propFile = file("tauri.properties")
    if (propFile.exists()) {
        propFile.inputStream().use { load(it) }
    }
}

val signingProperties = Properties().apply {
    val propFile = file("../../../android-signing.properties")
    if (propFile.exists()) {
        propFile.inputStream().use { load(it) }
    }
}

fun signingValue(env: String, key: String): String? =
    System.getenv(env)?.takeIf { it.isNotEmpty() } ?: signingProperties.getProperty(key)

gradle.taskGraph.whenReady {
    val needsRelease = allTasks.any { it.name.contains("Release") }
    val missing = listOf(
        "TAURI_KEYSTORE_PASSWORD" to "storePassword",
        "TAURI_KEY_PASSWORD" to "keyPassword",
    ).filter { (env, key) -> signingValue(env, key) == null }
    if (needsRelease && missing.isNotEmpty()) {
        throw GradleException(
            "Android release 签名缺少口令: 请设置环境变量 " + missing.joinToString { it.first } +
                "，或在 desktop/android-signing.properties 中填写 " +
                missing.joinToString { it.second } + "（该文件已 gitignore）。"
        )
    }
}

android {
    compileSdk = 36
    namespace = "com.nownexts.thirdc"
    defaultConfig {
        manifestPlaceholders["usesCleartextTraffic"] = "false"
        applicationId = "com.nownexts.thirdc"
        minSdk = 24
        targetSdk = 36
        versionCode = tauriProperties.getProperty("tauri.android.versionCode", "1").toInt()
        versionName = tauriProperties.getProperty("tauri.android.versionName", "1.0")
    }
    signingConfigs {
        // 签名口令不入库：优先读环境变量 TAURI_KEYSTORE_PASSWORD / TAURI_KEY_PASSWORD，
        // 其次读 gitignored 的 desktop/android-signing.properties（storePassword= / keyPassword=）
        create("release") {
            storeFile = file("../../../thirdc-release.keystore")
            storePassword = signingValue("TAURI_KEYSTORE_PASSWORD", "storePassword")
            keyAlias = "thirdc"
            keyPassword = signingValue("TAURI_KEY_PASSWORD", "keyPassword")
        }
    }
    buildTypes {
        getByName("debug") {
            manifestPlaceholders["usesCleartextTraffic"] = "true"
            isDebuggable = true
            isJniDebuggable = true
            isMinifyEnabled = false
            packaging {                jniLibs.keepDebugSymbols.add("*/arm64-v8a/*.so")
                jniLibs.keepDebugSymbols.add("*/armeabi-v7a/*.so")
                jniLibs.keepDebugSymbols.add("*/x86/*.so")
                jniLibs.keepDebugSymbols.add("*/x86_64/*.so")
            }
        }
        getByName("release") {
            // 本地内核走 http://127.0.0.1:7717，必须放行明文（仅环回）
            manifestPlaceholders["usesCleartextTraffic"] = "true"
            signingConfig = signingConfigs.getByName("release")
            isMinifyEnabled = true
            proguardFiles(
                *fileTree(".") { include("**/*.pro") }
                    .plus(getDefaultProguardFile("proguard-android-optimize.txt"))
                    .toList().toTypedArray()
            )
        }
    }
    kotlinOptions {
        jvmTarget = "1.8"
    }
    buildFeatures {
        buildConfig = true
    }
}

rust {
    rootDirRel = "../../../"
}

dependencies {
    implementation("androidx.webkit:webkit:1.14.0")
    implementation("androidx.appcompat:appcompat:1.7.1")
    implementation("androidx.activity:activity-ktx:1.10.1")
    implementation("com.google.android.material:material:1.12.0")
    implementation("androidx.lifecycle:lifecycle-process:2.10.0")
    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.1.4")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.5.0")
}

apply(from = "tauri.build.gradle.kts")