#![cfg_attr(target_arch = "powerpc", no_std)]
#![cfg_attr(target_arch = "powerpc", no_main)]

#[cfg(all(feature = "dashlaunch", feature = "boot-guard"))]
compile_error!(
    "dashlaunch (retail, loaded as a plugin) and boot-guard (devkit, loaded from DEVKIT:\\FlashEx) are different deployment models and cannot both be built in"
);

#[cfg(target_arch = "powerpc")]
mod backend;
#[cfg(all(target_arch = "powerpc", feature = "dashlaunch"))]
mod dashlaunch;
#[cfg(target_arch = "powerpc")]
mod entry;
#[cfg(target_arch = "powerpc")]
mod exports;
#[cfg(target_arch = "powerpc")]
mod kernel;
#[cfg(target_arch = "powerpc")]
mod lockfree;
#[cfg(target_arch = "powerpc")]
mod net;
// Devkit only: a dashlaunch plugin is loaded directly
// (docs/re-notes/retail-dashlaunch.md), not through the FlashEx override.
#[cfg(all(target_arch = "powerpc", not(feature = "dashlaunch")))]
mod override_module;
#[cfg(target_arch = "powerpc")]
mod panic;
#[cfg(target_arch = "powerpc")]
mod serve;
#[cfg(all(target_arch = "powerpc", feature = "smb"))]
mod smb;
#[cfg(target_arch = "powerpc")]
mod thread;
#[cfg(target_arch = "powerpc")]
mod trace;

#[cfg(not(target_arch = "powerpc"))]
fn main() {
    println!("openxbdm-server-xbox360 must be built for powerpc-unknown-xbox360");
}
