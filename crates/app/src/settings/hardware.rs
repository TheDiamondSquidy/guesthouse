//! Cores, memory and sound on the Overview, and V-Cache pinning and the video memory cap on the Advanced
//! page.

use super::{Shared, is_off};
use crate::rows::{explain, number_row};
use adw::prelude::*;
use guesthouse_app_core::host::{
    cpu_list, cpus_present, expand_to_siblings, parse_cpu_list, physical_cores, smt_threads, vcache_cpus,
};
use guesthouse_app_core::limits::{cores_range, ram_field};
use guesthouse_app_core::machine::Kind;
use guesthouse_app_core::settings::max_vram_gib_for;
use guesthouse_app_core::tasks::Status;

const VM_MUST_BE_OFF: &str = "The VM must be off to change these.";
const SOUND_SHORT: &str = "Through PipeWire";
const SOUND_NOTE: &str = "Off, the guest's sound card plays nothing, and QEMU gets no way to your audio server or any \
                          other socket of your desktop's.";
const VRAM_TITLE: &str = "Video Memory Cap";
const VRAM_NOTE: &str = "The most video memory one program in Windows can have. A program that goes past it loses \
                         its GPU device, and others carry on. 0 is no cap.";
const PIN_NOTE: &str =
    "Keeps the VM on the cores that sit beside the CPU's 3D V-Cache, which games run faster on.";

pub struct Hardware {
    group: adw::PreferencesGroup,
    advanced: adw::PreferencesGroup,
    cores: adw::SpinRow,
    ram: adw::SpinRow,
    vram: adw::SpinRow,
    whole_card: adw::SwitchRow,
    pin: adw::SwitchRow,
    pin_possible: bool,
    chosen: adw::EntryRow,
    each_vcpu: adw::SwitchRow,
    emulator: adw::EntryRow,
    tuning: adw::SwitchRow,
    sound: adw::SwitchRow,
}

impl Hardware {
    /// The video memory cap's row, whose top follows the machine's card.
    pub fn cap_row(&self) -> adw::SpinRow {
        self.vram.clone()
    }

    /// The sound row, which the Overview shows after the disk.
    pub fn sound_row(&self) -> &adw::SwitchRow {
        &self.sound
    }

