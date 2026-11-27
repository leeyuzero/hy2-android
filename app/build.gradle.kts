plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "com.pcxx.hy2box"
    compileSdk = 34

    buildFeatures {
        compose = true
    }

    defaultConfig {
        applicationId = "com.pcxx.hy2box"
        minSdk = 26
        targetSdk = 34
        versionCode = 1
        versionName = "0.1.0-stage1"
    }

    splits {
        abi {
            isEnable = true
            reset()
            include("arm64-v8a", "armeabi-v7a", "x86_64")
            // 额外产出一个包含全部 ABI 的通包，避免用户选错 ABI 导致安装异常
            isUniversalApk = true
        }
    }

    // 固定签名（自用侧载反复覆盖安装）：
    // 仅当 CI / 本地注入 HY2BOX_KEYSTORE 时启用；未注入则保留 AGP 默认 debug 签名，
    // 保证不配 Secret 也能正常编译。私钥本身绝不提交到仓库。
    signingConfigs {
        create("selfUse") {
            val ks = System.getenv("HY2BOX_KEYSTORE")
            if (!ks.isNullOrBlank()) {
                storeFile = file(ks)
                storePassword = System.getenv("HY2BOX_STORE_PASSWORD") ?: "hy2box2026"
                keyAlias = System.getenv("HY2BOX_KEY_ALIAS") ?: "hy2box"
                keyPassword = System.getenv("HY2BOX_KEY_PASSWORD") ?: "hy2box2026"
            }
        }
    }

    buildTypes {
        debug {
            if (!System.getenv("HY2BOX_KEYSTORE").isNullOrBlank()) {
                signingConfig = signingConfigs.getByName("selfUse")
            }
        }
        release {
            isMinifyEnabled = false
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.8.6")
    implementation("androidx.activity:activity-compose:1.9.2")
    implementation(platform("androidx.compose:compose-bom:2024.10.00"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.material3:material3")
}
