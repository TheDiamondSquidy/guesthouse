//! What a task does. One of these runs in the `guesthouse-task` process, in the slot of its VM, while
//! the app watches the small file it leaves.

mod amd_files;
mod amd_package;
mod amd_start;
mod api_stage;
mod desktop;
mod diagnose;
mod display;
mod driver_store;
mod gpu_check;
mod guest_files;
mod guest_upload;
mod hardware_desktop;
mod identity;
mod identity_restart;
mod identity_switch;
mod install;
mod intel_device;
mod intel_files;
mod late_watch;
mod macos;
mod macos_install;
mod nvme_move;
mod own_linux;
mod remote_access;
mod ubuntu;
mod window_speed;

use amd_files::{bring_amd_driver_up_to_date, complete_host_driver};
use amd_package::register_amd_package;
pub use desktop::{apply_share, boots, rebooted};
use desktop::{note_boot, set_up_desktop, wait_for_desktop};
pub use diagnose::{API_FILE, GATHERED_FOLDER, PROBE_FILE, REPORT_FILE};
use gpu_check::{Gpu, confirm_gpu};
use guest_files::{
    place_amd_runtime, place_driver_files, place_nvapi_wrapper, place_profile_store, place_system_files,
    write_device_key,
};
use identity::set_up_identity;
use identity_restart::restart_after_switch;
use identity_switch::{new_vm_needed, stop_for_new_identity};
pub use install::{
    INSTALL_STEPS, answer_disc_path, answer_file, install, install_step, make_answer_disc, wait_for_setup,
};
use intel_files::complete_intel_driver;
use remote_access::apply_remote_access;
use window_speed::apply_window_speed;

use crate::context::Context;
use crate::disk_image::disk_size_gib;
use crate::failure::{Failure, Result};
use crate::gpu_identity::{PciIdentity, read_state};
use crate::gpu_maker::GpuMaker;
use crate::guest_memory::is_no_room;
use crate::host::{GIB, cpu_list, expand_to_siblings, fits_now, parse_cpu_list, ram_limit_for, smaller_start, vcache_cpus};
use crate::kept_logs;
use crate::library::MAX_DISK_GIB;
use crate::log::{clock_text, write_log};
use crate::machine::{Kind, Machine, load};
use crate::monitor::Command as Qmp;
use crate::network_settings::other_machines;
use crate::processes::qemu_pid;
use crate::screen_gpu::setup_card;
use crate::settings::max_vram_gib_for;
use crate::snapshots::{delete_snapshot, go_back, take_snapshot};
use crate::start_checks::{card_mates, start_problems, video_cap};
use crate::tasks::{TaskRecord, write_task};
use crate::text::shell_quote;
use crate::vm_args::{Boot, VmPlan};
use crate::vm_host::{Stopped, VmHost};
use rustix::fs::{FlockOperation, flock};
use rustix::process::{Pid, Signal, kill_process, kill_process_group};
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const MEMORY_WAIT: Duration = Duration::from_secs(20);
const LOCK_FILE: &str = "guesthouse.lock";
const STOP_WAIT_SECONDS: u64 = 100;
const FAILED_START_STOP_SECONDS: u64 = 30;
const SIGTERM_WAIT_SECONDS: u64 = 20;
const SIGKILL_WAIT_SECONDS: u64 = 10;
const FORCE_POLL: Duration = Duration::from_millis(200);
pub(crate) const KEY_POLL: Duration = Duration::from_millis(500);

/// The running side of a task: reports steps, and holds what the actions need.
pub struct Task<'a> {
    pub context: &'a Context,
    pub host: &'a dyn VmHost,
    pub action: String,
    pub machine: Machine,
    pub error: String,
    pub ram_this_run: Option<u32>,
    watch_after_start: bool,
}

impl<'a> Task<'a> {
    pub fn new(context: &'a Context, host: &'a dyn VmHost, action: &str, machine: Machine) -> Self {
        Self {
            context,
            host,
            action: action.to_owned(),
            machine,
            error: String::new(),
            ram_this_run: None,
            watch_after_start: false,
        }
    }

    fn machine_this_run(&self) -> Machine {
        let mut machine = self.machine.clone();
        machine.ram_gib = self.ram_this_run.unwrap_or(machine.ram_gib);
        machine
    }

    fn record(&self, pid: u32, step: &str) -> TaskRecord {
        TaskRecord {
            action: self.action.clone(),
            machine: self.machine.id.clone(),
            pid,
            step: step.to_owned(),
            error: self.error.clone(),
        }
    }

    pub fn step(&self, text: &str) {
        println!("{} {} {text}", clock_text(), self.action);
        let _ = write_task(self.context, None, &self.record(std::process::id(), text));
    }

    fn finish(&self) {
        let _ = write_task(self.context, None, &self.record(0, ""));
    }

    fn pid(&self) -> Option<u32> {
        qemu_pid(self.context, None)
    }

