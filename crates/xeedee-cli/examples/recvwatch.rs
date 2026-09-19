//! Break at the entry of `sub_825C9F90` - the SBG-specific socket recv
//! wrapper found live in docs/HISTORY.md's fourteenth/fifteenth entries
//! (returns `SBG_E_*`-family codes directly, e.g. `0x80B10005` =
//! `SBG_E_CONNECTION_CLOSED`, and wraps a plain `recv()` via
//! `NetDll_recv`). Signature: `(a1=socket-wrapper-obj, a2=dest buffer,
//! a3=requested length, a4=out-param: bytes actually received)`.
//! Capture Lr (the real caller, never resolved statically - this
//! function has no direct call xrefs anywhere in the binary, only an
//! indirect vtable dispatch) and dump the destination buffer right
//! after the real recv() call returns, to see the actual raw bytes
//! pulled off the wire.
//!
//! Usage: cargo run --example recvwatch -- <host:730> [timeout-secs]

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

const RECV_ENTRY: u32 = 0x825c9f90;

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

fn dump_hex(data: &[u8]) {
    for chunk in data.chunks(16) {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        let text: String = chunk
            .iter()
            .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
            .collect();
        println!("  {:<48} |{}|", hex.join(" "), text);
    }
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
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    None
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let host = args.next().expect("usage: recvwatch <host> [timeout-secs] [hit-count]");
    let timeout_secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);
    let hit_count: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);

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
            name: "claude-recvwatch".to_owned(),
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
            address: RECV_ENTRY,
            clear: false,
        })
        .await
        .expect("set breakpoint");
    client.run(Go).await.expect("go");
    println!("== watching sbg recv wrapper entry at {RECV_ENTRY:#010x}, will dump {hit_count} hits ==");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut seen = 0u32;

    while seen < hit_count {
        let Some(tid) = poll_any_halt(&mut client, deadline).await else {
            println!("== TIMEOUT after {seen} hits ==");
            break;
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
        let a1 = find_reg(&regs, "Gpr3").unwrap_or(0);
        let a2 = find_reg(&regs, "Gpr4").unwrap_or(0);
        let a3 = find_reg(&regs, "Gpr5").unwrap_or(0);
        let a4 = find_reg(&regs, "Gpr6").unwrap_or(0);
        let lr = find_reg(&regs, "Lr").unwrap_or(0);
        seen += 1;
        println!(
            "== hit #{seen}: caller(Lr)={lr:#010x} a1(sock-obj)={a1:#010x} a2(buf)={a2:#010x} a3(len)={a3} a4(out-len-ptr)={a4:#010x} =="
        );
        if a2 != 0 {
            let peek_len = (a3.min(64)).max(4);
            if let Ok(snap) = client.run(GetMem { address: a2, length: peek_len }).await {
                println!("  dest buffer BEFORE recv (garbage/stale expected):");
                dump_hex(&snap.data);
            }
        }
        // Single-step past this call isn't easily done at the C level;
        // just continue and let it run - the interesting data is the
        // *destination buffer* after this returns, which the next
        // Message()/completion-side breakpoint already covers. Here we
        // mainly want the caller address and confirm this really fires
        // for our Get.
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
