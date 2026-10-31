# Table-level contract checks on the shipping MSI.
#
# These checks used to live on the Linux/wixl build path only (msiinfo), which meant the
# Windows build - the one that actually ships - ran NONE of them. The Linux path is gone
# now, so the invariants are checked here instead, through the Windows Installer COM API
# that ships with Windows (no extra tool to install, no msidump on the machine needed).
#
# What is checked:
#   1. every table the installer relies on is present (a damaged MSI installs but does nothing);
#   2. the package is a per-user install (no ALLUSERS) and the install directory is mounted
#      under %LOCALAPPDATA%\Programs, not under Program Files;
#   3. the uninstall entries exist (RemoveInstallFolder / RemoveProgramMenuDir), otherwise the
#      uninstall reports success and leaves the files behind;
#   4. the firewall custom actions are present with the exact netsh command lines, plus the
#      NETSH_EXE property they depend on (Source of a type-50 action is not expanded twice,
#      so a literal "[SystemFolder]netsh.exe" would make the rule silently fail);
#   5. the product version matches the version this build was made from;
#   6. the license page really is a gate (two ControlCondition rows plus a
#      conditional page transition), not a page that merely shows a text;
#   7. the license texts that must travel with the product are in the File table.
#
# Keep this file ASCII: Windows PowerShell parses .ps1 with the ambient ANSI codepage (GBK on
# the maintainer machine), so non-ASCII literals break the parser, not just the output.
param(
  [Parameter(Mandatory = $true)][string]$Msi,
  [string]$Version = ''
)

$ErrorActionPreference = 'Stop'
$installer = New-Object -ComObject WindowsInstaller.Installer
$db = $installer.GetType().InvokeMember('OpenDatabase', 'InvokeMethod', $null, $installer, @($Msi, 0))

function Rows($sql, $cols) {
    # A package that never needed these tables simply has no such table, and OpenView throws.
    # Swallow that into "no rows" so the caller prints the FAIL line that names the missing
    # invariant instead of dying with a COM exception nobody can read.
    try {
        $view = $db.GetType().InvokeMember('OpenView', 'InvokeMethod', $null, $db, @($sql))
    } catch { return @() }
    $null = $view.GetType().InvokeMember('Execute', 'InvokeMethod', $null, $view, $null)
    $out = @()
    while ($true) {
        $rec = $view.GetType().InvokeMember('Fetch', 'InvokeMethod', $null, $view, $null)
        if ($null -eq $rec) { break }
        $cells = @()
        for ($i = 1; $i -le $cols; $i++) {
            $cells += [string]$rec.GetType().InvokeMember('StringData', 'GetProperty', $null, $rec, @($i))
        }
        $out += , $cells
    }
    $null = $view.GetType().InvokeMember('Close', 'InvokeMethod', $null, $view, $null)
    return $out
}

function Has-Table($name) {
    # The _Tables view lists every table in the package.
    $t = @(Rows 'SELECT `Name` FROM `_Tables`' 1 | ForEach-Object { $_[0] })
    return ($t -contains $name)
}

$fail = 0

# ---- 1. required tables -------------------------------------------------
$required = @('File', 'Directory', 'Component', 'Feature', 'Media', 'Property', 'Registry',
  'RemoveFile', 'CustomAction', 'Shortcut', 'Dialog', 'Control', 'ControlEvent',
  'ControlCondition', 'InstallExecuteSequence', 'InstallUISequence', 'Upgrade', 'Icon', 'Binary')
$missing = @()
foreach ($t in $required) { if (-not (Has-Table $t)) { $missing += $t } }
if ($missing.Count -gt 0) {
    Write-Output ('FAIL missing tables: ' + ($missing -join ', '))
    $fail = 1
} else {
    Write-Output ('OK   ' + $required.Count + ' required tables present')
}

# ---- 2. per-user install, directory mounted under LocalAppData -----------
$props = @(Rows 'SELECT `Property`,`Value` FROM `Property`' 2)
$propMap = @{}
foreach ($p in $props) { $propMap[$p[0]] = $p[1] }
if ($propMap.ContainsKey('ALLUSERS')) {
    Write-Output 'FAIL the package sets ALLUSERS: this is not a per-user install'
    $fail = 1
} else {
    Write-Output 'OK   no ALLUSERS: per-user install'
}

$dirs = @(Rows 'SELECT `Directory`,`Directory_Parent`,`DefaultDir` FROM `Directory`' 3)
$dirMap = @{}
foreach ($d in $dirs) { $dirMap[$d[0]] = $d[1] }
if (-not $dirMap.ContainsKey('INSTALLFOLDER')) {
    Write-Output 'FAIL the Directory table has no INSTALLFOLDER row'
    $fail = 1
} else {
    # Walk up the parent chain: INSTALLFOLDER must end at LocalAppDataFolder, i.e. the
    # program lands in the user's own profile, never in Program Files.
    $chain = @('INSTALLFOLDER')
    $cur = 'INSTALLFOLDER'
    $guard = 0
    while ($dirMap.ContainsKey($cur) -and $guard -lt 16) {
        $cur = $dirMap[$cur]
        if (-not $cur) { break }
        $chain += $cur
        $guard++
    }
    if ($chain -contains 'LocalAppDataFolder') {
        Write-Output ('OK   install directory chain: ' + ($chain -join ' <- '))
    } else {
        Write-Output ('FAIL the install directory is not under LocalAppDataFolder: ' + ($chain -join ' <- '))
        $fail = 1
    }
}

