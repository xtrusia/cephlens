#requires -Version 5.1
$ErrorActionPreference = "Stop"

$installerUrl = $env:CEPHLENS_INSTALLER_URL
if ([string]::IsNullOrWhiteSpace($installerUrl)) {
    $installerUrl = "https://github.com/xtrusia/cephlens/releases/latest/download/cephlens-installer.ps1"
}

$tmpFile = Join-Path ([System.IO.Path]::GetTempPath()) ("cephlens-installer-{0}.ps1" -f [System.Guid]::NewGuid().ToString("N"))

try {
    Invoke-WebRequest -UseBasicParsing -Uri $installerUrl -OutFile $tmpFile
    & $tmpFile @args
} finally {
    Remove-Item -LiteralPath $tmpFile -Force -ErrorAction SilentlyContinue
}
