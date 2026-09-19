//! Live probe for the `SBG_E_NOT_CONNECTED` (0x80B10013) mystery found
//! 2026-09-15 (docs/HISTORY.md): after the HTTP grid route was implemented
//! and confirmed working, the *first* `GameService` call each boot
//! (`Store.Config`'s plain `Get`) succeeds, but every call after that
//! (`UserGet(UserStorage)`, `Get(Store.Config.Week872)`) fails instantly
//! with `SBG_E_NOT_CONNECTED`, with no corresponding TCP `CONNECT` ever
//! reaching `revival` - a pure client-side refusal before any network
//! attempt.
//!
//! Static disassembly of sub_825C69C0 (the shared "construct + enqueue a
//! GameService request" helper, called by one thin wrapper per action type
//! - Get/Set/UserGet/UserSet) found the exact gate at its very start:
//!
//!   lwz  r11, 0(r31)      ; r31 = a1 = the persistent SBG session/
//!                         ; connection-manager singleton (NOT the small
//!                         ; per-request Get/UserGet completion object -
//!                         ; that one's vtable is off_82018114, confirmed
//!                         ; unrelated by dumping it: its own slot 5/6 are
//!                         ; just GetBuffer/GetLen offset getters, 0x70/
//!                         ; 0x8070 for Get and 0x8C/0x808C for UserGet)
//!   lwz  r11, 0x14(r11)   ; r11 = vtable slot 5 (offset 0x14 = 20 bytes)
//!   mtctr r11
//!   bctrl                 ; call it - an "IsConnected()"-shaped virtual
//!                         ; call taking only `this`
//!   cmpwi r3, 0
//!   bne   loc_825C6A38    ; nonzero -> proceed normally
//!   ; r3 == 0 falls through to construct SBG_E_NOT_CONNECTED (0x80B10013)
//!   ; and bail before ever touching the network.
//!
//! This probe breaks right before the indirect call (to capture `a1` and
//! the resolved target function address - is it the SAME address for the
//! succeeding and failing calls, i.e. one singleton with one vtable?) and
//! right after it returns (to capture the actual boolean r3 - does the
//! check itself return false, confirming the gate - or does the call
//! never even happen for the failing requests, meaning sub_825C69C0 isn't
//! reached at all for them and something upstream is the real gate?).
//!
//! Usage: cargo run --example connectcheck -- <host:730> [timeout-secs]

use std::collections::HashMap;

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

const PRE_CALL: u32 = 0x825c6a1c; // mtctr r11 (r31=a1, r11=target addr already loaded)
const POST_CALL: u32 = 0x825c6a24; // cmpwi r3,0 (r3 = the check's return value)
const ENTRY: u32 = 0x825c69c0; // function entry, r3=a1 fresh in on this call
const MANAGER_CLOSE: u32 = 0x825c6628; // manager calls socket vtable[5] (close)
const SOCKET_CONNECT: u32 = 0x825c9ea0; // real socket opens/connects
const SOCKET_CLOSE: u32 = 0x825c9e30; // real socket closes and clears connected byte
const WATCH: [u32; 3] = [MANAGER_CLOSE, SOCKET_CONNECT, SOCKET_CLOSE];

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
    // `guide-launch` preserves this XBDM session (xam.xex never reloads),
    // but XamLoaderLaunchTitle fully reloads the *title* image - wiping
    // any software breakpoints planted in title .text (0x825c6xxx here) on
    // the very next relaunch, with no notification that it happened. A
    // one-shot "arm then watch" loses the race every time (confirmed live
    // 2026-09-15: zero hits after a guide-launch relaunch, vs. one stale
    // hit from the pre-relaunch instance). Re-sending the same Breakpoint
    // call is idempotent when already armed, so just keep re-arming on
    // every poll tick instead of trying to detect the reload.
    while std::time::Instant::now() < deadline {
        for addr in WATCH {
            let _ = client.run(Breakpoint { address: addr, clear: false }).await;
        }
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
    let host = args.next().expect("usage: connectcheck <host> [timeout-secs]");
    let timeout_secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(90);

    let connect_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut client = loop {
        let attempt = async {
            let transport = xeedee::transport::tokio::connect(host.as_str()).await?;
            let mut c = xeedee::Client::new(transport).read_banner().await?;
            c.run(Debugger {
                do_override: true,
                name: "claude-connectcheck".to_owned(),
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
        (MANAGER_CLOSE, "manager-close"),
        (SOCKET_CONNECT, "socket-connect"),
        (SOCKET_CLOSE, "socket-close"),
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
    println!("== watching sub_825C69C0's connected-check gate, {timeout_secs}s ==");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut call_no: u32 = 0;
    let mut vtables_seen: HashMap<u32, u32> = HashMap::new();

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
        let gpr11 = find_reg(&regs, "Gpr11").unwrap_or(0);
        let gpr31 = find_reg(&regs, "Gpr31").unwrap_or(0);

        if iar == ENTRY {
            call_no += 1;
            println!("== call #{call_no}: sub_825C69C0 entry a1(Gpr3)={gpr3:#010x} caller(Lr)={lr:#010x} ==");
        } else if iar == PRE_CALL {
            println!(
                "== call #{call_no}: about to call vtable[5]: a1(Gpr31)={gpr31:#010x} target(Gpr11)={gpr11:#010x} =="
            );
            let prev = vtables_seen.insert(gpr31, gpr11);
            if let Some(prev_target) = prev {
                if prev_target != gpr11 {
                    println!(
                        "  ** DIFFERENT target function for the SAME a1={gpr31:#010x}: was {prev_target:#010x}, now {gpr11:#010x} **"
                    );
                }
            }
        } else if iar == POST_CALL {
            println!(
                "== call #{call_no}: vtable[5] check returned Gpr3={gpr3:#010x} -> {} ==",
                if gpr3 == 0 { "FALSE (this is where SBG_E_NOT_CONNECTED gets constructed)" } else { "true (proceeds normally)" }
            );
        } else if iar == MANAGER_CLOSE {
            println!("== manager-close: session(Gpr3)={gpr3:#010x} caller(Lr)={lr:#010x} ==");
        } else if iar == SOCKET_CONNECT {
            println!("== socket-connect: socket(Gpr3)={gpr3:#010x} caller(Lr)={lr:#010x} ==");
        } else if iar == SOCKET_CLOSE {
            println!("== socket-close: socket(Gpr3)={gpr3:#010x} caller(Lr)={lr:#010x} ==");
        } else {
            println!("-- unexpected halt at iar={iar:#010x} lr={lr:#010x} on tid={tid:?}, continuing --");
        }

        let _ = client.run(Continue { thread: tid, single_step: false }).await;
        let _ = client.run(Go).await;
    }

    println!("== summary: {} distinct a1 singleton(s) observed ==", vtables_seen.len());
    for (a1, target) in &vtables_seen {
        println!("  a1={a1:#010x} -> vtable[5] target={target:#010x}");
    }

    if let Err(e) = client.run(Stop).await {
        println!("stop (cleanup): {e:?} (tolerated)");
    }
    let _ = client.run(ClearAllBreakpoints).await;
    let _ = client.run(Go).await;
    println!("== cleaned up, breakpoints cleared, console resumed ==");
}
