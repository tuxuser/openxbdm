//! `rpc`: call a function on the console.
//!
//! The client uploads a call buffer of big-endian quadwords: result,
//! return value, last error request, flags, integer count, double count,
//! module name pointer, function address or export ordinal, then the
//! integer arguments and the doubles. The module name follows the
//! arguments; its pointer is computed from the `buf_addr` the console
//! reports. The console streams `03 E5` markers while the call runs, 8
//! zero bytes once it finished, then the buffer.

use bytes::BytesMut;
use futures_util::io::AsyncRead;
use futures_util::io::AsyncReadExt;
use futures_util::io::AsyncWrite;
use futures_util::io::AsyncWriteExt;
use rootcause::prelude::ResultExt;

use crate::Client;
use crate::Connected;
use crate::commands::Hresult;
use crate::commands::Win32Error;
use crate::commands::error::report_argument;
use crate::commands::error::report_parse;
use crate::commands::kv::parse_kv_line;
use crate::error::ArgumentError;
use crate::error::Error;
use crate::error::ExpectedShape;
use crate::error::ParseError;
use crate::protocol::ArgBuilder;
use crate::protocol::Response;

const RPC_VERSION: u32 = 4;
const HEADER_SIZE: usize = 0x40;
const DONE_MARKER: [u8; 8] = [0; 8];
/// `last_error` value that asks the console to store the last error.
const LAST_ERROR_REQUESTED: u64 = 1;
const DOUBLE_RESULT: u64 = 0x1;

/// Thread context a call runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcContext {
    System,
    Title,
}

/// Where a call runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcThread {
    /// A new thread; `None` selects the console default processor.
    Own { processor: Option<u32> },
    /// The title thread of this name runs the call when it drains its
    /// queue.
    Named(String),
}

/// Function to call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcTarget {
    Address(u32),
    Export { module: String, ordinal: u32 },
}

