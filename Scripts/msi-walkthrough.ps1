# 0.4.0 MSI real-machine walkthrough (install / custom path / shortcut / uninstall).
#
# Kept ASCII-only on purpose: Windows PowerShell 5.1 reads a .ps1 without a BOM using
# the system code page (this machine: cp936), so UTF-8 Chinese comments get mis-decoded
# and can produce a *parse* error that looks unrelated to anything you wrote.
#
# Why a file at all: passing /i, /qn, /L*v from Git Bash gets mangled by MSYS path
# conversion, and the nested quoting through `powershell -Command` is fragile.
param(
    [string]$Msi = "E:\LinkX\Release\Windows\LinkX-0.4.0-x64.msi",
    [string]$InstallDir = "D:\LinkX",
    [string]$Log = "E:\LinkX\Temp\msi-upgrade.log"
)

$argList = @(
    '/i', $Msi,
    '/qn',
    '/L*v', $Log,
    "INSTALLFOLDER=$InstallDir",
    'OPT_FIREWALL=1',
    'OPT_STARTMENU=1'
)
$p = Start-Process msiexec.exe -ArgumentList $argList -Wait -PassThru
$code = $p.ExitCode
Write-Output ("msiexec exit=" + $code)

# This script is used as an acceptance gate, so it must fail the way a gate fails.
# Printing "msiexec exit=1603" and returning 0 means every caller reads it as green
# (review round 0.4.0 caught exactly this). 0 and 3010 (reboot required) are the only
# passes; anything else exits non-zero with the real code.
if ($code -ne 0 -and $code -ne 3010) { exit $code }
if (-not (Test-Path (Join-Path $InstallDir 'linkx.exe'))) {
    Write-Output ("FAIL: no linkx.exe under " + $InstallDir)
    exit 9001
}
Write-Output ("OK: installed at " + $InstallDir)
exit 0
