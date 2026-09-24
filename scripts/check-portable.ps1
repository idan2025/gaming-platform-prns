# Prove a portable launcher writes nothing outside its folder on Windows
# (`PLAN.md` §14, step 5b): run it with portable-data beside it, then fail if
# anything of it appeared in %APPDATA% or %LOCALAPPDATA%, or if nothing
# appeared in portable-data (portable mode never switched on, proving nothing).
#
# Usage: scripts/check-portable.ps1 <launcher.exe>
param([Parameter(Mandatory = $true)][string]$Exe)
$ErrorActionPreference = 'Stop'

$exe = (Resolve-Path $Exe).Path
$data = Join-Path (Split-Path $exe) 'portable-data'
$ids = @('org.idan2025.gamingplatformprns.launcher', 'gaming-platform-prns')
$roots = @($env:APPDATA, $env:LOCALAPPDATA)
foreach ($r in $roots) { foreach ($i in $ids) { Remove-Item -Recurse -Force (Join-Path $r $i) -ErrorAction SilentlyContinue } }
Remove-Item -Recurse -Force $data -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $data | Out-Null

$p = Start-Process -FilePath $exe -PassThru
Start-Sleep -Seconds 25
Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
Get-Process -Name msedgewebview2 -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 2

$leaked = @()
foreach ($r in $roots) { foreach ($i in $ids) { $p2 = Join-Path $r $i; if (Test-Path $p2) { $leaked += $p2 } } }
if ($leaked.Count -gt 0) { throw "A portable run wrote outside its folder: $($leaked -join ', ')" }
if (-not (Test-Path (Join-Path $data 'webview'))) {
    throw 'portable-data\webview was never created: portable mode did not switch on, so this proves nothing.'
}
Write-Output 'portable run: nothing outside portable-data; it holds:'
Get-ChildItem $data | ForEach-Object { $_.FullName }
Remove-Item -Recurse -Force $data
