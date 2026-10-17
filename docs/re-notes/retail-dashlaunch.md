# Retail loading through dashlaunch

Evidence: a Falcon retail console, development kernel 2.0.17559.0 (per
`XboxKrnlVersion`), `XboxHardwareInfo.Flags = 0x20000227`, running
DashLaunch 3.21 (`ldata.ID = 0x444C3330` "DL30", `svnVer = 601`). Verified
by probe modules built with the stock Microsoft XDK toolchain (`cl.exe`,
`link.exe`, `imagexex.exe`) and loaded over FTP via `SITE LAUNCH` and
dashlaunch's `dlaunchStartSysModule`. Addresses below are this console's;
treat offsets into `xboxkrnl.exe`/`launch.xex` as build-specific the same
way `nuances.md` already does for devkit builds.

## `ExDebugMonitorService(0, module)` registration fails on retail

Calling kernel ordinal 426 with service 0 (register as debug monitor, the
path `register_debug_monitor` in `kernel.rs` uses) returns
`STATUS_INVALID_SYSTEM_SERVICE` (`0xC000001C`), not a permission error.
Verified from two contexts:

- A plain title-class XEX (so trivially possible to reproduce without
  touching dashlaunch at all).
- A correctly-flagged system module (`EXPORTS_TO_TITLE` + `SYSTEM_DEBUGGER`
  + `DLL`, no title-process flag; same combination `xex.toml` already
  declares), loaded through dashlaunch's own `dlaunchStartSysModule`
  (ordinal 2 of `launch.xex`, which itself calls
  `XexLoadImage(path, XEX_LOADIMG_FLAG_DLL, 0, NULL)`).

Both give the identical status. The ordinal resolves
(`XexGetProcedureAddress` succeeds). The export is a stub that returns
`0xC000001C` for every service number, independent of the caller's
privilege or module type; see "Kernel image analysis". **Consequence:**
`register_debug_monitor`/`debug_monitor_data()` and everything that reads
the kernel-maintained data block through them (module list, loader lock,
title/system process objects -- `debugger.rs`, `memory.rs`,
`performance/module_snapshot.rs`) has no retail equivalent through this
kernel export; see "dashlaunch's own answer" below.

## dashlaunch's `ldata` is the retail equivalent

`launch.xex` exports `ldat` (ordinal 1) and `dlaunchStartSysModule`
(ordinal 2), resolved the normal way:

```
XexGetModuleHandle("launch.xex", &handle);
XexGetProcedureAddress(handle, 1, &ldat);       // pldata
XexGetProcedureAddress(handle, 2, &startSysMod); // DLAUNCHSTARTSYSMODULE
```

