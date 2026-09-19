//! Concrete next step from docs/NEXT.md: NetDll_recv's real signature has
//! a leading XNCALLER_TITLE constant, so at the raw thunk Gpr3=that
//! constant (not the socket), Gpr4=socket, Gpr5=buffer, Gpr6=length - the
//! previous probes (allrecv.rs, rawrecv.rs) used the wrong mapping and
//! dumped garbage. Break at NetDll_recv entry (0x8295543c) with the fixed
//! mapping, then arm a second, dynamically-discovered breakpoint at each
//! distinct caller's return address (Lr) so we catch the *post-recv*
//! state: the real return value (Gpr3 in the caller's frame = bytes
//! received, or a negative error) and the buffer's actual contents once
//! filled. NetDll_recv is a single shared thunk, so every caller in the
//! process breaks at the same entry address - one run of this probe
//! already found TWO distinct callers this way: `0x825d2d00`
//! (`sub_825D2BC0`, sock ...c000, fixed 48-byte reads - confirmed by the
//! buffer content to be revival-rs-port's own SNTP replies, magic `LOCL`
//! at offset 12 exactly matching `workspace/revival/src/sntp.rs` - a red
//! herring, NOT the SBG/Store.Config channel) and `0x825ca008` (sock
//! ...d001, requesting up to 61440 bytes - fired only once/twice right at
//! boot, never repeating, the much stronger candidate for the real
//! content/Store.Config fetch). This version tracks return breakpoints
//! per-caller (a HashSet, not a single Option) so all discovered callers
//! get their returns caught in one pass instead of just the first one.
//!
//! Usage: cargo run --example recvcapture -- <host:730> [timeout-secs] [hit-count]

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
use xeedee::commands::memory::GetMem;
use xeedee::commands::process::ThreadId;
use xeedee::commands::process::Threads;

const RECV_ENTRY: u32 = 0x8295543c;

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
    let host = args.next().expect("usage: recvcapture <host> [timeout-secs] [hit-count]");
    let timeout_secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(90);
    let hit_count: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);

    // Right after a magicboot, XBDM itself may briefly reset connections
    // while xam.xex/the title reload - retry connect+banner+debugger-connect
    // as a unit instead of assuming the first attempt lands.
    let connect_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut client = loop {
        let attempt = async {
            let transport = xeedee::transport::tokio::connect(host.as_str()).await?;
            let mut c = xeedee::Client::new(transport).read_banner().await?;
            c.run(Debugger {
                do_override: true,
                name: "claude-recvcapture".to_owned(),
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
    // A magicboot reloads the title xex fresh from disk, silently wiping any
    // software breakpoint patched into the OLD image's bytes without erroring
    // - so if the console is mid-reboot when we attach, the address may not
    // be valid/loaded yet. Retry until it takes instead of assuming one shot.
    let mut armed = false;
    for attempt in 0..40 {
        match client.run(Breakpoint { address: RECV_ENTRY, clear: false }).await {
            Ok(()) => {
                armed = true;
                println!("== armed recv entry bp on attempt {attempt} ==");
                break;
            }
            Err(e) => {
                if attempt == 0 {
                    println!("arm attempt failed (will retry): {e:?}");
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
    }
    if !armed {
        panic!("could not arm recv entry breakpoint after retries");
    }
    client.run(Go).await.expect("go");
    println!("== watching NetDll_recv entry={RECV_ENTRY:#010x}, up to {hit_count} return-pairs ==");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut seen = 0u32;
    // Per-thread pending call: (buf, len). Filled at entry, consumed at return.
    let mut pending: HashMap<ThreadId, (u32, u32)> = HashMap::new();
    let mut armed_returns: HashSet<u32> = HashSet::new();

    while seen < hit_count {
        let Some(tid) = poll_any_halt(&mut client, deadline).await else {
            println!("== TIMEOUT after {seen} return-pairs ==");
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

        if iar == RECV_ENTRY {
            // Fixed mapping: Gpr3=XNCALLER_TITLE const, Gpr4=socket, Gpr5=buf, Gpr6=len.
            let caller_const = find_reg(&regs, "Gpr3").unwrap_or(0);
            let sock = find_reg(&regs, "Gpr4").unwrap_or(0);
            let buf = find_reg(&regs, "Gpr5").unwrap_or(0);
            let len = find_reg(&regs, "Gpr6").unwrap_or(0);
            let lr = find_reg(&regs, "Lr").unwrap_or(0);
            println!(
                "-- entry: caller(Lr)={lr:#010x} xncaller_title={caller_const:#010x} sock={sock:#010x} buf={buf:#010x} len={len} --"
            );
            pending.insert(tid, (buf, len));
            if lr != 0 && armed_returns.insert(lr) {
                client
                    .run(Breakpoint { address: lr, clear: false })
                    .await
                    .expect("set return bp");
                println!("-- armed return breakpoint at caller Lr={lr:#010x} --");
            }
        } else if armed_returns.contains(&iar) {
            let ret = find_reg(&regs, "Gpr3").unwrap_or(0) as i32;
            seen += 1;
            println!("== hit #{seen} [return] iar={iar:#010x} recv()->{ret} (0x{ret:08x}) ==");
            if let Some((buf, _requested_len)) = pending.remove(&tid) {
                // Dump by actual bytes received (ret), not the originally
                // requested buffer size - the 61440-byte ask for the
                // 0x825ca008 caller was clamped to 64 before, truncating
                // real (up to ~900-byte) SBG response payloads.
                let actual = ret.max(0) as u32;
                let peek = actual.clamp(4, 2048);
                if buf != 0 && ret > 0 {
                    if let Ok(snap) = client.run(GetMem { address: buf, length: peek }).await {
                        println!("  buf@{buf:#010x} AFTER recv ({} of {} actual bytes shown):", peek, actual);
                        dump_hex(&snap.data);
                    }
                }
            } else {
                println!("  (no pending entry captured for this thread - missed the call's entry hit)");
            }
        } else {
            println!("-- unexpected halt at iar={iar:#010x} on tid={tid:?}, continuing --");
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
