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
    Plain { iso: String },
    Install { iso: String, answer: PathBuf },
    Gpu,
    UbuntuInstall(UbuntuInstall),
    Ubuntu(UbuntuBoot),
    OwnLinux { iso: String, gpu: UbuntuBoot },
    Macos(MacosBoot),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UbuntuInstall {
    pub iso: String,
    pub seed: PathBuf,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UbuntuBoot {
    WithGpu {
        image: PathBuf,
        allow_list: PathBuf,
        sensor_daemon: PathBuf,
        card: ForwardedCard,
    },
    NoGpu {
        reason_file: PathBuf,
    },
    Amd(AmdBoot),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkPlan {
    pub mode: Mode,
    pub passt: bool,
    pub bridge: String,
    pub mac: String,
    pub forwards: Vec<Forward>,
    pub resolver: Option<String>,
    pub only_through: Option<OnlyThrough>,
    pub passt_age: PasstAge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmPlan {
    pub boot: Boot,
    pub slot: Slot,
    pub name: String,
    pub qemu: PathBuf,
    pub qemu_files: Option<PathBuf>,
    pub firmware: Firmware,
    pub run_folder: PathBuf,
    pub agent_port: u16,
    pub files_port: Option<u16>,
    pub cores: u32,
    /// QEMU `-smp` value, SMT-aware when the host has hyperthreading.
    pub smp: String,
    pub ram: String,
    pub screen: String,
    pub disk: String,
    pub nvme: bool,
    pub vars: String,
    pub tpm: Option<PathBuf>,
    pub pin: Option<String>,
    pub share: Option<String>,
    pub drives: Vec<Drive>,
    pub share_program: PathBuf,
    pub launcher: PathBuf,
    pub vram: Option<String>,
    pub mouse: bool,
    pub render_identity: PciIdentity,
    pub network: NetworkPlan,
    pub tools_disc: Option<PathBuf>,
    pub sound: Option<Sound>,
    pub jail: Option<QemuJail>,
    pub jail_note: String,
    pub memory_by_path: Option<MemoryFiles>,
    pub drives_by_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sound {
    pub sink: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QemuJail {
    pub launcher: PathBuf,
    pub namespaces: bool,
    pub qemu_sandbox: bool,
    pub sound_socket: Option<PathBuf>,
    pub backing_files: Vec<PathBuf>,
}

impl VmPlan {
    pub fn new(context: &Context, machine: &Machine, boot: Boot, slot: Slot, vram_gib: u32) -> Self {
        let run_folder = context.paths.run_folder(slot);
        let drives = Self::drives_for(machine, &boot);
        let tools = context.paths.store.join("iso/tools.iso");
        let nat = machine.network_mode == Mode::Nat;
        let only_through = if nat { OnlyThrough::read(&machine.only_through) } else { None };
        let passt = nat && (only_through.is_some() || (machine.passt && passt_installed()));
        let passt_age = match &only_through {
            None if passt => passt_age_now().unwrap_or_default(),
            _ => PasstAge::default(),
        };
        let resolver = match &only_through {
            Some(through) => dns_now(&machine.dns, &through.interface),
            None if passt && passt_age == PasstAge::NoDnsHost => passt_age::outside_resolver_now(),
            None if passt => host_resolver(),
            None => None,
        };
        Self {
            boot,
            slot,
            name: machine.id.clone(),
            qemu: context.paths.qemu.clone(),
            qemu_files: Some(context.paths.helper("qemu/pc-bios")).filter(|folder| folder.is_dir()),
            firmware: context.paths.firmware.clone(),
            agent_port: context.paths.agent_port(slot),
            files_port: (machine.kind == crate::machine::Kind::Windows).then(|| slot.files_port()),
            cores: machine.cores,
            smp: crate::host::smp_topology(&context.cpus, machine.cores),
            ram: format!("{}G", machine.ram_gib),
            screen: if machine.macos() { machine.screen.clone() } else { machine.basic_screen().to_owned() },
            disk: machine.disk.clone(),
            nvme: machine.nvme,
            vars: machine.vars.clone(),
            tpm: machine.tpm.then(|| machine.tpm_folder(context)),
            pin: None,
            share: Some(machine.share.clone()).filter(|share| !share.is_empty()),
            drives,
            share_program: context.paths.binary(SHARE_PROGRAM),
            launcher: context.paths.binary(LAUNCHER),
            vram: (vram_gib > 0).then(|| format!("{vram_gib}G")),
            mouse: machine.has_gpu(),
            render_identity: shown_identity(context, machine),
            network: NetworkPlan {
                mode: machine.network_mode,
                passt,
                bridge: machine.bridge.clone(),
                mac: machine.mac.clone(),
                forwards: machine.forwards.clone(),
                resolver,
                only_through,
                passt_age,
            },
            tools_disc: tools.is_file().then_some(tools),
            sound: None,
            jail: None,
            jail_note: String::new(),
            memory_by_path: None,
            drives_by_path: None,
            run_folder,
        }
    }

    pub fn is_gpu(&self) -> bool {
        self.boot == Boot::Gpu
    }

    fn in_run_folder(&self, name: &str) -> String {
        self.run_folder.join(name).display().to_string()
    }

    fn screen_size(&self) -> (u64, u64) {
        let (width, height) = self.screen.split_once('x').unwrap_or_default();
        (width.parse().unwrap_or(0), height.parse().unwrap_or(0))
    }
}

pub fn on_path(program: &str) -> bool {
    program_on_path(program).is_some()
}

pub fn program_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).map(|folder| folder.join(program)).find(|found| is_program(found))
}

pub fn program_word(program: &str) -> String {
    program_on_path(program).map_or_else(|| program.to_owned(), |found| found.display().to_string())
}

fn is_program(path: &Path) -> bool {
    rustix::fs::access(path, rustix::fs::Access::EXEC_OK).is_ok() && path.is_file()
}

pub fn passt_installed() -> bool {
    on_path("passt")
}

pub fn passt_age_now() -> Option<PasstAge> {
    passt_age::age_now(passt_probe_words)
}

pub fn passt_start_problem() -> Option<String> {
    if !passt_installed() {
        return None;
    }
    let age = passt_age_now();
    let outside = (age == Some(PasstAge::NoDnsHost)).then(passt_age::outside_resolver_now).flatten();
    passt_age::start_problem(age, outside.as_deref())
}

fn host_resolver() -> Option<String> {
    let text = fs::read_to_string(RESOLVER_FILE).ok()?;
    text.lines().find_map(|line| {
        let address = line.strip_prefix("nameserver ")?.trim();
        address.bytes().all(|byte| byte.is_ascii_digit() || byte == b'.').then(|| address.to_owned())
    })
}

#[cfg(test)]
mod tests;
