/*
 * Windows 7 compatibility shim.
 *
 * Rust 1.78+ and getrandom 0.2.12+ import ProcessPrng from
 * bcryptprimitives.dll, which only exists on Windows 8+.
 *
 * This file provides a delay-load failure hook: when ProcessPrng
 * cannot be resolved (Windows 7), it returns a fallback that calls
 * RtlGenRandom (SystemFunction036) from advapi32.dll, which has
 * been available since Windows XP.
 *
 * build.rs passes /DELAYLOAD:bcryptprimitives.dll to the linker
 * so the DLL is loaded lazily instead of at process start.
 */

#include <windows.h>
#include <delayimp.h>
#include <string.h>

typedef BOOLEAN (WINAPI *RtlGenRandom_fn)(PVOID, ULONG);

static BOOL WINAPI FallbackProcessPrng(PBYTE pbData, SIZE_T cbData) {
    static RtlGenRandom_fn pRtlGenRandom = NULL;

    if (!pRtlGenRandom) {
        HMODULE h = LoadLibraryA("advapi32.dll");
        if (h)
            pRtlGenRandom = (RtlGenRandom_fn)GetProcAddress(h, "SystemFunction036");
    }
    if (!pRtlGenRandom)
        return FALSE;

    while (cbData > 0) {
        ULONG chunk = (cbData > (SIZE_T)0xFFFFFFFF) ? 0xFFFFFFFF : (ULONG)cbData;
        if (!pRtlGenRandom(pbData, chunk))
            return FALSE;
        pbData += chunk;
        cbData -= chunk;
    }
    return TRUE;
}

static FARPROC WINAPI delayLoadHook(unsigned dliNotify, PDelayLoadInfo pdli) {
    if (dliNotify == dliFailGetProc &&
        pdli->dlp.fImportByName &&
        strcmp(pdli->dlp.szProcName, "ProcessPrng") == 0) {
        return (FARPROC)FallbackProcessPrng;
    }
    return NULL;
}

/*
 * MSVC delay-load helper calls this function pointer when a
 * delay-loaded function cannot be resolved.
 */
const PfnDliHook __pfnDliFailureHook2 = delayLoadHook;
