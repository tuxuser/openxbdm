//! Cleanup helper: register as debugger, stop, clear every execution
//! breakpoint, go. `bp --clear` as a one-shot xeedee-cli connection
//! returns `400-` from real XBDM (confirmed 2026-09-13) - breakpoint
//! set/clear genuinely requires an active `debugger connect` session,
//! same as arming one does (see msgbp.rs).
//!
//! Usage: cargo run --example clearbp -- <host>

#[tokio::main]
async fn main() {
    let host = std::env::args().nth(1).expect("usage: clearbp <host>");
    let transport = xeedee::transport::tokio::connect(host.as_str())
        .await
        .expect("connect");
    let mut client = xeedee::Client::new(transport)
        .read_banner()
        .await
        .expect("banner");

    client
        .run(xeedee::commands::debug::Debugger {
            do_override: true,
            name: "claude-clearbp".to_owned(),
            user: "claude".to_owned(),
        })
        .await
        .expect("debugger connect");
    println!("debugger connect ok");

    match client.run(xeedee::commands::debug::Stop).await {
        Ok(()) => println!("stop ok"),
        Err(err) if format!("{err:?}").contains("already stopped") => {
            println!("console already stopped")
        }
        Err(err) => panic!("stop: {err:?}"),
    }

    client
        .run(xeedee::commands::debug::ClearAllBreakpoints)
        .await
        .expect("clearall");
    println!("clearall ok");

    for address in [0x825c69c0, 0x825c6a1c, 0x825c6a24, 0x825c6628, 0x825c9ea0, 0x825c9e30] {
        match client
            .run(xeedee::commands::debug::Breakpoint { address, clear: true })
            .await
        {
            Ok(()) => println!("cleared {address:#010x}"),
            Err(err) => println!("clear {address:#010x}: {err:?}"),
        }
    }

    client.run(xeedee::commands::debug::Go).await.expect("go");
    println!("go ok");

    if let Ok(threads) = client.run(xeedee::commands::process::Threads).await {
        for thread in threads {
            if let Ok(xeedee::commands::debug::StopState::Halted(reason)) = client
                .run(xeedee::commands::debug::IsStopped { thread })
                .await
            {
                println!("halted {thread:?}: {reason:?}");
            }
        }
    }
}
