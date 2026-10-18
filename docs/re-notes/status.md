# Status

Status labels:

- `specified`: behavior reconstructed from the stock image; see `commands/`.
- `implemented`: present in openxbdm and exercised by host tests.
- `verified`: exercised on a console or against an exact captured artifact.

## Source artifacts

| Artifact | Evidence | Status |
| --- | --- | --- |
| `xbdm.xex` | Flash file and Binary Ninja image have SHA-256 `36d0cd5e14ddbbcd070ace19c725ff9f2a2c96344c55161da7acac62bee348bd`; loaded module base `0x91f00000` | verified |
| Live XBDM | `dmversion` reports `2.0.21256.0`; module size is `0x000af000` | verified |

## Components

| Component | Crate | Status | Notes |
| --- | --- | --- | --- |
| Protocol grammar, status codes, authentication | `openxbdm-core` | implemented | Shared by client and server; `no_std` |
| Client library, CLI, C bindings | `openxbdm-client`, `openxbdm-cli`, `openxbdm-ffi` | implemented | Snapshot tests over captured replies; parity fixes listed per domain under `parity/` |
| Command engine | `openxbdm-server` | implemented | `no_std`, allocation-free; 177 of 177 stock commands in 11 domains |
| Host server and simulated console | `openxbdm-server-host` | implemented | One simulated console shared by all connections; every command exercised over TCP by `tests/` |
| SMB file sharing | `openxbdm-server` (`smb`), `smb2-server` | implemented | SMB 2.0.2 and 2.1 shares of every drive whose device exists; host listener on `--smb-listen`, console listener on TCP port 445 |
| Xbox 360 module | `openxbdm-server-xbox360` | verified | Backends for every domain, 253 of 253 exports, 103 of 103 non-default control codes; boots on a devkit as `xbdm.xex` and xshell loads against its exports |
| Command specifications | `docs/re-notes/commands/` | specified | 177 of 177 complete |
| Export table | `docs/re-notes/exports.md` | specified | 253 populated ordinals of 373 |
| Control dispatcher | `docs/re-notes/dispatcher.md` | specified | Codes 0x01 to 0xa9 |
| PGO runtime | `docs/re-notes/pgo.md` | implemented | Exports and `pogoctrl` |
| Boot-time replacement | `docs/re-notes/module-loading.md` | verified | Loader rule; `DEVKIT:\FlashEx\xbdm.xex` with a matching bounding path loads in place of flash XBDM |
| Boot from the kd host | `docs/re-notes/kd.md`, `crates/openxbdm-kd` | verified | With a kd host attached the kernel requests each module from the host before its own devices; `openxbdm-kd host --file` serves openxbdm without writing the disk |
| Retail loading through dashlaunch | `docs/re-notes/retail-dashlaunch.md`, `dashlaunch` feature | implemented and hardware-verified end to end: `openxbdm-server-xbox360` itself (not just a probe module) loaded via `dlaunchStartSysModule`, reachable from an external `xeedee` client over the LAN (not just loopback), and exercised through a broad command sweep | `ExDebugMonitorService` registration confirmed unsupported on retail; the `dashlaunch` feature hooks dashlaunch's `ldata` instead. `KeDebugMonitorData` is live-confirmed nonzero but its block is entirely zeroed on this kernel, closing out that path as structurally dead rather than merely unregistered. `modules` and `threads` now both work live instead: `kernel/retail.rs` resolves the module list from `XexPcToFileHeader`'s embedded constants (kernel ordinal 412; per-build word-offset table, verified live, not hardcoded addresses) and `KeSystemProcess`/`KeTitleProcess` by reading the calling thread's own, and a transiently-created thread's, `KTHREAD.Process` (fully dynamic, no per-build table needed), then stores both in the debug monitor data block at module load. Confirmed live end to end: `modules` lists all eight resident modules with real fields, `threads` lists real thread IDs. Getting there also needed two unrelated fixes, both in `retail-dashlaunch.md`: the module-list signature table was keyed on XBDM's build number (`21256`) instead of the kernel's own (`17559`, `XboxKrnlVersion`), and `xex-dashlaunch.toml` shared the devkit build's fixed load address, which collides with dashlaunch's own resident `launch.xex` on every retail boot. Networking needed its own three fixes (skip `XNetStartup`/`WSAStartup`, caller ID 2 not 3, `SO_DECRYPTSOCK`) before external connections worked at all. |
| Debugging over kdnet | `docs/re-notes/kd.md`, `crates/openxbdm-kd` (`gdbserver`) | verified | GDB and LLDB over the GDB remote protocol: every processor as a thread through processor switch (0x3150), breakpoints, software step, stop at an image load, debug output and module loads on the debugger console |
| Differential testing | `xeedee parity record`, `xeedee parity compare` | verified | Stock and openxbdm replies to `parity/hardware-commands.txt` |

Per-command, export, and control code coverage, remaining gaps, and
deliberate deviations: `parity.md`.

## Hardware

openxbdm boots on a devkit in place of stock XBDM and serves the
protocol; xshell loads against its exports. Differential run of
`parity/hardware-commands.txt` (1060 exchanges, 138 of 177 commands):
1042 exchanges equal, 18 equal outside environment-dependent fields, 0
differ. The other 39 commands are not run on hardware (persistent state,
flash or disk writes, user interface, console restarts); see
`parity/hardware.md`, which also lists the checks run outside the
command list.

Image requirements found on hardware:

- The image is packed on 4 KB pages (`link.x` starts `.text` at
  `0x91f01000`; xex-link derives section alignment from the small-pages
  flag) and ends at `0x91fe5000`, below xshell at `0x92000000`.
- The export table is in an executable section (`.xexp`): hypercall 0x17
  reads it only from protected code pages and otherwise fails with
  `0xc0000156` (`../acceleration/doc/re-notes/xex-loader.md`).
- The server thread is created not debuggable, with stock flags
  `0x040004a6`.
- Exports run server services inline under a startup lock until the
  command loop starts.

Deployment: `DEVELOPING.md`.

Flash image parsing, boot-chain research, and shadowboot image assembly
are maintained in the acceleration repository under `doc/re-notes`.
