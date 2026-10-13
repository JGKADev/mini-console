# Builds the release exe and installs it for the current user:
#   %LOCALAPPDATA%\Programs\Mini Console\mini-console.exe, plus a Start menu shortcut.
# No admin rights needed. "Start with Windows" is switched on from the app (Device page).
# Needs Rust (https://rustup.rs, MSVC toolchain) on PATH.
$ErrorActionPreference = 'Stop'
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "cargo not found. Install Rust from https://rustup.rs (MSVC toolchain), then run this again."
}

# Keep the builder's folder names (user name, Rust install, checkout path) out of the exe's panic
# messages. When several prefixes match, the last one wins. (The encoded form is separated by 0x1f,
# so paths with spaces work.)
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$env:CARGO_ENCODED_RUSTFLAGS = @(
    "--remap-path-prefix=$env:USERPROFILE=~"
    "--remap-path-prefix=$(Join-Path $cargoHome 'registry\src')=crates"
    "--remap-path-prefix=$PSScriptRoot=."
) -join [char]0x1f

Push-Location $PSScriptRoot
try {
    # cargo reports progress on stderr, which Windows PowerShell would treat as an error
    $ErrorActionPreference = 'Continue'
    cargo build --release 2>&1 | ForEach-Object { "$_" }
    $ErrorActionPreference = 'Stop'
    if ($LASTEXITCODE -ne 0) { throw "build failed" }
} finally { Pop-Location }

$dest = Join-Path $env:LOCALAPPDATA 'Programs\Mini Console'
New-Item -ItemType Directory -Force $dest | Out-Null
$exe = Join-Path $dest 'mini-console.exe'
if (Get-Process mini-console -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe }) {
    throw "The installed app is running. Close it first, then run this again."
}
Copy-Item (Join-Path $PSScriptRoot 'target\release\mini-console.exe') $exe -Force

$shell = New-Object -ComObject WScript.Shell
$lnk = $shell.CreateShortcut((Join-Path ([Environment]::GetFolderPath('Programs')) 'Mini Console.lnk'))
$lnk.TargetPath = $exe
$lnk.WorkingDirectory = $dest
$lnk.Description = 'Mini Console'
$lnk.Save()
Write-Output "Installed $exe and a Start menu shortcut."
