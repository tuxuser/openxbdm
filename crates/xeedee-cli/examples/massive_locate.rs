//! Live-trace tool for the MAD (Massive Ads) `OpenSession` blocker (see
//! docs/NEXT.md's current pointer). `HANDLE_RESPONSE` (`sub_825362B0`,
//! matched by string content alone during static RE as
//! `XMassiveAdClient::HandleResponse`) is the *default* target only for
//! historical reasons - a live breakpoint here **never fired** across two
//! full `LocateService` cycles (confirmed not a systemic breakpoint
//! problem: a sanity-check breakpoint on `XMassiveAdClient::Update`,
//! `0x8224D2B8`, fired immediately on the same mechanism), so this
//! function is confirmed NOT on the real execution path - don't restart
//! an investigation from it. Pass an explicit `[hex-addr]` (4th CLI arg)
//! instead.
//!
//! The tool that actually worked: point this at `Update` itself
//! (`0x8224D2B8`, called every frame) and it polls the real core-client
//! singleton (`dword_82A87184`; state at `+16`, error at `+4`, confirmed
//! via `GetState()`/`GetLastError()`'s own decompiles) on every hit,
//! printing a full object dump only when something changes. This is how
//! the real `STATE_LOCATE_SERVICE(7) -> STATE_ERROR(0)` transition (with
//! its `+4`/`+8` error-code pair and `+0x14`/`+0x18`/`+0x1C`/`+0x20`
//! watchdog-shaped reset) was caught byte-for-byte - see docs/NEXT.md.
//!
//! `i386kd`'s software `bp` also did not fire on `HANDLE_RESPONSE` this
//! session (title code on this RGLoader-Shadowboot kernel) - this tool
//! uses xeedee's native XBDM breakpoint primitive instead, the mechanism
//! that has actually worked for title code in this project
//! (`theone_walk.rs`, 150+ reliable hits, and `Update` above).
//!
//! Usage: cargo run --example massive_locate -- <host:730> [max-hits] [timeout-secs] [hex-addr]
//! Recommended: ... 20 60 0x8224D2B8

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use futures_util::io::AsyncBufReadExt;
use futures_util::io::BufReader;

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

/// Believed (from static RE alone, via string-content matching) to be
/// `MassiveAdClient3::XMassiveAdClient::HandleResponse` - dispatches on an
/// internal message-subtype field at `a2+20` (`4113` = LocateService
/// response, `4131` = the async DNS-resolution-thread completion message,
/// `4211` = a third, unidentified subtype). **Confirmed live to never
/// execute** for the `LocateService` response path (see module doc) -
/// kept only as the fallback default; pass a real target explicitly.
const HANDLE_RESPONSE: u32 = 0x825362B0;

type Client = xeedee::Client<xeedee::transport::tokio::TokioTransport, xeedee::Connected>;

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