    /// Cores and RAM go in `group`, and the expert rows in `advanced`.
    pub fn new(shared: &Shared, group: &adw::PreferencesGroup, advanced: &adw::PreferencesGroup) -> Self {
        let context = shared.window.context();
        let machine = shared.machine.borrow().clone();
        let host_cores = physical_cores(&context.cpus);
        let threads = smt_threads(&context.cpus);
        let (least_cores, most_cores) = cores_range(&context, &machine);
        let cores = number_row("Cores", machine.cores.into(), least_cores.into(), most_cores.into());
        cores.set_subtitle(&if threads > 1 {
            format!("This PC has {host_cores}, most with {threads} threads")
        } else {
            format!("This PC has {host_cores}")
        });
        if threads > 1 {
            explain(&cores, &format!("A multiple of {threads} gives the guest matching cores and threads."));
        }
        group.add(&cores);
        let wanted = machine.ram_gib.into();
        let field = ram_field(&context, &machine);
        // What it has stays, so a setting that no longer fits isn't changed behind the user's back.
        let ram =
            number_row("RAM", machine.ram_gib.into(), field.lower as f64, field.upper.max(wanted) as f64);
        // The first sentence is the figure, and the reasons behind it go behind the info button.
        match field.subtitle.split_once(". ") {
            Some((figure, reasons)) => {
                ram.set_subtitle(figure);
                explain(&ram, reasons);
            }
            None => ram.set_subtitle(field.subtitle.trim_end_matches('.')),
        }
        group.add(&ram);
        let whole_card = adw::SwitchRow::builder()
            .title("Whole Card")
            .subtitle("Lets the cap go up to the card's full size")
            .active(machine.vram_whole_card)
            .build();
        explain(
            &whole_card,
            "Normally the host's desktop keeps part of the card's memory, and the cap can't go past what is \
             left. This lifts that, so the cap can be the card's full size, and no cap means all of it. If the \
             host needs video memory while the guest has it, the host's programs can fail.",
        );
        whole_card.set_visible(machine.kind == Kind::Windows);
        let cpus = vcache_cpus(&context.cpus);
        let pin = adw::SwitchRow::builder().title("V-Cache Pinning").active(machine.vcache_pin).build();
        pin.set_subtitle(&if cpus.is_empty() {
            "This CPU has no V-Cache cores".to_owned()
        } else {
            format!("On CPUs {}", cpu_list(&cpus))
        });
        explain(&pin, PIN_NOTE);
        advanced.add(&pin);
        let chosen = adw::EntryRow::builder()
            .title("Choose CPUs, like 0-7,16-23 (empty for any)")
            .text(machine.pin_cpus.as_str())
            .show_apply_button(true)
            .build();
        chosen.set_tooltip_text(Some(&format!(
            "The CPUs this PC lists are {}. The VM only runs on the ones you choose, and they replace \
             V-Cache pinning. Choose at least as many as the machine has cores. A core's other thread is \
             added to any you choose, so the host never shares a core with the VM. `lscpu --extended` in a \
             terminal shows which CPUs share a core.",
            cpu_list(&cpus_present(&context.cpus))
        )));
        advanced.add(&chosen);
        let each_vcpu = adw::SwitchRow::builder()
            .title("Pin Each vCPU")
            .subtitle("One host CPU for each virtual CPU")
            .active(machine.pin_vcpus)
            .build();
        explain(
            &each_vcpu,
            "Gives every virtual CPU a host CPU of its own, a core's two threads to neighbouring vCPUs, \
             instead of letting the host move them about. Needs CPUs chosen above, or V-Cache pinning.",
        );
        advanced.add(&each_vcpu);
        let emulator = adw::EntryRow::builder()
            .title("Emulator CPUs, like 16-19 (empty for the CPUs above)")
            .text(machine.emulator_cpus.as_str())
            .show_apply_button(true)
            .build();
        emulator.set_tooltip_text(Some(
            "Where QEMU's own threads (disk, network, sound) run when each vCPU is pinned. Pick CPUs outside \
             the ones above, such as spare E-cores, so they never compete with a vCPU.",
        ));
        advanced.add(&emulator);
        let tuning = adw::SwitchRow::builder()
            .title("Match the Host CPU")
            .subtitle("A steady timestamp counter and the host's cache sizes")
            .active(machine.cpu_tuning)
            .visible(machine.kind == Kind::Windows)
            .build();
        explain(
            &tuning,
            "Gives Windows a steady, invariant timestamp counter and the host's real cache sizes. Can make \
             games smoother. Windows machines only.",
        );
        advanced.add(&tuning);
        let vram =
            number_row(VRAM_TITLE, machine.vram_gib.into(), 0.0, max_vram_gib_for(&context, &machine).into());
        vram.set_subtitle("In GiB, 0 for none");
        explain(&vram, VRAM_NOTE);
        // Only Windows' GPU-PV has a cap. The row is still made, as the card's row sets its top.
        if machine.kind == Kind::Windows {
            advanced.add(&vram);
            advanced.add(&whole_card);
        }
        // Only a GPU machine has ever had sound, and macOS and Android guests count as one. The Overview
        // adds it after the disk.
        let sound = adw::SwitchRow::builder()
            .title("Sound")
            .subtitle(SOUND_SHORT)
            .active(machine.sound)
            .visible(machine.kind == Kind::Windows || machine.macos() || machine.android())
            .build();
        explain(&sound, SOUND_NOTE);
        let this = Self {
            group: group.clone(),
            advanced: advanced.clone(),
            cores,
            ram,
            vram,
            whole_card,
            pin,
            pin_possible: !cpus.is_empty(),
            chosen,
            each_vcpu,
            emulator,
            tuning,
            sound,
        };
        this.connect(shared);
        this
    }

