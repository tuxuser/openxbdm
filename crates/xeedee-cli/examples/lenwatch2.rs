//! Precise version of `lenwatch.rs`: rather than catching the *first*
//! `PostMessage`-response construction (ambiguous - the Register ack
//! might construct one too, before `Store.Config`'s), break exactly at
//! `0x821BE3D0` inside `v2.0.1317.0`'s `Message()` (`sub_821BE388`,
//! confirmed in docs/HISTORY.md's thirteenth entry to be the real,
//! Store.Config-specific call - it's the only Get-type action before the
//! fatal CONFIG_ERROR aborts the boot), where the query struct pointer
//! (v3) is already resolved in r31. From there:
//!   1. read v3 = Gpr31
//!   2. getmem(v3+40) -> the response object pointer P for *this exact*
//!      confirmed Store.Config call (unambiguous)
//!   3. getmem(P+0x8070) -> the length field's value *right now*, before
//!      Message()'s own accessors ever run
//!   4. arm a WRITE watchpoint on P+0x8070 anyway, then continue - covers
//!      the (unlikely) case of a late write after this point too
//!
//! Usage: cargo run --example lenwatch2 -- <host:730> [timeout-secs]

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

const MESSAGE_V3_READY: u32 = 0x821be3d0;
const LEN_OFFSET: u32 = 0x8070;
const P_FIELD_OFFSET: u32 = 40; // v3+40 = P

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
    let host = args.next().expect("usage: lenwatch2 <host> [timeout-secs]");
    let timeout_secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);

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
            name: "claude-lenwatch2".to_owned(),
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
            address: MESSAGE_V3_READY,
            clear: false,
        })
        .await
        .expect("set breakpoint");
    client.run(Go).await.expect("go");
    println!("== watching for the confirmed Store.Config Message() call at {MESSAGE_V3_READY:#010x} ==");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let Some(tid) = poll_any_halt(&mut client, deadline).await else {
        println!("== TIMEOUT: Message() never hit ==");
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
    let v3 = find_reg(&regs, "Gpr31").expect("Gpr31 (v3)");
    println!("== HIT: v3 (query struct) = {v3:#010x} ==");

    let v3_dump = client
        .run(GetMem {
            address: v3,
            length: 64,
        })
        .await
        .expect("getmem v3");
    let p = be32(&v3_dump.data, P_FIELD_OFFSET as usize).expect("P field");
    println!("== P (response object, v3+40) = {p:#010x} ==");

    let len_addr = p.wrapping_add(LEN_OFFSET);
    let p_head = client
        .run(GetMem {
            address: p,
            length: 0x30,
        })
        .await
        .expect("getmem p head");
    println!("P object header (first 0x30 bytes):");
    for chunk in p_head.data.chunks(16) {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        println!("  {}", hex.join(" "));
    }

    let len_now = client
        .run(GetMem {
            address: len_addr,
            length: 4,
        })
        .await
        .expect("getmem length field");
    let len_val = be32(&len_now.data, 0).unwrap_or(0xffffffff);
    println!("== length field @ {len_addr:#010x} RIGHT NOW (before Message()'s own accessors run) = {len_val} ==");

    let buf_head = client
        .run(GetMem {
            address: p.wrapping_add(0x70),
            length: 64,
        })
        .await
        .expect("getmem buffer head");
    println!("embedded value buffer (P+0x70) first 64 bytes:");
    for chunk in buf_head.data.chunks(16) {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        println!("  {}", hex.join(" "));
    }

    // Also arm a watchpoint in case something writes it *after* this
    // point (belt-and-suspenders - Message()'s own accessors are reads,
    // but this covers any later producer we haven't found yet).
    if let Err(e) = client.run(Stop).await {
        println!("stop (rearm): {e:?} (tolerated)");
    }
    client
        .run(Breakpoint {
            address: MESSAGE_V3_READY,
            clear: true,
        })
        .await
        .expect("clear message breakpoint");
    client
        .run(DataBreakpoint {
            address: len_addr,
            size: 4,
            kind: DataBreakKind::Write,
            clear: false,
        })
        .await
        .expect("set data breakpoint");
    let _ = client
        .run(Continue {
            thread: tid,
            single_step: false,
        })
        .await;
    client.run(Go).await.expect("go (resume)");
    println!("== resumed, watching for any later write to the length field ==");

    let deadline2 = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    match poll_any_halt(&mut client, deadline2).await {
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
            println!("== a later write DID happen (thread {tid2}) ==");
            for r in &regs2 {
                println!("  {} = {}", r.name, r.value);
            }
        }
        None => {
            println!("== confirmed: no later write either - this exact Store.Config response object's length field stays {len_val} forever ==");
        }
    }

    if let Err(e) = client.run(Stop).await {
        println!("stop (cleanup): {e:?} (tolerated)");
    }
    let _ = client.run(ClearAllBreakpoints).await;
    let _ = client.run(Go).await;
    println!("== cleaned up, breakpoints cleared, console resumed ==");
}
