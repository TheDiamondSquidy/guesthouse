//! Pins a running QEMU's threads one by one, as a hand-written libvirt domain does with `vcpupin` and
//! `emulatorpin`: each vCPU thread to a CPU of its own, and every other thread to the CPUs for the
//! emulator. The process as a whole is already kept to the machine's CPUs by `taskset`, which leaves the
//! scheduler free to move vCPUs between them and to put the emulator on the same CPUs as the guest.

use crate::host::cpu_list;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// How long QEMU is given to make its vCPU threads, as they exist before the guest runs.
pub const WAIT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(100);

/// Where each kind of thread goes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThreadPins {
    /// The CPU for vCPU 0, vCPU 1 and so on. A vCPU past the end is left where it is.
    pub vcpus: Vec<u32>,
    /// The CPUs for every thread that isn't a vCPU. Empty leaves them where they are.
    pub emulator: Vec<u32>,
}

/// One thread of the QEMU process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread {
    pub id: u32,
    pub name: String,
}

/// The vCPU a thread runs, from the name QEMU gives it: `CPU 3/KVM`.
fn vcpu_number(name: &str) -> Option<usize> {
    name.strip_prefix("CPU ")?.strip_suffix("/KVM")?.parse().ok()
}

/// The threads of this process, by id.
pub fn read_threads(processes: &Path, pid: u32) -> Vec<Thread> {
    let Ok(folder) = fs::read_dir(processes.join(pid.to_string()).join("task")) else { return Vec::new() };
    let mut threads: Vec<Thread> = folder
        .flatten()
        .filter_map(|entry| {
            let id = entry.file_name().to_str()?.parse().ok()?;
            let name = fs::read_to_string(entry.path().join("comm")).ok()?.trim().to_owned();
            Some(Thread { id, name })
        })
        .collect();
    threads.sort_by_key(|thread| thread.id);
    threads
}

/// What to set: the thread and the CPUs it is to run on, as taskset takes them.
pub fn assignments(pins: &ThreadPins, threads: &[Thread]) -> Vec<(u32, String)> {
    let emulator = cpu_list(&pins.emulator);
    threads
        .iter()
        .filter_map(|thread| match vcpu_number(&thread.name) {
            Some(number) => pins.vcpus.get(number).map(|cpu| (thread.id, cpu.to_string())),
            None if !pins.emulator.is_empty() => Some((thread.id, emulator.clone())),
            None => None,
        })
        .collect()
}

