//! What this PC can give a machine: its cores, its memory and its disk.

use crate::context::Context;
use crate::failure::Result;
use crate::guest_memory::NO_ROOM_WORDS;
use crate::held_memory::used_by_others;
use crate::machine::{MIN_RAM_GIB, Machine, sorted_names};
use guesthouse_daemon::slot::Slot;
use rustix::fs::statvfs;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

pub const GIB: u64 = 1 << 30;
pub const IO_SPACE_GIB: u64 = 2;
const HOST_SHARE_DIVISOR: u64 = 4;
pub const HOST_KEEPS_LEAST_GIB: u64 = 6;
pub const HOST_KEEPS_FLOOR_GIB: u64 = 3;
pub const HOST_KEEPS_MOST_GIB: u64 = 16;
pub const IO_AT_START_GIB: u64 = 1;
pub const FREE_AT_START_GIB: u64 = 1;
const KIB: u64 = 1024;
const MOST_CPU_NUMBER: u32 = 8191;

fn cpu_numbers(root: &Path) -> Vec<u32> {
    sorted_names(root).iter().filter_map(|name| name.strip_prefix("cpu")?.parse().ok()).collect()
}

pub fn cpus_present(root: &Path) -> Vec<u32> {
    cpu_numbers(root)
}

pub fn parse_cpu_list(text: &str) -> std::result::Result<Vec<u32>, String> {
    let mut cpus = BTreeSet::new();
    for part in text.split(',').map(str::trim).filter(|part| !part.is_empty()) {
        let number = |digits: &str| {
            digits.trim().parse::<u32>().map_err(|_| format!("\"{part}\" is not a CPU or a range like 0-7."))
        };
        match part.split_once('-') {
            Some((first, last)) => {
                let (first, last) = (number(first)?, number(last)?);
                if first > last {
                    return Err(format!("\"{part}\" runs backwards. Write it as {last}-{first}."));
                }
                if last > MOST_CPU_NUMBER {
                    return Err(format!(
                        "\"{part}\" is past the highest CPU number Linux gives, {MOST_CPU_NUMBER}."
                    ));
                }
                cpus.extend(first..=last);
            }
            None => {
                let cpu = number(part)?;
                if cpu > MOST_CPU_NUMBER {
                    return Err(format!(
                        "\"{part}\" is past the highest CPU number Linux gives, {MOST_CPU_NUMBER}."
                    ));
                }
                cpus.insert(cpu);
            }
        }
    }
    Ok(cpus.into_iter().collect())
}

pub fn vcache_cpus(root: &Path) -> Vec<u32> {
    let mut sizes = BTreeMap::new();
    for cpu in cpu_numbers(root) {
        let text = fs::read_to_string(root.join(format!("cpu{cpu}/cache/index3/size"))).unwrap_or_default();
        if let Ok(size) = text.trim().trim_end_matches('K').parse::<u64>() {
            sizes.insert(cpu, size);
        }
    }
    let largest = sizes.values().copied().max();
    if sizes.values().collect::<BTreeSet<_>>().len() < 2 {
        return Vec::new();
    }
    sizes.iter().filter(|(_, size)| Some(**size) == largest).map(|(cpu, _)| *cpu).collect()
}

pub fn cpu_list(cpus: &[u32]) -> String {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &cpu in cpus {
        match runs.last_mut() {
            Some(run) if run.1 + 1 == cpu => run.1 = cpu,
            _ => runs.push((cpu, cpu)),
        }
    }
    let text = |(first, last): &(u32, u32)| {
        if first == last { first.to_string() } else { format!("{first}-{last}") }
    };
    runs.iter().map(text).collect::<Vec<_>>().join(",")
}

pub fn physical_cores(root: &Path) -> u32 {
    physical_core_groups(root).len() as u32
}

/// Each physical core as the list of its logical CPUs (SMT / hyperthread siblings).
pub fn physical_core_groups(root: &Path) -> Vec<Vec<u32>> {
    let mut by_core: BTreeMap<(String, String), BTreeSet<u32>> = BTreeMap::new();
    for cpu in cpu_numbers(root) {
        let folder = root.join(format!("cpu{cpu}/topology"));
        let read = |name: &str| fs::read_to_string(folder.join(name)).ok();
        if let Some(text) = read("thread_siblings_list") {
            if let Ok(siblings) = parse_cpu_list(text.trim()) {
                if let Some(&first) = siblings.first() {
                    by_core.entry((String::new(), first.to_string())).or_default().extend(siblings);
                    continue;
                }
            }
        }
        if let (Some(core), Some(package)) = (read("core_id"), read("physical_package_id")) {
            by_core.entry((package.trim().to_owned(), core.trim().to_owned())).or_default().insert(cpu);
        }
    }
    let mut groups: Vec<Vec<u32>> = by_core.into_values().map(|set| set.into_iter().collect()).collect();
    groups.sort_by_key(|g| g.first().copied().unwrap_or(0));
    groups
}

