ENTRY(_start)

/* Retail, dashlaunch-loaded build only (xex-dashlaunch.toml): dashlaunch's
   own `launch.xex` is resident at 0x91f00000 on every boot (confirmed live,
   docs/re-notes/retail-dashlaunch.md), so this build cannot share link.x's
   address with the devkit build without colliding
   (STATUS_CONFLICTING_ADDRESSES). 0x91500000 is a free gap between
   `hud.xex` and `ximecore.xex`, confirmed live the same way.
   Otherwise identical to link.x -- see that file for section layout
   reasoning. */
SECTIONS {
    . = 0x91501000;
    .text : {
        *(.text._start)
        *(.text .text.*)
    }
    . = ALIGN(4);
    .xthunks : { *(.xthunks .xthunks.*) }

    . = ALIGN(0x1000);
    .rodata : { *(.rodata .rodata.*) }

    . = ALIGN(0x1000);
    .data : { *(.data .data.*) *(.sdata .sdata.*) }
    . = ALIGN(4);
    .xvars : { *(.xvars .xvars.*) }

    . = ALIGN(0x1000);
    .bss : { *(.bss .bss.*) *(.sbss .sbss.*) *(COMMON) }

    /* Export records: kept as GC roots, read by xex-link, not loaded. */
    . = ALIGN(4);
    .xexports : { KEEP(*(.xexports)) }

    /DISCARD/ : { *(.eh_frame) *(.eh_frame_hdr) *(.comment) *(.note .note.*) }
}
