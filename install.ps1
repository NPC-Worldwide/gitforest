$ErrorActionPreference = "Stop"
$Bucket = "https://storage.googleapis.com/gitforest-executables"
$Platform = "windows-x64"
$Asset = "gitforest-windows-x64.zip"
$InstallDir = Join-Path $env:LOCALAPPDATA "gitforest"
$BinDir = Join-Path $InstallDir "gitforest\bin"

$Latest = "latest"
try {
    $Manifest = Invoke-RestMethod -Uri "$Bucket/manifest.json" -TimeoutSec 10
    if ($Manifest.version) { $Latest = $Manifest.version }
} catch {}

Write-Host "Installing gitforest $Latest for windows/x64..."
$Tmp = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $Tmp | Out-Null
try {
    $ZipPath = Join-Path $Tmp $Asset
    Invoke-WebRequest -Uri "$Bucket/$Platform/$Asset" -OutFile $ZipPath
    if (Test-Path $InstallDir) { Remove-Item -Recurse -Force $InstallDir }
    New-Item -ItemType Directory -Path $InstallDir | Out-Null
    Expand-Archive -Path $ZipPath -DestinationPath $InstallDir -Force
    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($UserPath -notlike "*$BinDir*") {
        [Environment]::SetEnvironmentVariable("Path", "$UserPath;$BinDir", "User")
        Write-Host "Added $BinDir to your user PATH. Restart your shell."
    }
} finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}

Write-Host "gitforest $Latest installed."
Write-Host "Run: gitforest --help"
Write-Host "Alias: gtfo --help"
