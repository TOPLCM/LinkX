# Verify the exe actually shipped INSIDE an MSI: same bytes as the freshly built one, and the
# PE machine matches the package's own architecture. Table-level gates only look at installer
# metadata, so a package that names itself arm64 while carrying an x64 binary passes all of them
# (that is exactly what happened to the 0.5.3 arm64 build). Run by Scripts/build-msi-windows.sh.
param(
    [Parameter(Mandatory = $true)][string]$Msi,
    [Parameter(Mandatory = $true)][string]$Exe,
    [Parameter(Mandatory = $true)][string]$Arch
)
$ErrorActionPreference = 'Stop'
$fail = 0

# Validate the arch BEFORE the switch: Write-Output inside a switch that is being assigned to a
# variable goes into that variable, so the failure reason would be swallowed and $fail left 0.
if ($Arch -notin @('x64', 'arm64')) { throw "unknown -Arch $Arch (expect x64 or arm64)" }
$expectedMachine = switch ($Arch) {
    'x64' { 0x8664 }
    'arm64' { 0xAA64 }
}

function Get-PeMachine([string]$path) {
    $fs = [System.IO.File]::OpenRead($path)
    try {
        $br = New-Object System.IO.BinaryReader($fs)
        if ($br.ReadUInt16() -ne 0x5A4D) { throw "not a PE file: $path" }
        $fs.Position = 0x3C
        $peOff = $br.ReadUInt32()
        $fs.Position = $peOff + 4
        return $br.ReadUInt16()
    } finally {
        $fs.Dispose()
    }
}

# Administrative extract: the supported way to pull payload files out without touching the system.
$out = Join-Path $env:TEMP ("linkx-payload-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Force -Path $out | Out-Null
try {
    $log = Join-Path $out 'extract.log'
    $proc = Start-Process -FilePath msiexec.exe `
        -ArgumentList @('/a', "`"$Msi`"", "targetdir=`"$out`"", '/qn', "/L*v `"$log`"") `
        -Wait -PassThru
    if ($proc.ExitCode -ne 0 -and $proc.ExitCode -ne 3010) {
        Write-Output "FAIL: msiexec /a exited with $($proc.ExitCode) (see $log)"
        exit 1
    }
    $found = @(Get-ChildItem -Path $out -Recurse -Filter 'linkx.exe' -ErrorAction SilentlyContinue)
    if ($found.Count -eq 0) {
        Write-Output 'FAIL: no linkx.exe inside the package (administrative extract found nothing)'
        exit 1
    }
    if ($found.Count -gt 1) {
        Write-Output "FAIL: $found.Count copies of linkx.exe in one package - payload is ambiguous"
        exit 1
    }
    $packed = $found[0].FullName

    $want = (Get-FileHash -Path $Exe -Algorithm SHA256).Hash
    $got = (Get-FileHash -Path $packed -Algorithm SHA256).Hash
    if ($want -ne $got) {
        Write-Output "FAIL: packaged exe is not the exe we just built"
        Write-Output "      built   : $Exe  $want"
        Write-Output "      packaged: $packed  $got"
        $fail = 1
    }
    $machine = Get-PeMachine $packed
    if ($machine -ne $expectedMachine) {
        Write-Output ("FAIL: packaged exe PE machine is 0x{0:X4}, package says -arch {1} (want 0x{2:X4})" -f $machine, $Arch, $expectedMachine)
        $fail = 1
    }
    if ($fail -eq 0) {
        Write-Output ("  payload ok: linkx.exe {0} B, PE 0x{1:X4}, hash matches the built exe" -f $found[0].Length, $machine)
    }
} finally {
    Remove-Item -Path $out -Recurse -Force -ErrorAction SilentlyContinue
}
exit $fail
