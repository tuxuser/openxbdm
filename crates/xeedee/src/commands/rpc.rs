//! XDRPC-style remote procedure calls.
//!
//! XBDM has a native `rpc system` command that lets a debugger invoke an
//! arbitrary kernel/module export directly on the console, with real
//! arguments, through the *normal* call path - unlike `dbgextld`
//! (debug-extension loading), which maps a module into memory but never
//! calls its entry point (verified: loading a DLL_PROCESS_ATTACH-based
//! plugin this way never runs its `DllMain`). A real RPC call to
//! `XexLoadImage` genuinely loads and initializes a module - its `DllMain`
//! runs, hooks install, everything works exactly as if the title itself
//! had called `LoadLibrary`.
//!
//! Protocol reverse-engineered and ported from
//! <https://github.com/ClementDreptin/ModuleLoader> (`src/XDRPC.c`). No
//! official documentation exists for this wire format; the byte layout
//! below is empirically-derived, not from a spec.
//!
//! Wire sequence for one call:
//! 1. Send `rpc system version=4 buf_size=<N> processor=5 thread=`. XBDM
//!    replies `204- buf_addr=<hex>` - the address, in the *console's*
//!    memory, where our upload will land.
//! 2. Immediately (same connection, no further command) write an `N`-byte
//!    binary buffer encoding the module name, ordinal, and arguments (see
//!    [`build_call_buffer`]).
//! 3. Read a status-line response confirming the call completed.
//! 4. If a return value is wanted: read an "unknown" padding packet (8
//!    bytes normally, 16 on a reviewer kit / RGH - `DmGetConsoleType`
//!    determines which), then read back the same `N`-byte buffer with the
//!    64-bit return value written into its second `u64` slot.

use futures_io::AsyncRead;
use futures_io::AsyncWrite;
use futures_util::io::AsyncReadExt;
use futures_util::io::AsyncWriteExt;
use rootcause::prelude::*;

use crate::client::Client;
use crate::client::Connected;
use crate::commands::info::ConsoleType;
use crate::commands::info::GetConsoleType;
use crate::error::Error;
use crate::error::FramingError;
use crate::protocol::Response;
use crate::protocol::framing::LineBuffer;
use crate::protocol::response::read_response;

/// One argument to a remote call.
#[derive(Debug, Clone)]
pub enum RpcArg {
    Integer(u64),
    String(String),
}

/// Round a byte length up to the next multiple of 8, matching XDRPC.c's
/// `SizeOfString` (which pads with zero bytes, including - notably - zero
/// extra bytes, no null terminator, when the length is already a multiple
/// of 8; this is faithfully preserved since it's the proven-working
/// layout, not something we get to fix independently).
fn padded_len(byte_len: usize) -> usize {
    byte_len.div_ceil(8) * 8
}

fn write_u64_be(buf: &mut Vec<u8>, at: usize, value: u64) {
    buf[at..at + 8].copy_from_slice(&value.to_be_bytes());
}

/// Total size of the binary buffer this call needs: XBDM has to be told
/// this up front (in the `rpc system` command line) before we know
/// `buf_addr`, so it's split out from [`build_call_buffer`].
fn compute_buffer_size(module_name: &str, args: &[RpcArg]) -> usize {
    let has_string_args = args.iter().any(|a| matches!(a, RpcArg::String(_)));
    let mut buffer_size = 0x40usize;
    if has_string_args {
        buffer_size += 8;
    }
    for arg in args {
        buffer_size += match arg {
            RpcArg::Integer(_) => 8,
            RpcArg::String(s) => padded_len(s.len()),
        };
    }
    buffer_size += padded_len(module_name.len());
    buffer_size
}

/// Build the binary buffer XBDM's `rpc system` handler expects, once
/// `buf_addr` (the console-side landing address for this buffer) is known.
fn build_call_buffer(
    buf_addr: u32,
    module_name: &str,
    ordinal: u32,
    args: &[RpcArg],
) -> Vec<u8> {
    let has_string_args = args.iter().any(|a| matches!(a, RpcArg::String(_)));
    let module_name_padded = padded_len(module_name.len());
    let buffer_size = compute_buffer_size(module_name, args);

    let mut buf = vec![0u8; buffer_size];

    // first_buffer_address: where the ordinal/args/module-name region of
    // this same buffer will sit once uploaded, expressed in the console's
    // address space (buf_addr + our own 0x40-byte header + one u64 per
    // argument - matches XDRPC.c's `firstBufferAddress` exactly).
    let first_buffer_address = buf_addr as u64 + 0x40 + (args.len() as u64) * 8;

    // Bytes 0..32 stay zero. Then:
    write_u64_be(&mut buf, 32, args.len() as u64); // num args
    // Bytes 40..48 stay zero.
    write_u64_be(&mut buf, 48, first_buffer_address);
    write_u64_be(&mut buf, 56, ordinal as u64);

    let mut cursor = 64;
    if has_string_args {
        let second_buffer_address = first_buffer_address + module_name_padded as u64;
        write_u64_be(&mut buf, cursor, second_buffer_address);
        cursor += 8;
    }

    for arg in args {
        if let RpcArg::Integer(v) = arg {
            write_u64_be(&mut buf, cursor, *v);
            cursor += 8;
        }
    }

    buf[cursor..cursor + module_name.len()].copy_from_slice(module_name.as_bytes());
    cursor += module_name_padded;

    for arg in args {
        if let RpcArg::String(s) = arg {
            buf[cursor..cursor + s.len()].copy_from_slice(s.as_bytes());
            cursor += padded_len(s.len());
        }
    }

    buf
}

