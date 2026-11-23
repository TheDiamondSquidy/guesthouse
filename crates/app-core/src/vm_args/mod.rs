//! What a VM starts with, as data: every QEMU argument, and nothing here runs anything.
//!
//! The machine's settings and the state of the PC go in, and [`VmPlan::qemu_command`] gives the words
//! to run. The order of devices is what the guest has already seen, so it must not change.

mod amd;
mod arguments;
mod drives;
mod forwarding_jail;
mod macos;
mod nic;
mod own_linux;
mod passt_age;
mod qemu_jail;
mod standalone;
mod ubuntu;

pub use amd::{AmdBoot, RENDER_SERVER};
pub use macos::{MacosBoot, MacosCard, UDMABUF};
pub use nic::{PASST_PID_FILE, only_through_probe_words, passt_probe_words};
pub use passt_age::PasstAge;
pub use qemu_jail::{PASST_FD, PID_FILE, TAP_FD};
pub use standalone::memory_files_in;
pub use ubuntu::{FORWARDED_INPUT_SOCKET, FRAMES_PREFIX, INSTALL_LOG, ssh_port};

use crate::context::Context;
use crate::firmware::Firmware;
use crate::gpu_identity::{PciIdentity, shown_identity};
use crate::jailing::LAUNCHER;
use crate::linux::card::ForwardedCard;
use crate::machine::{Drive, Machine};
use crate::network::{Forward, Mode};
use crate::only_through::{OnlyThrough, dns_now};
use guesthouse_daemon::memory_files::MemoryFiles;
use guesthouse_daemon::slot::Slot;
use std::fs;
use std::path::{Path, PathBuf};

/// QEMU's monitor for the viewer, which takes the socket its D-Bus display listens on.
pub const DISPLAY_SOCKET: &str = "display.sock";
pub const SHARE_PROGRAM: &str = "guesthouse-share";
/// The file QEMU's output goes to, which holds why it failed to start.
pub const OUTPUT_FILE: &str = "vm-gpupv.out";
/// Where a guest reaches a shared folder.
pub const SHARE_ADDRESS: &str = "10.0.2.4";

const RESOLVER_FILE: &str = "/etc/resolv.conf";

/// How a VM is to boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Boot {
    /// A machine with no GPU: its disk, and its ISO first if it has one. The drive is there either
    /// way, as a tray with nothing in it, so the ISO can be ejected while the VM runs.
    Plain { iso: String },
    /// Windows from its disc, with the answer disc beside it.
    Install { iso: String, answer: PathBuf },
    /// With the GPU, which needs the daemon up first.
    Gpu,
    /// Ubuntu's installer, from the ISO's own kernel.
    UbuntuInstall(UbuntuInstall),
    /// An installed Ubuntu guest.
    Ubuntu(UbuntuBoot),
    /// A plain machine holding a Linux the user installed, with the forwarded GPU if it can have it.
    OwnLinux { iso: String, gpu: UbuntuBoot },
    /// A plain machine holding macOS, on reims-vgpu's QEMU.
    Macos(MacosBoot),
}

/// What Ubuntu's unattended install starts from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UbuntuInstall {
    pub iso: String,
    pub seed: PathBuf,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
}

/// An Ubuntu guest, or a Linux the user installed, gets the forwarded GPU, an AMD card, or the reason it
/// has no GPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UbuntuBoot {
    WithGpu {
        image: PathBuf,
        allow_list: PathBuf,
        sensor_daemon: PathBuf,
        card: ForwardedCard,
    },
    /// A file holding the reason, which reaches the guest so that it can say why.
    NoGpu {
        reason_file: PathBuf,
    },
    Amd(AmdBoot),
}

/// What the first NIC is, and what the private link carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkPlan {
    pub mode: Mode,
    /// NAT through passt, rather than QEMU's own.
    pub passt: bool,
    pub bridge: String,
    pub mac: String,
    pub forwards: Vec<Forward>,
    /// The DNS server passt passes the guest's names on to: the host's, or while the guest goes out
    /// through one interface, one that works there. For a passt too old for that, one off the PC that
    /// the guest asks itself.
    pub resolver: Option<String>,
    /// NAT only: the one interface all of the guest's traffic goes through. It takes passt, whatever
    /// `passt` says, and passt must be started by the jail's launcher, which guards its connections.
    pub only_through: Option<OnlyThrough>,
    /// Which of the app's words this PC's passt takes.
    pub passt_age: PasstAge,
}

/// Everything QEMU's arguments are built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmPlan {
    pub boot: Boot,
    pub slot: Slot,
    /// The machine's id, which an Ubuntu guest's QEMU goes by.
    pub name: String,
    pub qemu: PathBuf,
    /// QEMU's own files, if it isn't the system's, which knows where its files are.
    pub qemu_files: Option<PathBuf,
