# Cross-compiles libopus.a for one or more Android ABIs using the NDK, so
# audiopus_sys can link it (it ships only a Windows prebuilt and can't
# cross-compile itself). Output: crates/android/opus-android/<dir>/libopus.a
#
# Usage:
#   pwsh crates\android\build-libopus-android.ps1                 # all ABIs
#   pwsh crates\android\build-libopus-android.ps1 -Triples aarch64-linux-android
#
# Requires: Android NDK (ANDROID_NDK_HOME), cmake + ninja (ship with VS Build Tools).

param(
    [string[]]$Triples = @("aarch64-linux-android", "x86_64-linux-android", "armv7-linux-androideabi")
)

$ErrorActionPreference = 'Stop'
$opusVer = "1.5.2"

# triple -> (cmake ANDROID_ABI, output dir name)
$map = @{
    "aarch64-linux-android"   = @("arm64-v8a", "aarch64")
    "x86_64-linux-android"    = @("x86_64", "x86_64")
    "armv7-linux-androideabi" = @("armeabi-v7a", "armv7")
    "i686-linux-android"      = @("x86", "i686")
}

$ndk = $env:ANDROID_NDK_HOME
if (-not $ndk) { $ndk = "$env:LOCALAPPDATA\Android\Sdk\ndk\27.2.12479018" }
if (-not (Test-Path "$ndk\build\cmake\android.toolchain.cmake")) {
    throw "NDK not found at $ndk — set ANDROID_NDK_HOME"
}
$cmake = (Get-Command cmake -ErrorAction SilentlyContinue).Source
if (-not $cmake) { $cmake = "C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe" }
$ninja = (Get-Command ninja -ErrorAction SilentlyContinue).Source
if (-not $ninja) { $ninja = "C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja\ninja.exe" }

# Fetch + extract opus source once.
$tmp = "$env:TEMP\opus-$opusVer.tar.gz"
if (-not (Test-Path "$env:TEMP\opus-$opusVer")) {
    Invoke-WebRequest "https://downloads.xiph.org/releases/opus/opus-$opusVer.tar.gz" -OutFile $tmp
    tar -xzf $tmp -C $env:TEMP
}
$src = "$env:TEMP\opus-$opusVer"

foreach ($triple in $Triples) {
    $abi, $dir = $map[$triple]
    Write-Output "=== libopus for $triple ($abi) ==="
    $build = "$env:TEMP\opus-android-$dir"
    & $cmake -S $src -B $build -G Ninja -DCMAKE_MAKE_PROGRAM="$ninja" `
        -DCMAKE_TOOLCHAIN_FILE="$ndk\build\cmake\android.toolchain.cmake" `
        -DANDROID_ABI=$abi -DANDROID_PLATFORM=android-26 `
        -DOPUS_BUILD_SHARED_LIBRARY=OFF -DCMAKE_BUILD_TYPE=Release | Out-Null
    & $cmake --build $build | Out-Null
    $out = "$PSScriptRoot\opus-android\$dir"
    New-Item -ItemType Directory -Force $out | Out-Null
    Copy-Item "$build\libopus.a" "$out\libopus.a" -Force
    Write-Output "  -> $out\libopus.a"
}
