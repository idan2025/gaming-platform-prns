# Fetch WireGuard's signed wintun.dll, digest-checked, into a directory
# (`PLAN.md` §14 step 4): the Windows room adapter loads it from beside
# lan-helper.exe, never by search order.
#
# Shipped unmodified, with its license beside it, as the prebuilt-binaries
# license in the zip allows for software that uses only the wintun.h API.
#
# Usage: scripts/fetch-wintun.ps1 <dest-dir>
#   writes <dest-dir>/wintun.dll and <dest-dir>/wintun-LICENSE.txt
param([Parameter(Mandatory = $true)][string]$Dest)
$ErrorActionPreference = 'Stop'

# Pinned: the digest decides, never the URL. Bump both together.
$version = '0.14.1'
$sha256 = '07c256185d6ee3652e09fa55c0b673e2624b565e02c4b9091c79ca7d2f24ef51'

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) "wintun-$([guid]::NewGuid())"
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
$zip = Join-Path $tmp 'wintun.zip'
Invoke-WebRequest -Uri "https://www.wintun.net/builds/wintun-$version.zip" -OutFile $zip
$hash = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
if ($hash -ne $sha256) { throw "wintun-$version.zip digest mismatch: $hash" }
Expand-Archive -Path $zip -DestinationPath $tmp
New-Item -ItemType Directory -Force -Path $Dest | Out-Null
Copy-Item (Join-Path $tmp 'wintun\bin\amd64\wintun.dll') (Join-Path $Dest 'wintun.dll')
Copy-Item (Join-Path $tmp 'wintun\LICENSE.txt') (Join-Path $Dest 'wintun-LICENSE.txt')
Remove-Item -Recurse -Force $tmp
Write-Output "wintun $version -> $Dest"
