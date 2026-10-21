//! Migration decisions independent of seL4, for host-side testing.

use super::topology::Topology;

const ACTIVE_PCT: u64 = 2;
const LIGHT_PCT: u64 = 50;

pub struct Sample {
    pub guest: usize,
    pub vcpu: usize,
    pub cpu: usize,
    pub used: u64,
    pub movable: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Migration {
    pub guest: usize,
    pub vcpu: usize,
    pub src: usize,
    pub dst: usize,
}

/// Reduce contention between physical cores first, then between their
/// logical CPUs. Each strict improvement reduces the sum of squared busy
/// counts, so equal-load SMT siblings do not trade vCPUs every cooldown.
pub fn choose(topology: &Topology, sample: &[Sample], elapsed_us: u64) -> Option<Migration> {
    if elapsed_us == 0 {
        return None;
    }
    let cpus = topology.physical.len();
    let mut load = alloc::vec![0u64; cpus];
    let mut active = alloc::vec![0u64; cpus];
    for s in sample {
        load[s.cpu] += s.used;
        if s.used * 100 >= elapsed_us * ACTIVE_PCT {
            active[s.cpu] += 1;
        }
    }
    // Half a fair share still recognizes a starved vCPU on a crowded CPU.
    let is_busy = |s: &Sample| s.used * 100 >= elapsed_us * ACTIVE_PCT && s.used * active[s.cpu] * 2 >= elapsed_us;
    let mut busy = alloc::vec![0i64; cpus];
    let mut physical_busy = alloc::vec![0i64; cpus];
    let mut physical_load = alloc::vec![0u64; cpus];
    let guests = sample.iter().map(|s| s.guest + 1).max().unwrap_or(0);
    let mut guest_cpu = alloc::vec![alloc::vec![0usize; cpus]; guests];
    let mut guest_core = alloc::vec![alloc::vec![0usize; cpus]; guests];
    for s in sample {
        let p = topology.physical[s.cpu];
        physical_load[p] += s.used;
        guest_cpu[s.guest][s.cpu] += 1;
        guest_core[s.guest][p] += 1;
        if is_busy(s) {
            busy[s.cpu] += 1;
            physical_busy[p] += 1;
        }
    }
    let mut best = None;
    for s in sample.iter().filter(|s| s.movable && is_busy(s)) {
        let src = s.cpu;
        let ps = topology.physical[src];
        for dst in 0..cpus {
            if src == dst {
                continue;
            }
            let pd = topology.physical[dst];
            let physical_gain = if ps == pd { 0 } else { physical_busy[ps] - physical_busy[pd] - 1 };
            let logical_gain = busy[src] - busy[dst] - 1;
            // Retain the lightly-loaded-target exception of the flat
            // balancer, provided it does not worsen physical contention.
            let light = busy[src] >= 2 && load[dst] * 100 < elapsed_us * LIGHT_PCT;
            if physical_gain < 0 || (physical_gain == 0 && (logical_gain < 0 || (logical_gain == 0 && !light))) {
                continue;
            }
            let guest_on_core = guest_core[s.guest][pd] > usize::from(topology.same_core(src, dst));
            let guest_on_cpu = guest_cpu[s.guest][dst] != 0;
            let score = (
                physical_gain,
                logical_gain,
                !guest_on_core,
                !guest_on_cpu,
                u64::MAX - physical_load[pd],
                u64::MAX - load[dst],
                s.used,
            );
            if best.as_ref().is_none_or(|(old, _)| score > *old) {
                best = Some((score, Migration { guest: s.guest, vcpu: s.vcpu, src, dst }));
            }
        }
    }
    best.map(|(_, migration)| migration)
}
