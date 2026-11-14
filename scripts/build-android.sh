#!/usr/bin/env bash
# Build the Android APK: Rust cdylib via cargo-ndk, then Gradle packages it.
#
# Needs: rustup targets for the ABIs (aarch64-linux-android, x86_64-linux-android), cargo-ndk,
# an Android SDK + NDK (ANDROID_HOME / ANDROID_NDK_HOME, or android/local.properties) and JDK 17-21.
#
#   scripts/build-android.sh                     # arm64-v8a (phones)
#   ABIS="arm64-v8a x86_64" scripts/build-android.sh   # + x86_64 for the emulator
#
# Output: target/android/PawchiveDownloader-<version>.apk, plus the unstripped .so per ABI
# (target/android/symbols/<abi>/) for symbolizing crash addresses with addr2line.
set -euo pipefail
cd "$(dirname "$0")/.."

ABIS=${ABIS:-arm64-v8a}
version=$(sed -nE 's/^version = "([^"]+)"/\1/p' Cargo.toml | head -1)

ndk_args=()
for abi in $ABIS; do ndk_args+=(-t "$abi"); done

rm -rf android/app/nativeLibs
cargo ndk "${ndk_args[@]}" --platform 26 -o android/app/nativeLibs \
    build --release -p pawchive-gui --lib

(cd android && ./gradlew --quiet assembleRelease)

out=target/android
rm -rf "$out" && mkdir -p "$out/symbols"
cp android/app/build/outputs/apk/release/app-release.apk "$out/PawchiveDownloader-$version.apk"
for abi in $ABIS; do
    mkdir -p "$out/symbols/$abi"
    cp "android/app/nativeLibs/$abi/libpawchive_gui.so" "$out/symbols/$abi/"
done
echo "$out/PawchiveDownloader-$version.apk"
