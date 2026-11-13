[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$PackageDirectory,
    [Parameter(Mandatory)]
    [string]$Version,
    [string]$OutputDirectory = (Join-Path $PSScriptRoot '../dist'),
    [string]$Vpk = 'vpk',
    [switch]$ForLocalTest
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
$stage = (Get-Item -LiteralPath $PackageDirectory).FullName
$exe = Join-Path $stage 'scanner-workbench.exe'
$product = [Diagnostics.FileVersionInfo]::GetVersionInfo($exe)
if ($product.ProductVersion -cne $Version -or $Version -notmatch '^\d+\.\d+\.\d+$') {
    throw 'The staged scanner and installer version must match.'
}

$output = New-Item -ItemType Directory -Path $OutputDirectory -Force
$channel = 'win-x86'
$id = 'ScannerWorkbenchApp'
$vpkOptions = @(
    'pack', '--packId', $id, '--packTitle', 'Scanner Workbench',
    '--packVersion', $Version, '--packDir', $stage, '--mainExe', 'scanner-workbench.exe',
    '--outputDir', $output.FullName, '--runtime', $channel, '--channel', $channel,
    '--icon', (Join-Path $repo 'assets/scanner/workbench-icon.ico'), '--delta', 'None'
)
if ($ForLocalTest) {
    $vpkOptions += @('--shortcuts', 'None')
} else {
    $vpkOptions += '--noPortable'
}
& $Vpk @vpkOptions
if ($LASTEXITCODE -ne 0) { throw "Velopack failed to package the scanner ($LASTEXITCODE)." }

$feedPath = Join-Path $output.FullName "releases.$channel.json"
$feed = Get-Content -LiteralPath $feedPath -Raw | ConvertFrom-Json
$name = "$id-$Version-$channel-full.nupkg"
$asset = @($feed.Assets | Where-Object { $_.FileName -ceq $name })
if ($asset.Count -ne 1 -or $asset[0].PackageId -cne $id -or $asset[0].Version -cne $Version -or $asset[0].Type -ne 'Full') {
    throw 'The release feed does not describe the packaged version.'
}
$package = Get-Item -LiteralPath (Join-Path $output.FullName $name)
if ($asset[0].Size -ne $package.Length -or $asset[0].SHA256 -ne (Get-FileHash -LiteralPath $package.FullName -Algorithm SHA256).Hash) {
    throw 'The update package does not match its release feed.'
}
$installer = Get-Item -LiteralPath (Join-Path $output.FullName "$id-$channel-Setup.exe")
Get-Item -LiteralPath $installer.FullName,$package.FullName,$feedPath
