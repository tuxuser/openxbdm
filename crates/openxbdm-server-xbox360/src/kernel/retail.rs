//! Debug monitor data for retail kernels, where `ExDebugMonitorService`
//! registration fails on every build checked so far
//! (`docs/re-notes/retail-dashlaunch.md`). Fills the fields of the block
//! that the module list and thread walks read, so those consumers work
//! unchanged.

use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering;
use core::time::Duration;

use xbox360::console::kernel_version;
use xbox360::module::Ordinal;
use xbox360::module::SystemModule;
use xbox360::module::resolve_export;
use xbox360::time::sleep;
use xbox360_sys::processor::current_thread;
use xbox360_sys::xboxkrnl;
use xbox360_sys::xboxkrnl::Handle;
use xbox360_sys::xboxkrnl::Kthread;
use xbox360_sys::xboxkrnl::ThreadCreationFlags;

use super::address::thread_at;
use super::monitor::debug_monitor_data;

/// `XexPcToFileHeader` walks the loaded module list without registration
/// and materializes the list head and its spin lock each as a `lis`/`addi`
/// pair. Word offsets are per kernel build, verified live once each; a
/// build not in this table is unsupported, not guessed at.
struct ModuleListSignature {
    /// `kernel_version().build`: `XboxKrnlVersion`, not the XBDM build
    /// `dmversion` reports (docs/re-notes/nuances.md).
    build: u16,
    /// Word indices from the function start of the `lis`/`addi` pair of
    /// the spin lock address.
    lock: (u32, u32),
    /// Same, for the list head address.
    head: (u32, u32),
}

const MODULE_LIST_SIGNATURES: &[ModuleListSignature] = &[ModuleListSignature {
    build: 17559,
    lock: (3, 5),
    head: (9, 11),
}];

const XEX_PC_TO_FILE_HEADER: Ordinal = match Ordinal::new(412) {
    Some(ordinal) => ordinal,
    None => panic!("ordinal 0"),
};

/// `lis rD, imm16` (`addis rD, 0, imm16`).
fn decode_lis(word: u32) -> Option<(u32, u16)> {
    (word >> 26 == 15 && (word >> 16) & 0x1f == 0).then(|| ((word >> 21) & 0x1f, word as u16))
}

/// `addi rD, rA, imm16`, as `(rA, imm16)`.
fn decode_addi(word: u32) -> Option<(u32, i16)> {
    (word >> 26 == 14).then(|| ((word >> 16) & 0x1f, word as i16))
}

/// The address a `lis`/`addi` pair at word indices `pair` of `function`
/// materializes. None on any opcode or register mismatch: a build whose
/// code differs from the signature must fail, not read whatever is there.
fn materialized_address(function: u32, pair: (u32, u32)) -> Option<u32> {
    let word = |index: u32| {
        // SAFETY: `function` is a resolved kernel export; its code is
        // mapped and readable as data.
        unsafe {
            core::ptr::with_exposed_provenance::<u32>((function + 4 * index) as usize)
                .read_volatile()
        }
    };
    let (register, high) = decode_lis(word(pair.0))?;
    let (base, low) = decode_addi(word(pair.1))?;
    (base == register).then(|| (u32::from(high) << 16).wrapping_add(low as i32 as u32))
}

/// Store the loaded module list head and its spin lock read out of
/// `XexPcToFileHeader`. Leaves the user-mode list absent: on the builds
/// checked so far this list already holds every module, and walks treat
/// an absent second head as the end. Returns whether it succeeded.
pub fn adopt_module_list() -> bool {
    let build = kernel_version().build;
    let Some(signature) = MODULE_LIST_SIGNATURES.iter().find(|s| s.build == build) else {
        return false;
    };
    let Some(function) = SystemModule::Kernel
        .handle()
        .and_then(|kernel| resolve_export(kernel, XEX_PC_TO_FILE_HEADER))
        .ok()
        .map(|address| address.raw())
    else {
        return false;
    };
    let (Some(head), Some(lock)) = (
        materialized_address(function, signature.head),
        materialized_address(function, signature.lock),
    ) else {
        return false;
    };

    let data = debug_monitor_data();
    // SAFETY: the addresses are the kernel's loader list head and lock, the
    // values the query service would have stored.
    unsafe {
        data.loaded_module_list.store(head, Ordering::Relaxed);
        data.loaded_module_list_lock.store(lock, Ordering::Relaxed);
    }
    true
}

fn process_of(thread: &Kthread) -> u32 {
    thread.process
}

static TITLE_PROCESS_PROBE: AtomicU32 = AtomicU32::new(0);

/// Flags of the probe thread: the bits verified on hardware, with
/// `SYSTEM_PROCESS` clear so the thread lands in the title process.
const TITLE_PROCESS_PROBE_FLAGS: ThreadCreationFlags =
    ThreadCreationFlags::from_bits_retain(0x0400_0081);

extern "C" fn title_process_probe(_context: u32) -> u32 {
    // SAFETY: the current thread object is live while it runs.
    let process = process_of(unsafe { thread_at(current_thread()) });
    TITLE_PROCESS_PROBE.store(process, Ordering::Release);
    // SAFETY: a thread created without the XAPI wrapper ends only here.
    unsafe { xboxkrnl::ExTerminateThread(0) }
}

/// `KeTitleProcess`: the process of a thread created without
/// `SYSTEM_PROCESS`, read once it has run.
fn title_process() -> Option<u32> {
    const ATTEMPTS: u32 = 50;

    TITLE_PROCESS_PROBE.store(0, Ordering::Release);
    let mut handle = Handle::NULL.raw();
    // SAFETY: `handle` is a live local and the entry has the start
    // signature.
    let status = unsafe {
        xboxkrnl::ExCreateThread(
            &mut handle,
            0x2000,
            core::ptr::null_mut(),
            0,
            title_process_probe as *const () as usize as u32,
            0,
            TITLE_PROCESS_PROBE_FLAGS,
        )
    };
    status.result().ok()?;

    let mut process = None;
    for _ in 0..ATTEMPTS {
        process = core::num::NonZeroU32::new(TITLE_PROCESS_PROBE.load(Ordering::Acquire));
        if process.is_some() {
            break;
        }
        sleep(Duration::from_millis(1));
    }
    // SAFETY: `handle` came from `ExCreateThread` and is not used again.
    unsafe { xboxkrnl::NtClose(Handle::from_raw(handle)) };
    process.map(|process| process.get())
}

/// Store `KeSystemProcess`, the process of the calling thread (every thread
/// of this module is a system thread), and `KeTitleProcess`. Returns
/// whether both resolved.
pub fn adopt_processes() -> bool {
    let data = debug_monitor_data();
    // SAFETY: the calling thread object is live while it runs.
    let system = process_of(unsafe { thread_at(current_thread()) });
    // SAFETY: the value is the kernel's system process object.
    unsafe { data.system_process.store(system, Ordering::Relaxed) };
    let Some(title) = title_process() else {
        return false;
    };
    // SAFETY: the value is the kernel's title process object.
    unsafe { data.title_process.store(title, Ordering::Relaxed) };
    true
}
