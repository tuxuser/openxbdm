//! Break at the entry of `sub_82240290` (the real status/completion
//! propagation function found live in docs/HISTORY.md's fourteenth
//! entry - it copies `Q+72` into `P+72` where `P = *(Q+76)`, and calls an
//! optional callback at `Q+68` with arg `Q+64` beforehand). Read `Q`
//! (=Gpr3 at entry) and dump its first 0x60 bytes to find the real
//! callback function pointer at Q+68 and its argument at Q+64, so they
//! can be decompiled directly instead of guessed at.
//!
//! Usage: cargo run --example qwatch -- <host:730> [timeout-secs]

use xeedee::commands::debug::Breakpoint;
use xeedee::commands::debug::ClearAllBreakpoints;
use xeedee::commands::debug::Continue;
use xeedee::commands::debug::ContextGroups;
use xeedee::commands::debug::Debugger;
use xeedee::commands::debug::GetContext;
use xeedee::commands::debug::Go;
use xeedee::commands::debug::IsStopped;
use xeedee::commands::debug::Stop;
use xeedee::commands::debug::StopState;
use xeedee::commands::memory::GetMem;
use xeedee::commands::process::ThreadId;
use xeedee::commands::process::Threads;

const COMPLETE_ENTRY: u32 = 0x82240290;

fn find_reg(regs: &[xeedee::commands::debug::RegisterValue], name: &str) -> Option<u32> {
    regs.iter()
        .find(|r| r.name.eq_ignore_ascii_case(name))
        .and_then(|r| {
            let v = r
                .value
                .trim_start_matches("0x")
                .trim_start_matches("0X")
                .trim_start_matches("0q")
                .trim_start_matches("0Q");
            let v = if v.len() > 8 { &v[v.len() - 8..] } else { v };
            u64::from_str_radix(v, 16).ok().map(|x| x as u32)
        })
}

fn be32(bytes: &[u8], off: usize) -> Option<u32> {
    let b = bytes.get(off..off + 4)?;
    Some(u32::from_be_bytes(b.try_into().ok()?))
}

async fn poll_any_halt(
    client: &mut xeedee::Client<xeedee::transport::tokio::TokioTransport, xeedee::Connected>,
    deadline: std::time::Instant,
) -> Option<ThreadId> {
    while std::time::Instant::now() < deadline {
        if let Ok(threads) = client.run(Threads).await {
            for tid in threads {
                if let Ok(StopState::Halted(_)) = client.run(IsStopped { thread: tid }).await {
                    return Some(tid);
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    None
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let host = args.next().expect("usage: qwatch <host> [timeout-secs]");
    let timeout_secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(90);

    let transport = xeedee::transport::tokio::connect(host.as_str())
        .await
        .expect("connect");
    let mut client = xeedee::Client::new(transport)
        .read_banner()
        .await
        .expect("banner");

    client
        .run(Debugger {
            do_override: true,
            name: "claude-qwatch".to_owned(),
            user: "claude".to_owned(),
        })
        .await
        .expect("debugger connect");
    println!("== debugger connect ==");

    if let Err(e) = client.run(Stop).await {
        println!("stop: {e:?} (tolerated)");
    }
    client
        .run(Breakpoint {
            address: COMPLETE_ENTRY,
            clear: false,
        })
        .await
        .expect("set breakpoint");
    client.run(Go).await.expect("go");
    println!("== watching for completion-propagation entry at {COMPLETE_ENTRY:#010x} ==");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let Some(tid) = poll_any_halt(&mut client, deadline).await else {
        println!("== TIMEOUT: never hit ==");
        if let Err(e) = client.run(Stop).await {
            println!("stop: {e:?} (tolerated)");
        }
        let _ = client.run(ClearAllBreakpoints).await;
        let _ = client.run(Go).await;
        return;
    };

    let regs = client
        .run(GetContext {
            thread: tid,
            groups: ContextGroups {
                control: true,
                integer: true,
                floating_point: false,
                vector: false,
            },
        })
        .await
        .expect("getcontext");
    let q = find_reg(&regs, "Gpr3").expect("Gpr3 (Q)");
    println!("== HIT: Q = {q:#010x} ==");

    let q_dump = client
        .run(GetMem {
            address: q,
            length: 0x60,
        })
        .await
        .expect("getmem Q");
    println!("Q object (first 0x60 bytes):");
    for (i, chunk) in q_dump.data.chunks(16).enumerate() {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        println!("  +{:#04x}: {}", i * 16, hex.join(" "));
    }
    let callback = be32(&q_dump.data, 68).unwrap_or(0);
    let callback_arg = be32(&q_dump.data, 64).unwrap_or(0);
    let p = be32(&q_dump.data, 76).unwrap_or(0);
    let q_status = be32(&q_dump.data, 72).unwrap_or(0);
    println!("== Q+64 (callback arg) = {callback_arg:#010x} ==");
    println!("== Q+68 (callback fn)  = {callback:#010x} ==");
    println!("== Q+72 (Q's status)   = {q_status:#010x} ==");
    println!("== Q+76 (P pointer)    = {p:#010x} ==");

    if p != 0 {
        let p_dump = client
            .run(GetMem {
                address: p,
                length: 0x60,
            })
            .await
            .expect("getmem P");
        println!("P object (first 0x60 bytes, BEFORE status propagation):");
        for (i, chunk) in p_dump.data.chunks(16).enumerate() {
            let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
            println!("  +{:#04x}: {}", i * 16, hex.join(" "));
        }
        let buf_dump = client
            .run(GetMem {
                address: p.wrapping_add(0x70),
                length: 64,
            })
            .await
            .expect("getmem P buffer");
        println!("P+0x70 embedded buffer, first 64 bytes (BEFORE this completion runs):");
        for chunk in buf_dump.data.chunks(16) {
            let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
            println!("  {}", hex.join(" "));
        }
        let len_dump = client
            .run(GetMem {
                address: p.wrapping_add(0x8070),
                length: 4,
            })
            .await
            .expect("getmem P length");
        println!(
            "P+0x8070 length field (BEFORE): {}",
            be32(&len_dump.data, 0).unwrap_or(0xffffffff)
        );
    }

    if let Err(e) = client.run(Stop).await {
        println!("stop (cleanup): {e:?} (tolerated)");
    }
    let _ = client.run(ClearAllBreakpoints).await;
    let _ = client
        .run(Continue {
            thread: tid,
            single_step: false,
        })
        .await;
    let _ = client.run(Go).await;
    println!("== cleaned up, breakpoints cleared, console resumed ==");
}
