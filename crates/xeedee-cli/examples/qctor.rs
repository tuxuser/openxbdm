//! Break at the entry of `sub_82240BE0` (Q's real constructor, found live
//! in docs/HISTORY.md's fourteenth entry - it calls `P`'s own vtable
//! slot 3 (`sub_82232908`, which unconditionally `memset`s P's entire
//! 32KB value buffer to zero *every time this runs*, then - only if not
//! still E_PENDING - calls some other object `a3`'s vtable+32 passing a
//! pointer derived from P's buffer). Capture the real arguments to find
//! `a3`'s identity/vtable and settle what that vtable+32 call actually
//! does - almost certainly the real content-fill step, or proof there
//! isn't one.
//!
//! Usage: cargo run --example qctor -- <host:730> [timeout-secs]

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

const Q_CTOR: u32 = 0x82240be0;

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

async fn dump(
    client: &mut xeedee::Client<xeedee::transport::tokio::TokioTransport, xeedee::Connected>,
    label: &str,
    addr: u32,
    len: u32,
) {
    if addr == 0 {
        println!("{label}: null");
        return;
    }
    match client.run(GetMem { address: addr, length: len }).await {
        Ok(snap) => {
            println!("{label} @ {addr:#010x}:");
            for (i, chunk) in snap.data.chunks(16).enumerate() {
                let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
                println!("  +{:#04x}: {}", i * 16, hex.join(" "));
            }
        }
        Err(e) => println!("{label}: getmem error {e:?}"),
    }
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let host = args.next().expect("usage: qctor <host> [timeout-secs]");
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
            name: "claude-qctor".to_owned(),
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
            address: Q_CTOR,
            clear: false,
        })
        .await
        .expect("set breakpoint");
    client.run(Go).await.expect("go");
    println!("== watching for Q construction at {Q_CTOR:#010x} ==");

    const P_VTABLE: u32 = 0x82018114;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let (tid, a1, a2, a3, a4) = loop {
        let Some(tid) = poll_any_halt(&mut client, deadline).await else {
            println!("== TIMEOUT: never hit the right instance ==");
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
        let a1 = find_reg(&regs, "Gpr3").unwrap_or(0);
        let a2 = find_reg(&regs, "Gpr4").unwrap_or(0);
        let a3 = find_reg(&regs, "Gpr5").unwrap_or(0);
        let a4 = find_reg(&regs, "Gpr6").unwrap_or(0);
        let p_vtable = if a4 != 0 && a4 != 0xffffffff {
            client
                .run(GetMem { address: a4, length: 4 })
                .await
                .ok()
                .and_then(|s| be32(&s.data, 0))
                .unwrap_or(0)
        } else {
            0
        };
        if p_vtable == P_VTABLE {
            println!("== HIT (matching P vtable): a1(new Q)={a1:#010x} a2={a2:#010x} a3={a3:#010x} a4(P)={a4:#010x} ==");
            break (tid, a1, a2, a3, a4);
        }
        println!("skipping unrelated hit: a4={a4:#010x} vtable={p_vtable:#010x}");
        let _ = client.run(Continue { thread: tid, single_step: false }).await;
        let _ = client.run(Go).await;
    };
    let _ = (a1, a3);

    dump(&mut client, "a2 object", a2, 0x60).await;

    let inner_a3 = if a2 != 0 {
        match client.run(GetMem { address: a2.wrapping_add(72), length: 4 }).await {
            Ok(snap) => be32(&snap.data, 0).unwrap_or(0),
            Err(_) => 0,
        }
    } else {
        0
    };
    println!("== *(a2+72) [the real 'a3' passed to P's vtable slot 3] = {inner_a3:#010x} ==");
    if inner_a3 != 0 {
        dump(&mut client, "a3 object (source of the vtable+32 call)", inner_a3, 0x40).await;
        let vtable = match client.run(GetMem { address: inner_a3, length: 4 }).await {
            Ok(snap) => be32(&snap.data, 0).unwrap_or(0),
            Err(_) => 0,
        };
        if vtable != 0 {
            dump(&mut client, "a3's vtable", vtable, 0x40).await;
            let slot32 = match client.run(GetMem { address: vtable.wrapping_add(32), length: 4 }).await {
                Ok(snap) => be32(&snap.data, 0).unwrap_or(0),
                Err(_) => 0,
            };
            println!("== a3 vtable slot at +32 (the method P's slot-3 calls) = {slot32:#010x} ==");
        }
    }

    dump(&mut client, "P object", a4, 0x60).await;
    dump(&mut client, "P+0x70 buffer (before this construction's memset)", a4.wrapping_add(0x70), 32).await;

    if let Err(e) = client.run(Stop).await {
        println!("stop (cleanup): {e:?} (tolerated)");
    }
    let _ = client.run(ClearAllBreakpoints).await;
    let _ = client.run(Continue { thread: tid, single_step: false }).await;
    let _ = client.run(Go).await;
    println!("== cleaned up, breakpoints cleared, console resumed ==");
}
