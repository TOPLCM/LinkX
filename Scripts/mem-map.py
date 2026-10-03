#!/usr/bin/env python3
"""进程内存归因（只读，不改任何被测代码）。

用途：回答"这 27MB 到底是什么"，以及"关掉功能开关后少的是哪一类页"。
已知结论：镜像页占大头，功能开关省不下多少——所以别拿开关当内存卖点。

做法：VirtualQueryEx 走完整虚拟地址空间，按区类型统计
- 保留(reserve) vs 提交(commit)
- 私有堆 / 映射文件 / 镜像(DLL+EXE)
再对已提交页用 QueryWorkingSetEx 判定是否真的驻留（驻留才进工作集），
镜像区按模块名归并，得到"哪个 DLL 占了多少驻留"。

线程数/GDI/USER 对象数一并给出：本产品模块成本主要在"线程与句柄"，
只看 MB 会得出错误结论。
"""
import ctypes as C
import ctypes.wintypes as W
import json
import sys

k32 = C.windll.kernel32
psapi = C.windll.psapi
user32 = C.windll.user32

PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
PROCESS_VM_READ = 0x0010
MEM_COMMIT = 0x1000
MEM_IMAGE = 0x1000000
MEM_MAPPED = 0x40000
MEM_PRIVATE = 0x20000
PAGE_SIZE = 4096


class MBI(C.Structure):
    _fields_ = [
        ("BaseAddress", C.c_void_p),
        ("AllocationBase", C.c_void_p),
        ("AllocationProtect", W.DWORD),
        ("RegionSize", C.c_size_t),
        ("State", W.DWORD),
        ("Protect", W.DWORD),
        ("Type", W.DWORD),
    ]


class WSX(C.Structure):
    _fields_ = [("addr", C.c_void_p), ("attrs", C.c_ulonglong)]


class PMC(C.Structure):
    _fields_ = [
        ("cb", W.DWORD),
        ("PeakWorkingSetSize", C.c_size_t),
        ("WorkingSetSize", C.c_size_t),
        ("QuotaPeakPagedPoolUsage", C.c_size_t),
        ("QuotaPagedPoolUsage", C.c_size_t),
        ("QuotaPeakNonPagedPoolUsage", C.c_size_t),
        ("QuotaNonPagedPoolUsage", C.c_size_t),
        ("PagefileUsage", C.c_size_t),
        ("PeakPagefileUsage", C.c_size_t),
        ("PrivateUsage", C.c_size_t),
    ]


class MODULEINFO(C.Structure):
    _fields_ = [("lpBaseOfDll", C.c_void_p), ("SizeOfImage", W.DWORD), ("EntryPoint", C.c_void_p)]


k32.OpenProcess.restype = W.HANDLE
k32.OpenProcess.argtypes = [W.DWORD, W.BOOL, W.DWORD]
k32.CloseHandle.argtypes = [W.HANDLE]
k32.CreateToolhelp32Snapshot.restype = W.HANDLE
k32.CreateToolhelp32Snapshot.argtypes = [W.DWORD, W.DWORD]
k32.Thread32First.restype = W.BOOL
k32.Thread32First.argtypes = [W.HANDLE, C.c_void_p]
k32.Thread32Next.restype = W.BOOL
k32.Thread32Next.argtypes = [W.HANDLE, C.c_void_p]
psapi.EnumProcessModulesEx.argtypes = [W.HANDLE, C.c_void_p, W.DWORD, C.POINTER(W.DWORD), W.DWORD]
psapi.GetModuleFileNameExW.argtypes = [W.HANDLE, W.HMODULE, C.c_void_p, W.DWORD]
psapi.GetModuleInformation.argtypes = [W.HANDLE, W.HMODULE, C.POINTER(MODULEINFO), W.DWORD]
psapi.QueryWorkingSetEx.argtypes = [W.HANDLE, C.c_void_p, W.DWORD]
psapi.K32GetProcessMemoryInfo = getattr(k32, "K32GetProcessMemoryInfo")
user32.GetGuiResources.restype = W.DWORD
user32.GetGuiResources.argtypes = [W.HANDLE, W.DWORD]