# ---- 3. uninstall entries ------------------------------------------------
# RemoveFile's primary key is FileKey (column 1); FileName (column 3) carries the
# wildcard. The folder-removal rows are keyed by their component name, so look at
# both columns - checking only FileName would report a false failure.
$removeRows = @(Rows 'SELECT `FileKey`,`Component_`,`FileName` FROM `RemoveFile`' 3)
$removeKeys = @($removeRows | ForEach-Object { $_[0] })
$removeNames = @($removeRows | ForEach-Object { $_[2] })
foreach ($want in @('RemoveInstallFolder', 'RemoveProgramMenuDir')) {
    if (($removeKeys -notcontains $want) -and ($removeNames -notcontains $want)) {
        Write-Output ('FAIL RemoveFile has no ' + $want + ': the uninstall would leave it behind')
        $fail = 1
    } else {
        Write-Output ('OK   RemoveFile carries ' + $want)
    }
}

# ---- 4. firewall custom actions ----------------------------------------
# A type-50 action runs Source (the executable) with Target (the arguments); the
# executable is the NETSH_EXE property, so "netsh" shows up in Source and the
# firewall command itself in Target.
$ca = @(Rows 'SELECT `Action`,`Type`,`Source`,`Target` FROM `CustomAction`' 4)
$fwAdd = @($ca | Where-Object { $_[0] -eq 'FwAdd' })
$fwDel = @($ca | Where-Object { $_[0] -eq 'FwDel' })
if ($fwAdd.Count -eq 0 -or $fwDel.Count -eq 0) {
    Write-Output 'FAIL the firewall custom actions (FwAdd/FwDel) are missing'
    $fail = 1
} else {
    Write-Output 'OK   firewall actions FwAdd/FwDel present'
}
foreach ($row in @($fwAdd + $fwDel)) {
    if ($row[3] -notlike '*advfirewall*') {
        Write-Output ('FAIL ' + $row[0] + ' does not run an advfirewall command: ' + $row[3])
        $fail = 1
    }
}
# The delete must be filtered: a bare name="LinkX" would remove every rule with that name.
$fwDelCmd = ''
if ($fwDel.Count -gt 0) { $fwDelCmd = $fwDel[0][3] }
if ($fwDelCmd -notlike '*protocol=TCP*' -or $fwDelCmd -notlike '*localport=55676*' -or $fwDelCmd -notlike '*dir=in*') {
    Write-Output ('FAIL FwDel is not filtered by protocol/localport/dir: ' + $fwDelCmd)
    $fail = 1
} else {
    Write-Output 'OK   FwDel is filtered by protocol/localport/dir'
}
$fwAddCmd = ''
if ($fwAdd.Count -gt 0) { $fwAddCmd = $fwAdd[0][3] }
if ($fwAddCmd -notlike '*name="LinkX"*' -or $fwAddCmd -notlike '*localport=55676*') {
    Write-Output ('FAIL FwAdd does not add the expected rule: ' + $fwAddCmd)
    $fail = 1
} else {
    Write-Output 'OK   FwAdd adds the LinkX rule on TCP 55676'
}
if (-not $propMap.ContainsKey('NETSH_EXE')) {
    Write-Output 'FAIL no NETSH_EXE property: the firewall actions cannot find netsh.exe'
    $fail = 1
} elseif ($propMap['NETSH_EXE'] -notlike '*netsh.exe*') {
    Write-Output ('FAIL NETSH_EXE does not point at netsh.exe: ' + $propMap['NETSH_EXE'])
    $fail = 1
} else {
    Write-Output 'OK   NETSH_EXE points at netsh.exe'
}

