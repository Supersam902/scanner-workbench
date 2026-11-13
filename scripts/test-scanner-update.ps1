[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$OldPortableZip,
    [Parameter(Mandatory)]
    [string]$NewFeed,
    [Parameter(Mandatory)]
    [string]$OutputDirectory
)

$ErrorActionPreference = 'Stop'
$test = New-Item -ItemType Directory -Path $OutputDirectory
$install = Join-Path $test.FullName 'installed'
Expand-Archive -LiteralPath $OldPortableZip -DestinationPath $install
$exe = Join-Path $install 'current/scanner-workbench.exe'
$feedPath = (Resolve-Path -LiteralPath $NewFeed).Path
$feedName = 'releases.win-x86.json'
$assets = (Get-Content -LiteralPath (Join-Path $feedPath $feedName) -Raw | ConvertFrom-Json).Assets
$asset = $assets | Sort-Object { [version]$_.Version } | Select-Object -Last 1
$before = [Diagnostics.FileVersionInfo]::GetVersionInfo($exe).ProductVersion
if ([version]$asset.Version -le [version]$before) { throw 'The test feed needs a newer version.' }

$badFeed = New-Item -ItemType Directory -Path (Join-Path $test.FullName 'bad-feed')
Copy-Item -LiteralPath (Join-Path $feedPath $feedName) -Destination $badFeed
$shortPackage = Join-Path $badFeed.FullName $asset.FileName
[IO.File]::WriteAllBytes($shortPackage, [byte[]]@(0x50,0x4b))
$result = Join-Path $test.FullName 'result.txt'
$state = New-Item -ItemType Directory -Path (Join-Path $test.FullName 'vehicle-data')
$saved = Join-Path $state.FullName 'settings.json'
Set-Content -LiteralPath $saved -Value '{"favorites":["OBD:010C"]}' -Encoding utf8
$savedHash = (Get-FileHash -LiteralPath $saved).Hash

function Invoke-LocalCheck($Directory, [switch]$Apply) {
    if (Test-Path -LiteralPath $result) { Remove-Item -LiteralPath $result }
    $start = [Diagnostics.ProcessStartInfo]::new($exe)
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.WorkingDirectory = $test.FullName
    $start.ArgumentList.Add('--verify-local-update')
    $start.ArgumentList.Add($Directory)
    $start.ArgumentList.Add($result)
    $start.ArgumentList.Add($state.FullName)
    if ($Apply) { $start.ArgumentList.Add('--apply') }
    $process = [Diagnostics.Process]::Start($start)
    try {
        if (!$process.WaitForExit(60000)) {
            $process.Kill()
            throw "The scanner did not finish the local update check for $Directory."
        }
        $process.ExitCode
    } finally {
        $process.Dispose()
    }
}

if ((Invoke-LocalCheck $badFeed.FullName) -ne 1 -or
    (Get-Content -LiteralPath $result -Raw) -notlike 'Size did not match*') {
    throw 'A truncated package was not rejected.'
}
if ([Diagnostics.FileVersionInfo]::GetVersionInfo($exe).ProductVersion -ne $before) {
    throw 'A failed download changed the running version.'
}
if ((Invoke-LocalCheck $feedPath) -ne 0 -or (Get-Content -LiteralPath $result -Raw) -ne $asset.Version) {
    throw 'A fresh download after the rejected package did not stage.'
}
$cache = Join-Path $install "packages/$($asset.FileName)"
[IO.File]::WriteAllBytes($cache, [byte[]]@(0x50,0x4b))
if ((Invoke-LocalCheck $feedPath) -ne 0 -or (Get-Content -LiteralPath $result -Raw) -ne $asset.Version -or
    (Get-FileHash -LiteralPath $cache -Algorithm SHA256).Hash -ne $asset.SHA256) {
    throw 'A corrupt cached package was not downloaded again.'
}
if ((Invoke-LocalCheck $feedPath -Apply) -ne 0) { throw 'The update could not start.' }
$limit = [DateTime]::UtcNow.AddSeconds(90)
do {
    if (Test-Path -LiteralPath $result) { break }
    Start-Sleep -Milliseconds 200
} while ([DateTime]::UtcNow -lt $limit)

if (!(Test-Path -LiteralPath $result) -or (Get-Content -LiteralPath $result -Raw) -ne $asset.Version -or
    [Diagnostics.FileVersionInfo]::GetVersionInfo($exe).ProductVersion -ne $asset.Version) {
    throw 'The updated scanner did not restart with the expected version.'
}
if ((Get-FileHash -LiteralPath $saved).Hash -ne $savedHash) { throw 'Saved vehicle data changed during the update.' }
"Verified update $before to $($asset.Version), restart, recovery after truncated and corrupt packages, and preserved vehicle data."
