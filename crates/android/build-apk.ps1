# Builds a signed release APK per Android ABI into dist/.
#
# audiopus_sys links libopus from a single OPUS_LIB_DIR, so we build one ABI at a
# time (set build_targets + OPUS_LIB_DIR, clean the cached opus build script,
# build). Produces dist/newfoundsync-<abi>.apk for each requested ABI.
#
# Usage:
#   pwsh crates\android\build-apk.ps1                      # arm64 + x86_64
#   pwsh crates\android\build-apk.ps1 -Triples aarch64-linux-android
#
# Requires: Android SDK+NDK, cargo-apk, the dev keystore (release.keystore), and
# (for the libopus C build) cmake + ninja from the VS Build Tools.

param(
    [string[]]$Triples = @("aarch64-linux-android", "x86_64-linux-android")
)

# NOTE: keep this 'Continue' — cargo writes normal progress to stderr, which
# would abort the script under 'Stop' in Windows PowerShell. We gate on
# $LASTEXITCODE after each cargo call instead.
$ErrorActionPreference = 'Continue'
$map = @{
    "aarch64-linux-android"   = "aarch64"
    "x86_64-linux-android"    = "x86_64"
    "armv7-linux-androideabi" = "armv7"
}

$repo = (Resolve-Path "$PSScriptRoot\..\..").Path
$cargoToml = "$PSScriptRoot\Cargo.toml"

# Toolchain env.
$env:ANDROID_HOME = if ($env:ANDROID_HOME) { $env:ANDROID_HOME } else { "$env:LOCALAPPDATA\Android\Sdk" }
$env:ANDROID_NDK_HOME = if ($env:ANDROID_NDK_HOME) { $env:ANDROID_NDK_HOME } else { "$env:ANDROID_HOME\ndk\27.2.12479018" }
$cmakeBin = "C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin"
if (Test-Path $cmakeBin) { $env:PATH = "$cmakeBin;" + $env:PATH }

$dist = "$repo\dist"
New-Item -ItemType Directory -Force $dist | Out-Null
$orig = Get-Content $cargoToml -Raw

try {
    foreach ($triple in $Triples) {
        $dir = $map[$triple]
        Write-Output "=================  $triple  ================="
        $opus = "$PSScriptRoot\opus-android\$dir"
        if (-not (Test-Path "$opus\libopus.a")) {
            & "$PSScriptRoot\build-libopus-android.ps1" -Triples $triple
        }
        # Point build_targets at this single ABI.
        $toml = Get-Content $cargoToml -Raw
        $toml = $toml -replace 'build_targets = \[[^\]]*\]', "build_targets = [`"$triple`"]"
        Set-Content $cargoToml $toml -Encoding utf8

        $env:OPUS_LIB_DIR = $opus
        # Align native .so LOAD segments to 16 KB so the app installs on Android 15
        # devices that use 16 KB memory pages (NDK r27 defaults to 4 KB; 16 KB is
        # backward-compatible with 4 KB devices). cargo-apk's ndk-build reads plain
        # RUSTFLAGS (not the target-specific var) and folds it into the final flags;
        # with --target set it applies only to the android link, not host scripts.
        Remove-Item Env:\CARGO_ENCODED_RUSTFLAGS -ErrorAction SilentlyContinue
        $env:RUSTFLAGS = "-Clink-arg=-Wl,-z,max-page-size=16384"
        Push-Location $repo
        cargo clean -p audiopus_sys | Out-Null
        cargo apk build --release -p newfoundsync-android
        $code = $LASTEXITCODE
        Pop-Location
        if ($code -ne 0) { throw "cargo apk build failed for $triple (exit $code)" }

        Copy-Item "$repo\target\release\apk\newfoundsync.apk" "$dist\newfoundsync-$dir.apk" -Force
        Write-Output "  -> dist\newfoundsync-$dir.apk"
    }
}
finally {
    # Restore the original Cargo.toml (default build_targets).
    Set-Content $cargoToml $orig -Encoding utf8
}
Write-Output "Done. APKs in $dist"
