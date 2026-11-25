# Installs the per-user Windows toolchain needed to build the FrameMate Android APK.
#
#   powershell -ExecutionPolicy Bypass -File scripts/setup-android-windows.ps1
#
# Everything lands under %LOCALAPPDATA%\framemate-build and %LOCALAPPDATA%\Android\Sdk,
# so no administrator rights are required. Re-running is safe: finished steps are skipped.
#
# Requires the MSVC C++ toolchain + Windows SDK (Visual Studio Build Tools) for the Rust
# host target; the script checks for them and stops with instructions if they're missing.

$ErrorActionPreference = "Continue"
$ProgressPreference = "SilentlyContinue"
try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12 } catch {}

$root = Join-Path $env:LOCALAPPDATA "framemate-build"
$downloads = Join-Path $root "downloads"
New-Item -ItemType Directory -Force -Path $root, $downloads | Out-Null

function Fail($message) {
    Write-Host ""
    Write-Host "ERROR: $message" -ForegroundColor Red
    exit 1
}

function Download($url, $out) {
    if (Test-Path $out) {
        Write-Host "  already downloaded: $(Split-Path -Leaf $out)"
        return
    }
    Write-Host "  downloading $url"
    try {
        Invoke-WebRequest -Uri $url -OutFile $out -UseBasicParsing
    } catch {
        if (Test-Path $out) { Remove-Item $out -Force -ErrorAction SilentlyContinue }
        Fail "download failed: $url`n$($_.Exception.Message)"
    }
}

# --- MSVC / Windows SDK (host linker for Rust build scripts and proc macros) ---------------
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$hasMsvc = $false
if (Test-Path $vswhere) {
    $msvc = & $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
    if ($LASTEXITCODE -eq 0 -and $msvc) { $hasMsvc = $true }
}
if (-not $hasMsvc) {
    $hasMsvc = (Test-Path "$env:ProgramFiles\Microsoft Visual Studio") -or (Test-Path "${env:ProgramFiles(x86)}\Microsoft Visual Studio")
}
if (-not (Test-Path "${env:ProgramFiles(x86)}\Windows Kits\10\Lib")) {
    Fail "the Windows SDK was not found. Install 'Desktop development with C++' from Visual Studio Build Tools, then re-run this script."
}
Write-Host "MSVC / Windows SDK: present"

# --- JDK 21 (Temurin) ----------------------------------------------------------------------
$jdkHome = Join-Path $root "jdk-21"
if (-not (Test-Path (Join-Path $jdkHome "bin\javac.exe"))) {
    Write-Host "JDK 21: installing"
    $zip = Join-Path $downloads "jdk21.zip"
    Download "https://api.adoptium.net/v3/binary/latest/21/ga/windows/x64/jdk/hotspot/normal/eclipse?project=jdk" $zip
    $tmp = Join-Path $root "jdk-tmp"
    if (Test-Path $tmp) { Remove-Item $tmp -Recurse -Force }
    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    $inner = Get-ChildItem $tmp -Directory | Select-Object -First 1
    if (-not $inner) { Fail "the JDK archive did not contain a directory" }
    if (Test-Path $jdkHome) { Remove-Item $jdkHome -Recurse -Force }
    Move-Item $inner.FullName $jdkHome
    Remove-Item $tmp -Recurse -Force
} else {
    Write-Host "JDK 21: present"
}

# --- Deno ----------------------------------------------------------------------------------
$denoDir = Join-Path $root "deno"
if (-not (Test-Path (Join-Path $denoDir "deno.exe"))) {
    Write-Host "Deno: installing"
    $zip = Join-Path $downloads "deno.zip"
    Download "https://github.com/denoland/deno/releases/latest/download/deno-x86_64-pc-windows-msvc.zip" $zip
    New-Item -ItemType Directory -Force -Path $denoDir | Out-Null
    Expand-Archive -Path $zip -DestinationPath $denoDir -Force
} else {
    Write-Host "Deno: present"
}

# --- Rust (rustup, per-user) ---------------------------------------------------------------
$cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
if (-not (Test-Path (Join-Path $cargoBin "cargo.exe"))) {
    Write-Host "Rust: installing via rustup"
    $exe = Join-Path $downloads "rustup-init.exe"
    Download "https://win.rustup.rs/x86_64" $exe
    & $exe -y --default-toolchain stable --profile minimal --no-modify-path
    if (-not (Test-Path (Join-Path $cargoBin "cargo.exe"))) { Fail "rustup did not install cargo" }
} else {
    Write-Host "Rust: present"
}

$env:Path = "$cargoBin;$env:Path"
$env:JAVA_HOME = $jdkHome
$env:ANDROID_HOME = Join-Path $env:LOCALAPPDATA "Android\Sdk"
$env:ANDROID_SDK_ROOT = $env:ANDROID_HOME
$env:NDK_HOME = Join-Path $env:ANDROID_HOME "ndk\29.0.14206865"
$env:ANDROID_NDK_HOME = $env:NDK_HOME
$env:Path = "$jdkHome\bin;$env:ANDROID_HOME\cmdline-tools\latest\bin;$env:ANDROID_HOME\platform-tools;$denoDir;$env:Path"

