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

## `KeDebugMonitorData` is nonzero but the block is entirely zeroed

Live on the same console (kernel 2.0.17559.0, `openxbdm-dashlaunch.xex`
loaded): `KeDebugMonitorData` at `0x801a5758` (confirmed live by
disassembling the three call sites named above -- `0x800860b0` contains
`lis r31,0x801a` / `lwz r11,0x5758(r31)`, landing on exactly this address)
currently holds `0x915d96c0`, not null. Reading the full `0x68`-byte block
at that address returns all zero: `module_list()`, `loader_lock()`,
the dispatcher slot, and `processor_count()` are all `0`. `modules` and
`threads` are still empty through `xeedee-cli` at the same moment.

Conclusion: a nonzero `KeDebugMonitorData` does not imply a populated
block or an active monitor on this kernel. Something (most likely a
kernel boot-time allocation, independent of any module ever calling
`ExDebugMonitorService`) reserves a zeroed placeholder so the pointer is
never null, but nothing on this build ever fills it in -- consistent
with "every service number fails unconditionally," not merely service 0.
This closes the loop started in "Kernel image analysis" above: the gap is
structural on this kernel, not an artifact of openxbdm's own failed
registration attempt.

## Per-processor control blocks near the debug globals

`0x801a5780`-`0x801a579c` holds six pointers, `0x1000` apart:
`0x801b0100`, `0x801b1100`, `0x801b2100`, `0x801b3100`, `0x801b4100`,
`0x801b5100`. Reading the first one shows a `+0x10` field of `6` (the
hardware thread count: 3 cores x 2 threads, Xenon) and a `+0x14` field
that varies per entry (`5`, `2`, ... -- plausibly a logical processor
index, though not confirmed in which direction). Likely a KPCR-style
per-processor table; not pursued further since it does not bear on the
module/thread list gap.

## A working, registration-free path to the module list: `XexPcToFileHeader`

`xbdm-nate`'s retail/JTAG build (`../xbdm-nate`, consulted here only for
technique, not copied -- see `dmmain.c`'s `_resolve()`) never calls
`ExDebugMonitorService` for the module list at all. It resolves the
address of an always-present, ordinarily-exported kernel function, then
reads two of that function's own instruction words to recover the
absolute address of a kernel global the function references internally
(`lis`/`addi` pair materializing a constant address -- a byte-code
reading trick, not a documented API). Nate's own ordinals and word
offsets are for a different, older kernel build and do not apply here;
the technique itself does, and was re-derived independently, live,
against this console's actual kernel build.

Nate resolves `PsLoadedModuleList` and `XexLoadedModuleListLock` off
kernel ordinal 412. On this kernel (2.0.17559.0), ordinal 412 is
`XexPcToFileHeader` (confirmed from the XDK's own `xboxkrnl.lib` import
records, not from any third-party source). Its address was computed by
parsing the live kernel image's own PE export directory by hand (image
base `0x80040000`, confirmed from `MmGetMmGlobalData`-style constants
already in these notes; export table at RVA `0x15e00`, ordinal base 1,
`AddressOfFunctions` at RVA `0x15e2c`; ordinal 412 -> index 411 -> RVA
`0x3b498` -> address `0x8007b498`). Disassembling it live (via Capstone,
PPC big-endian) gives:

```
lis   r11, 0x801a
addi  r30, r11, 0x5e18      ; r30 = 0x801a5e18  (module list spinlock)
...
bl    0x80070138            ; acquire spinlock r30 (lwarx/stwcx. + IRQL raise; confirmed by disassembly)
lis   r11, 0x8017
addi  r10, r11, 0x700       ; r10 = 0x80170700  (module list head, sentinel LIST_ENTRY)
lwz   r11, 0(r10)           ; r11 = head.Flink
...loop:
  lwz   r9, 0x18(r11)       ; entry.Base
  cmplw cr6, r9, r31        ; r31 = search PC (first argument)
  bgt   cr6, advance
  lwz   r8, 0x20(r11)       ; entry.Size
  add   r8, r8, r9          ; end = Base + Size
  cmplw cr6, r31, r8
  blt   cr6, found          ; Base <= pc < end -> return this entry
advance:
  lwz   r11, 0(r11)         ; entry.Flink
  cmplw cr6, r11, r10
  bne   cr6, loop
...
bl    0x8007018c            ; release spinlock r30
```

Verified live by walking the list by hand: `head.Flink` (`*0x80170700`)
is `0x801a5d60`. Reading that node's `+0x18`/`+0x20` fields gives
`Base=0x80040000`, `Size=0x001b0000` -- an exact match for
`xboxkrnl.exe`'s own `ImageBase`/`SizeOfImage`, independently parsed
moments earlier from the live PE header. Strong confirmation the walk and
field offsets are correct for this build.

