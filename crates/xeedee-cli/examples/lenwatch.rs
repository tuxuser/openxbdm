//! Chase the v2.0.1317.0 `Message()` empty-buffer mystery (docs/HISTORY.md's
//! thirteenth entry) one level further: catch the *construction* of a
//! `PostMessage`-response object (vtable `0x82018114`) via an execution
//! breakpoint at its constructor's return (`sub_82233150`, returns at
//! `0x822331AC` with the new object's address already in r3), then arm a
//! WRITE data breakpoint on that specific instance's length field
//! (`this+0x8070` - confirmed live: the object's embedded value buffer
//! sits at `this+0x70`, a fixed 0x8000-byte capacity, with a 4-byte
//! length immediately after it at `this+0x8070`; the constructor zeroes
//! it and it is the *only* other place in the whole binary touching that
//! exact offset). If nothing ever writes it, the buffer is provably never
//! filled in - not truncated, not corrupted, never touched after
//! construction.
//!
//! Usage: cargo run --example lenwatch -- <host:730> [timeout-secs]

use xeedee::commands::debug::Breakpoint;
use xeedee::commands::debug::ClearAllBreakpoints;
use xeedee::commands::debug::Continue;
use xeedee::commands::debug::ContextGroups;
use xeedee::commands::debug::DataBreakKind;
use xeedee::commands::debug::DataBreakpoint;
use xeedee::commands::debug::Debugger;
use xeedee::commands::debug::GetContext;
use xeedee::commands::debug::Go;
use xeedee::commands::debug::IsStopped;
use xeedee::commands::debug::Stop;
use xeedee::commands::debug::StopState;
use xeedee::commands::process::ThreadId;
use xeedee::commands::process::Threads;

const CTOR_RET: u32 = 0x822331ac;
const LEN_OFFSET: u32 = 0x8070;

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
            // 0q values can be full 64-bit; keep only the low 32 bits.
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
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    None
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let host = args.next().expect("usage: lenwatch <host> [timeout-secs]");
    let timeout_secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(120);

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
            name: "claude-lenwatch".to_owned(),
            user: "claude".to_owned(),
        })
        .await
        .expect("debugger connect");
    println!("== debugger connect ==");

    if let Err(e) = client.run(Stop).await { println!("stop: {e:?} (tolerated - may already be stopped)"); }
    client
        .run(Breakpoint {
            address: CTOR_RET,
            clear: false,
        })
        .await
        .expect("set ctor breakpoint");
    client.run(Go).await.expect("go");
    println!("== watching for PostMessage-response construction at {CTOR_RET:#010x} ==");

    let overall_deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

    let Some(tid) = poll_any_halt(&mut client, overall_deadline).await else {
        println!("== TIMEOUT: constructor never hit - no PostMessage response object built at all ==");
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
    let obj = find_reg(&regs, "Gpr3").expect("Gpr3");
    let len_addr = obj.wrapping_add(LEN_OFFSET);
    println!("== constructed object @ {obj:#010x}, watching length field @ {len_addr:#010x} ==");

    // Swap the exec breakpoint for a write watchpoint on this instance's
    // length field, then let this thread (and everyone else) run.
    if let Err(e) = client.run(Stop).await { println!("stop (rearm): {e:?} (tolerated)"); }
    client
        .run(Breakpoint {
            address: CTOR_RET,
            clear: true,
        })
        .await
        .expect("clear ctor breakpoint");
    client
        .run(DataBreakpoint {
            address: len_addr,
            size: 4,
            kind: DataBreakKind::Write,
            clear: false,
        })
        .await
        .expect("set data breakpoint");
    let _ = client.run(Continue { thread: tid, single_step: false }).await;
    client.run(Go).await.expect("go (resume)");
    println!("== resumed, watching for a write to the length field ==");

    match poll_any_halt(&mut client, overall_deadline).await {
        Some(tid2) => {
            let regs2 = client
                .run(GetContext {
                    thread: tid2,
                    groups: ContextGroups {
                        control: true,
                        integer: true,
                        floating_point: false,
                        vector: false,
                    },
                })
                .await
                .expect("getcontext");
            println!("== HIT: something wrote the length field (thread {tid2}) ==");
            for r in &regs2 {
                println!("  {} = {}", r.name, r.value);
            }
        }
        None => {
            println!(
                "== TIMEOUT: length field @ {len_addr:#010x} was NEVER written in {timeout_secs}s - \
                 the buffer is provably abandoned at its constructed value (zero), not filled and not truncated =="
            );
        }
    }

    let _ = client.run(Stop).await;
    let _ = client.run(ClearAllBreakpoints).await;
    let _ = client.run(Go).await;
    println!("== cleaned up, breakpoints cleared, console resumed ==");
}