pub fn smt_threads(root: &Path) -> u32 {
    physical_core_groups(root).into_iter().map(|g| g.len() as u32).max().unwrap_or(1).max(1)
}

/// Expand a CPU list so every chosen logical CPU brings its full SMT sibling set.
pub fn expand_to_siblings(root: &Path, cpus: &[u32]) -> Vec<u32> {
    let wanted: BTreeSet<u32> = cpus.iter().copied().collect();
    if wanted.is_empty() {
        return Vec::new();
    }
    let mut out = BTreeSet::new();
    for group in physical_core_groups(root) {
        if group.iter().any(|c| wanted.contains(c)) {
            out.extend(group);
        }
    }
    out.extend(wanted);
    out.into_iter().collect()
}

pub fn cpus_for_physical_cores(root: &Path, count: u32) -> Vec<u32> {
    physical_core_groups(root).into_iter().take(count as usize).flatten().collect()
}

/// QEMU `-smp` topology string for `vcpu_count` vCPUs on this host.
pub fn smp_topology(root: &Path, vcpu_count: u32) -> String {
    let threads = smt_threads(root);
    if threads > 1 && vcpu_count >= threads && vcpu_count % threads == 0 {
        let cores = vcpu_count / threads;
        format!("{vcpu_count},sockets=1,cores={cores},threads={threads}")
    } else {
        format!("{vcpu_count},sockets=1,cores={vcpu_count},threads=1")
    }
}

