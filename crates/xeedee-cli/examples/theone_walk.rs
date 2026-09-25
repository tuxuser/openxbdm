//! Self-contained live-trace tool for the `TheOne`/`TheMob` nesting
//! question (see docs/NEXT.md's current pointer): walks every
//! `sub_821D8ED0` (XSOR tree->Lua converter) entry, using the native XBDM
//! `break`/`go`/`continue` primitives on one persistent debug connection
//! (not raw setmem trap injection - see docs/HISTORY.md for why that
//! proved unreliable this session: every hit needs `go`, not just
//! per-thread `continue`, to release the whole target).
//!
//! Runs its own debugstr reader on a second connection (xeedee's typed
//! `Client` doesn't model XBDM's async NOTIFY channel - interleaving it
//! with command/response traffic on one socket would corrupt response
//! framing, per `client/engine.rs`'s own doc comment) so the whole trace
//! is one self-contained process: no external `x360 debugstr`, FIFO, or
//! shell polling loop needed.
//!
//! Node layout (confirmed live this session):
//!   +0x00: inline name bytes if strlen<16, else a heap pointer to the name
//!   +0x10: name length (u32)
//!   +0x1C: scalar-value pointer (0 for a container node)
//!   +0x20/+0x24/+0x28: children vector begin/end/cap (4-byte pointers)
//!
//! Usage: cargo run --example theone_walk -- <host:730> [max-hits] [timeout-secs]

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

const ENTRY: u32 = 0x821D8ED0;

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

struct Node {
    name: Option<String>,
    namelen: u32,
    scalar_ptr: u32,
    children_begin: u32,
    children_end: u32,
}

impl Node {
    fn count(&self) -> i64 {
        if self.children_end >= self.children_begin {
            ((self.children_end - self.children_begin) / 4) as i64
        } else {
            -1
        }
    }
}

