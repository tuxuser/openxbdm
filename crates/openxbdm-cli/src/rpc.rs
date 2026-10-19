//! `rpc` subcommands.

use futures_io::AsyncRead;
use futures_io::AsyncWrite;
use openxbdm_client::Client;
use openxbdm_client::Connected;
use openxbdm_client::commands::Rpc;
use openxbdm_client::commands::RpcContext;
use openxbdm_client::commands::RpcReturnKind;
use openxbdm_client::commands::RpcTarget;
use openxbdm_client::commands::RpcThread;
use openxbdm_client::commands::RpcValue;
use openxbdm_client::error::Error;
use rootcause::Report;

use crate::args::RpcCommand;

pub async fn rpc<T>(
    client: &mut Client<T, Connected>,
    command: RpcCommand,
) -> Result<(), Report<Error>>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let RpcCommand::Call {
        address,
        module,
        ordinal,
        integers,
        doubles,
        system,
        processor,
        thread,
        double_result,
        last_error,
    } = command;

    // clap requires `--address` or both `--module` and `--ordinal`.
    let target = match (address, module, ordinal) {
        (Some(address), None, None) => RpcTarget::Address(address),
        (None, Some(module), Some(ordinal)) => RpcTarget::Export { module, ordinal },
        _ => unreachable!("clap enforces the target arguments"),
    };

    let call = Rpc {
        context: if system {
            RpcContext::System
        } else {
            RpcContext::Title
        },
        thread: match thread {
            Some(name) => RpcThread::Named(name),
            None => RpcThread::Own { processor },
        },
        target,
        integers,
        doubles,
        return_kind: if double_result {
            RpcReturnKind::Double
        } else {
            RpcReturnKind::Integer
        },
        last_error,
    };

    let outcome = client.rpc(&call).await?;
    print!("result={:#010x}", outcome.result.0);
    match outcome.value {
        RpcValue::Integer(value) => print!(" value={value:#018x}"),
        RpcValue::Double(value) => print!(" value={value}"),
    }
    if last_error {
        let code = outcome.last_error.map_or(0, |error| error.code());
        print!(" lasterr={code:#010x}");
    }
    println!();

    Ok(())
}
