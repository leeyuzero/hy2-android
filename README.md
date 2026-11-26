# hy2-android —— 初版骨架（Stage 1）

Android 端 Hysteria2 客户端，Rust 内核 + Kotlin UI。当前为**初版骨架**：编译配置、JNI 桥接、VpnService 框架、配置解析、Gradle wrapper 与 GitHub Actions 在线编译已就位；真机跑通所需的系统网络栈细节见文末「Stage 2 待办」。

## 技术栈
- **内核**：Rust（Tokio + quinn），复用 `rsteria2` crate 0.1（Hysteria2 客户端协议库）
- **桥接**：JNI（Rust 导出 `Java_...` 符号，Kotlin 侧 `System.loadLibrary("hy2core")`）
- **VPN**：Android `VpnService` + tun2socks 模式
- **UI**：Kotlin + Jetpack Compose（最小界面，仅用于验证）
- **构建**：Gradle 8.7 + AGP 8.6 + cargo-ndk

## 推荐构建方式：GitHub Actions 在线编译（无需本机环境）

本机只需 Git，**不需要** Android SDK / Rust / Gradle。

1. 将本目录上传为 GitHub 仓库（例如 `hy2-android`）：
   ```bash
   cd hy2-android
   git init
   git add .
   git commit -m "Stage 1 skeleton"
   git branch -M main
   git remote add origin https://github.com/<你的账号>/hy2-android.git
   git push -u origin main
   ```
2. 推送后自动触发 `.github/workflows/build.yml`，Actions 页面可见进度。
3. 构建完成后在本次 Action 页面底部 **Artifacts → hy2box-stage1-apk** 下载 APK。
   - 也可手动触发：Actions 标签 → Build hy2-android APK → Run workflow。
4. 免费公开仓库的 Actions 分钟数足够（单次约 10–20 分钟）。

CI 流程：安装 JDK17 + Rust(stable, Android targets) + cargo-ndk + Android SDK(API34/NDK26) → `cargo ndk build --release` 产出三个 ABI 的 `libhy2core.so` → `./gradlew assembleRelease` → 上传 APK。

## 本地构建（需自行装环境）
前置：Android SDK (API 34 + NDK 26.3)、Rust 1.85+、cargo-ndk、JDK 17+。
```bash
export ANDROID_HOME=/path/to/android-sdk
./scripts/build.sh
```

## 目录
```
├── app/src/main/java/com/pcxx/hy2box/
│   ├── MainActivity.kt          # 最小 UI：输入配置、连接/断开
│   ├── Hy2VpnService.kt         # VpnService：建 TUN、设路由、启动 Rust
│   ├── Hy2Bridge.kt             # JNI 声明
│   └── ConfigStore.kt           # SharedPreferences
├── core/
│   ├── Cargo.toml               # rsteria2 / tokio / jni
│   └── src/lib.rs               # JNI 实现 + hy2 smoke test
├── .github/workflows/build.yml  # 在线编译配置
├── scripts/build.sh             # 本地一键构建
└── gradle/wrapper/ + gradlew    # 已包含，无需本地安装 Gradle
```

## Stage 2 待办（真机跑通 hy2 前必须完成）
1. **tun2socks 数据面**：`Hy2VpnService` 中 TUN → SOCKS5 转发目前为占位；需引入 `tun2socks` crate 或 `netstack`，把 TUN 的 IP 包转成 SOCKS5 会话。
2. **Rust 常驻 SOCKS5 入站**：`lib.rs` 现为单次 smoke test，需改为常驻监听并把 `rsteria2` 的 TCP/UDP 流接进来。
3. **DNS**：VpnService 下 DNS 需强制走隧道（拦截 53 或 DoH 转发）。
4. **保活与重连**：接入 `rsteria2::ReconnectableClient`。
5. **兼容性**：Android 14+ 前台服务类型、通知权限、OEM 保活白名单。
