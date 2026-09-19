//! Mount an Xbox 360 devkit's drives as a local FUSE filesystem.
//!
//! This binary is only functional on non-Windows hosts. On Windows a
//! tiny stub main exists to give a clear runtime error instead of failing
//! the workspace build due to platform-incompatible FUSE dependencies.

//! Mount an Xbox 360 devkit's drives as a local FUSE filesystem.
//!
//! This binary is only functional on non-Windows hosts. On Windows a
//! tiny stub main exists to give a clear runtime error instead of failing
//! the workspace build due to platform-incompatible FUSE dependencies.

#[cfg(not(windows))]
mod unix_main {
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::time::Duration;

    use clap::Parser;
    use xeedee::Client;
    use xeedee::transport::tokio::Target;
    use xeedee::transport::tokio::connect_target_timeout;

    // filesystem module lives at crate root and is only compiled on non-Windows
    use crate::filesystem::XbdmFs;

    /// Mount an Xbox 360 devkit's drives as a local FUSE filesystem.
    #[derive(Parser, Debug)]
    pub struct Cli {
        /// Console hostname or IP (same accepted forms as `xeedee --host`).
        pub host: String,
        /// Local directory to mount onto. Must already exist.
        pub mountpoint: PathBuf,
        /// XBDM port.
        #[arg(long, default_value_t = 730)]
        pub port: u16,
        /// Connection timeout in seconds.
        #[arg(long, default_value_t = 15)]
        pub timeout: u64,
        /// Mount read-only: mutating operations (write, mkdir, rm, mv, create,
        /// truncate) fail with EROFS.
        #[arg(long)]
        pub read_only: bool,
        /// Log filter, e.g. `info`, `debug`, `xeedee=trace`.
        #[arg(long, default_value = "info")]
        pub log: String,
    }

    pub fn main() -> ExitCode {
        let cli = Cli::parse();

        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_new(&cli.log)
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .with_target(false)
            .init();

        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("failed to build tokio runtime: {e}");
                return ExitCode::FAILURE;
            }
        };

        let target = Target::parse(&cli.host, cli.port);
        let timeout = Duration::from_secs(cli.timeout);
        let client = match rt.block_on(async {
            let transport = connect_target_timeout(&target, timeout).await?;
            Client::new(transport).read_banner().await
        }) {
            Ok(client) => client,
            Err(report) => {
                eprintln!("error connecting to {target}: {report:?}");
                return ExitCode::FAILURE;
            }
        };

        let fs = XbdmFs::new(target, timeout, client, rt, cli.read_only);
        let mut config = fuser::Config::default();
        config.mount_options = vec![fuser::MountOption::FSName("xeedee".to_owned())];

        match fuser::mount2(fs, &cli.mountpoint, &config) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("mount error: {e}");
                ExitCode::FAILURE
            }
        }
    }
}

// filesystem module lives at crate root and is only compiled on non-Windows
#[cfg(not(windows))]
mod filesystem;

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    // call into the unix-specific main wrapper
    unix_main::main()
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    eprintln!("xeedee-fuse is not supported on Windows.");
    std::process::ExitCode::FAILURE
}
