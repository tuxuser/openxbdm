//! Compare v2.0.1317.0's native per-cookie SBG completion record at request
//! insertion, response lookup, and the final value copy.  This deliberately
//! observes the stock path with `SetupSbgGetFillPatch1317` disabled.
//!
//! Usage: cargo run --example sbgrecord -- <host:730> [timeout-secs]

use xeedee::commands::debug::{
    Breakpoint, ClearAllBreakpoints, ContextGroups, Continue, Debugger, GetContext, Go, IsStopped,
    Stop, StopState,
};
use xeedee::commands::memory::GetMem;
use xeedee::commands::process::{ThreadId, Threads};

const REGISTER_COPY: u32 = 0x825c6b6c;
const LOOKUP_COPY: u32 = 0x825c6114;
const RESPONSE_RECORD: u32 = 0x825c61d0;
const BEFORE_VALUE_COPY: u32 = 0x825c6258;
const VALUE_COPY: u32 = 0x825c6264;
const WATCH: [u32; 5] = [
    REGISTER_COPY,
    LOOKUP_COPY,
    RESPONSE_RECORD,
    BEFORE_VALUE_COPY,
    VALUE_COPY,
];

fn find_reg(regs: &[xeedee::commands::debug::RegisterValue], name: &str) -> Option<u32> {
    regs.iter()
        .find(|r| r.name.eq_ignore_ascii_case(name))
        .and_then(|r| {
            let value = r
                .value
                .trim_start_matches("0x")
                .trim_start_matches("0X")
                .trim_start_matches("0q")
                .trim_start_matches("0Q");
            let value = if value.len() > 8 {
                &value[value.len() - 8..]
            } else {
                value
            };
            u64::from_str_radix(value, 16).ok().map(|x| x as u32)
        })
}

fn be32(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn print_record(label: &str, address: u32, data: &[u8]) {
    if data.len() < 0x20 {
        println!(
            "{label}: short read at {address:#010x}: {} bytes",
            data.len()
        );
        return;
    }
    let cookie = u16::from_be_bytes([data[0x10], data[0x11]]);
    println!(
        "{label}: record={address:#010x} buffer={:#010x} capacity={:#010x} length_ptr={:#010x} context={:#010x} cookie={cookie:#06x} kind={:#04x} completion={:#010x} callback={:#010x}",
        be32(data, 0),
        be32(data, 4),
        be32(data, 8),
        be32(data, 12),
        data[0x12],
        be32(data, 0x18),
        be32(data, 0x1c),
    );
    println!(
        "  raw={}",
        data.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
}

async fn read_record(
    client: &mut xeedee::Client<xeedee::transport::tokio::TokioTransport, xeedee::Connected>,
    label: &str,
    address: u32,
) {
    match client
        .run(GetMem {
            address,
            length: 0x20,
        })
        .await
    {
        Ok(memory) => print_record(label, address, &memory.data),
        Err(error) => println!("{label}: failed to read {address:#010x}: {error:?}"),
    }
}

async fn poll_any_halt(
    client: &mut xeedee::Client<xeedee::transport::tokio::TokioTransport, xeedee::Connected>,
    deadline: std::time::Instant,
) -> Option<ThreadId> {
    while std::time::Instant::now() < deadline {
        // A guide launch reloads title .text and erases software breakpoints.
        // Re-applying an existing breakpoint is idempotent.
        for address in WATCH {
            let _ = client
                .run(Breakpoint {
                    address,
                    clear: false,
                })
                .await;
        }
        if let Ok(threads) = client.run(Threads).await {
            for thread in threads {
                if let Ok(StopState::Halted(_)) = client.run(IsStopped { thread }).await {
                    return Some(thread);
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    }
    None
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let host = args
        .next()
        .expect("usage: sbgrecord <host:730> [timeout-secs]");
    let timeout_secs = args.next().and_then(|s| s.parse().ok()).unwrap_or(90u64);

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
            name: "sbgrecord".to_owned(),
            user: "codex".to_owned(),
        })
        .await
        .expect("debugger connect");
    println!("== debugger connected ==");

    let _ = client.run(Stop).await;
    for address in WATCH {
        match client
            .run(Breakpoint {
                address,
                clear: false,
            })
            .await
        {
            Ok(()) => println!("armed {address:#010x}"),
            Err(error) => println!(
                "pending {address:#010x}: {error:?} (title not mapped yet; polling will retry)"
            ),
        }
    }
    client.run(Go).await.expect("resume");
    println!("== tracing for {timeout_secs}s; relaunch the title now ==");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    while let Some(thread) = poll_any_halt(&mut client, deadline).await {
        let regs = client
            .run(GetContext {
                thread,
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
        let sp = find_reg(&regs, "Gpr1").unwrap_or(0);
        let r3 = find_reg(&regs, "Gpr3").unwrap_or(0);
        let r4 = find_reg(&regs, "Gpr4").unwrap_or(0);
        let r5 = find_reg(&regs, "Gpr5").unwrap_or(0);
        let r30 = find_reg(&regs, "Gpr30").unwrap_or(0);
        let r31 = find_reg(&regs, "Gpr31").unwrap_or(0);

        match iar {
            REGISTER_COPY => {
                println!("\n== REGISTER lr={lr:#010x} tree_record={r3:#010x} ==");
                read_record(&mut client, "request", r4).await;
            }
            LOOKUP_COPY => {
                println!("\n== LOOKUP lr={lr:#010x} ==");
                read_record(&mut client, "tree", r4).await;
            }
            RESPONSE_RECORD => {
                println!("\n== RESPONSE RECORD lr={lr:#010x} ==");
                read_record(&mut client, "local", sp.wrapping_add(0x60)).await;
            }
            BEFORE_VALUE_COPY => {
                println!(
                    "\n== BEFORE VALUE COPY lr={lr:#010x} buffer(r30)={r30:#010x} length_ptr(r31)={r31:#010x} decoded_len(r5)={r5:#010x} =="
                );
                read_record(&mut client, "local", sp.wrapping_add(0x60)).await;
            }
            VALUE_COPY => println!(
                "\n== VALUE COPY lr={lr:#010x} destination(r3)={r3:#010x} source(r4)={r4:#010x} length(r5)={r5:#010x} =="
            ),
            _ => println!("unexpected halt iar={iar:#010x} lr={lr:#010x}"),
        }

        let _ = client
            .run(Continue {
                thread,
                single_step: false,
            })
            .await;
        let _ = client.run(Go).await;
        if std::time::Instant::now() >= deadline {
            break;
        }
    }

    let _ = client.run(Stop).await;
    let _ = client.run(ClearAllBreakpoints).await;
    let _ = client.run(Go).await;
    println!("== cleanup complete ==");
}