async fn read_node(client: &mut Client, addr: u32) -> Option<Node> {
    let mem = client.run(GetMem { address: addr, length: 40 }).await.ok()?;
    let d = &mem.data;
    let namelen = u32::from_be_bytes(d[0x10..0x14].try_into().ok()?);
    let name = if namelen < 16 && namelen as usize <= d.len() {
        Some(String::from_utf8_lossy(&d[0..namelen as usize]).into_owned())
    } else {
        None
    };
    let scalar_ptr = u32::from_be_bytes(d[0x1C..0x20].try_into().ok()?);
    let children_begin = u32::from_be_bytes(d[0x20..0x24].try_into().ok()?);
    let children_end = u32::from_be_bytes(d[0x24..0x28].try_into().ok()?);
    Some(Node { name, namelen, scalar_ptr, children_begin, children_end })
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

/// Runs on its own connection for the whole program lifetime: attaches as
/// a debugger, switches into debugstr-notify mode on this socket (so it
/// never issues another typed command afterward - the connection is now a
/// pure async line stream), and prints every line with an elapsed-time
/// prefix, reconnecting on drop. This replaces the external
/// `x360 debugstr` + FIFO + `tail` chain used earlier this session.
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
            .run(Debugger { do_override: false, name: "claude-debugstr".to_owned(), user: "claude".to_owned() })
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
    let host = args.next().expect("usage: theone_walk <host:730> [max-hits] [timeout-secs]");
    let max_hits: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(500);
    let timeout_secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(120);

    let start = std::time::Instant::now();
    let stop_debugstr = Arc::new(AtomicBool::new(false));
    let debugstr_handle = tokio::spawn(debugstr_task(host.clone(), start, stop_debugstr.clone()));

    let transport = xeedee::transport::tokio::connect(host.as_str()).await.expect("connect");
    let mut client = xeedee::Client::new(transport).read_banner().await.expect("banner");

    client
        .run(Debugger { do_override: true, name: "claude-theone".to_owned(), user: "claude".to_owned() })
        .await
        .expect("debugger connect");
    println!("== debugger connected, session persistent ==");

    if let Err(e) = client.run(Stop).await {
        println!("stop: {e:?} (tolerated)");
    }
    client.run(Breakpoint { address: ENTRY, clear: false }).await.expect("arm");
    client.run(Go).await.expect("go");
    println!("== armed {ENTRY:#010x}, watching ==");

    for hit_num in 1..=max_hits {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        let Some(tid) = poll_any_halt(&mut client, deadline).await else {
            println!("hit {hit_num}: no halt within {timeout_secs}s, stopping");
            break;
        };

        let regs = client
            .run(GetContext { thread: tid, groups: ContextGroups { control: true, integer: true, floating_point: false, vector: false } })
            .await
            .expect("getcontext");
        let r3 = find_reg(&regs, "Gpr3").unwrap_or(0);

        let node = match read_node(&mut client, r3).await {
            Some(n) => n,
            None => {
                println!("hit {hit_num}: node=0x{r3:08x} <getmem failed>");
                let _ = client.run(Breakpoint { address: ENTRY, clear: true }).await;
                let _ = client.run(Continue { thread: tid, single_step: false }).await;
                let _ = client.run(Breakpoint { address: ENTRY, clear: false }).await;
                let _ = client.run(Go).await;
                continue;
            }
        };

        let tag = match &node.name {
            Some(s) => format!("\"{s}\""),
            None => format!("(heap-ptr, len={})", node.namelen),
        };
        println!(
            "hit {hit_num}: node=0x{r3:08x} name={tag} scalar_ptr=0x{:08x} children_count={}",
            node.scalar_ptr,
            node.count()
        );

        if node.name.as_deref() == Some("TheOne") {
            let n = node.count();
            println!("*** FOUND TheOne NODE at 0x{r3:08x} children_count={n} ***");
            let kids: Vec<u32> = if n > 0 {
                let kids_mem = client
                    .run(GetMem { address: node.children_begin, length: (n as u32) * 4 })
                    .await
                    .expect("getmem children");
                (0..n as usize)
                    .map(|i| u32::from_be_bytes(kids_mem.data[i * 4..i * 4 + 4].try_into().unwrap()))
                    .collect()
            } else {
                Vec::new()
            };
            for kaddr in kids {
                if let Some(kn) = read_node(&mut client, kaddr).await {
                    let ktag = match &kn.name {
                        Some(s) => format!("\"{s}\""),
                        None => format!("(heap-ptr, len={})", kn.namelen),
                    };
                    println!(
                        "    child 0x{kaddr:08x} name={ktag} scalar_ptr=0x{:08x} children_count={}",
                        kn.scalar_ptr,
                        kn.count()
                    );
                    if kn.scalar_ptr != 0 {
                        if let Ok(sv) = client.run(GetMem { address: kn.scalar_ptr, length: 32 }).await {
                            println!("      scalar bytes: {:02x?}", sv.data);
                        }
                    }
                } else {
                    println!("    child 0x{kaddr:08x} <getmem failed>");
                }
            }
            let _ = client.run(Stop).await;
            let _ = client.run(ClearAllBreakpoints).await;
            let _ = client.run(Continue { thread: tid, single_step: false }).await;
            let _ = client.run(Go).await;
            println!("== done, console released ==");
            stop_debugstr.store(true, Ordering::Relaxed);
            debugstr_handle.abort();
            return;
        }

        let _ = client.run(Breakpoint { address: ENTRY, clear: true }).await;
        let _ = client.run(Continue { thread: tid, single_step: false }).await;
        let _ = client.run(Breakpoint { address: ENTRY, clear: false }).await;
        if let Err(e) = client.run(Go).await {
            println!("hit {hit_num}: go error: {e:?}");
        }
    }

    println!("== finished without finding a 2-child node; releasing console ==");
    let _ = client.run(Stop).await;
    let _ = client.run(ClearAllBreakpoints).await;
    let _ = client.run(Go).await;
    stop_debugstr.store(true, Ordering::Relaxed);
    debugstr_handle.abort();
}