fn parse_buf_addr(head: &str) -> Result<u32, rootcause::Report<Error>> {
    let hex = head.trim().strip_prefix("buf_addr=").ok_or_else(|| {
        rootcause::Report::new(Error::from(FramingError::HeadTooShort))
            .attach(format!("expected \"buf_addr=<hex>\", got {head:?}"))
    })?;
    u32::from_str_radix(hex.trim(), 16)
        .map_err(|_| rootcause::Report::new(Error::from(FramingError::HeadTooShort)))
        .into_report()
        .attach_with(|| format!("parsing buf_addr hex {hex:?}"))
}

/// Drain any buffered bytes from `scratch` first, then read the rest of
/// `buf` straight off the transport. Mirrors the leftover-draining pattern
/// `file.rs`'s `read_length_prefix` uses for the same reason: a previous
/// line read may have over-read into what's actually binary payload.
async fn read_exact_raw<R>(
    reader: &mut R,
    scratch: &mut LineBuffer,
    buf: &mut [u8],
) -> Result<(), rootcause::Report<Error>>
where
    R: AsyncRead + Unpin,
{
    let mut filled = 0usize;
    if !scratch.as_bytes().is_empty() {
        let leftover = scratch.as_bytes();
        let take = core::cmp::min(leftover.len(), buf.len());
        buf[..take].copy_from_slice(&leftover[..take]);
        scratch.buf.drain(..take);
        filled = take;
    }
    while filled < buf.len() {
        let n = reader
            .read(&mut buf[filled..])
            .await
            .map_err(Error::from)
            .into_report()
            .attach("reading rpc binary payload")?;
        if n == 0 {
            return Err(rootcause::Report::new(Error::ConnectionClosed));
        }
        filled += n;
    }
    Ok(())
}

impl<T> Client<T, Connected>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    /// Call `<module_name>!<ordinal>(args...)` on the console and,
    /// optionally, read back its 64-bit return value.
    ///
    /// This is the same primitive DashLaunch-adjacent tools use to load a
    /// module without a reboot - see this module's docs for why it's not
    /// interchangeable with `dbg-ext load`.
    pub async fn rpc_call(
        &mut self,
        module_name: &str,
        ordinal: u32,
        args: &[RpcArg],
        want_return: bool,
    ) -> Result<Option<u64>, rootcause::Report<Error>> {
        // Nothing in the `rpc system` command line or call buffer tells the
        // console whether the caller wants a return value back - XBDM
        // always follows the completion line with the padding+buffer-echo
        // packet. `want_return` therefore only controls whether *we* parse
        // it, never whether the console sends it: skipping the read when
        // `want_return` is false leaves those bytes sitting in the stream
        // and corrupts whatever command is sent next on this connection
        // (confirmed live: a `false` call immediately followed by a native
        // `GetMem` on the same connection made the GetMem response frame
        // as non-UTF-8 binary garbage). So always drain it below; only
        // whether we bother decoding it varies.
        let console_type = self.run(GetConsoleType).await?;

        let buffer_size = compute_buffer_size(module_name, args);

        let command = format!("rpc system version=4 buf_size={buffer_size} processor=5 thread=");
        let response = self.send_raw(&command).await?;
        let head = match response {
            Response::SendBinary { head } => head,
            other => {
                return Err(rootcause::Report::new(Error::from(FramingError::HeadTooShort))
                    .attach(format!("expected 204 send-binary with buf_addr, got {other:?}")));
            }
        };
        let buf_addr = parse_buf_addr(&head)?;

        let call_buffer = build_call_buffer(buf_addr, module_name, ordinal, args);
        debug_assert_eq!(call_buffer.len(), buffer_size);

        {
            let (transport, _scratch) = self.transport_and_scratch();
            transport
                .write_all(&call_buffer)
                .await
                .map_err(Error::from)
                .into_report()
                .attach("uploading rpc call buffer")?;
            transport
                .flush()
                .await
                .map_err(Error::from)
                .into_report()
                .attach("flushing rpc call buffer")?;
        }

        // Status line confirming the call itself completed. When a return
        // value follows, XBDM answers with a bare `203- binary response
        // follows` here (no length prefix - the actual bytes are read
        // separately below) rather than a `200 OK`; ModuleLoader's C client
        // only checks the leading digit is `2`, so accept any success
        // shape.
        {
            let (transport, scratch) = self.transport_and_scratch();
            let status = read_response(transport, scratch, None).await?;
            match status {
                Response::Line { .. } | Response::Multiline { .. } | Response::Binary { .. } => {}
                other => {
                    return Err(rootcause::Report::new(Error::from(FramingError::HeadTooShort))
                        .attach(format!("unexpected rpc completion response: {other:?}")));
                }
            }
        }

        let unknown_packet_len = match console_type {
            ConsoleType::ReviewerKit => 16,
            _ => 8,
        };

        let (transport, scratch) = self.transport_and_scratch();
        let mut discard = vec![0u8; unknown_packet_len];
        read_exact_raw(transport, scratch, &mut discard).await?;

        let mut reply = vec![0u8; buffer_size];
        read_exact_raw(transport, scratch, &mut reply).await?;

        if !want_return {
            return Ok(None);
        }

        let return_value = u64::from_be_bytes(reply[8..16].try_into().unwrap());
        Ok(Some(return_value))
    }
}