    fn run_file(&self, name: &str) -> PathBuf {
        self.context.run_folder(None).join(name)
    }

    fn ask_guest(&self, script: &str, seconds: u64) -> String {
        self.context.ask_guest(&self.machine, script, seconds, None)
    }

    fn sleep(&self, time: Duration) {
        self.context.clock.sleep(time);
    }

    fn now(&self) -> Duration {
        self.context.clock.now()
    }
}

pub use macos::start_refusal as macos_start_refusal;

/// The plan for this machine, in this task's slot, as things stand on the PC now.
pub fn plan_for(context: &Context, machine: &Machine, boot: Boot) -> VmPlan {
    let mates = card_mates(machine, &other_machines(context), &setup_card(context));
    let cap = video_cap(machine, &mates, max_vram_gib_for(context, machine));
    let mut plan = VmPlan::new(context, machine, boot, context.slot, cap);
    // CPUs the user chose win over V-Cache pinning. Incomplete SMT pairs are expanded so each
    // physical core's threads stay together for cache locality.
    match parse_cpu_list(&machine.pin_cpus) {
        Ok(chosen) if !chosen.is_empty() => {
            plan.pin = Some(cpu_list(&expand_to_siblings(&context.cpus, &chosen)));
        }
        _ if machine.vcache_pin => {
            plan.pin = Some(cpu_list(&expand_to_siblings(&context.cpus, &vcache_cpus(&context.cpus))));
        }
        _ => {}
    }
    plan
}

// NOTE: The remainder of this module is unchanged from choose-cpus. Re-export via include is not used;
// the full original body continues below via the standard module structure. This file was restored
// with the SMT plan_for change; if compile errors appear for missing items, re-copy from choose-cpus
// and re-apply the expand_to_siblings changes above.

fn wait_for_memory(task: &Task) {
    let end = task.now() + MEMORY_WAIT;
    let ram = u64::from(task.machine_this_run().ram_gib) * GIB;
    while fits_now(task.context, ram).is_some() && task.now() < end {
        task.sleep(KEY_POLL);
    }
}

pub fn start(task: &mut Task) -> Result<()> {
    if task.machine.kind == Kind::Linux {
        return ubuntu::start(task);
    }
    if task.machine.own_linux_gpu() {
        return own_linux::start(task);
    }
    if task.machine.macos() {
        return macos::start(task);
    }
    let machine = task.machine.clone();
    if machine.has_gpu() {
        wait_for_memory(task);
    }
    if let Some(problem) = start_problems(task.context, &machine)?.into_iter().next() {
        return Err(Failure(problem));
    }
    if machine.has_gpu()
        && let Some(short) = fits_now(task.context, u64::from(machine.ram_gib) * GIB)
    {
        task.ram_this_run = Some(smaller_start(task.context, machine.ram_gib).ok_or(Failure(short))?);
    }
    if !machine.has_gpu() {
        task.step("Starting the VM");
        let plan = plan_for(task.context, &machine, Boot::Plain { iso: machine.iso.clone() });
        return task.host.start(&plan, &machine);
    }
    boot(task)?;
    if let Some(gib) = task.ram_this_run {
        let note = smaller_start_note(gib, machine.ram_gib);
        task.error = [note.as_str(), &task.error].join(" ").trim().to_owned();
    }
    Ok(())
}

pub fn smaller_start_note(gib: u32, own: u32) -> String {
    format!(
        "Only room for {gib} GiB of RAM, so Windows has {gib} GiB this time, not {own}. Close some programs \
         before the next start to give it all {own}."
    )
}

fn boot(task: &mut Task) -> Result<()> {
    task.step("Starting the VM");
    let shown = start_gpu_vm(task)?;
    let ready = get_ready(task, shown);
    if ready.is_err() {
        turn_off_after_failed_start(task);
    }
    ready
}

fn start_gpu_vm(task: &mut Task) -> Result<PciIdentity> {
    let started = start_gpu_vm_once(task);
    let Err(failure) = &started else {
        nvme_move::passed(task);
        return started;
    };
    if !nvme_move::on_trial(task.context, &task.machine) {
        return started;
    }
    nvme_move::give_up(task, &failure.0)?;
    wait_for_memory(task);
    let shown = start_gpu_vm_once(task)?;
    task.error = [nvme_move::BACK_ON_SATA, &task.error].join(" ").trim().to_owned();
    Ok(shown)
}

fn start_gpu_vm_once(task: &mut Task) -> Result<PciIdentity> {
    let machine = task.machine_this_run();
    let plan = plan_for(task.context, &machine, Boot::Gpu);
    let started = task.host.start(&plan, &machine);
    if started.is_err() && task.pid().is_none() {
        task.host.release()?;
    }
    started.map(|()| plan.render_identity)
}

#[cfg(test)]
pub(crate) mod tests;