**Resolved, live, via the `rpc` command (see "Calling into the kernel
live" below):** the `+0x00` value that looked like garbage
(`0x3a0c5f48`) was not garbage -- it is a legitimate `ExAllocatePool`
heap address. `ExAllocatePoolWithTag` (ordinal 10) returned an address
in the exact same `0x3a0xxxxxx` range on this console
(`0x3a13bd48`), which is what prompted re-reading that address instead
of discarding it as corrupt. It is a second, fully-formed list node:
`Flink=0x3a0d3f48`, `Blink=0x801a5d60` (back to the `xboxkrnl.exe`
node, confirming a proper doubly-linked circular list), `+0x18`/`+0x1c`
both `0x815f0000` (`Base`), `+0x20` `0x00610000` (`Size`), and a
`BaseDllName`/`FullDllName` pair resolving to `\Device\Flash\xam.xex`.
One more hop (`Flink=0x3a0d3f48`) lands on `Base=0x91f00000` -- this
console's own loaded `openxbdm-dashlaunch.xex`. So this really is one
unified list (`xboxkrnl.exe` -> `xam.xex` -> our own module, continuing
from there), exactly as `xbdm-nate` assumed, no second list. The earlier
"two lists" theory above (and the `xkelib`-vs-live struct-offset
"discrepancy" that motivated it) is superseded: `xkelib`'s
`LDR_DATA_TABLE_ENTRY` is not wrong, `+0x18` (`NtHeadersBase`) and
`+0x1c` (`ImageBase`) are simply equal for every node seen so far
(plausible if the mapped image's PE/NT header sits at the very start of
the image once XEX-loaded, unlike a loose PE file with a DOS stub
first). `kernel.rs` should still read `Base`/`Size` at `+0x18`/`+0x20`
-- that is what the live kernel code itself reads for the comparison,
and it is correct regardless of whichever of `+0x18`/`+0x1c` is "really"
`ImageBase` by name.

## Calling into the kernel live, via the `rpc` XBDM command

`openxbdm`'s own `commands/group6.md` already specs the `rpc` command in
full (`version=4 system buf_size=... [thread=...]`), separately
reverse engineered from devkit XBDM's handler, independent of
`xbdm-nate`. A prebuilt client (`x360 rpc-call <module> <ordinal>
[args...]`, this session's workspace, built on the same `xeedee`
transport as `xeedee-cli`) drives it directly: `<module>!<ordinal>(args)`
in a system thread, returning the call's integer result. Confirmed
live against this console (`dashlaunch`-loaded, kernel `2.0.17559.0`):

- `rpc-call xboxkrnl.exe 412 0x1 0x0` (a PC that cannot match any
  module, output pointer NULL) returned `0` cleanly, no crash --
  confirms the "no match" path never touches the output pointer,
  matching the disassembly, and confirms `rpc system` calls work
  against the `dashlaunch`-hosted XBDM here at all.
- `rpc-call xboxkrnl.exe 10 0x10 0x6f786264` (`ExAllocatePoolWithTag`,
  ordinal 10, confirmed against both the XDK's `xboxkrnl.lib` and
  `xkelib`'s `kernelExpEnum.h`) returned a genuine heap address, used
  above to recognize the `0x3a0xxxxxx` range as real memory rather than
  garbage.
- `rpc-call xboxkrnl.exe 15 <addr>` (`ExFreePool`, ordinal 15) freed it
  afterward; returned `0`.

This is a safe, general-purpose way to continue this kind of
investigation without hand-walking raw `getmem` dumps or risking a
blind write: allocate scratch with ordinal 10, pass its address as any
function's output parameter, read the result with `getmem`, free with
ordinal 15. Worth reaching for before any further live kernel-structure
work on this console.

**Cross-reference, not a source of truth:** `xkelib`
(`github.com/cOzInABox/xkelib`, community-maintained, used here only to
sanity-check where to look, per this session's direction that the
clean-room constraint targets the 1vs100 side of this workspace, not
openxbdm) ships `kernel/kernelext21256.0.def`, matching this console's
exact `dmversion` (`2.0.21256.0`). It independently confirms ordinal
412 = `XexPcToFileHeader` and ordinal 89 = `KeDebugMonitorData` -- same
answer as the XDK's own `xboxkrnl.lib` and our disassembly, good
cross-check. Its `LDR_DATA_TABLE_ENTRY` (`kernel/keXexInfo.h`), however,
puts `ImageBase` at `+0x1c` (three leading 8-byte `LIST_ENTRY`s:
`InLoadOrderLinks`, `InClosureOrderLinks`, `InInitializationOrderLinks`)
-- not `+0x18`, where our live read and live disassembly, on this exact
build, put it. Rather than treat this as a contradiction to resolve by
picking a side, it supports the two-list theory above: `xkelib`'s
struct most plausibly describes the richer, title-capable entry type
(worth the extra `InClosureOrderLinks` bookkeeping XEX loading needs),
while `PsLoadedModuleList`/`XexPcToFileHeader` on this build may walk a
slimmer, kernel-image-only list with a smaller leading-`LIST_ENTRY`
count. Any further use of `xkelib` here should stay at this level --
pointing at what to check live -- not as a source of addresses or
offsets to port directly; everything that ends up in `kernel.rs` still
needs this console's own disassembly or an on-console call to confirm.

**Why this matters:** this is a real, live-verified, retail-compatible
path to at least part of the module list, entirely independent of
`ExDebugMonitorService`/`KeDebugMonitorData` (which the two sections
above show is dead on this kernel). A clean-room port should not
hardcode this build's addresses (`0x801a5e18`, `0x80170700`,
`0x8007b498`); instead, resolve `XexPcToFileHeader`'s address at runtime
via `XexGetProcedureAddress(xboxkrnl.exe, 412)` (works across kernel
builds, same as any other export use in this codebase) and recover the
embedded addresses by scanning its first dozen or so instructions for
the `lis`/`addi` pair pattern at runtime, rather than hardcoding word
offsets the way `xbdm-nate` does -- `_resolve()`'s fixed offsets are
exactly the kind of per-build fragility this avoids.
