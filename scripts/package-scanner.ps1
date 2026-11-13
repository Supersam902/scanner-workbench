[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$Binary,
    [string]$ExpectedVersion,
    [string]$OutputDirectory = (Join-Path $PSScriptRoot '../dist'),
    [ValidatePattern('^[a-zA-Z0-9][a-zA-Z0-9._-]*$')]
    [string]$ArchiveName = 'scanner-workbench-mfc3-windows-x86',
    [ValidatePattern('^[a-zA-Z0-9][a-zA-Z0-9._-]*$')]
    [string]$FolderName = 'Scanner-Workbench',
    [ValidatePattern('^[a-zA-Z0-9][a-zA-Z0-9._-]*\.exe$')]
    [string]$ExecutableName = 'scanner-workbench.exe'
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
$binaryPath = (Get-Item -LiteralPath $Binary).FullName
$bytes = [IO.File]::ReadAllBytes($binaryPath)
if ($bytes.Length -lt 0x40 -or [BitConverter]::ToUInt16($bytes, 0) -ne 0x5a4d) {
    throw 'Expected a Windows executable.'
}
$pe = [BitConverter]::ToInt32($bytes, 0x3c)
if ($pe -lt 0 -or $pe -gt $bytes.Length - 94 -or
    [BitConverter]::ToUInt32($bytes, $pe) -ne 0x4550 -or
    [BitConverter]::ToUInt16($bytes, $pe + 4) -ne 0x14c -or
    [BitConverter]::ToUInt16($bytes, $pe + 92) -ne 2) {
    throw 'Expected a 32-bit Windows GUI executable for the MFC3.'
}
if ($ExpectedVersion -and [Diagnostics.FileVersionInfo]::GetVersionInfo($binaryPath).ProductVersion -cne $ExpectedVersion) {
    throw "The scanner executable does not identify itself as version $ExpectedVersion."
}

$output = New-Item -ItemType Directory -Path $OutputDirectory -Force
$stage = Join-Path $output.FullName $FolderName
if (Test-Path -LiteralPath $stage) {
    throw "Choose an empty destination for $FolderName."
}
$codes = Join-Path $stage 'dtc_codes'
$obdb = Join-Path $stage 'obdb-data'
New-Item -ItemType Directory -Path $codes,$obdb -Force | Out-Null
Copy-Item -LiteralPath $binaryPath -Destination (Join-Path $stage $ExecutableName)
Copy-Item -LiteralPath (Join-Path $repo 'LICENSE') -Destination $stage
Copy-Item -LiteralPath (Join-Path $repo 'assets/fonts/OFL-Barlow.txt') -Destination $stage
$guide = Get-Content -LiteralPath (Join-Path $repo 'docs/scanner-workbench.txt') -Raw
if ($ExpectedVersion) {
    $build = if ($ExpectedVersion -eq 'development') { 'development build' } else { "version $ExpectedVersion" }
    $guide = $guide -replace '(?m)^Windows 32-bit build,[^\r\n]*', "Windows 32-bit $build"
}
Set-Content -LiteralPath (Join-Path $stage 'START HERE.txt') -Value $guide -Encoding utf8 -NoNewline
Get-ChildItem -LiteralPath (Join-Path $repo 'dtc_codes') -Filter '*.json' -File |
    Copy-Item -Destination $codes
if (!(Test-Path -LiteralPath (Join-Path $codes '_generic.json'))) {
    throw 'Missing generic trouble-code definitions.'
}
foreach ($name in @('saej1979.json', 'LICENSE-CC-BY-SA-4.0.txt', 'ATTRIBUTION.txt')) {
    Copy-Item -LiteralPath (Join-Path $repo "data/obdb/$name") -Destination $obdb
}

$zip = Join-Path $output.FullName "$ArchiveName.zip"
Compress-Archive -LiteralPath $stage -DestinationPath $zip -Force
$archive = [IO.Compression.ZipFile]::OpenRead($zip)
try {
    foreach ($file in Get-ChildItem -LiteralPath $stage -File -Recurse) {
        $relative = [IO.Path]::GetRelativePath($stage, $file.FullName).Replace('\', '/')
        $entry = $archive.GetEntry("$FolderName/$relative")
        if (!$entry) { throw "Missing archive entry: $relative" }
        $stream = $entry.Open()
        try { $hash = (Get-FileHash -InputStream $stream -Algorithm SHA256).Hash }
        finally { $stream.Dispose() }
        if ($hash -ne (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash) {
            throw "Archive content mismatch: $relative"
        }
    }
} finally {
    $archive.Dispose()
}
$digest = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
Set-Content -LiteralPath "$zip.sha256" -Value "$digest  $ArchiveName.zip`n" -NoNewline -Encoding ascii
Get-Item -LiteralPath $zip,"$zip.sha256"
