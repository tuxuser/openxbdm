//! Consolidated probe: arm breakpoints on BOTH real socket-recv thunks
//! at once (`NetDll_recv` @ 0x8295543c, `NetDll_WSARecv` @ 0x8295544c -
//! the `__imp_*` addresses found first are data import-table slots, not
//! code, and can't take an execution breakpoint) and log every hit
//! (which one, the caller via Lr, and args) without needing a relaunch
//! per hypothesis. Lets one boot answer "does SBG use blocking recv or
//! overlapped WSARecv, and who calls it" in a single pass.
//!
//! Usage: cargo run --example allrecv -- <host:730> [timeout-secs] [hit-count]

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

const RECV: u32 = 0x8295543c;
const WSARECV: u32 = 0x8295544c;

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
        println!("    {:<48} |{}|", hex.join(" "), text);
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
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    None
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let host = args.next().expect("usage: allrecv <host> [timeout-secs] [hit-count]");
    let timeout_secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(90);
    let hit_count: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);

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
            name: "claude-allrecv".to_owned(),
            user: "claude".to_owned(),
        })
        .await
        .expect("debugger connect");
    println!("== debugger connect ==");

    if let Err(e) = client.run(Stop).await {
        println!("stop: {e:?} (tolerated)");
    }
    client.run(Breakpoint { address: RECV, clear: false }).await.expect("set recv bp");
    client.run(Breakpoint { address: WSARECV, clear: false }).await.expect("set wsarecv bp");
    client.run(Go).await.expect("go");
    println!("== watching recv={RECV:#010x} wsarecv={WSARECV:#010x}, up to {hit_count} hits ==");

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
        let iar = find_reg(&regs, "Iar").unwrap_or(0);
        let lr = find_reg(&regs, "Lr").unwrap_or(0);
        let which = if iar == RECV {
            "recv"
        } else if iar == WSARECV {
            "wsarecv"
        } else {
            "other"
        };
        let a1 = find_reg(&regs, "Gpr3").unwrap_or(0);
        let a2 = find_reg(&regs, "Gpr4").unwrap_or(0);
        let a3 = find_reg(&regs, "Gpr5").unwrap_or(0);
        let a4 = find_reg(&regs, "Gpr6").unwrap_or(0);
        seen += 1;
        println!(
            "== hit #{seen} [{which}] iar={iar:#010x} caller(Lr)={lr:#010x} a1={a1:#010x} a2={a2:#010x} a3={a3:#010x} a4={a4:#010x} =="
        );
        // For recv(sock, buf, len, flags): a2=buf. For WSARecv(sock,
        // lpBuffers,...): a2=WSABUF array ptr, dump that too.
        if which == "recv" && a2 != 0 {
            if let Ok(snap) = client.run(GetMem { address: a2, length: a3.min(48).max(4) }).await {
                println!("  buf ptr (pre-recv, likely stale):");
                dump_hex(&snap.data);
            }
        }
        if which == "wsarecv" && a2 != 0 {
            if let Ok(snap) = client.run(GetMem { address: a2, length: 8 }).await {
                println!("  WSABUF[0] {{len, ptr}}:");
                dump_hex(&snap.data);
            }
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
