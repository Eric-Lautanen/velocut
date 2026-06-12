// crates/velocut-core/src/windows.rs
//
// Windows FFI helpers shared across the workspace.
// Centralises extern blocks that were previously duplicated in worker.rs and main.rs.
//
// All functions are gated behind `#[cfg(windows)]` — on other platforms
// they compile to no-ops so callers don't need their own cfg gates.

/// Set the current thread's OS scheduling priority to below normal.
///
/// Used by the encode thread so the UI, audio, and scrub-decode threads
/// are never starved during CPU encodes. The encoder still runs full-speed
/// when cores are idle but yields immediately to higher-priority work.
///
/// On Windows: calls SetThreadPriority(THREAD_PRIORITY_BELOW_NORMAL).
/// On other platforms: calls nice(10).
pub fn lower_thread_priority() {
    #[cfg(windows)]
    {
        extern "system" {
            fn GetCurrentThread() -> *mut core::ffi::c_void;
            fn SetThreadPriority(hThread: *mut core::ffi::c_void, nPriority: i32) -> i32;
        }
        const THREAD_PRIORITY_BELOW_NORMAL: i32 = -1;
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        extern "C" {
            fn nice(incr: i32) -> i32;
        }
        unsafe {
            nice(10);
        }
    }
}
