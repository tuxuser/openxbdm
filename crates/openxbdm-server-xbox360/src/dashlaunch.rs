//! dashlaunch's `launch.xex` export 1 (`ldata`): the retail exception-hook
//! source, used when `ExDebugMonitorService` registration is unavailable
//! (`docs/re-notes/retail-dashlaunch.md`; verified on kernel 2.0.17559.0,
//! dashlaunch 3.21).
//!
//! `ldata` is resolved at runtime, not through a static import: dashlaunch
//! is not one of the libraries the loader's import table mechanism knows
//! about, and its load address is not fixed across builds the way
//! `xboxkrnl.exe`/`xam.xex` ordinals are. `DebugRoutine`/`DebugStepPatch`
//! are already dashlaunch's own per-kernel-build resolution (its
//! `kernpat[]` table); this module only needs to read what it finds there,
//! not maintain its own copy of that table.

use crate::kernel;

/// Byte offsets into `ldata` (dashlaunch `dll/exports.h`), as observed live:
/// `ID`(u32) `ltype`(u32) `link`[260] `dev`[260] `versionMaj`(u16)
/// `versionMin`(u16) `targetKernel`(u16) `svnVer`(u16) `options`(u32)
/// `DebugRoutine`(u32) `DebugStepPatch`(u32) ...
mod offset {
    pub const ID: u32 = 0x00;
    pub const DEBUG_ROUTINE: u32 = 0x21c;
    pub const DEBUG_STEP_PATCH: u32 = 0x220;
}

/// `ldata.ID` of a recognized dashlaunch build ("DL30").
const LAUNCH_DATA_ID: u32 = u32::from_be_bytes(*b"DL30");

fn read_u32(address: u32) -> u32 {
    // SAFETY: callers pass the address of a live `ldata` dashlaunch keeps
    // for the life of the system.
    unsafe { core::ptr::read_volatile(address as usize as *const u32) }
}

fn write_u32(address: u32, value: u32) {
    // SAFETY: callers pass the address of a live, writable kernel object
    // (a kernel debug-routine pointer or step-patch instruction word).
    unsafe { core::ptr::write_volatile(address as usize as *mut u32, value) }
}

/// Resolve `launch.xex` export 1 (`ldata`). `None` when dashlaunch is not
/// loaded, or its `ldata.ID` does not match a build this was checked
/// against (a mismatched layout would misread `DebugRoutine`).
fn ldata() -> Option<u32> {
    let module = kernel::module_handle(c"launch.xex").ok()?;
    let address = kernel::resolve_export(module, 1).ok()?;
    (read_u32(address + offset::ID) == LAUNCH_DATA_ID).then_some(address)
}

/// The kernel's debug-routine pointer (`KiDebugRoutine`), already resolved
/// by dashlaunch for the running kernel build.
pub fn debug_routine_variable() -> Option<u32> {
    ldata().map(|address| read_u32(address + offset::DEBUG_ROUTINE))
}

/// Address of the kernel instruction dashlaunch nops to stop the kernel
/// skipping single-step exceptions (write `0x6000_0000`, a `nop`, there to
/// enable them).
fn debug_step_patch_address() -> Option<u32> {
    ldata().map(|address| read_u32(address + offset::DEBUG_STEP_PATCH))
}

/// Patch the kernel instruction dashlaunch's `DebugStepPatch` names to a
/// `nop`, enabling single-step exceptions. Returns the patched address and
/// the instruction it replaced, to restore with [`restore_single_step`];
/// `None` when dashlaunch is not loaded or not recognized.
pub fn enable_single_step() -> Option<(u32, u32)> {
    let address = debug_step_patch_address()?;
    let previous = read_u32(address);
    write_u32(address, 0x6000_0000);
    Some((address, previous))
}

/// Undo [`enable_single_step`].
pub fn restore_single_step(address: u32, previous: u32) {
    write_u32(address, previous);
}