def module_names(hproc):
    arr = (C.c_void_p * 1024)()
    got = W.DWORD()
    psapi.EnumProcessModulesEx(hproc, arr, C.sizeof(arr), C.byref(got), 0x03)
    out = {}
    n = got.value // C.sizeof(C.c_void_p)
    buf = C.create_unicode_buffer(260)
    for i in range(n):
        hm = W.HMODULE(arr[i] or 0)
        psapi.GetModuleFileNameExW(hproc, hm, C.addressof(buf), 260)
        mi = MODULEINFO()
        if psapi.GetModuleInformation(hproc, hm, C.byref(mi), C.sizeof(mi)):
            base = mi.lpBaseOfDll or 0
            out[(base, base + mi.SizeOfImage)] = buf.value
    return out


def find_module(names, addr):
    for (lo, hi), nm in names.items():
        if lo <= addr < hi:
            return nm.rsplit("\\", 1)[-1]
    return None


def main(pid):
    h = k32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, False, pid)
    if not h:
        h = k32.OpenProcess(0x0400 | 0x0010 | 0x0020 | 0x0008, False, pid)  # QUERY_INFORMATION|VM_READ|VM_OP
    if not h:
        print("OpenProcess failed", C.WinError(C.GetLastError()))
        return 1

    names = module_names(h)

    pmc = PMC()
    pmc.cb = C.sizeof(PMC)
    k32.K32GetProcessMemoryInfo(h, C.byref(pmc), pmc.cb)

    # ---- 走地址空间 ----
    mbi = MBI()
    addr = 0
    regions = 0
    buckets = {}          # 类别 -> {commit, resident}
    mod_res = {}          # 模块 -> 驻留字节
    wsx_sample = (WSX * 4096)()
    while regions < 400000:
        ok = k32.VirtualQueryEx(h, C.c_void_p(addr), C.byref(mbi), C.sizeof(mbi))
        if not ok:
            break
        size = mbi.RegionSize
        if size == 0:
            break
        regions += 1
        if mbi.State == MEM_COMMIT:
            kind = {MEM_IMAGE: "image(DLL/EXE)", MEM_MAPPED: "mapped(文件/共享)",
                    MEM_PRIVATE: "private(堆/栈)"}.get(mbi.Type, f"type:{mbi.Type:#x}")
            b = buckets.setdefault(kind, {"commit": 0, "resident": 0, "regions": 0})
            b["regions"] += 1
            b["commit"] += size
            nres = 0
            npages = size // PAGE_SIZE
            off = 0
            while off < npages:
                take = min(4096, npages - off)
                for i in range(take):
                    wsx_sample[i].addr = addr + (off + i) * PAGE_SIZE
                    wsx_sample[i].attrs = 0
                if psapi.QueryWorkingSetEx(h, wsx_sample, C.sizeof(WSX) * take):
                    for i in range(take):
                        v = wsx_sample[i].attrs
                        if v & 1:                       # Valid 位
                            nres += PAGE_SIZE
                else:
                    break
                off += take
            b["resident"] += nres
            if kind.startswith("image"):
                nm = find_module(names, addr) or "(未归名)"
                mod_res[nm] = mod_res.get(nm, 0) + nres
        addr += size

    # 线程数：ctypes 的 Toolhelp32 枚举在某些宿主上拿不到，改用 Get-Process 兜底
    th = 0
    try:
        import subprocess
        o = subprocess.run(["powershell", "-NoProfile", "-Command",
                            f"(Get-Process -Id {pid}).Threads.Count"],
                           capture_output=True, text=True).stdout.strip()
        th = int(o) if o.isdigit() else 0
    except Exception:
        pass

    # 归因汇总：自己的 exe vs 系统镜像 vs 第三方注入（IME 之类）
    own = sum(v for k, v in mod_res.items() if k.lower().startswith("linkx"))
    total_img = sum(mod_res.values())
    inject = {k: v for k, v in mod_res.items()
              if not k.lower().startswith(("linkx", "ntdll", "kernel32", "kernelbase", "gdi32",
                                           "user32", "win32u", "msvcrt", "ucrtbase", "advapi32",
                                           "shell32", "ole32", "oleaut32", "combase", "rpcrt4",
                                           "sechost", "imm32", "msctf", "win32k", "uxtheme",
                                           "clbcatq", "bcrypt", "crypt", "wintypes", "kernel",
                                           "windows.storage", "kernel.appcore", "gpedit",
                                           "textinput", "coremessaging", "coreuicomponents",
                                           "dwmapi", "gdi32full", "user32", "appcore"))}
    # 归因汇总：自己的 exe / 系统镜像 / 第三方注入（IME 之类会注入每个 GUI 进程）
    own = sum(v for k, v in mod_res.items() if k.lower().startswith("linkx"))
    total_img = sum(mod_res.values())
    SYS_PREFIX = ("ntdll", "kernel32", "kernelbase", "kernel.appcore", "gdi32", "gdi32full",
                  "user32", "win32u", "msvcrt", "ucrtbase", "advapi32", "sechost", "shell32",
                  "ole32", "oleaut32", "combase", "rpcrt4", "imm32", "msctf", "uxtheme",
                  "clbcatq", "bcrypt", "cryptsp", "windows.storage", "textinput",
                  "coremessaging", "coreuicomponents", "dwmapi", "win32k", "powrprof",
                  "winsta", "tsworkspace", "srpapi", "wldcore", "wintypes", "propsys",
                  "shcore", "imagehlp", "version", "sspicli", "devobj", "cfgmgr32",
                  "windows.staterepeater", "resourcepolicy", "resources", "twinapi",
                  "microsoft", "windows.", "twinapi", "d2d1", "dwrite", "d3d11",
                  "dxgi", "plab dll", "ncrypt", "nsi", "wsock", "ws2_32", "profapi",
                  "gpapi", "samcli", "dnsapi", "iphlpapi", "cryptbase", "cryptpro",
                  "msvcp", "msvcr", "vcruntime", "bcp47", "typedata", "coml2", "核",
                  "apphelp", "sxs", "kernel")
    inject = {k: v for k, v in mod_res.items()
              if not k.lower().startswith("linkx")
              and not any(k.lower().startswith(p) for p in SYS_PREFIX)}

    out = {
        "pid": pid,
        "working_set_mb": round(pmc.WorkingSetSize / 1048576, 2),
        "peak_working_set_mb": round(pmc.PeakWorkingSetSize / 1048576, 2),
        "private_usage_mb": round(pmc.PrivateUsage / 1048576, 2),
        "pagefile_usage_mb": round(pmc.PagefileUsage / 1048576, 2),
        "regions_scanned": regions,
        "threads": th,
        "by_class": {k: {"commit_mb": round(v["commit"] / 1048576, 2),
                         "resident_mb": round(v["resident"] / 1048576, 2),
                         "regions": v["regions"]} for k, v in sorted(buckets.items())},
        "top_modules_mb": {k: round(v / 1048576, 2)
                           for k, v in sorted(mod_res.items(), key=lambda x: -x[1])[:18]},
        "module_count": len(names),
        "own_exe_resident_mb": round(own / 1048576, 2),
        "image_resident_total_mb": round(total_img / 1048576, 2),
        "thirdparty_injected_mb": {k: round(v / 1048576, 2)
                                   for k, v in sorted(inject.items(), key=lambda x: -x[1])[:12]},
        "own_exe_resident_mb": round(own / 1048576, 2),
        "image_resident_total_mb": round(total_img / 1048576, 2),
        "thirdparty_injected": {k: round(v / 1048576, 2)
                                 for k, v in sorted(inject.items(), key=lambda x: -x[1])[:12]},
        "gdi_objects": user32.GetGuiResources(h, 1),
        "user_objects": user32.GetGuiResources(h, 0),
    }
    k32.CloseHandle(h)
    print(json.dumps(out, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main(int(sys.argv[1])))
