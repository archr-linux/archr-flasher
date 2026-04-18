/*
 * Windows 7 compatibility shim for bcryptprimitives.dll
 *
 * Rust 1.78+ and getrandom 0.2.12+ import ProcessPrng from
 * bcryptprimitives.dll, which only exists on Windows 8+.
 *
 * This is compiled as a standalone bcryptprimitives.dll that:
 * - On Win8+: forwards all calls to the real system DLL
 * - On Win7: implements ProcessPrng using RtlGenRandom (advapi32)
 *
 * The DLL is placed next to the .exe. Windows DLL search order
 * loads it before the system directory, so it intercepts the import.
 *
 * Build (MSVC):
 *   cl /LD /Fe:bcryptprimitives.dll win7_shim.c advapi32.lib
 *
 * Build (MinGW):
 *   x86_64-w64-mingw32-gcc -shared -o bcryptprimitives.dll win7_shim.c -ladvapi32
 */

#ifdef _WIN32

#include <windows.h>

typedef BOOLEAN (WINAPI *RtlGenRandom_fn)(PVOID, ULONG);
typedef BOOL (WINAPI *ProcessPrng_fn)(PBYTE, SIZE_T);

static HMODULE hReal = NULL;
static ProcessPrng_fn pRealProcessPrng = NULL;
static RtlGenRandom_fn pRtlGenRandom = NULL;
static BOOL initialized = FALSE;

static void init_once(void) {
    if (initialized) return;
    initialized = TRUE;

    /* Try to load the REAL bcryptprimitives.dll from system32 */
    WCHAR sysdir[MAX_PATH];
    GetSystemDirectoryW(sysdir, MAX_PATH);
    wcscat_s(sysdir, MAX_PATH, L"\\bcryptprimitives.dll");
    hReal = LoadLibraryW(sysdir);

    if (hReal) {
        pRealProcessPrng = (ProcessPrng_fn)GetProcAddress(hReal, "ProcessPrng");
    }

    if (!pRealProcessPrng) {
        /* Win7 fallback: use RtlGenRandom from advapi32 */
        HMODULE hAdv = GetModuleHandleA("advapi32.dll");
        if (!hAdv) hAdv = LoadLibraryA("advapi32.dll");
        if (hAdv) {
            pRtlGenRandom = (RtlGenRandom_fn)GetProcAddress(hAdv, "SystemFunction036");
        }
    }
}

__declspec(dllexport)
BOOL WINAPI ProcessPrng(PBYTE pbData, SIZE_T cbData) {
    init_once();

    if (pRealProcessPrng) {
        return pRealProcessPrng(pbData, cbData);
    }

    if (pRtlGenRandom) {
        while (cbData > 0) {
            ULONG chunk = (cbData > 0xFFFFFFFF) ? 0xFFFFFFFF : (ULONG)cbData;
            if (!pRtlGenRandom(pbData, chunk))
                return FALSE;
            pbData += chunk;
            cbData -= chunk;
        }
        return TRUE;
    }

    return FALSE;
}

BOOL WINAPI DllMain(HINSTANCE hinstDLL, DWORD fdwReason, LPVOID lpvReserved) {
    (void)hinstDLL; (void)lpvReserved;
    if (fdwReason == DLL_PROCESS_DETACH && hReal) {
        FreeLibrary(hReal);
    }
    return TRUE;
}

#endif /* _WIN32 */