async fn poll_any_halt(client: &mut Client, deadline: std::time::Instant) -> Option<ThreadId> {
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

fn hexdump(data: &[u8], base: u32) -> String {
    let mut out = String::new();
    for (i, chunk) in data.chunks(16).enumerate() {
        let offset = base as usize + i * 16;
        let hex: String = chunk.iter().map(|b| format!("{b:02x} ")).collect();
        out.push_str(&format!("  +0x{offset:04x}  {hex}\n"));
    }
    out
}

fn u32_at(data: &[u8], offset: usize) -> Option<u32> {
    data.get(offset..offset + 4)
        .map(|s| u32::from_be_bytes(s.try_into().unwrap()))
}

/// Same self-contained debugstr reader as `theone_walk.rs` - a second
/// connection, since xeedee's typed `Client` can't interleave XBDM's async
/// NOTIFY channel with command/response traffic on one socket.
async fn debugstr_task(host: String, start: std::time::Instant, stop: Arc<AtomicBool>) {
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let transport = match xeedee::transport::tokio::connect(host.as_str()).await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[debugstr] connect failed: {e:?}, retrying");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
        };
        let mut client = match xeedee::Client::new(transport).read_banner().await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[debugstr] banner failed: {e:?}, retrying");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
        };
        if let Err(e) = client
            .run(Debugger {
                do_override: false,
                name: "claude-massive-debugstr".to_owned(),
                user: "claude".to_owned(),
            })
            .await
        {
            eprintln!("[debugstr] debugger connect failed: {e:?}, retrying");
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            continue;
        }
        if let Err(e) = client.send_raw("notify reconnectport=0").await {
            eprintln!("[debugstr] notify failed: {e:?}, retrying");
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            continue;
        }
        println!("[debugstr] subscribed");
        let transport = client.into_inner();
        let mut reader = BufReader::new(transport);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    if !trimmed.is_empty() {
                        println!("[debugstr {:>7}ms] {trimmed}", start.elapsed().as_millis());
                    }
                }
                Err(_) => break,
            }
            if stop.load(Ordering::Relaxed) {
                return;
            }
        }
        if stop.load(Ordering::Relaxed) {
            return;
        }
        println!("[debugstr] connection dropped, reconnecting");
    }
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let host = args.next().expect("usage: massive_locate <host:730> [max-hits] [timeout-secs] [hex-addr]");
    let max_hits: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);
    let timeout_secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(120);
    let target: u32 = args
        .next()
        .map(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).expect("bad hex addr"))
        .unwrap_or(HANDLE_RESPONSE);

    let start = std::time::Instant::now();
    let stop_debugstr = Arc::new(AtomicBool::new(false));
    let debugstr_handle = tokio::spawn(debugstr_task(host.clone(), start, stop_debugstr.clone()));

    let transport = xeedee::transport::tokio::connect(host.as_str()).await.expect("connect");
    let mut client = xeedee::Client::new(transport).read_banner().await.expect("banner");

    client
        .run(Debugger { do_override: true, name: "claude-massive".to_owned(), user: "claude".to_owned() })
        .await
        .expect("debugger connect");
    println!("== debugger connected, session persistent ==");

    if let Err(e) = client.run(Stop).await {
        println!("stop: {e:?} (tolerated)");
    }
    client.run(Breakpoint { address: target, clear: false }).await.expect("arm");
    client.run(Go).await.expect("go");
    println!("== armed target@{target:#010x}, watching (guide-launch the title now) ==");

    // `dword_82A87184` is the fixed-address global pointer to the real
    // `XMassiveAdClient` core object (confirmed via `GetState()`/
    // `GetLastError()`'s own decompiles: `*(dword_82A87184 + 16)` / `+ 4`).
    // Poll it on every `Update()` hit and only print when something on it
    // actually changes, so a breakpoint on a per-frame function stays
    // legible instead of flooding the log.
    const CORE_PTR: u32 = 0x82A87184;
    let mut last_state: Option<u32> = None;
    let mut last_error: Option<u32> = None;
    let mut last_request: Option<u32> = None;

    for hit_num in 1..=max_hits {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        let Some(tid) = poll_any_halt(&mut client, deadline).await else {
            println!("hit {hit_num}: no halt within {timeout_secs}s, stopping");
            break;
        };

        let core = match client.run(GetMem { address: CORE_PTR, length: 4 }).await {
            Ok(mem) => u32_at(&mem.data, 0).unwrap_or(0),
            Err(_) => 0,
        };

        if core != 0 {
            if let Ok(mem) = client.run(GetMem { address: core, length: 400 }).await {
                let d = &mem.data;
                let state = u32_at(d, 16);
                let error = u32_at(d, 4);
                let request = u32_at(d, 384);
                if state != last_state || error != last_error || request != last_request {
                    println!(
                        "hit {hit_num}: core=0x{core:08x} state={state:?} error={error:?} current_request={request:?}"
                    );
                    println!("  core object dump (0..400):\n{}", hexdump(d, 0));
                    if let Some(req) = request
                        && req != 0
                        && let Ok(reqmem) = client.run(GetMem { address: req, length: 160 }).await
                    {
                        println!("  current_request object dump (0..160):\n{}", hexdump(&reqmem.data, 0));
                    }
                    last_state = state;
                    last_error = error;
                    last_request = request;

                    // Error state 0 with a real error code is the exact
                    // transition we're chasing - stop right after dumping it
                    // instead of burning more hits on ambient frame noise.
                    if state == Some(0) && error.is_some_and(|e| e != 0) {
                        println!("== reached STATE_ERROR, stopping trace ==");
                        let _ = client.run(Stop).await;
                        let _ = client.run(ClearAllBreakpoints).await;
                        let _ = client.run(Continue { thread: tid, single_step: false }).await;
                        let _ = client.run(Go).await;
                        stop_debugstr.store(true, Ordering::Relaxed);
                        debugstr_handle.abort();
                        return;
                    }
                }
            }
        }

        let _ = client.run(Breakpoint { address: target, clear: true }).await;
        let _ = client.run(Continue { thread: tid, single_step: false }).await;
        let _ = client.run(Breakpoint { address: target, clear: false }).await;
        if let Err(e) = client.run(Go).await {
            println!("hit {hit_num}: go error: {e:?}");
        }
    }

    println!("== finished; releasing console ==");
    let _ = client.run(Stop).await;
    let _ = client.run(ClearAllBreakpoints).await;
    let _ = client.run(Go).await;
    stop_debugstr.store(true, Ordering::Relaxed);
    debugstr_handle.abort();
}
