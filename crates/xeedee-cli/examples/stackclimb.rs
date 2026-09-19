//! Follow-up to configfill.rs: that probe proved the `Get` completion
//! object's buffer/length are never filled with real content (only zeroed,
//! once, by `sub_82232908` - the object's own vtable slot 3, "dispatch"),
//! but every caller in this codebase is reached through indirect/vtable
//! calls that IDA can't resolve statically (confirmed dead end: no direct
//! xrefs to sub_82232908, sub_821BE388, GetBuffer/GetLength, etc. - only
//! .pdata metadata). This probe climbs the real call stack live instead:
//! break at a known leaf (NetDll_recv's real SBG caller, or
//! sub_82232908's entry), then chain return-breakpoints upward through
//! Lr at each halt to reveal each caller's address, which can then be
//! decompiled offline in tools/ida-1317/.
//!
//! Usage: cargo run --example stackclimb -- <host:730> [timeout-secs] [levels]

use std::collections::HashMap;
use std::collections::HashSet;

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
use xeedee::commands::process::ThreadId;
use xeedee::commands::process::Threads;

const RECV_ENTRY: u32 = 0x8295543c;
const DISPATCH_ENTRY: u32 = 0x82232908;
const WRAPPER_ENTRY: u32 = 0x82240be0;
const POST_ENTRY: u32 = 0x82241610;
const RECV_WRAPPER_ENTRY: u32 = 0x825c9f90;
const QUEUE_PUMP_ENTRY: u32 = 0x822325e0;
const FRAME_PROCESS_ENTRY: u32 = 0x825c6088;
const FRAME_PROCESS_RET_NOTFOUND: u32 = 0x825c60a8;

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
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    None
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let host = args.next().expect("usage: stackclimb <host> [timeout-secs] [levels]");
    let timeout_secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(90);
    let max_levels: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(4);

    let connect_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut client = loop {
        let attempt = async {
            let transport = xeedee::transport::tokio::connect(host.as_str()).await?;
            let mut c = xeedee::Client::new(transport).read_banner().await?;
            c.run(Debugger {
                do_override: true,
                name: "claude-stackclimb".to_owned(),
                user: "claude".to_owned(),
            })
            .await?;
            Ok::<_, rootcause::Report<xeedee::Error>>(c)
        }
        .await;
        match attempt {
            Ok(c) => break c,
            Err(e) => {
                if std::time::Instant::now() >= connect_deadline {
                    panic!("could not connect+attach after retries: {e:?}");
                }
                println!("connect/attach failed (will retry): {e:?}");
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    };
    println!("== debugger connect ==");

    if let Err(e) = client.run(Stop).await {
        println!("stop: {e:?} (tolerated)");
    }

    for (addr, label) in [
        (RECV_ENTRY, "recv-entry"),
        (DISPATCH_ENTRY, "dispatch-entry"),
        (WRAPPER_ENTRY, "wrapper-entry"),
        (POST_ENTRY, "post-entry"),
        (RECV_WRAPPER_ENTRY, "recv-wrapper-entry"),
        (QUEUE_PUMP_ENTRY, "queue-pump-entry"),
    ] {
        let mut armed = false;
        for attempt in 0..60 {
            match client.run(Breakpoint { address: addr, clear: false }).await {
                Ok(()) => {
                    armed = true;
                    println!("== armed {label} bp at {addr:#010x} on attempt {attempt} ==");
                    break;
                }
                Err(e) => {
                    if attempt == 0 {
                        println!("arm {label} attempt failed (will retry): {e:?}");
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
            }
        }
        if !armed {
            panic!("could not arm {label} breakpoint after retries");
        }
    }
    client.run(Go).await.expect("go");
    println!("== watching recv={RECV_ENTRY:#010x} dispatch={DISPATCH_ENTRY:#010x}, climbing up to {max_levels} levels each ==");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    // Per-thread climb state: (root label, levels climbed so far).
    let mut climbing: HashMap<ThreadId, (&'static str, u32)> = HashMap::new();
    let mut armed_returns: HashSet<u32> = HashSet::new();
    let mut recv_hits = 0u32;
    let mut dispatch_hits = 0u32;

    loop {
        if std::time::Instant::now() >= deadline {
            println!("== TIMEOUT ==");
            break;
        }
        let Some(tid) = poll_any_halt(&mut client, deadline).await else {
            println!("== TIMEOUT (no halt) ==");
            break;
        };
        let regs = client
            .run(GetContext {
                thread: tid,
                groups: ContextGroups { control: true, integer: true, floating_point: false, vector: false },
            })
            .await
            .expect("getcontext");
        let iar = find_reg(&regs, "Iar").unwrap_or(0);
        let lr = find_reg(&regs, "Lr").unwrap_or(0);
        let gpr3 = find_reg(&regs, "Gpr3").unwrap_or(0);
        let gpr4 = find_reg(&regs, "Gpr4").unwrap_or(0);
        let gpr5 = find_reg(&regs, "Gpr5").unwrap_or(0);
        let gpr6 = find_reg(&regs, "Gpr6").unwrap_or(0);

        if iar == RECV_ENTRY {
            // Only the real SBG channel requests a large buffer (up to
            // 61440 bytes); the SNTP red herring always asks for 48.
            if gpr6 > 256 {
                recv_hits += 1;
                println!(
                    "== recv-entry hit #{recv_hits}: sock={gpr4:#010x} buf={gpr5:#010x} len={gpr6} caller(Lr)={lr:#010x} =="
                );
                if lr != 0 && armed_returns.insert(lr) {
                    client.run(Breakpoint { address: lr, clear: false }).await.expect("arm return bp");
                    climbing.insert(tid, ("recv", 0));
                    println!("  -- climbing from recv, level 0, armed return bp at {lr:#010x} --");
                }
            }
        } else if iar == WRAPPER_ENTRY {
            println!(
                "== wrapper-entry hit: a1={gpr3:#010x} a2={gpr4:#010x} a3={gpr5:#010x} a4={gpr6:#010x} caller(Lr)={lr:#010x} (fresh entry - Lr is reliable here) =="
            );
        } else if iar == POST_ENTRY {
            println!(
                "== post-entry hit: a1(scheduler)={gpr3:#010x} a2(msg)={gpr4:#010x} caller(Lr)={lr:#010x} (fresh entry - Lr is reliable here) =="
            );
        } else if iar == RECV_WRAPPER_ENTRY {
            println!(
                "== recv-wrapper-entry hit: a1(transport)={gpr3:#010x} a2(buf)={gpr4:#010x} a3(len)={gpr5:#010x} caller(Lr)={lr:#010x} (fresh entry - Lr is reliable here) =="
            );
        } else if iar == QUEUE_PUMP_ENTRY {
            println!(
                "== queue-pump-entry hit: a1={gpr3:#010x} a2={gpr4:#010x} caller(Lr)={lr:#010x} (fresh entry - Lr is reliable here) =="
            );
        } else if iar == DISPATCH_ENTRY {
            dispatch_hits += 1;
            println!("== dispatch-entry hit #{dispatch_hits}: P(this)={gpr3:#010x} caller(Lr)={lr:#010x} ==");
            if lr != 0 && armed_returns.insert(lr) {
                client.run(Breakpoint { address: lr, clear: false }).await.expect("arm return bp");
                climbing.insert(tid, ("dispatch", 0));
                println!("  -- climbing from dispatch, level 0, armed return bp at {lr:#010x} --");
            }
        } else if armed_returns.contains(&iar) {
            let (root, level) = climbing.get(&tid).copied().unwrap_or(("?", 0));
            println!(
                "== [{root}] climb level {level} halted at {iar:#010x}, its own Lr(caller)={lr:#010x} gpr3={gpr3:#010x} gpr4={gpr4:#010x} =="
            );
            let next_level = level + 1;
            if next_level < max_levels && lr != 0 && armed_returns.insert(lr) {
                client.run(Breakpoint { address: lr, clear: false }).await.expect("arm next return bp");
                climbing.insert(tid, (root, next_level));
                println!("  -- climbing further, level {next_level}, armed return bp at {lr:#010x} --");
            } else {
                println!("  -- stopping climb for this thread (max levels or dup Lr) --");
                climbing.remove(&tid);
            }
        } else {
            println!("-- unexpected halt at iar={iar:#010x} lr={lr:#010x} on tid={tid:?}, continuing --");
        }

        let _ = client.run(Continue { thread: tid, single_step: false }).await;
        let _ = client.run(Go).await;
    }

    if let Err(e) = client.run(Stop).await {
        println!("stop (cleanup): {e:?} (tolerated)");
    }
    let _ = client.run(ClearAllBreakpoints).await;
    let _ = client.run(Go).await;
    println!("== cleaned up, breakpoints cleared, console resumed ==");
}
