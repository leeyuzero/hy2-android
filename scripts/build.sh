#!/usr/bin/env bash
# Stage 1 本地/通用构建脚本（Linux/macOS）
# 前置：Android SDK (API 34 + NDK 26), Rust 1.85+, cargo-ndk, JDK 17+
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
: "${ANDROID_HOME:?请设置 ANDROID_HOME}"

echo "[1/3] 编译 Rust 内核到 Android ABI"
cargo ndk \
  -t arm64-v8a \
  -t armeabi-v7a \
  -t x86_64 \
  -o app/src/main/jniLibs \
  --manifest-path core/Cargo.toml \
  build --release

echo "[2/3] Gradle 打包 APK"
chmod +x ./gradlew
./gradlew assembleRelease --no-daemon

echo "[3/3] 产物："
ls -lh app/build/outputs/apk/release/