pub fn host_keeps(total: u64) -> u64 {
    (total / HOST_SHARE_DIVISOR).clamp(HOST_KEEPS_LEAST_GIB * GIB, HOST_KEEPS_MOST_GIB * GIB)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RamOffer {
    pub total: u64,
    pub keeps: u64,
    pub others: u64,
}

impl RamOffer {
    pub fn new(total: u64, others: u64) -> Self {
        Self { total, keeps: host_keeps(total), others }
    }

    pub fn limit_gib(&self) -> u64 {
        let least = u64::from(MIN_RAM_GIB);
        let comfortable = self.after_keeping(self.keeps);
        if comfortable < least && self.tight(least) { least } else { comfortable }
    }

    fn after_keeping(&self, keeps: u64) -> u64 {
        self.total.saturating_sub(keeps + IO_SPACE_GIB * GIB + self.others) / GIB
    }

    pub fn tight(&self, least: u64) -> bool {
        self.others < GIB / 2
            && self.after_keeping(self.keeps) < least
            && self.after_keeping(HOST_KEEPS_FLOOR_GIB * GIB) >= least
    }

    fn reasons(&self) -> String {
        let mut text = format!(
            "This PC has {} GiB of RAM and keeps {} GiB for itself",
            self.total / GIB,
            self.keeps.div_ceil(GIB)
        );
        if self.others >= GIB / 2 {
            text += &format!(", and the VMs running hold {} GiB", self.others.div_ceil(GIB));
        }
        text + "."
    }

    pub fn explanation(&self, least: u64) -> String {
        let limit = self.limit_gib();
        if self.tight(least) {
            let keeps = self.total / GIB - least - IO_SPACE_GIB;
            return format!(
                "{least} GiB, the least a GPU machine runs on. This PC has {} GiB of RAM, so it keeps only about \
                 {keeps} GiB for itself while the machine runs, not the {HOST_KEEPS_LEAST_GIB} GiB it would like.",
                self.total / GIB
            );
        }
        if limit >= least {
            return format!("Up to {limit} GiB. {}", self.reasons());
        }
        let way_out = if self.others >= GIB / 2 {
            "Stop another VM to make room."
        } else {
            "A GPU machine needs a PC with more RAM."
        };
        format!(
            "A GPU machine needs at least {least} GiB, and this PC can give one {limit} GiB. {} {way_out}",
            self.reasons()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RamField {
    pub lower: u64,
    pub upper: u64,
    pub value: u64,
    pub fits: bool,
    pub subtitle: String,
}

impl RamField {
    pub fn new(limit: u64, least: u64, wanted: u64, explanation: String) -> Self {
        let fits = limit >= least;
        let upper = if fits { limit } else { least };
        Self { lower: least, upper, value: wanted.clamp(least, upper), fits, subtitle: explanation }
    }

    pub fn for_gpu(context: &Context, slot: Slot, least: u64, wanted: u64) -> Self {
        let (limit, explanation) = match context.ram_limit {
            Some(limit) => (limit, format!("Up to {limit} GiB.")),
            None => {
                let offer = ram_offer(context, slot);
                (offer.limit_gib(), offer.explanation(least))
            }
        };
        Self::new(limit, least, wanted, explanation)
    }

    pub fn for_plain(context: &Context, least: u64, wanted: u64) -> Self {
        let limit = plain_ram_limit(context);
        Self::new(limit, least, wanted, format!("Up to {limit} GiB."))
    }
}

fn meminfo_bytes(context: &Context, field: &str) -> u64 {
    let text = fs::read_to_string(&context.meminfo).unwrap_or_default();
    let line = text.lines().find_map(|line| line.strip_prefix(field)?.strip_prefix(':'));
    let kib = line.and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok());
    kib.unwrap_or(0) * KIB
}

pub fn available_ram(context: &Context) -> u64 {
    meminfo_bytes(context, "MemAvailable")
}

pub fn total_ram(context: &Context) -> u64 {
    meminfo_bytes(context, "MemTotal")
}

pub fn ram_offer(context: &Context, slot: Slot) -> RamOffer {
    RamOffer::new(total_ram(context), used_by_others(context, slot))
}

pub fn fits_now(context: &Context, ram_bytes: u64) -> Option<String> {
    if context.ram_limit.is_some_and(|limit| ram_bytes <= limit * GIB) {
        return None;
    }
    let available = available_ram(context);
    let swap = meminfo_bytes(context, "SwapFree");
    (!room_for(ram_bytes, available, swap)).then(|| short_now(ram_bytes, available))
}

fn room_for(ram_bytes: u64, available: u64, swap: u64) -> bool {
    let claimed = ram_bytes + IO_AT_START_GIB * GIB;
    let kept = FREE_AT_START_GIB * GIB;
    available >= claimed && available + swap.min(kept) >= claimed + kept
}

pub fn smaller_start(context: &Context, wanted_gib: u32) -> Option<u32> {
    fits_now(context, u64::from(wanted_gib) * GIB)?;
    (MIN_RAM_GIB..wanted_gib).rev().find(|gib| fits_now(context, u64::from(*gib) * GIB).is_none())
}

pub fn short_now(ram_bytes: u64, available: u64) -> String {
    format!(
        "This machine needs {} GiB of RAM, and {} {NO_ROOM_WORDS} right now, with {} GiB of that kept for the \
         GPU and the PC. Close some programs, like a browser or a game, or stop another VM, and start it \
         again. Or give it less RAM.",
        ram_bytes.div_ceil(GIB),
        available / GIB,
        IO_AT_START_GIB + FREE_AT_START_GIB,
    )
}

pub fn host_ram_limit(context: &Context) -> Result<u64> {
    match context.ram_limit {
        Some(limit) => Ok(limit),
        None => Ok(ram_offer(context, context.slot).limit_gib()),
    }
}

pub fn plain_ram_limit(context: &Context) -> u64 {
    let total = total_ram(context);
    (total.saturating_sub(host_keeps(total)) / GIB).max(1)
}

pub fn ram_limit_for(context: &Context, machine: &Machine) -> Result<u64> {
    if !machine.has_gpu() {
        return Ok(plain_ram_limit(context));
    }
    if let Some(limit) = context.ram_limit {
        return Ok(limit);
    }
    let slot = crate::tasks::slot_of(context, machine).unwrap_or(context.slot);
    Ok(ram_offer(context, slot).limit_gib())
}

pub fn free_gib(path: &Path) -> Result<f64> {
    let disk = statvfs(path).map_err(std::io::Error::from)?;
    Ok((disk.f_bavail * disk.f_frsize) as f64 / GIB as f64)
}

#[cfg(test)]
pub mod stand_in {
    use crate::testing::Scratch;
    use std::path::PathBuf;

    pub fn fake_cpus(scratch: &Scratch, name: &str, sizes: &[u32]) -> PathBuf {
        let half = (sizes.len() / 2).max(1);
        for (cpu, size) in sizes.iter().enumerate() {
            scratch.write(&format!("{name}/cpu{cpu}/cache/index3/size"), format!("{size}K\n"));
            let core_id = cpu % half;
            scratch.write(&format!("{name}/cpu{cpu}/topology/core_id"), format!("{core_id}\n"));
            scratch.write(&format!("{name}/cpu{cpu}/topology/physical_package_id"), "0\n");
            if sizes.len() >= 2 {
                let sibling = if cpu < half { cpu + half } else { cpu - half };
                let (a, b) = if cpu < sibling { (cpu, sibling) } else { (sibling, cpu) };
                scratch.write(
                    &format!("{name}/cpu{cpu}/topology/thread_siblings_list"),
                    format!("{a},{b}\n"),
                );
            } else {
                scratch.write(
                    &format!("{name}/cpu{cpu}/topology/thread_siblings_list"),
                    format!("{cpu}\n"),
                );
            }
        }
        std::fs::create_dir_all(scratch.path(name).join("cpufreq")).unwrap();
        scratch.path(name)
    }
}

#[cfg(test)]
mod tests {
    use super::stand_in::fake_cpus;
    use super::*;
    use crate::testing::Scratch;

    #[test]
    fn the_larger_cache_is_found_as_on_a_two_die_cpu() {
        let scratch = Scratch::new();
        let sizes: Vec<u32> = [[98304; 8], [32768; 8], [98304; 8], [32768; 8]].concat();
        let root = fake_cpus(&scratch, "cpu", &sizes);
        let expected: Vec<u32> = (0..8).chain(16..24).collect();
        assert_eq!(vcache_cpus(&root), expected);
        assert_eq!(cpu_list(&vcache_cpus(&root)), "0-7,16-23");
        assert_eq!(physical_cores(&root), 16);
    }

    #[test]
    fn cpu_lists_are_read_as_taskset_reads_them() {
        assert_eq!(parse_cpu_list("0-7,16-23").unwrap(), (0..8).chain(16..24).collect::<Vec<u32>>());
        assert_eq!(parse_cpu_list(" 4, 0-2 ,2 ").unwrap(), vec![0, 1, 2, 4]);
        assert_eq!(parse_cpu_list("").unwrap(), Vec::<u32>::new());
        assert_eq!(cpu_list(&parse_cpu_list("0-15").unwrap()), "0-15");
        for bad in ["a", "1-", "-3", "5-2", "1,,x", "0-99999999", "9999"] {
            assert!(parse_cpu_list(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn with_one_cache_size_there_is_nothing_to_pin_to() {
        let scratch = Scratch::new();
        assert!(vcache_cpus(&fake_cpus(&scratch, "flat", &[32768; 4])).is_empty());
        assert_eq!((cpu_list(&[3]), cpu_list(&[0, 1, 3])), ("3".to_owned(), "0-1,3".to_owned()));
    }

    #[test]
    fn physical_cores_group_smt_siblings_together() {
        let scratch = Scratch::new();
        let root = fake_cpus(&scratch, "smt", &[32768; 8]);
        let groups = physical_core_groups(&root);
        assert_eq!(groups.len(), 4);
        assert_eq!(smt_threads(&root), 2);
        assert_eq!(cpus_for_physical_cores(&root, 2), vec![0, 1, 4, 5]);
        assert_eq!(expand_to_siblings(&root, &[0, 1]), vec![0, 1, 4, 5]);
        assert_eq!(smp_topology(&root, 8), "8,sockets=1,cores=4,threads=2");
        assert_eq!(smp_topology(&root, 7), "7,sockets=1,cores=7,threads=1");
        assert_eq!(smp_topology(&root, 4), "4,sockets=1,cores=2,threads=2");
    }

    const fn sold_with(gib: u64) -> u64 {
        gib * GIB * 97 / 100
    }

    #[test]
    fn the_host_keeps_a_quarter_but_never_less_than_a_desktop_needs_or_more_than_it_did() {
        assert_eq!(host_keeps(sold_with(16)), HOST_KEEPS_LEAST_GIB * GIB);
        assert_eq!(host_keeps(32 * GIB), 8 * GIB);
        assert_eq!(host_keeps(91 * GIB), HOST_KEEPS_MOST_GIB * GIB);
    }

    #[test]
    fn what_a_pc_offers_follows_its_ram() {
        assert_eq!(RamOffer::new(sold_with(16), 0).limit_gib(), 7, "a 16 GB PC");
        assert_eq!(RamOffer::new(30 * GIB, 0).limit_gib(), 20, "issue 9's PC");
        assert_eq!(RamOffer::new(sold_with(32), 0).limit_gib(), 21);
        assert_eq!(RamOffer::new(91 * GIB, 0).limit_gib(), 73);
        assert_eq!(RamOffer::new(40 * GIB, 0).limit_gib(), 28, "the host keeps its share");
        assert_eq!(RamOffer::new(sold_with(8), 0).limit_gib(), 0);
    }
}
