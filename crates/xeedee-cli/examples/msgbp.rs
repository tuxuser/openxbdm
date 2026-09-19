//! One-shot persistent-session breakpoint probe for the v2.0.1317.0
//! `Message()` decode bug (see docs/HISTORY.md's twelfth entry).
//!
//! Registers as the debugger, halts the system, arms an execution
//! breakpoint at a caller-supplied address, resumes, then polls every
//! live thread's `isstopped` until one reports halted at that address
//! (or a timeout elapses). On hit, dumps the integer register context
//! plus a memory preview at r29/r31 (this project's v5/v6), then clears
//! the breakpoint and continues the thread so the console isn't left
//! stuck.
//!
//! Usage: cargo run --example msgbp -- <host> <hex-address> [timeout-secs]

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

fn parse_u32(s: &str) -> u32 {
    let t = s.trim();
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u32::from_str_radix(h, 16).unwrap()
    } else {
        t.parse().unwrap()
    }
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let host = args.next().expect("usage: msgbp <host> <hex-address> [timeout-secs]");
    let target_addr = parse_u32(&args.next().expect("need address"));
    let timeout_secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(90);

    let transport = xeedee::transport::tokio::connect(host.as_str())
        .await
        .expect("connect");
    let mut client = xeedee::Client::new(transport)
        .read_banner()
        .await
        .expect("banner");

    println!("== debugger connect ==");
    client
        .run(Debugger {
            do_override: true,
            name: "claude-msgbp".to_owned(),
            user: "claude".to_owned(),
        })
        .await
        .expect("debugger connect");

    println!("== stop ==");
    if let Err(e) = client.run(Stop).await { println!("stop: {e:?} (tolerated - may already be stopped)"); }

    println!("== break ADDR={target_addr:#010x} ==");
    client
        .run(Breakpoint {
            address: target_addr,
            clear: false,
        })
        .await
        .expect("set breakpoint");

    println!("== go ==");
    client.run(Go).await.expect("go");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut hit: Option<ThreadId> = None;

    while std::time::Instant::now() < deadline {
        let threads = match client.run(Threads).await {
            Ok(t) => t,
            Err(e) => {
                println!("threads error: {e:?}");
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                continue;
            }
        };
        for tid in threads {
            match client.run(IsStopped { thread: tid }).await {
                Ok(StopState::Halted(reason)) => {
                    println!("thread {tid} halted: {reason:?}");
                    // xeedee's `parse_stop_reason` doesn't recognize this
                    // console's real breakpoint-hit wire text and falls
                    // back to `Other(String)` - confirmed 2026-09-13. Since
                    // only one breakpoint is ever armed at a time by this
                    // tool, treat any halt as the hit rather than matching
                    // the (never-constructed) `StopReason::Breakpoint`.
                    hit = Some(tid);
                }
                Ok(StopState::Running) => {}
                Err(e) => println!("isstopped({tid}) error: {e:?}"),
            }
        }
        if hit.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }

    let Some(tid) = hit else {
        println!("== TIMEOUT: breakpoint never hit within {timeout_secs}s ==");
        println!("== clearing breakpoint and leaving console running ==");
        let _ = client
            .run(Breakpoint {
                address: target_addr,
                clear: true,
            })
            .await;
        return;
    };

    println!("== HIT at thread {tid} ==");
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
    for r in &regs {
        println!("  {} = {}", r.name, r.value);
    }

    let find = |name: &str| -> Option<u32> {
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
    };

    let v5 = find("Gpr29");
    let v6 = find("Gpr31");
    println!("v5 (Gpr29, data ptr)  = {v5:#?}");
    println!("v6 (Gpr31, byte count) = {v6:#?}");

    if let Some(ptr) = v5 {
        if ptr != 0 {
            let len = v6.unwrap_or(64).clamp(1, 512);
            match client.run(GetMem { address: ptr, length: len }).await {
                Ok(snap) => {
                    println!("memory at v5 ({len} bytes):");
                    for chunk in snap.data.chunks(16) {
                        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
                        println!("  {}", hex.join(" "));
                    }
                    let text: String = snap
                        .data
                        .iter()
                        .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
                        .collect();
                    println!("  ascii: {text}");
                }
                Err(e) => println!("getmem error: {e:?}"),
            }
        }
    }

    println!("== clearing all breakpoints ==");
    let _ = client.run(ClearAllBreakpoints).await;

    println!("== continuing thread {tid} ==");
    let _ = client
        .run(Continue {
            thread: tid,
            single_step: false,
        })
        .await;

    println!("== done ==");
}
