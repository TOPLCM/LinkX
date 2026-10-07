# Verifies that the shipping MSI really carries the "close the running LinkX before removing
# it" guard. Missing it produces no error at all: the uninstall still clears the registry and
# the firewall rule and still returns success, while the body keeps running with stale
# pairing state. Checks the action row, its DLL in the Binary table, and that it is ordered
# BEFORE RemoveFiles (the extension's own default lands at 3999, i.e. after the removal).
#
# Keep this file ASCII: Windows PowerShell parses .ps1 with the ambient ANSI codepage (GBK on
# the maintainer machine), so non-ASCII literals break the parser, not just the output.
param([Parameter(Mandatory = $true)][string]$Msi)

$ErrorActionPreference = 'Stop'
$installer = New-Object -ComObject WindowsInstaller.Installer
$db = $installer.GetType().InvokeMember('OpenDatabase', 'InvokeMethod', $null, $installer, @($Msi, 0))

function Rows($sql, $cols) {
    $view = $db.GetType().InvokeMember('OpenView', 'InvokeMethod', $null, $db, @($sql))
    $null = $view.GetType().InvokeMember('Execute', 'InvokeMethod', $null, $view, $null)
    $out = @()
    while ($true) {
        $rec = $view.GetType().InvokeMember('Fetch', 'InvokeMethod', $null, $view, $null)
        if ($null -eq $rec) { break }
        $cells = @()
        for ($i = 1; $i -le $cols; $i++) {
            # StringData is a property on Record (get-only); invoking it as a method fails with
            # DISP_E_MEMBERNOTFOUND, which looks like "the table is missing" but is not.
            $cells += [string]$rec.GetType().InvokeMember('StringData', 'GetProperty', $null, $rec, @($i))
        }
        $out += , $cells
    }
    $null = $view.GetType().InvokeMember('Close', 'InvokeMethod', $null, $view, $null)
    return $out
}

$fail = 0

$ca = Rows 'SELECT `Action`,`Type`,`Source`,`Target` FROM `CustomAction`' 4
$close = @($ca | Where-Object { $_[0] -like 'Wix4CloseApplications_*' -and $_[0] -notlike '*Deferred*' })
if ($close.Count -eq 0) {
    Write-Output 'FAIL no Wix4CloseApplications_* custom action: CloseApplication did not make it into the package'
    $fail = 1
} else {
    Write-Output ('OK   action ' + $close[0][0] + ' type=' + $close[0][1] + ' dll=' + $close[0][2] + ' entry=' + $close[0][3])
    $binaries = @(Rows 'SELECT `Name` FROM `Binary`' 1 | ForEach-Object { $_[0] })
    if ($binaries -notcontains $close[0][2]) {
        Write-Output ('FAIL Binary table has no ' + $close[0][2] + ': the action cannot run')
        $fail = 1
    } else {
        Write-Output ('OK   Binary carries ' + $close[0][2])
    }
}

$seq = Rows 'SELECT `Action`,`Sequence` FROM `InstallExecuteSequence`' 2
$rm = @($seq | Where-Object { $_[0] -eq 'RemoveFiles' })
$cl = @($seq | Where-Object { $_[0] -like 'Wix4CloseApplications_*' -and $_[0] -notlike '*Deferred*' })
if ($rm.Count -eq 0) {
    Write-Output 'FAIL InstallExecuteSequence has no RemoveFiles row (table damaged?)'
    $fail = 1
} elseif ($cl.Count -eq 0) {
    Write-Output 'FAIL InstallExecuteSequence has no CloseApplications row'
    $fail = 1
} else {
    $a = [int]$cl[0][1]
    $b = [int]$rm[0][1]
    if ($a -ge $b) {
        Write-Output ('FAIL wrong order: CloseApplications=' + $a + ' is not before RemoveFiles=' + $b)
        $fail = 1
    } else {
        Write-Output ('OK   order ' + $a + ' < ' + $b + ' (app is closed before costing and removal)')
    }
}

$files = @(Rows 'SELECT `File`,`FileName` FROM `File`' 2)
$exe = @($files | Where-Object { $_[1] -like 'linkx.exe*' -or $_[0] -like '*MainExecutable*' })
if ($exe.Count -eq 0) {
    Write-Output 'FAIL File table has no linkx.exe: Target="linkx.exe" would match nothing'
    $fail = 1
} else {
    Write-Output ('OK   File table body: ' + $exe[0][0] + ' -> ' + $exe[0][1])
}

# The graceful WM_CLOSE only works on a build that treats a bare WM_CLOSE as "the installer asked
# me to quit". Builds already installed out there treat it as "user pressed X" and pop a confirm
# dialog, so Restart Manager gives up and Windows Installer shows its own "applications should be
# closed" dialog - measured, and it stalls the upgrade until someone clicks. The `taskkill /F`
# fallback right behind it is what makes the outcome deterministic, so a silent drop of it is a
# regression of the very defect this guard exists to close.
$kill = @($ca | Where-Object { $_[0] -eq 'KillLinkX' })
if ($kill.Count -eq 0) {
    Write-Output 'FAIL no KillLinkX custom action: the force-kill fallback is gone'
    $fail = 1
} else {
    if ($kill[0][2] -ne 'TASKKILL_EXE') {
        Write-Output ('FAIL KillLinkX Source should be the expanded path property TASKKILL_EXE, got ' + $kill[0][2])
        $fail = 1
    }
    $cmd = @(Rows 'SELECT `Property`,`Value` FROM `Property`' 2)
    $setter = @($ca | Where-Object { $_[0] -eq 'SetTaskkill' })
    if ($setter.Count -eq 0) {
        Write-Output 'FAIL no SetTaskkill (type 51): TASKKILL_EXE would stay an unexpanded [SystemFolder] path'
        $fail = 1
    }
    if ($kill[0][3] -notmatch '/IM linkx\.exe' -or $kill[0][3] -notmatch '/F') {
        Write-Output ('FAIL KillLinkX command line is not a force kill by image name: ' + $kill[0][3])
        $fail = 1
    } else {
        Write-Output ('OK   fallback kills by image name: ' + $kill[0][3])
    }
}

$seqAll = Rows 'SELECT `Action`,`Sequence` FROM `InstallExecuteSequence`' 2
$validate = @($seqAll | Where-Object { $_[0] -eq 'InstallValidate' })
$killSeq = @($seqAll | Where-Object { $_[0] -eq 'KillLinkX' })
$setSeq = @($seqAll | Where-Object { $_[0] -eq 'SetTaskkill' })
if ($validate.Count -eq 1 -and $killSeq.Count -eq 1 -and $cl.Count -eq 1) {
    $v = [int]$validate[0][1]
    $k = [int]$killSeq[0][1]
    $c = [int]$cl[0][1]
    if ($k -ge $v -or $c -ge $k) {
        Write-Output ('FAIL wrong order: CloseApplications=' + $c + ' KillLinkX=' + $k + ' InstallValidate=' + $v)
        $fail = 1
    } elseif ($setSeq.Count -ne 1 -or [int]$setSeq[0][1] -ge $k) {
        Write-Output ('FAIL SetTaskkill must run before KillLinkX, got ' + (($setSeq | ForEach-Object { $_[1] }) -join ','))
        $fail = 1
    } else {
        Write-Output ('OK   fallback ordered ' + $c + ' < ' + [int]$setSeq[0][1] + ' < ' + $k + ' < ' + $v)
    }
}

if ($fail) { Write-Output 'RESULT: not ok'; exit 1 }
Write-Output 'RESULT: the pre-uninstall close guard is in place'