/// Kind of the return value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcReturnKind {
    /// r3.
    Integer,
    /// f1.
    Double,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rpc {
    pub context: RpcContext,
    pub thread: RpcThread,
    pub target: RpcTarget,
    pub integers: Vec<u64>,
    pub doubles: Vec<f64>,
    pub return_kind: RpcReturnKind,
    pub last_error: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RpcValue {
    Integer(u64),
    Double(f64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RpcOutcome {
    /// Low word of the sign-extended result quadword.
    pub result: Hresult,
    pub value: RpcValue,
    /// Requested and nonzero.
    pub last_error: Option<Win32Error>,
}

impl Rpc {
    fn module_name(&self) -> Option<&str> {
        match &self.target {
            RpcTarget::Address(_) => None,
            RpcTarget::Export { module, .. } => Some(module),
        }
    }

    fn arguments_end(&self) -> usize {
        HEADER_SIZE + 8 * (self.integers.len() + self.doubles.len())
    }

    fn buffer_size(&self) -> Result<u32, rootcause::Report<Error>> {
        let name = self.module_name().map_or(0, |name| name.len() + 1);
        u32::try_from(self.arguments_end() + name)
            .map_err(|_| report_argument(ArgumentError::MemoryLengthOverflow))
    }

    fn wire_line(&self, size: u32) -> Result<String, rootcause::Report<Error>> {
        let mut line = ArgBuilder::new("rpc")
            .dec("version", RPC_VERSION)
            .flag(match self.context {
                RpcContext::System => "system",
                RpcContext::Title => "title",
            })
            .hex32("buf_size", size);
        match &self.thread {
            RpcThread::Own { processor: None } => {}
            RpcThread::Own {
                processor: Some(processor),
            } => line = line.dec("processor", *processor),
            RpcThread::Named(name) => {
                line = line.quoted("thread", name).map_err(report_argument)?;
            }
        }

        Ok(line.finish())
    }

    /// The call buffer for a buffer at console address `address`.
    fn buffer(&self, address: u32, size: u32) -> Vec<u8> {
        let mut buffer = Vec::with_capacity(size as usize);
        let (module, target) = match &self.target {
            RpcTarget::Address(target) => (0, *target),
            // The name follows the arguments.
            RpcTarget::Export { ordinal, .. } => (
                u64::from(address.wrapping_add(self.arguments_end() as u32)),
                *ordinal,
            ),
        };
        let flags = match self.return_kind {
            RpcReturnKind::Integer => 0,
            RpcReturnKind::Double => DOUBLE_RESULT,
        };
        let header = [
            0,
            0,
            if self.last_error {
                LAST_ERROR_REQUESTED
            } else {
                0
            },
            flags,
            self.integers.len() as u64,
            self.doubles.len() as u64,
            module,
            u64::from(target),
        ];
        let arguments = self
            .integers
            .iter()
            .copied()
            .chain(self.doubles.iter().map(|double| double.to_bits()));
        for quad in header.into_iter().chain(arguments) {
            buffer.extend_from_slice(&quad.to_be_bytes());
        }

        if let Some(name) = self.module_name() {
            buffer.extend_from_slice(name.as_bytes());
            buffer.push(0);
        }

        buffer
    }

    fn outcome(&self, buffer: &[u8]) -> RpcOutcome {
        let quad = |offset: usize| {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&buffer[offset..offset + 8]);
            u64::from_be_bytes(bytes)
        };
        let value = quad(0x8);

        RpcOutcome {
            result: Hresult(quad(0) as u32),
            value: match self.return_kind {
                RpcReturnKind::Integer => RpcValue::Integer(value),
                RpcReturnKind::Double => RpcValue::Double(f64::from_bits(value)),
            },
            last_error: self
                .last_error
                .then(|| Win32Error::new(quad(0x10) as u32))
                .flatten(),
        }
    }
}

impl<T> Client<T, Connected>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    /// Run `rpc`: upload the call buffer and wait for the call to finish.
    pub async fn rpc(&mut self, call: &Rpc) -> Result<RpcOutcome, rootcause::Report<Error>> {
        if call.module_name().is_some_and(|name| name.contains('\0')) {
            return Err(report_argument(ArgumentError::ModuleNameContainsNul));
        }

        let size = call.buffer_size()?;
        let line = call.wire_line(size)?;
        let head = match self.send_raw(&line).await? {
            Response::SendBinary { head } => head,
            // A version mismatch is a 200 line with `error=`.
            Response::Line { head, .. } => {
                return Err(rootcause::Report::new(Error::RpcRejected { message: head }));
            }
            other => {
                return Err(rootcause::Report::new(Error::UnexpectedStatus {
                    expected: ExpectedShape::SendBinary204,
                    got: other.status(),
                }));
            }
        };

        // Eight hex digits without a prefix.
        let address = parse_kv_line(&head)
            .require("buf_addr")
            .and_then(|value| {
                u32::from_str_radix(value.as_str(), 16)
                    .map_err(|_| ParseError::InvalidHexDigits { key: "buf_addr" })
            })
            .map_err(report_parse)?;

        self.transport_mut()
            .write_all(&call.buffer(address, size))
            .await
            .map_err(Error::from)
            .into_report()
            .attach("sending rpc call buffer")?;

        self.read_post_upload_stream().await?;
        let (transport, engine) = self.split_streaming();
        let mut leftover = engine.take_inbox();
        let mut buffer = vec![0u8; size as usize];

        let read = async {
            let mut marker = [0u8; 8];
            loop {
                read_exact(transport, &mut leftover, &mut marker).await?;
                if marker == DONE_MARKER {
                    break;
                }
            }

            read_exact(transport, &mut leftover, &mut buffer).await
        };
        if let Err(error) = read.await {
            engine.abort_stream(None);
            return Err(error);
        }

        engine.end_stream(&leftover);

        Ok(call.outcome(&buffer))
    }
}

/// Fill `output` from `leftover`, then from `reader`.
async fn read_exact<R>(
    reader: &mut R,
    leftover: &mut BytesMut,
    output: &mut [u8],
) -> Result<(), rootcause::Report<Error>>
where
    R: AsyncRead + Unpin,
{
    let take = leftover.len().min(output.len());
    output[..take].copy_from_slice(&leftover.split_to(take));

    match reader.read_exact(&mut output[take..]).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            Err(rootcause::Report::new(Error::ConnectionClosed))
        }
        Err(error) => Err(rootcause::Report::new(Error::from(error)).attach("reading rpc stream")),
    }
}
