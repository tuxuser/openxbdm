//! Decisive follow-up to stackclimb.rs. Static disassembly of
//! sub_825C6088 (0x825c6088, the per-frame response processor, called from
//! sub_825C6800 <- sub_825C6C58 <- the recv-poll loop) found real fill
//! logic for SBG response kind 0x82 (ServiceResponse - our Store.Config
//! case): look up the pending request by cookie in a table at `a1+120`; if
//! found, copy a 32-byte bookkeeping struct out of it (buffer ptr at +0x10,
//! length-out ptr etc. at following offsets); decompress the payload via
//! sub_825C82C0; if the registered buffer capacity is enough, write the
//! real length via the length-out pointer and memcpy the decompressed
//! bytes into the registered buffer pointer (0x825c6264's `bl
//! sub_824F3AA0`).
//!
//! Open question this probe answers: does this code path actually run for
//! our real Store.Config response, and if so, does the registered buffer
//! pointer match the `Get` completion object's buffer (`P+0x70`, known
//! from configfill.rs to stay zero)? Four breakpoints:
//!   - 0x825c6088 entry - does this function run at all for our response?
//!   - 0x825c60ec - the "cookie not found in table" bail-out path
//!   - 0x825c61d0 - the kind==0x82 branch was taken
//!   - 0x825c6264 - the actual `bl sub_824F3AA0` fill memcpy for kind 0x82
//!     (r3=dest buffer, r4=src decompressed data, r5=length at this point)
//!
//! Usage: cargo run --example fillcheck -- <host:730> [timeout-secs]

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
use xeedee::commands::memory::GetMem;
use xeedee::commands::process::ThreadId;
use xeedee::commands::process::Threads;

const FRAME_ENTRY: u32 = 0x825c6088;
const NOT_FOUND: u32 = 0x825c60ec;
const KIND_82_TAKEN: u32 = 0x825c61d0;
const FILL_MEMCPY: u32 = 0x825c6264;
const DISPATCH_ENTRY: u32 = 0x82232908;

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
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    }
    None
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let host = args.next().expect("usage: fillcheck <host> [timeout-secs]");
    let timeout_secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(90);

    let connect_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut client = loop {
        let attempt = async {
            let transport = xeedee::transport::tokio::connect(host.as_str()).await?;
            let mut c = xeedee::Client::new(transport).read_banner().await?;
            c.run(Debugger {
                do_override: true,
                name: "claude-fillcheck".to_owned(),
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
        (FRAME_ENTRY, "frame-entry"),
        (NOT_FOUND, "not-found"),
        (KIND_82_TAKEN, "kind-82-taken"),
        (FILL_MEMCPY, "fill-memcpy"),
        (DISPATCH_ENTRY, "dispatch-entry"),
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
    println!("== watching frame-processing path + dispatch, {timeout_secs}s ==");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut last_p: Option<u32> = None;

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

        if iar == DISPATCH_ENTRY {
            last_p = Some(gpr3);
            println!("== dispatch-entry: P={gpr3:#010x} a2={gpr4:#010x} a3={gpr5:#010x} caller(Lr)={lr:#010x} (remembering P for comparison) ==");
        } else if iar == FRAME_ENTRY {
            println!("== frame-entry: a1(transport)={gpr3:#010x} a2(frame)={gpr4:#010x} a3={gpr5:#010x} a4={gpr6:#010x} caller(Lr)={lr:#010x} ==");
            if gpr4 != 0 {
                if let Ok(snap) = client.run(GetMem { address: gpr4, length: 26 }).await {
                    println!("  frame header bytes:");
                    dump_hex(&snap.data);
                }
            }
        } else if iar == NOT_FOUND {
            println!("== NOT-FOUND hit: cookie lookup failed to find a registered pending request! (error 0x80B1000C will be returned) ==");
        } else if iar == KIND_82_TAKEN {
            println!("== kind==0x82 (ServiceResponse) branch taken - this IS our Store.Config reply shape ==");
            // r30 = registered buffer ptr, r31 = registered length-out ptr,
            // per the disassembly, loaded a few instructions further in -
            // read them straight from the register file at this halt if
            // available, else after continuing a step. GetContext gives us
            // gpr30/31 directly since we requested the integer group.
            let gpr30 = find_reg(&regs, "Gpr30").unwrap_or(0);
            let gpr31 = find_reg(&regs, "Gpr31").unwrap_or(0);
            println!("  registered buffer/length pointers so far: gpr30={gpr30:#010x} gpr31={gpr31:#010x}");
            if let Some(p) = last_p {
                println!("  last known P from dispatch-entry was {p:#010x} -> expected buffer ptr {:#010x}", p.wrapping_add(0x70));
            }
        } else if iar == FILL_MEMCPY {
            println!(
                "== FILL MEMCPY about to run: dest(Gpr3)={gpr3:#010x} src(Gpr4)={gpr4:#010x} len(Gpr5)={gpr5:#010x} caller(Lr)={lr:#010x} =="
            );
            if let Some(p) = last_p {
                let expected = p.wrapping_add(0x70);
                println!(
                    "  last known P={p:#010x}, expected buffer ptr {expected:#010x} -> {}",
                    if gpr3 == expected { "MATCH - this IS our P's buffer!" } else { "MISMATCH - filling a different object!" }
                );
            }
            if gpr4 != 0 && gpr5 > 0 && gpr5 < 4096 {
                if let Ok(snap) = client.run(GetMem { address: gpr4, length: gpr5.min(128) }).await {
                    println!("  source bytes about to be copied:");
                    dump_hex(&snap.data);
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
