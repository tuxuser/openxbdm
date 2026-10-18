//! File-backed startup trace: writes lines through `NtCreateFile`/
//! `NtWriteFile` to a fixed HDD path, independent of the command engine or
//! any server state. Exists to diagnose module startup on hardware with no
//! kernel debugger attached, where `DbgPrint` is not otherwise observable
//! (`docs/re-notes/retail-dashlaunch.md`). Behind the `trace-log` feature;
//! disabled, [`log`] compiles to nothing (AGENTS.md: binary size).
//!
//! Opens, writes, and closes the file on every call rather than holding
//! one handle for the module's lifetime: an external reader (FTP `RETR`)
//! needs to open the file too, and a long-held handle serialized every
//! other open against it even with every share bit set, for a reason not
//! chased further here. [`NEXT_OFFSET`] tracks the append position across
//! calls since each open starts a fresh handle at offset 0.

#[cfg(feature = "trace-log")]
use core::fmt::Write as _;
#[cfg(feature = "trace-log")]
use core::sync::atomic::AtomicU32;
#[cfg(feature = "trace-log")]
use core::sync::atomic::Ordering;

#[cfg(feature = "trace-log")]
use xbox360_sys::xboxkrnl;
#[cfg(feature = "trace-log")]
use xbox360_sys::xboxkrnl::AccessMask;
#[cfg(feature = "trace-log")]
use xbox360_sys::xboxkrnl::CreateDisposition;
#[cfg(feature = "trace-log")]
use xbox360_sys::xboxkrnl::CreateOptions;
#[cfg(feature = "trace-log")]
use xbox360_sys::xboxkrnl::FileAttributes;
#[cfg(feature = "trace-log")]
use xbox360_sys::xboxkrnl::Handle;
#[cfg(feature = "trace-log")]
use xbox360_sys::xboxkrnl::IoStatusBlock;
#[cfg(feature = "trace-log")]
use xbox360_sys::xboxkrnl::ObjectAttributeFlags;
#[cfg(feature = "trace-log")]
use xbox360_sys::xboxkrnl::ObjectAttributes;
#[cfg(feature = "trace-log")]
use xbox360_sys::xboxkrnl::ShareAccess;

#[cfg(feature = "trace-log")]
use crate::kernel;

#[cfg(feature = "trace-log")]
const LOG_PATH: &[u8] = br"\Device\Harddisk0\Partition1\Apps\oxprobe\openxbdm-trace.log";

/// Byte offset the next write lands at; each call opens a fresh handle
/// (which starts at offset 0), so this is the only record of where the
/// file has grown to.
#[cfg(feature = "trace-log")]
static NEXT_OFFSET: AtomicU32 = AtomicU32::new(0);

#[cfg(feature = "trace-log")]
fn open_and_write(disposition: CreateDisposition, offset: u64, bytes: &[u8]) -> bool {
    let name = kernel::ansi_string(LOG_PATH);
    let attributes = ObjectAttributes {
        root_directory: Handle::NULL,
        object_name: &name,
        attributes: ObjectAttributeFlags::CASE_INSENSITIVE,
    };
    let mut handle = Handle::NULL.raw();
    let mut io_status = IoStatusBlock::default();
    // SAFETY: `handle`, `attributes`, the name it points at and
    // `io_status` are live locals.
    let status = unsafe {
        xboxkrnl::NtCreateFile(
            (&raw mut handle).cast(),
            AccessMask::GENERIC_WRITE,
            &attributes,
            &mut io_status,
            core::ptr::null(),
            FileAttributes::empty(),
            ShareAccess::READ | ShareAccess::WRITE | ShareAccess::DELETE,
            disposition,
            // Lines land on disk before the call returns, so one survives
            // a hang or bug check right after it.
            CreateOptions::SYNCHRONOUS_IO_NONALERT,
        )
    };
    if status.result().is_err() {
        return false;
    }
    let handle = Handle::from_raw(handle);
    if !bytes.is_empty() {
        // SAFETY: `bytes`, `io_status` and `offset` are live for the
        // synchronous write.
        unsafe {
            xboxkrnl::NtWriteFile(
                handle,
                Handle::NULL,
                0,
                0,
                &mut io_status,
                bytes.as_ptr(),
                bytes.len() as u32,
                &offset,
            )
        };
    }
    // SAFETY: `handle` came from `NtCreateFile` and is not used again.
    unsafe { xboxkrnl::NtClose(handle) };
    true
}

/// Truncate the trace file, discarding any previous run's contents. Call
/// once, as early as possible in module startup.
#[cfg(feature = "trace-log")]
pub fn open() {
    NEXT_OFFSET.store(0, Ordering::Release);
    open_and_write(CreateDisposition::OverwriteIf, 0, &[]);
}

#[cfg(not(feature = "trace-log"))]
pub fn open() {}

pub fn close() {}

/// Fixed-size line buffer: no allocator is available in this `no_std`
/// binary, so formatting goes through `core::fmt::Write` on the stack
/// instead of `alloc::format!`. A line longer than this is truncated.
#[cfg(feature = "trace-log")]
struct LineBuffer {
    data: [u8; 256],
    len: usize,
}

#[cfg(feature = "trace-log")]
impl core::fmt::Write for LineBuffer {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let remaining = self.data.len() - self.len;
        let take = bytes.len().min(remaining);
        self.data[self.len..self.len + take].copy_from_slice(&bytes[..take]);
        self.len += take;
        Ok(())
    }
}

/// Append one formatted line. A no-op before [`open`], if it failed, or if
/// the `trace-log` feature is off (use the [`trace`] macro, not this
/// directly, so the formatting itself compiles away too).
#[cfg(feature = "trace-log")]
pub fn log(args: core::fmt::Arguments<'_>) {
    let mut buffer = LineBuffer {
        data: [0; 256],
        len: 0,
    };
    let _ = buffer.write_fmt(args);
    let newline_at = buffer.len.min(buffer.data.len() - 2);
    buffer.data[newline_at..newline_at + 2].copy_from_slice(b"\r\n");
    let len = (buffer.len + 2).min(buffer.data.len());
    let offset = u64::from(NEXT_OFFSET.load(Ordering::Acquire));
    if open_and_write(CreateDisposition::OpenIf, offset, &buffer.data[..len]) {
        NEXT_OFFSET.fetch_add(len as u32, Ordering::AcqRel);
    }
}

/// Logs a line when the `trace-log` feature is on; otherwise the
/// arguments still type-check (catching format errors) but nothing is
/// emitted or evaluated at runtime.
macro_rules! trace {
    ($($arg:tt)*) => {
        #[cfg(feature = "trace-log")]
        $crate::trace::log(core::format_args!($($arg)*));
        #[cfg(not(feature = "trace-log"))]
        if false {
            let _ = core::format_args!($($arg)*);
        }
    };
}
pub(crate) use trace;
