#!/usr/bin/env bash
# 防分叉门禁：检查机器上是否注册着**多于一份** LinkX。
#
# 为什么会分叉：WiX v4 的 ProductCode 由 (UpgradeCode, 版本, 语言) 确定性生成。
# 同一版本先用 per-machine 装一次（写 HKLM）、再用 per-user 装一次（写 HKCU），
# 得到的是**同一个 ProductCode 落在两个作用域**：msiexec 返回 0、看起来装好了，
# 实际机器上留下两份程序、两套身份与信任库，对端下次连接就是"新设备"要重新配对。
# per-user 包又检测不到 per-machine 旧装（MajorUpgrade 跨不过作用域），所以谁都不会自动清掉它。
#
# 用法：bash Scripts/check-install-single.sh
# 退出码：0 = 只有一份或没有；1 = 分叉；2 = 查询本身失败（绝不把"没输出"读成"没问题"）
set -uo pipefail

ps='
$ErrorActionPreference = "Stop"
$rows = @()
foreach ($hive in @("HKLM:", "HKCU:")) {
  $base = $hive + "\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall"
  if (-not (Test-Path $base)) { continue }
  foreach ($k in Get-ChildItem $base) {
    $p = Get-ItemProperty -Path $k.PSPath
    if ($p.DisplayName -eq "LinkX") {
      $scope = "per-user"; if ($hive -eq "HKLM:") { $scope = "per-machine" }
      $rows += , @($scope, $k.PSChildName, [string]$p.DisplayVersion, [string]$p.InstallLocation)
    }
  }
}
$exe = ""
$proc = Get-Process -Name linkx -ErrorAction SilentlyContinue | Select-Object -First 1
if ($proc) { $exe = $proc.Path }
Write-Output ("FOUND=" + $rows.Count)
foreach ($r in $rows) { Write-Output ("  " + $r[0] + " | " + $r[1] + " | ver=" + $r[2] + " | loc=" + $r[3]) }
if ($exe) { Write-Output ("RUNNING=" + $exe) } else { Write-Output "RUNNING=(未运行)" }
$scopes = ($rows | ForEach-Object { $_[0] } | Sort-Object -Unique)
Write-Output ("SCOPES=" + ($scopes -join "+"))
'

out="$(powershell -NoProfile -Command "$ps" 2>&1)"
rc=$?
echo "$out"
if [ $rc -ne 0 ]; then
  echo "❌ 注册表查询失败（rc=$rc）——按失败处理，不能当成"没有问题""
  exit 2
fi

n="$(printf '%s\n' "$out" | sed -n 's/^FOUND=\([0-9][0-9]*\)$/\1/p' | head -1)"
if [ -z "$n" ]; then
  echo "❌ 没读到 FOUND 计数——按失败处理（本项目吃过"空输出被读成没问题"的亏）"
  exit 2
fi
scopes="$(printf '%s\n' "$out" | sed -n 's/^SCOPES=\(.*\)$/\1/p' | head -1)"

if [ "$n" -le 1 ]; then
  echo "✅ LinkX 注册份数 = $n，不分叉"
  exit 0
fi

echo "❌ 注册着 $n 份 LinkX（作用域：$scopes）—— 正是「同一版本落在两个作用域」的形状。"
echo "   两份程序各持身份与信任库，表现就是"对端变成新设备、必须重新配对"。"
echo "   处理：对照上面 RUNNING= 那行确认要留哪一份，另一份执行"
echo "         msiexec /x {对应 ProductCode} /qn        （per-machine 那份需要管理员权限）"
echo "   再到「程序和功能」核对只剩一份。"
exit 1
