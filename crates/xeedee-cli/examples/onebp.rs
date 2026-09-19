//! Minimal single-breakpoint probe: arm exactly once (no re-arm loop),
//! wait for a halt, dump context + surrounding memory, clear, done.
//! Written to rule out re-arm-loop corruption seen with `sbgrecord`.
//!
//! Usage: cargo run --example onebp -- <host:730> <hex address> [timeout-secs]

use xeedee::commands::debug::{
    Breakpoint, ClearAllBreakpoints, ContextGroups, Continue, Debugger, GetContext, Go, IsStopped,
    Stop, StopState,
};
use xeedee::commands::memory::GetMem;
use xeedee::commands::process::{ThreadId, Threads};

fn find_reg(regs: &[xeedee::commands::debug::RegisterValue], name: &str) -> Option<u32> {
    regs.iter()
        .find(|r| r.name.eq_ignore_ascii_case(name))
        .and_then(|r| {
            let value = r
                .value
                .trim_start_matches("0x")
                .trim_start_matches("0X")
                .trim_start_matches("0q")
                .trim_start_matches("0Q");
            let value = if value.len() > 8 {
                &value[value.len() - 8..]
            } else {
                value
            };
            u64::from_str_radix(value, 16).ok().map(|x| x as u32)
        })
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let host = args.next().expect("usage: onebp <host:730> <hex addr> [timeout-secs]");
    let addr_str = args.next().expect("need address");
    let address = u32::from_str_radix(addr_str.trim_start_matches("0x"), 16).expect("bad hex address");
    let timeout_secs = args.next().and_then(|s| s.parse().ok()).unwrap_or(60u64);

    let transport = xeedee::transport::tokio::connect(host.as_str()).await.expect("connect");
    let mut client = xeedee::Client::new(transport).read_banner().await.expect("banner");
    client
        .run(Debugger { do_override: true, name: "onebp".to_owned(), user: "codex".to_owned() })
        .await
        .expect("debugger connect");
    println!("== debugger connected ==");

    println!("before-arm bytes:");
    if let Ok(mem) = client.run(GetMem { address, length: 16 }).await {
        println!("  {:02x?}", mem.data);
    }

    let _ = client.run(Stop).await;
    match client.run(Breakpoint { address, clear: false }).await {
        Ok(()) => println!("armed {address:#010x} (once, no re-arm)"),
        Err(e) => println!("arm failed: {e:?}"),
    }
    client.run(Go).await.expect("resume");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut hit_thread: Option<ThreadId> = None;
    while std::time::Instant::now() < deadline {
        if let Ok(threads) = client.run(Threads).await {
            for thread in threads {
                if let Ok(StopState::Halted(reason)) = client.run(IsStopped { thread }).await {
                    println!("HALT thread={thread:?} reason={reason:?}");
                    hit_thread = Some(thread);
                    break;
                }
            }
        }
        if hit_thread.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    if let Some(thread) = hit_thread {
        let regs = client
            .run(GetContext {
                thread,
                groups: ContextGroups { control: true, integer: true, floating_point: false, vector: false },
            })
            .await
            .expect("getcontext");
        let iar = find_reg(&regs, "Iar").unwrap_or(0);
        let lr = find_reg(&regs, "Lr").unwrap_or(0);
        println!("iar={iar:#010x} lr={lr:#010x}");
        for i in 3..=10 {
            if let Some(v) = find_reg(&regs, &format!("Gpr{i}")) {
                println!("  r{i}={v:#010x}");
            }
        }
        let _ = client.run(Continue { thread, single_step: false }).await;
    } else {
        println!("no halt within {timeout_secs}s");
    }

    println!("after bytes (still armed or post-continue):");
    if let Ok(mem) = client.run(GetMem { address, length: 16 }).await {
        println!("  {:02x?}", mem.data);
    }

    let _ = client.run(Stop).await;
    let _ = client.run(ClearAllBreakpoints).await;

    println!("after-clear bytes:");
    if let Ok(mem) = client.run(GetMem { address, length: 16 }).await {
        println!("  {:02x?}", mem.data);
    }

    let _ = client.run(Go).await;
    println!("== done ==");
}
