param([string]$Version = '0.1.0', [string]$Executable)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$exe = if ($Executable) { (Resolve-Path -LiteralPath $Executable).Path } else { Join-Path $repo 'target/x86_64-pc-windows-msvc/release/HaloContinued.exe' }
if (!(Test-Path -LiteralPath $exe)) { throw 'Run tools/build-rust.ps1 first.' }
if ((Get-Item -LiteralPath $exe).Length -gt 10MB) { throw 'Executable exceeds the 10 MiB engineering target.' }
if ($env:USERPROFILE) {
  $binaryText = [Text.Encoding]::Latin1.GetString([IO.File]::ReadAllBytes($exe))
  if ($binaryText.Contains($env:USERPROFILE)) {
    throw 'Executable contains a local build profile path. Rebuild with tools/build-rust.ps1.'
  }
}
$output = Join-Path $repo "dist/HaloContinued-$Version-windows-x64"
[IO.Directory]::CreateDirectory($output) | Out-Null
Copy-Item -LiteralPath $exe -Destination $output
Copy-Item -LiteralPath (Join-Path $repo 'LICENSE') -Destination (Join-Path $output 'LICENSE-HaloBattery.txt')
# Ship the same consumer README as the repository, retaining the upstream license name.
$guide = Get-Content -LiteralPath (Join-Path $repo 'README.md') -Raw
$guide = $guide.Replace('](LICENSE)', '](LICENSE-HaloBattery.txt)')
$guide | Set-Content -LiteralPath (Join-Path $output 'README.md') -Encoding utf8
# The consumer README includes protocol credits; packaging needs no docs tree.
$notices = Join-Path $output 'licenses'
[IO.Directory]::CreateDirectory($notices) | Out-Null
Push-Location $repo
try {
  $metadata = cargo metadata --locked --format-version 1 | ConvertFrom-Json
  if ($LASTEXITCODE -ne 0) { throw 'Cargo metadata failed.' }
  $summary = @('Halo Continued third-party notices', '', 'HIDAPI, SQLite and the MSVC CRT are linked into the portable executable.', 'SQLite is in the public domain: https://sqlite.org/copyright.html', 'Protocol credits are in README.md; the MIT license is supplied alongside this file.', '')
  foreach ($package in $metadata.packages | Where-Object source) {
    $summary += "$($package.name) $($package.version): $($package.license)"
    $folder = Split-Path $package.manifest_path
    $licenseFiles = Get-ChildItem -LiteralPath $folder -File | Where-Object { $_.Name -match '^(LICENSE|LICENCE|COPYING|NOTICE)' }
    foreach ($file in $licenseFiles) {
      Copy-Item -LiteralPath $file.FullName -Destination (Join-Path $notices "$($package.name)-$($package.version)-$($file.Name)")
    }
    # HIDAPI's original C-backend license is nested under the crate.
    if ($package.name -eq 'hidapi') {
      foreach ($file in Get-ChildItem -LiteralPath (Join-Path $folder 'etc/hidapi') -Filter 'LICENSE*' -File) {
        Copy-Item -LiteralPath $file.FullName -Destination (Join-Path $notices "hidapi-C-$($file.Name)")
      }
    }
  }
  $summary | Set-Content -LiteralPath (Join-Path $output 'THIRD-PARTY-NOTICES.txt') -Encoding utf8
  $hash = (Get-FileHash -LiteralPath (Join-Path $output 'HaloContinued.exe') -Algorithm SHA256).Hash
  "$hash  HaloContinued.exe" | Set-Content -LiteralPath (Join-Path $output 'SHA256SUMS.txt')
  Compress-Archive -Path (Join-Path $output '*') -DestinationPath "$output.zip" -Force
  Get-Item -LiteralPath "$output.zip" | Select-Object FullName,Length
} finally { Pop-Location }