fn taskset(thread: u32, cpus: &str) -> Result<(), String> {
    let output = Command::new("taskset")
        .args(["-pc", cpus, &thread.to_string()])
        .output()
        .map_err(|error| format!("taskset could not run: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
}

/// Pins the process's threads once all its vCPUs exist, or `wait` is up, or it is gone. Returns a line
/// for each thing that went wrong, and one saying what was done. A thread that can't be pinned is only
/// left where it was: the VM runs either way.
pub fn apply(
    processes: &Path,
    pid: u32,
    pins: &ThreadPins,
    wait: Duration,
    alive: &mut dyn FnMut() -> bool,
) -> Vec<String> {
    apply_with(processes, pid, pins, wait, alive, &taskset)
}

pub(crate) fn apply_with(
    processes: &Path,
    pid: u32,
    pins: &ThreadPins,
    wait: Duration,
    alive: &mut dyn FnMut() -> bool,
    set: &dyn Fn(u32, &str) -> Result<(), String>,
) -> Vec<String> {
    let started = Instant::now();
    let vcpus_now =
        |threads: &[Thread]| threads.iter().filter(|thread| vcpu_number(&thread.name).is_some()).count();
    let mut threads = read_threads(processes, pid);
    while vcpus_now(&threads) < pins.vcpus.len() && started.elapsed() < wait && alive() {
        sleep(POLL);
        threads = read_threads(processes, pid);
    }
    let mut notes = Vec::new();
    if vcpus_now(&threads) < pins.vcpus.len() {
        notes.push(format!(
            "Only {} of {} vCPU threads were there to pin.",
            vcpus_now(&threads),
            pins.vcpus.len()
        ));
    }
    let mut done = 0;
    for (thread, cpus) in assignments(pins, &threads) {
        match set(thread, &cpus) {
            Ok(()) => done += 1,
            Err(why) => notes.push(format!("Thread {thread} was not pinned to {cpus}: {why}")),
        }
    }
    notes.push(format!("Pinned {done} threads."));
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Scratch;
    use std::cell::RefCell;

    fn thread(id: u32, name: &str) -> Thread {
        Thread { id, name: name.to_owned() }
    }

    fn qemu_threads() -> Vec<Thread> {
        vec![
            thread(100, "qemu-system-x86"),
            thread(101, "IO mon_iothread"),
            thread(102, "CPU 0/KVM"),
            thread(103, "CPU 1/KVM"),
            thread(104, "CPU 2/KVM"),
            thread(105, "worker"),
        ]
    }

    #[test]
    fn each_vcpu_gets_its_own_cpu_and_the_rest_get_the_emulators() {
        let pins = ThreadPins { vcpus: vec![0, 1, 2], emulator: vec![16, 17, 18] };
        let got = assignments(&pins, &qemu_threads());
        let want = [(100, "16-18"), (101, "16-18"), (102, "0"), (103, "1"), (104, "2"), (105, "16-18")];
        assert_eq!(got, want.iter().map(|(id, cpus)| (*id, (*cpus).to_owned())).collect::<Vec<_>>());
    }

    #[test]
    fn with_no_emulator_cpus_only_the_vcpus_are_pinned() {
        let pins = ThreadPins { vcpus: vec![4, 5, 6], emulator: vec![] };
        let got: Vec<u32> = assignments(&pins, &qemu_threads()).into_iter().map(|(id, _)| id).collect();
        assert_eq!(got, vec![102, 103, 104]);
    }

    #[test]
    fn a_vcpu_past_the_list_is_left_alone() {
        let pins = ThreadPins { vcpus: vec![7], emulator: vec![] };
        assert_eq!(assignments(&pins, &qemu_threads()), vec![(102, "7".to_owned())]);
    }

    #[test]
    fn threads_are_read_from_the_process_folder_by_id() {
        let scratch = Scratch::new();
        scratch.write("proc/55/task/55/comm", "qemu-system-x86\n");
        scratch.write("proc/55/task/57/comm", "CPU 1/KVM\n");
        scratch.write("proc/55/task/56/comm", "CPU 0/KVM\n");
        let threads = read_threads(&scratch.path("proc"), 55);
        assert_eq!(
            threads,
            vec![thread(55, "qemu-system-x86"), thread(56, "CPU 0/KVM"), thread(57, "CPU 1/KVM")]
        );
        assert!(read_threads(&scratch.path("proc"), 99).is_empty(), "a process that isn't there has none");
    }

    #[test]
    fn apply_pins_what_it_can_and_says_what_it_could_not() {
        let scratch = Scratch::new();
        for (id, name) in [(55, "qemu-system-x86"), (56, "CPU 0/KVM"), (57, "CPU 1/KVM")] {
            scratch.write(&format!("proc/55/task/{id}/comm"), format!("{name}\n"));
        }
        let pins = ThreadPins { vcpus: vec![2, 3], emulator: vec![8] };
        let set = RefCell::new(Vec::new());
        let notes =
            apply_with(&scratch.path("proc"), 55, &pins, Duration::ZERO, &mut || true, &|id, cpus| {
                set.borrow_mut().push((id, cpus.to_owned()));
                if id == 57 { Err("Operation not permitted".to_owned()) } else { Ok(()) }
            });
        assert_eq!(*set.borrow(), vec![(55, "8".to_owned()), (56, "2".to_owned()), (57, "3".to_owned())]);
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes[0].contains("Thread 57") && notes[0].contains("Operation not permitted"), "{notes:?}");
        assert_eq!(notes[1], "Pinned 2 threads.");
    }

    #[test]
    fn apply_gives_up_waiting_when_the_process_is_gone() {
        let scratch = Scratch::new();
        let pins = ThreadPins { vcpus: vec![0, 1], emulator: vec![] };
        let notes =
            apply_with(&scratch.path("proc"), 9, &pins, Duration::from_secs(60), &mut || false, &|_, _| {
                Ok(())
            });
        assert_eq!(
            notes,
            vec!["Only 0 of 2 vCPU threads were there to pin.".to_owned(), "Pinned 0 threads.".to_owned()]
        );
    }
}
