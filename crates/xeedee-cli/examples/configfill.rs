//! Live probe for the still-open "Store.Config Value ends up empty" bug
//! (docs/HISTORY.md, 2026-09-13 + 2026-09-15 entries). Static disassembly of
//! `tools/ida-1317/` found the `Get`-result completion object's layout:
//! `GetBuffer()` (vtable slot 5, 0x82232758) returns `this+0x70` (an inline
//! ~32 KiB buffer), `GetLength()` (vtable slot 6, 0x82232760) returns
//! `*(u32*)(this+0x8070)` (a trailing length field zeroed at construction by
//! `sub_82233150`, 0x82233150). A binary-wide search for any other
//! instruction writing that literal offset found none - this probe checks
//! that live: break at the constructor to learn a real object's address,
//! then arm WRITE data breakpoints on both fields and see whether anything
//! writes them before `Message()` (the Lua binding, entry 0x821BE388) reads
//! them.
//!
//! Usage: cargo run --example configfill -- <host:730> [timeout-secs]

use std::collections::HashSet;

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
use xeedee::commands::memory::GetMem;
use xeedee::commands::process::ThreadId;
use xeedee::commands::process::Threads;

const CTOR: u32 = 0x82233150;
const GET_LENGTH: u32 = 0x82232760;
const GET_BUFFER: u32 = 0x82232758;
const MESSAGE_ENTRY: u32 = 0x821BE388;
const BUF_OFF: u32 = 0x70;
const LEN_OFF: u32 = 0x8070;

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
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    None
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let host = args.next().expect("usage: configfill <host> [timeout-secs]");
    let timeout_secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(90);

    let connect_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut client = loop {
        let attempt = async {
            let transport = xeedee::transport::tokio::connect(host.as_str()).await?;
            let mut c = xeedee::Client::new(transport).read_banner().await?;
            c.run(Debugger {
                do_override: true,
                name: "claude-configfill".to_owned(),
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

    // Arm the constructor + Message() breakpoints, retrying since a
    // magicboot can still be settling the fresh title image.
    for (addr, label) in [(CTOR, "ctor"), (MESSAGE_ENTRY, "Message()"), (GET_LENGTH, "GetLength"), (GET_BUFFER, "GetBuffer")] {
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
    println!("== watching ctor={CTOR:#010x} Message={MESSAGE_ENTRY:#010x} GetLength={GET_LENGTH:#010x} GetBuffer={GET_BUFFER:#010x} ==");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut watched: HashSet<u32> = HashSet::new();
    let mut ctor_hits = 0u32;

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

        if iar == CTOR {
            ctor_hits += 1;
            let this_ptr = find_reg(&regs, "Gpr3").unwrap_or(0);
            println!("== ctor hit #{ctor_hits}: P={this_ptr:#010x} caller(Lr)={lr:#010x} ==");
            if watched.len() < 2 {
                for (off, label) in [(BUF_OFF, "buffer"), (LEN_OFF, "length")] {
                    let addr = this_ptr.wrapping_add(off);
                    match client
                        .run(DataBreakpoint { address: addr, size: 4, kind: DataBreakKind::Write, clear: false })
                        .await
                    {
                        Ok(()) => {
                            watched.insert(addr);
                            println!("  -- armed WRITE watch on {label} @ {addr:#010x} (P+{off:#x}) --");
                        }
                        Err(e) => println!("  -- failed to arm watch on {label} @ {addr:#010x}: {e:?} --"),
                    }
                }
            } else {
                println!("  (already watching 2 fields, not adding more)");
            }
        } else if watched.contains(&iar) {
            println!("== DATA WRITE hit @ {iar:#010x} (this is the DAR-matched address) caller-ish Lr={lr:#010x} ==");
            for (name, val) in [("Iar", iar), ("Lr", lr)] {
                println!("  {name}={val:#010x}");
            }
            if let Ok(snap) = client.run(GetMem { address: iar & !0xF, length: 64 }).await {
                println!("  memory around write target:");
                dump_hex(&snap.data);
            }
        } else if iar == MESSAGE_ENTRY {
            let a1 = find_reg(&regs, "Gpr3").unwrap_or(0);
            println!("== Message() entry: a1={a1:#010x} caller(Lr)={lr:#010x} ==");
        } else if iar == GET_LENGTH || iar == GET_BUFFER {
            let this_ptr = find_reg(&regs, "Gpr3").unwrap_or(0);
            let label = if iar == GET_LENGTH { "GetLength" } else { "GetBuffer" };
            println!("== {label}() called: this(P)={this_ptr:#010x} caller(Lr)={lr:#010x} ==");
            if iar == GET_LENGTH {
                let addr = this_ptr.wrapping_add(LEN_OFF);
                if let Ok(snap) = client.run(GetMem { address: addr, length: 4 }).await {
                    println!("  P+0x8070 currently: {:02x?}", snap.data);
                }
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