# The delete must also run on a fresh install or an upgrade, not only on a real
# uninstall. The product code is generated per build, so every install of a new
# build is an upgrade: the add action runs every time while a delete gated on
# "real uninstall only" never does, and the identical inbound rules pile up in
# the firewall. The delete has to fire before the add in that pass.
# InstallExecuteSequence: Action, Condition, Sequence
$seq = @(Rows 'SELECT * FROM `InstallExecuteSequence`' 3)
$fwDelSeq = @($seq | Where-Object { $_[0] -eq 'FwDel' })
$fwAddSeq = @($seq | Where-Object { $_[0] -eq 'FwAdd' })
if ($fwDelSeq.Count -eq 0 -or $fwAddSeq.Count -eq 0) {
    Write-Output 'FAIL the firewall actions are missing from the execute sequence'
    $fail = 1
} else {
    $fwDelCond = [string]$fwDelSeq[0][1]
    if ($fwDelCond -notlike '*OPT_FIREWALL*' -or $fwDelCond -notlike '*NOT Installed*') {
        Write-Output ('FAIL FwDel does not run on a fresh install or upgrade: ' + $fwDelCond)
        Write-Output '     identical inbound rules would pile up in the firewall'
        $fail = 1
    } else {
        Write-Output 'OK   FwDel also runs on a fresh install or upgrade, not only on uninstall'
    }
    if ([int]$fwDelSeq[0][2] -ge [int]$fwAddSeq[0][2]) {
        Write-Output ('FAIL FwDel runs after FwAdd: the stale rule would survive the install')
        $fail = 1
    } else {
        Write-Output 'OK   FwDel is scheduled before FwAdd'
    }
}

# ---- 5. product version --------------------------------------------------
if ($Version -ne '') {
    if ($propMap.ContainsKey('ProductVersion') -and $propMap['ProductVersion'] -eq $Version) {
        Write-Output ('OK   ProductVersion = ' + $Version)
    } else {
        Write-Output ('FAIL ProductVersion is ' + $propMap['ProductVersion'] + ', expected ' + $Version)
        $fail = 1
    }
}

# ---- 6. the license gate --------------------------------------------------
# The license page must be a gate, not a suggestion. Two independent mechanisms
# enforce that, and BOTH have to be present:
#   1) ControlCondition carries two rows for the Next button: Disable when the
#      box is not ticked and Enable when it is. A single Enable row leaves the
#      control in its default (clickable) state while the condition is false -
#      that was the old bug, the button looked available and the page advanced.
#   2) The Publish event that moves to the next page carries the same condition,
#      so even a control that somehow got re-enabled does not advance the wizard.
#
# SELECT * instead of naming the columns: the Windows Installer SQL view does
# not resolve every column name on these tables (it returns an empty result
# instead of an error), and a check that silently compares nothing is worse
# than no check at all. The column order used below is what the packages
# actually carry, verified by reading the table back.
# ControlCondition: Dialog_, Control, Action, Condition
$cc = @(Rows 'SELECT * FROM `ControlCondition`' 4)
$ccGate = @($cc | Where-Object { $_[0] -eq 'LicenseDlg' -and $_[1] -eq 'Next' -and $_[3] -like '*LicenseAccepted*' })
$hasEnable = @($ccGate | Where-Object { $_[2] -eq 'Enable' }).Count -gt 0
$hasDisable = @($ccGate | Where-Object { $_[2] -eq 'Disable' }).Count -gt 0
if (-not $hasEnable) {
    Write-Output 'FAIL no Enable condition on LicenseAccepted: the Next button stays disabled'
    $fail = 1
} else {
    Write-Output 'OK   ControlCondition enables the Next button once the license is accepted'
}
if (-not $hasDisable) {
    Write-Output 'FAIL no Disable condition on LicenseAccepted: the Next button stays clickable until ticked'
    $fail = 1
} else {
    Write-Output 'OK   ControlCondition disables the Next button until the license is accepted'
}

# ControlEvent: Dialog_, Control, Event, Argument, Condition
$ce = @(Rows 'SELECT * FROM `ControlEvent`' 5)
$publish = @($ce | Where-Object { $_[0] -eq 'LicenseDlg' -and $_[1] -eq 'Next' -and $_[2] -eq 'NewDialog' -and $_[3] -eq 'InstallDirDlg' -and $_[4] -like '*LicenseAccepted*' })
if ($publish.Count -eq 0) {
    Write-Output 'FAIL the license page does not gate the page transition: it would advance unchecked'
    $fail = 1
} else {
    Write-Output 'OK   the license page only advances to the install folder page once accepted'
}

# ---- 7. the license files that ship with the product ----------------------
# Apache-2.0 section 4 and GPL-3 section 4 both require the license texts to
# travel with the product. The install directory must carry all of them.
# File.FileName holds "short|long"; the long name is the one that matters.
$fileNames = @(Rows 'SELECT `FileName` FROM `File`' 1 | ForEach-Object { ($_[0] -split '\|')[-1] })
foreach ($want in @('LICENSE', 'THIRD_PARTY_LICENSES.md', 'LICENSE-localsend.txt',
                    'NOTICE-localsend.txt', 'PORTING-localsend.md')) {
    if ($fileNames -notcontains $want) {
        Write-Output ('FAIL the package does not ship ' + $want)
        $fail = 1
    } else {
        Write-Output ('OK   the package ships ' + $want)
    }
}

if ($fail -ne 0) {
    Write-Output 'RESULT: the MSI does not satisfy the table-level contract'
    exit 1
}
Write-Output 'RESULT: the MSI satisfies the table-level contract'
exit 0
