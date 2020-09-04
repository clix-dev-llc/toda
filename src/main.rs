#![feature(box_syntax)]
#![feature(async_closure)]
#![feature(vec_into_raw_parts)]
#![feature(atomic_mut_ptr)]
#![allow(clippy::or_fun_call)]
#![allow(clippy::too_many_arguments)]

extern crate derive_more;

mod fuse_device;
mod futex;
mod hookfs;
mod injector;
mod mount;
mod mount_injector;
mod namespace;
mod ptrace;
mod replacer;
mod utils;

use injector::InjectorConfig;
use mount_injector::{MountInjectionGuard, MountInjector};
use replacer::{Replacer, UnionReplacer};
use utils::encode_path;

use anyhow::Result;
use nix::sys::mman::{mlockall, MlockAllFlags};
use nix::sys::signal::{signal, SigHandler, Signal};
use nix::unistd::{pipe, read, write};
use structopt::StructOpt;
use tracing::{error, info, Level};

use std::os::unix::io::RawFd;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

#[derive(StructOpt, Debug, Clone)]
#[structopt(name = "basic")]
struct Options {
    #[structopt(short, long)]
    pid: i32,

    #[structopt(long)]
    path: PathBuf,

    #[structopt(short = "v", long = "verbose", default_value = "trace")]
    verbose: String,
}

fn inject(option: Options) -> Result<MountInjectionGuard> {
    info!("parse injector configs");
    let injector_config: Vec<InjectorConfig> = serde_json::from_reader(std::io::stdin())?;
    info!("inject with config {:?}", injector_config);

    let path = option.path.clone();
    let fuse_dev = fuse_device::read_fuse_dev_t()?;

    let before_mount = Arc::new(futex::Futex::new());
    let after_mount = Arc::new(futex::Futex::new());
    let cloned_before_mount = before_mount.clone();
    let cloned_after_mount = after_mount.clone();
    
    let handler = namespace::with_mnt_pid_namespace(
        box move || -> Result<_> {
            let mut replacer = UnionReplacer::new();
            replacer.prepare(&path, &path)?;

            if let Err(err) = fuse_device::mkfuse_node(fuse_dev) {
                info!("fail to make /dev/fuse node: {}", err)
            }

            cloned_before_mount.wake(1)?;
            cloned_after_mount.wait()?;

            // At this time, `mount --move` has already been executed.
            // Our FUSE are mounted on the "path", so we
            replacer.run()?;
            drop(replacer);
            info!("replacer detached");

            Ok(())
        },
        option.pid,
    )?;

    before_mount.wait()?;

    let mut injection = MountInjector::create_injection(&option.path, injector_config)?;
    let mount_guard = injection.mount(option.pid)?;

    after_mount.wake(1)?;

    handler.join()??;
    info!("enable injection");
    mount_guard.enable_injection();

    Ok(mount_guard)
}

fn resume(option: Options, mut mount_guard: MountInjectionGuard) -> Result<()> {
    info!("disable injection");
    mount_guard.disable_injection();
    let path = option.path.clone();

    let pid = option.pid;

    let before_recover_mount = Arc::new(futex::Futex::new());
    let after_recover_mount = Arc::new(futex::Futex::new());
    let cloned_before_recover_mount = before_recover_mount.clone();
    let cloned_after_recover_mount = after_recover_mount.clone();
    let handler = namespace::with_mnt_pid_namespace(
        box move || -> Result<_> {
            let (_, new_path) = encode_path(&path)?;

            let mut replacer = UnionReplacer::new();
            replacer.prepare(&path, &new_path)?;
            info!("running replacer");
            replacer.run()?;

            cloned_before_recover_mount.wake(1)?;
            after_recover_mount.wait()?;

            drop(replacer);
            info!("replacers detached");
            info!("recover successfully");
            Ok(())
        },
        pid,
    )?;

    before_recover_mount.wait()?;
    info!("recovering mount");
    mount_guard.recover_mount(option.pid)?;
    cloned_after_recover_mount.wake(1)?;

    if let Err(err) = handler.join()? {
        error!("join error: {:?}", err);
    }

    Ok(())
}

static mut SIGNAL_PIPE_WRITER: RawFd = 0;

const SIGNAL_MSG: [u8; 6] = *b"SIGNAL";

extern "C" fn signal_handler(_: libc::c_int) {
    unsafe {
        write(SIGNAL_PIPE_WRITER, &SIGNAL_MSG).unwrap();
    }
}

fn main() -> Result<()> {
    mlockall(MlockAllFlags::MCL_CURRENT)?;

    let (reader, writer) = pipe()?;
    unsafe {
        SIGNAL_PIPE_WRITER = writer;
    }

    // ignore dying children
    // unsafe { signal(Signal::SIGCHLD, SigHandler::SigIgn)? };
    unsafe { signal(Signal::SIGINT, SigHandler::Handler(signal_handler))? };
    unsafe { signal(Signal::SIGTERM, SigHandler::Handler(signal_handler))? };

    let option = Options::from_args();
    let verbose = Level::from_str(&option.verbose)?;
    let subscriber = tracing_subscriber::fmt().with_max_level(verbose).finish();
    tracing::subscriber::set_global_default(subscriber).expect("no global subscriber has been set");

    let mount_injector = inject(option.clone())?;

    info!("waiting for signal to exit");
    let mut buf = vec![0u8; 6];
    read(reader, buf.as_mut_slice())?;
    info!("start to recover and exit");

    resume(option, mount_injector)?;

    Ok(())
}
