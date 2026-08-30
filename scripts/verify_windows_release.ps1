param(
    [Parameter(Mandatory = $true)]
    [string]$ClientZip,

    [Parameter(Mandatory = $true)]
    [string]$DaemonZip,

    [Parameter(Mandatory = $true)]
    [string]$Installer
)

$ErrorActionPreference = "Stop"
$temporaryDirectory = Join-Path ([System.IO.Path]::GetTempPath()) ("cterm-package-" + [guid]::NewGuid())

function Assert-File {
    param([string]$Path)

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "Release package contract: missing file $Path"
    }
    if ((Get-Item -LiteralPath $Path).Length -eq 0) {
        throw "Release package contract: empty file $Path"
    }
}

function Assert-Licenses {
    param([string]$Root)

    Assert-File (Join-Path $Root "LICENSE")
    Assert-File (Join-Path $Root "THIRD_PARTY_LICENSES.md")
    Assert-File (Join-Path $Root "LICENSES/KARPELESLAB-CTERM-MIT.txt")
}

function Assert-Checksum {
    param([string]$Asset)

    $sidecar = "$Asset.sha256"
    Assert-File $sidecar
    $line = (Get-Content -LiteralPath $sidecar -Raw).Trim()
    if ($line -notmatch '^([0-9a-fA-F]{64})\s+\*?(.+)$') {
        throw "Release package contract: malformed checksum sidecar $sidecar"
    }
    if ($Matches[2] -ne (Split-Path -Leaf $Asset)) {
        throw "Release package contract: checksum filename does not match $(Split-Path -Leaf $Asset)"
    }
    $actual = (Get-FileHash -LiteralPath $Asset -Algorithm SHA256).Hash
    if ($actual -ne $Matches[1]) {
        throw "Release package contract: SHA-256 mismatch for $Asset"
    }
}

try {
    New-Item -ItemType Directory -Path $temporaryDirectory | Out-Null

    $clientExtract = Join-Path $temporaryDirectory "client"
    $daemonExtract = Join-Path $temporaryDirectory "daemon"
    Expand-Archive -LiteralPath $ClientZip -DestinationPath $clientExtract
    Expand-Archive -LiteralPath $DaemonZip -DestinationPath $daemonExtract

    $clientRoot = Join-Path $clientExtract "cterm-windows-x86_64"
    Assert-File (Join-Path $clientRoot "cterm.exe")
    Assert-File (Join-Path $clientRoot "ctermd.exe")
    Assert-File (Join-Path $clientRoot "README.md")
    Assert-Licenses $clientRoot

    $daemonRoot = Join-Path $daemonExtract "ctermd-windows-x86_64"
    Assert-File (Join-Path $daemonRoot "ctermd.exe")
    Assert-File (Join-Path $daemonRoot "README.md")
    Assert-Licenses $daemonRoot

    Assert-File $Installer
    $sevenZip = (Get-Command 7z.exe).Source
    $installerListing = (& $sevenZip l -slt $Installer | Out-String)
    if ($LASTEXITCODE -ne 0) {
        throw "Release package contract: 7-Zip could not inspect $Installer"
    }
    foreach ($requiredName in @("cterm.exe", "ctermd.exe", "THIRD_PARTY_LICENSES.md", "KARPELESLAB-CTERM-MIT.txt")) {
        if ($installerListing -notmatch [regex]::Escape($requiredName)) {
            throw "Release package contract: installer does not contain $requiredName"
        }
    }

    foreach ($asset in @($ClientZip, $DaemonZip, $Installer)) {
        Assert-Checksum $asset
    }

    Write-Host "Release package contract: Windows archives and installer are complete"
}
finally {
    if (Test-Path -LiteralPath $temporaryDirectory) {
        Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force
    }
}
