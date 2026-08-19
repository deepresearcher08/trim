# trim Windows Installer Script
$ErrorActionPreference = "Stop"

$Repo = "deepresearcher08/trim"
$Target = "x86_64-pc-windows-msvc"
$Asset = "trim-$Target.zip"
$Url = "https://github.com/$Repo/releases/latest/download/$Asset"
$InstallDir = "$env:LOCALAPPDATA\trim\bin"

Write-Host "Downloading trim for Windows ($Target)..." -ForegroundColor Cyan

$TempZip = Join-Path $env:TEMP $Asset
Invoke-WebRequest -Uri $Url -OutFile $TempZip

if (-not (Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
}

Write-Host "Extracting to $InstallDir..." -ForegroundColor Cyan
Expand-Archive -Path $TempZip -DestinationPath $InstallDir -Force
Remove-Item $TempZip -Force

# Add to user PATH if not already present
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($UserPath -notlike "*$InstallDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$UserPath;$InstallDir", "User")
    Write-Host "Added $InstallDir to user PATH." -ForegroundColor Green
}

Write-Host "Successfully installed trim to $InstallDir\trim.exe!" -ForegroundColor Green
Write-Host "Restart your terminal or run: trim --help" -ForegroundColor Yellow