`ldata` layout (32-bit fields except where noted; verified by reading it
live, offsets match dashlaunch's own `exports.h`):

| Offset | Field | Observed value |
|---:|---|---|
| 0x00 | `ID` | `0x444C3330` ("DL30") |
| 0x04 | `ltype` | `0x00000001` |
| 0x08 | `link[260]` | -- |
| 0x10c | `dev[260]` | -- |
| 0x210 | `versionMaj` (u16) | 3 |
| 0x212 | `versionMin` (u16) | 21 |
| 0x214 | `targetKernel` (u16) | 17559 |
| 0x216 | `svnVer` (u16) | 601 |
| 0x218 | `options` | -- |
| 0x21c | `DebugRoutine` | `0x801A58C4` |
| 0x220 | `DebugStepPatch` | `0x800727DC` |

These two addresses are dashlaunch's own per-build kernel-patch resolution
(its `kernpat[]` table), already picked for the kernel actually running --
openxbdm does not need its own per-build address table for this part, only
code to read `ldat` and use whatever it finds. This is exactly
`xbdm-nate`'s `dmmain.c` technique
(`g_dmGlobals.pLaunchData->DebugRoutine`/`DebugStepPatch`), confirmed live
on current dashlaunch (3.21) and kernel (17559), not just plausible from
reading old source.

Hooking the two addresses directly (`*DebugStepPatch = 0x60000000` nop;
save and replace `*DebugRoutine`) is therefore the retail exception-hook
mechanism, replacing `debugger.rs`'s `install()`/`uninstall()` kernel-side
when `ExDebugMonitorService` is unavailable. The `dashlaunch` feature
implements exactly this: `install()`/`uninstall()` still try
`register_debug_monitor` first (`debug_monitor_data()` word 0 nonzero means
it worked) and only fall back to `ldata` when it did not, in case a future
or different kernel build does support service 0. Consumers of
`debug_monitor_data()` outside the exception hook itself (module list,
loader lock, title/system process objects -- `memory.rs`,
`performance/profiler.rs`, `performance/capture.rs`,
`performance/module_snapshot.rs`, `performance/xtrace.rs`,
`session/swap.rs`) have no retail source yet; each now guards the zero
case instead of locking or reading through address 0, but the data itself
(module list, process objects) stays empty on retail, confirmed live
through `xeedee modules`/`xeedee threads` returning no rows while other
commands work normally.

## HV peek/poke: dashlaunch's syscall, not a new expansion

The patched HV already exposes a syscall (`li r0,0; sc`, 5-register
convention, key `0x72627472` = "rbtr") that dashlaunch wraps as
`HvxPeekCall`/`HvxPeek`/`HvxSetState` (`usvr/util.cpp`, `dll/utility.c`).
Confirmed live: `HvxPeekCall(0x72627472, 0xFF, 0, 0, 1)` returned `2`
(dashlaunch's own `HvxGetPeekVer` decodes this as `HVPEEK_MEMCPY`, its
newest protocol version). openxbdm should call this syscall directly by
number for whatever HV-level memory access it needs, not install its own
HV expansion the way `xbdm-nate`'s dead `hvx.c` does. The exposed surface
observed in dashlaunch's own source is memory peek (type 5) and a
protection/key-mode switch (`HvxSetState` modes 0-3); no SPR peek/poke
primitive was found there.

## `KeSetSpecialPurposeRegister` works directly; no HV mediation needed

`xbdm-nate`'s Release (JTAG) build routes DABR/DABRX through its own HV
expansion instead of calling `KeSetSpecialPurposeRegister` directly,
implying the kernel export didn't work for that purpose on JTAG consoles
of that era. On this console and kernel, calling
`KeSetSpecialPurposeRegister(0x3f5 /* DABR */, 0)` directly:

- Resolves (ordinal 165).
- Returns `0`.
- Does not fault or hang; execution continued through every later
  check in the same probe run.

This only proves the call is safe to make, not that the breakpoint
actually traps on hardware (not exercised here). Recommend using the
kernel export directly in `backend/debugger.rs` and confirming the
breakpoint fires in a follow-up hardware test, rather than building any
HV-mediated path for this.

## Networking: title-network caller ID, no `XNetStartup`, and `SO_DECRYPTSOCK`

Three fixes, all under the `dashlaunch` feature, each found from a distinct
hardware failure and each confirmed fixed by the next hardware run:

- **Skip `XNetStartup`/`WSAStartup`.** Devkit XBDM calls both itself on its
  own caller ID. Under dashlaunch the title/system network is already
  initialized by XAM before the plugin loads; calling `XNetStartup` again
  on an unused caller ID returned `WSAEINVAL` (10022) in a loop, confirmed
  via the `trace-log` feature once file-backed logging was added (no
  kernel debugger is available on this console). `initialize_network()`
  is a no-op under `dashlaunch`.
- **Caller ID 2 (`DMNETWORK`), not 3 (`XBDM`).** With `XNetStartup`
  skipped, caller 3 (devkit XBDM's own ID) was never initialized by
  anyone, so socket creation failed with `WSANOTINITIALISED` (10093).
  `xbdm-nate`'s retail/JTAG code uses caller 2 throughout and never calls
  `XNetStartup` itself, for the same reason. `XNCALLER_XBDM` is `2` under
  `dashlaunch`, `3` otherwise.
- **`SO_DECRYPTSOCK` (`mark_insecure()` in `net.rs`).** With the above two
  fixed, the server listened successfully and a loopback connection
  reached the real `201- connected` banner, but an external LAN peer got
  no response (connection timed out, not refused). The title-network
  caller is subject to the Xbox Live secure-channel requirement that
  devkit XBDM's separate debug network interface is not; both
  `xbdm-nate` (`dmnet.h`) and dashLaunch's own code (`usvr/util.cpp`)
  mark their sockets insecure before listening. Applied in `net::listen`,
  `net::bind_datagram`, and `performance/studio.rs`'s own listener (the
  only three places in the tree that create a listening socket --
  `smb.rs` reuses `net::listen`).

All three are hardware-confirmed together: after the third fix, an
external client (`xeedee`, run from off-console) connected to port 730
and a broad sweep of its commands worked (see below). Before it, only a
loopback connection worked.

## Command surface, confirmed live through `xeedee` (external client)

With the three networking fixes above, `openxbdm-server-xbox360`
(`dashlaunch` feature, no `trace-log`) was loaded via
`dlaunchStartSysModule` and driven entirely from `xeedee` running on a
separate host on the LAN, not loopback:

- Working normally: `ping`, `dbgname`, `dmversion`, `consoletype`,
  `consolefeatures`, `drivelist`, `consolemem`, `altaddr`, `netaddrs`,
  `getpid`, `xbeinfo`, `sockets`, `file ls`, `file get` (byte-identical,
  MD5-checked, for both a small file and a 648 KiB one), `magicboot`.
- `modules` and `threads` return zero rows: the direct, now-observed
  consequence of the `ExDebugMonitorService`/`debug_monitor_data()` gap
  above -- these commands walk the kernel-maintained module/thread lists
  through that data block, which stays zero on retail. Not a crash or a
  protocol bug; a real functionality gap with no retail data source yet.
- `systime` returned `ClockNotSet` -- genuine console state (no time
  sync yet this boot), not a bug.
- `df E` returned `AccessDenied` -- not chased further.
- `magicboot --title <path>` (no `--cold`) caused a full console reboot
  rather than the in-place retitle its own `--help` text implies ("Launch
  a title, relaunching in place unless `--cold` is given"). The console
  came back up cleanly afterward. Not isolated to confirm whether this is
  `dashlaunch`-specific (plausibly related to the same
  `debug_monitor_data()` gap, since in-place retitling is exactly the
  kind of operation that would use the kernel-maintained process/module
  state) or a general behavior; worth a follow-up if in-place relaunch
  matters for a particular workflow.

## Loading as a dashlaunch plugin

`launch.ini`'s `[Plugins]` section (`plugin1`..`plugin5`) is loaded by
`loadPlugins()` with `XexLoadImage(path, XEX_LOADIMG_FLAG_DLL, 0, NULL)` --
flag 8, a plain system-DLL load with **no bounding-path check and no
FlashEx search order**; none of `module-loading.md`'s bounding-path rules
apply to a dashlaunch-loaded module. `dlaunchStartSysModule` (ordinal 2)
is the same call, triggerable at runtime instead of waiting for a reboot,
and returns the real `XexLoadImage` `NTSTATUS` (confirmed: it varied with
build correctness across several live attempts, not a constant success).

A module must be built as:

- XEX module flags `EXPORTS_TO_TITLE` + `SYSTEM_DEBUGGER` + `DLL`, **no**
  title-process flag. A module that is also flagged as a title (the
  default of a bare `/DLL /SUBSYSTEM:XBOX` link with no manifest override)
  is rejected by `XexLoadImage` with `STATUS_ACCESS_DENIED` (`0xC0000022`).
- Placed at a load address nothing else already occupies. A collision
  (observed when reusing the address of an already-resident module) is
  rejected with `STATUS_CONFLICTING_ADDRESSES` (`0xC0000018`), not
  silently handled. `0x91f00000`, the address `xex.toml` and `link.x`
  already use, is this console's resident debug monitor's own address in
  this test environment specifically (this console was also running a
  devkit-style kernel debug session); on a plain retail boot with nothing
  else resident there it is expected to be free, matching dashlaunch's own
  `ldat` being observed to load itself at `0x91f193c0`-ish, i.e. in the
  same general window.

No bespoke device path works from inside a loaded plugin's Win32 file I/O
(`CreateFile`/xapilib): it needs XAM-brokered path translation that a bare
system module never gets set up for. Raw NT calls
(`NtCreateFile`/`NtWriteFile`/`NtClose`, exactly `kernel.rs`'s existing
`NtCreateFile` import and `xbdm-nate`'s `file.c` pattern) work immediately
with a plain `\Device\Harddisk0\Partition1\...` path. This is further
confirmation that openxbdm's existing choice to use raw `Nt*` kernel
imports throughout, rather than any Win32 wrapper, is required, not
stylistic.

## Build-tooling notes (not openxbdm-specific, but recorded in case the
## `xex-link`/Buck2 pipeline needs the same workarounds)

Building a correctly-flagged, non-default-based XEX2 with the stock XDK
command-line tools took three fixes that are easy to miss:

- `link.exe`'s own embedded XEX conversion (triggered whenever
  `/SUBSYSTEM:XBOX` is set, regardless of output extension) validates the
  requested `/BASE` against a narrow set of addresses per declared module
  type and fails most others with a misleading "is it 64K aligned?"
  message even when the address is fully aligned. `/XEX:NO` disables this
  pass and emits a plain PE instead.
- That plain PE must then be run through `imagexex.exe /IN:... /OUT:...
  /CONFIG:manifest.xml` as a separate step, with the manifest declaring
  `sysdll`+`sysdbg` and (for a non-default base) a matching `<baseaddr>`.
- The DLL link needs `/ALIGN:128,4096` (section:file) for `imagexex.exe`
  to accept the result at all; without it the same misleading alignment
  error appears regardless of the base address chosen.

## Kernel image analysis (kernel 2.0.17559.0)

Read from a live `getmem` dump of `0x80000000..0x801f0000` (read-only; the
range below `0x80040000` is not code). `xboxkrnl.exe` PE image base is
`0x80040000`; addresses below come from its export table and are
build-specific.

| Export | Ordinal | Address | Body |
|---|---:|---|---|
| `ExDebugMonitorService` | 426 | `0x8007bb68` | `lis r3,0xC000; ori r3,r3,0x1c; blr`: fails every service number |
| profiler enable | 669 | `0x80071b60` | `li r3,1; blr` |
| profiler stop/set routine | 670, 671 | `0x800bab90` | `blr`; both ordinals share one address |
| `KeDebugMonitorData` | 89 | `0x801a5758` | data word, not code |
| `setretail` target | 700 | `0x80065768` | real function |
| `KeSetSpecialPurposeRegister` | 165 | `0x800734cc` | hypervisor call wrapper (`li r0,7; sc`) |

Consequences:

- Services 1, 2, 4, 6, 7 and 8 of `ExDebugMonitorService` fail on this
  kernel, not only service 0. Service 1 is what fills the debug monitor
  data block, so the block stays zero.
- The profiler "enable" export reports success without doing anything and
  the other two are no-ops. Profiler start succeeds with no samples.
- The kernel side of `KeDebugMonitorData` is present. Three sites
  (`0x800860b0`, `0x8014d544`, `0x8014d5ac`) load the pointer and, when it
  is nonzero, call the routine at block offset `+0x18` with event codes
  137, 155 and 150. Publishing the block pointer and a dispatcher at `+0x18`
  is therefore sufficient for those notifications.
- Kernel text is writable by the plugin: `0x800727dc` reads `60 00 00 00`
  (the `DebugStepPatch` nop). A stub replacement of the export is possible
  with the same write method.
- The module list heads, loader lock, process objects and processor count
  are kernel globals that service 1 copies. Their addresses are not yet
  identified. Candidates lie in the debug globals at `0x801a5748..0x801a58e0`.

## Live sweep, console `192.168.42.29` (`xeedee-cli`, read-only)

Falcon, `Krnl=2.0.17559.0`, `openxbdm-dashlaunch.xex` on the HDD, server
reports `Type=DevKit`, `dmversion` 2.0.21256.0.

| Result | Commands |
|---|---|
| Works | `ping`, `dmversion`, `consoletype`, `consolefeatures`, `dbgname`, `altaddr`, `getpid`, `consolemem`, `netaddrs`, `xbeinfo`, `drivelist`, `getconsoleid`, `systeminfo`, `sockets`, `file ls` (`HDD:`, `GAME:`), `getmem`, `walkmem`, `pclist`, `querypc` |
| Empty (no data block) | `modules`, `threads` |
| `403 no such module` | `modsections` for `xboxkrnl.exe` and `xam.xex` |
| `405 no such thread` | `threadinfo`, `isstopped`, `getcontext` |
| `414 access denied` | `df` on `GAME`, `E`, `HDD`, `DEVKIT` (not isolated) |
| `406` | `systime` (clock not set) |
| Reply `OK`, no effect | `dvdeject`: tray did not move |

`dvdeject` calls `HalOpenCloseODDTray` and discards the result, as the
original does, so the `OK` reply does not show success. Cause not isolated;
it does not depend on `ExDebugMonitorService`.

Not exercised: `querypc` for `%` counter types (client `--kind` rejects hex),
breakpoints, `stop`/`go`, `magicboot`, the `counters.rs` per-processor
blocks.