Write-Host "Rust targets: adding Android (and the musl target pinned by rust-toolchain.toml)"
& (Join-Path $cargoBin "rustup.exe") target add aarch64-linux-android
& (Join-Path $cargoBin "rustup.exe") target add aarch64-unknown-linux-musl

# --- Android command-line tools ------------------------------------------------------------
$sdk = $env:ANDROID_HOME
$cmdlineTools = Join-Path $sdk "cmdline-tools\latest"
$sdkmanager = Join-Path $cmdlineTools "bin\sdkmanager.bat"
if (-not (Test-Path $sdkmanager)) {
    Write-Host "Android command-line tools: installing"
    $zip = Join-Path $downloads "commandlinetools.zip"
    Download "https://dl.google.com/android/repository/commandlinetools-win-13114758_latest.zip" $zip
    $tmp = Join-Path $root "cmdline-tools-tmp"
    if (Test-Path $tmp) { Remove-Item $tmp -Recurse -Force }
    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    New-Item -ItemType Directory -Force -Path (Split-Path $cmdlineTools) | Out-Null
    if (Test-Path $cmdlineTools) { Remove-Item $cmdlineTools -Recurse -Force }
    Move-Item (Join-Path $tmp "cmdline-tools") $cmdlineTools
    Remove-Item $tmp -Recurse -Force
} else {
    Write-Host "Android command-line tools: present"
}

Write-Host "Android SDK: accepting licenses"
1..100 | ForEach-Object { "y" } | & $sdkmanager --sdk_root="$sdk" --licenses | Out-Null

Write-Host "Android SDK: installing packages (this is the big download)"
# Android 17 is published as the minor-version package android-37.0, not android-37.
$packages = @("platform-tools", "platforms;android-37.0", "build-tools;37.0.0", "ndk;29.0.14206865")
foreach ($package in $packages) {
    Write-Host "  sdkmanager $package"
    & $sdkmanager --sdk_root="$sdk" $package
    if ($LASTEXITCODE -ne 0) {
        Write-Host "  WARNING: sdkmanager could not install $package" -ForegroundColor Yellow
    }
}

# --- Persist environment for future shells -------------------------------------------------
function Set-UserEnvironment($name, $value) {
    if ([Environment]::GetEnvironmentVariable($name, "User") -ne $value) {
        [Environment]::SetEnvironmentVariable($name, $value, "User")
    }
}
Set-UserEnvironment "JAVA_HOME" $jdkHome
Set-UserEnvironment "ANDROID_HOME" $sdk
Set-UserEnvironment "ANDROID_SDK_ROOT" $sdk
Set-UserEnvironment "NDK_HOME" $env:NDK_HOME
Set-UserEnvironment "ANDROID_NDK_HOME" $env:NDK_HOME

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
$pathParts = @()
if ($userPath) { $pathParts = $userPath -split ';' | Where-Object { $_ } }
foreach ($entry in @("$jdkHome\bin", "$sdk\cmdline-tools\latest\bin", "$sdk\platform-tools", $cargoBin, $denoDir)) {
    if ($pathParts -notcontains $entry) { $pathParts += $entry }
}
[Environment]::SetEnvironmentVariable("Path", ($pathParts -join ';'), "User")

# Gradle can locate the SDK through ANDROID_HOME, but a local.properties is belt-and-braces.
$projectLocalProperties = Join-Path $PSScriptRoot "..\app\src-tauri\gen\android\local.properties"
$sdkProperty = "sdk.dir=" + ($sdk -replace '\\', '/')
if (-not (Test-Path $projectLocalProperties) -or -not (Select-String -Path $projectLocalProperties -Pattern '^sdk\.dir=' -Quiet)) {
    Set-Content -Path $projectLocalProperties -Value $sdkProperty -Encoding ASCII
}

# --- Verify --------------------------------------------------------------------------------
Write-Host ""
Write-Host "=== Installed ==="
& (Join-Path $jdkHome "bin\java.exe") -version 2>&1 | ForEach-Object { "  $_" }
& (Join-Path $cargoBin "cargo.exe") --version
& (Join-Path $denoDir "deno.exe") --version | Select-Object -First 1
& (Join-Path $cargoBin "rustup.exe") target list --installed | ForEach-Object { "  target: $_" }
if (Test-Path (Join-Path $env:NDK_HOME "source.properties")) { Write-Host "  NDK: $env:NDK_HOME" }

$missing = @()
foreach ($package in @("platforms\android-37.0", "build-tools\37.0.0", "ndk\29.0.14206865", "platform-tools")) {
    if (-not (Test-Path (Join-Path $sdk $package))) { $missing += $package }
}
if ($missing.Count -gt 0) {
    Write-Host ""
    Write-Host "Missing SDK components: $($missing -join ', ')" -ForegroundColor Yellow
    Write-Host "Run: `"$sdkmanager`" --list | Select-String 'android-37|build-tools;37|ndk;29'"
    exit 1
}

Write-Host ""
Write-Host "Toolchain ready. Build with:" -ForegroundColor Green
Write-Host "  cd app"
Write-Host "  deno install --frozen"
Write-Host "  deno task tauri android build --apk --debug --target aarch64"
Write-Host ""
Write-Host "A debug APK is signed with the debug key and installs directly:"
Write-Host "  app/src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk"
