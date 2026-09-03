# Emumerate private committed (MEM_PRIVATE | MEM_COMMIT) VAD regions of a
# process and histogram them, largest-first, to find where private bytes live.
param([int]$ProcId = 0)

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class VAD {
    [StructLayout(LayoutKind.Sequential)]
    public struct MEMORY_BASIC_INFORMATION {
        public IntPtr BaseAddress;
        public IntPtr AllocationBase;
        public uint AllocationProtect;
        public UIntPtr RegionSize;
        public uint State;
        public uint Protect;
        public uint Type;
    }
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern UIntPtr VirtualQueryEx(IntPtr hProcess, IntPtr lpAddress,
        out MEMORY_BASIC_INFORMATION lpBuffer, UIntPtr dwLength);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern IntPtr OpenProcess(uint dwDesiredAccess, bool bInheritHandle, int dwProcessId);
    [DllImport("kernel32.dll")]
    public static extern bool CloseHandle(IntPtr hObject);
}
"@

$PROCESS_QUERY_INFORMATION = 0x0400
$h = [VAD]::OpenProcess($PROCESS_QUERY_INFORMATION, $false, $ProcId)
if ($h -eq [IntPtr]::Zero) { Write-Error "cannot open pid $ProcId"; exit 1 }

$addr = [IntPtr]::Zero
$mbi = New-Object VAD+MEMORY_BASIC_INFORMATION
$size = [Runtime.InteropServices.Marshal]::SizeOf($mbi)
$groups = @{}
$byBase = @{}
$total = 0
while ($true) {
    $res = [VAD]::VirtualQueryEx($h, $addr, [ref]$mbi, [System.UIntPtr]::new($size))
    if ($res -eq [UIntPtr]::Zero) { break }
    $len = [int64][uint64]$mbi.RegionSize
    if ($len -le 0) { break }
    if (($mbi.State -eq 0x1000) -and ($mbi.Type -eq 0x20000)) {  # MEM_COMMIT + MEM_PRIVATE
        $total += $len
        $base = ("0x{0:X}" -f $mbi.AllocationBase.ToInt64())
        if (-not $byBase.ContainsKey($base)) { $byBase[$base] = 0 }
        $byBase[$base] += $len
        $key = if ($len -ge 65536) {
            if ($len -ge 16MB) { ">=16MB" } elseif ($len -ge 4MB) { "4-16MB" } elseif ($len -ge 1MB) { "1-4MB" } else { "64K-1MB" }
        } else { "<64KB" }
        $groups[$key] = $groups[$key] + $len
    }
    $next = $addr.ToInt64() + $len
    if ($next -le $addr.ToInt64()) { break }
    $addr = [IntPtr]$next
}
[VAD]::CloseHandle($h) | Out-Null

"total private committed: $([math]::Round($total/1MB)) MB"
$groups.GetEnumerator() | Sort-Object Value -Descending | ForEach-Object {
    "  {0,-12} {1,8} MB" -f $_.Key, [math]::Round($_.Value/1MB)
}
"--- by allocation base, all bases (sorted desc) ---"
$byBase.GetEnumerator() | Sort-Object Value -Descending | Select-Object -First 40 | ForEach-Object {
    "  $($_.Key)  $([math]::Round($_.Value/1MB,1)) MB"
}
"--- readable summary: $([math]::Round($total/1MB))MB committed ---"