    fn connect(&self, shared: &Shared) {
        let saving = shared.clone();
        self.cores.connect_value_notify(move |row| {
            saving.machine.borrow_mut().cores = row.value() as u32;
            saving.save();
        });
        let saving = shared.clone();
        self.ram.connect_value_notify(move |row| {
            saving.machine.borrow_mut().ram_gib = row.value() as u32;
            saving.save();
        });
        let saving = shared.clone();
        self.vram.connect_value_notify(move |row| {
            saving.machine.borrow_mut().vram_gib = row.value() as u32;
            saving.save();
        });
        let cap_row = self.vram.clone();
        let saving = shared.clone();
        self.whole_card.connect_active_notify(move |row| {
            let most = {
                let mut machine = saving.machine.borrow_mut();
                machine.vram_whole_card = row.is_active();
                max_vram_gib_for(&saving.window.context(), &machine)
            };
            // The cap row's own limit moves with it, and a cap past the new one comes down to it.
            cap_row.adjustment().set_upper(f64::from(most));
            if cap_row.value() > f64::from(most) {
                cap_row.set_value(f64::from(most));
            }
            saving.save();
        });
        let saving = shared.clone();
        self.pin.connect_active_notify(move |row| {
            saving.machine.borrow_mut().vcache_pin = row.is_active();
            saving.save();
        });
        self.chosen.connect_changed(|row| {
            row.remove_css_class("error");
            row.set_tooltip_text(None);
        });
        let saving = shared.clone();
        self.chosen.connect_apply(move |row| {
            let present = cpus_present(&saving.window.context().cpus);
            let problem = match parse_cpu_list(&row.text()) {
                Err(why) => Some(why),
                Ok(cpus) => {
                    let missing: Vec<u32> =
                        cpus.iter().copied().filter(|cpu| !present.contains(cpu)).collect();
                    (!missing.is_empty()).then(|| format!("This PC has no CPU {}.", cpu_list(&missing)))
                }
            };
            if let Some(problem) = problem {
                row.add_css_class("error");
                row.set_tooltip_text(Some(&problem));
                return;
            }
            // Saved the way taskset takes it, so `4,0-2` reads back as `0-2,4`, with each core's other
            // thread added, as the VM is started on them.
            let chosen = parse_cpu_list(&row.text()).unwrap_or_default();
            let text = cpu_list(&expand_to_siblings(&saving.window.context().cpus, &chosen));
            row.set_text(&text);
            saving.machine.borrow_mut().pin_cpus = text;
            saving.save();
        });
        let saving = shared.clone();
        self.each_vcpu.connect_active_notify(move |row| {
            saving.machine.borrow_mut().pin_vcpus = row.is_active();
            saving.save();
        });
        let saving = shared.clone();
        self.tuning.connect_active_notify(move |row| {
            saving.machine.borrow_mut().cpu_tuning = row.is_active();
            saving.save();
        });
        self.emulator.connect_changed(|row| {
            row.remove_css_class("error");
            row.set_tooltip_text(None);
        });
        let saving = shared.clone();
        self.emulator.connect_apply(move |row| {
            let present = cpus_present(&saving.window.context().cpus);
            let chosen = match parse_cpu_list(&row.text()) {
                Err(why) => Err(why),
                Ok(cpus) => {
                    let missing: Vec<u32> =
                        cpus.iter().copied().filter(|cpu| !present.contains(cpu)).collect();
                    if missing.is_empty() {
                        Ok(cpus)
                    } else {
                        Err(format!("This PC has no CPU {}.", cpu_list(&missing)))
                    }
                }
            };
            match chosen {
                Err(problem) => {
                    row.add_css_class("error");
                    row.set_tooltip_text(Some(&problem));
                }
                Ok(cpus) => {
                    let text = cpu_list(&cpus);
                    row.set_text(&text);
                    saving.machine.borrow_mut().emulator_cpus = text;
                    saving.save();
                }
            }
        });
        let saving = shared.clone();
        self.sound.connect_active_notify(move |row| {
            saving.machine.borrow_mut().sound = row.is_active();
            saving.save();
        });
    }

    pub fn set_status(&self, status: &Status) {
        let off = is_off(status);
        self.cores.set_sensitive(off);
        self.ram.set_sensitive(off);
        self.vram.set_sensitive(off);
        self.whole_card.set_sensitive(off);
        self.pin.set_sensitive(off && self.pin_possible);
        self.chosen.set_sensitive(off);
        self.each_vcpu.set_sensitive(off);
        self.emulator.set_sensitive(off);
        self.tuning.set_sensitive(off);
        self.sound.set_sensitive(off);
        self.group.set_description((!off).then_some(VM_MUST_BE_OFF));
        self.advanced.set_description((!off).then_some(VM_MUST_BE_OFF));
    }
}
