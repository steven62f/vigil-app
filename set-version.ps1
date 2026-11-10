param(
    [Parameter(Mandatory)]
    [ValidatePattern('^\d+\.\d+\.\d+$')]
    [string]$Version
)

$root = $PSScriptRoot

# package.json
$pkg = Get-Content "$root\package.json" -Raw
$pkg = $pkg -replace '"version":\s*"[^"]*"', "`"version`": `"$Version`""
[System.IO.File]::WriteAllText("$root\package.json", $pkg, [System.Text.UTF8Encoding]::new($false))

# src-tauri/Cargo.toml
$cargo = Get-Content "$root\src-tauri\Cargo.toml" -Raw
$cargo = [regex]::Replace($cargo, '(?m)^version = "[^"]*"', "version = `"$Version`"", 1)
[System.IO.File]::WriteAllText("$root\src-tauri\Cargo.toml", $cargo, [System.Text.UTF8Encoding]::new($false))

# src-tauri/tauri.conf.json
$tauri = Get-Content "$root\src-tauri\tauri.conf.json" -Raw
$tauri = $tauri -replace '"version":\s*"[^"]*"', "`"version`": `"$Version`""
[System.IO.File]::WriteAllText("$root\src-tauri\tauri.conf.json", $tauri, [System.Text.UTF8Encoding]::new($false))

Write-Host "Version mise a jour : $Version"
Write-Host "  package.json"
Write-Host "  src-tauri/Cargo.toml"
Write-Host "  src-tauri/tauri.conf.json"
