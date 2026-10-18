//! Host CPU scheduling (DESIGN.md §7): per-guest CPU caps and a balancer
//! that moves vCPUs between cores by the CPU time they consumed.
//!
//! seL4 does not balance; every vCPU's two scheduling contexts (SC_K, SC_U)
//! live on one core until `SchedControl_Configure` on another core's
//! SchedControl moves them.

use alloc::vec::Vec;

use crate::Host;
use crate::guest::State;

/// Balancer period.
pub const BALANCE_MS: u64 = 100;
/// A vCPU using this share of a period (percent) is active.
const ACTIVE_PCT: u64 = 2;
/// A core whose vCPUs used less than this share of a period (percent) has
/// room for one more busy vCPU.
const LIGHT_PCT: u64 = 50;
/// A vCPU that moved stays put for this many periods.
const COOLDOWN: u32 = 10;

pub fn consumed(sc: sel4::cap::SchedContext) -> u64 {
    let r = sel4::with_ipc_buffer_mut(|ipc| ipc.inner_mut().seL4_SchedContext_Consumed(sc.bits()));
    if r.error != 0 {
        sel4::debug_println!("l4h-root: SchedContext_Consumed({:#x}) failed: {}", sc.bits(), r.error);
        return 0;
    }
    r.consumed
}

/// Budget of a vCPU's scheduling contexts for a CPU cap in percent
/// (0 = uncapped).
pub fn budget_us(timeslice_us: u64, cap_pct: u32) -> u64 {
    if cap_pct == 0 || cap_pct >= 100 {
        timeslice_us
    } else {
        (timeslice_us * cap_pct as u64 / 100).max(MIN_BUDGET_US)
    }
}

/// seL4's minimum budget is a few times the kernel WCET; stay well above.
const MIN_BUDGET_US: u64 = 200;

/// (Re)configure vCPU `v` of guest `g` on `core` with the guest's cap.
pub fn configure(h: &Host, gid: usize, v: usize, core: usize) -> bool {
    let Some(g) = h.guest(gid) else { return false };
    let vcpu = &g.vcpus[v];
    let period = h.info.timeslice_us;
    let budget = budget_us(period, g.cpu_cap_pct);
    let sc = h.info.sched_control(core);
    let r = sc
        .sched_control_configure_flags(vcpu.sc_k, budget, period, 0, 0, 0)
        .and_then(|_| sc.sched_control_configure_flags(vcpu.sc_u, budget, period, 0, 0, 0));
    if let Err(e) = r {
        sel4::debug_println!("l4h-root: configure guest {} vCPU {} on core {}: {:?}", gid, v, core, e);
    }
    r.is_ok()
}

/// Apply a new CPU cap to every vCPU of guest `gid`.
pub fn set_cap(h: &mut Host, gid: usize, cap_pct: u32) -> bool {
    let Some(Some(g)) = h.guests.get_mut(gid) else { return false };
    g.cpu_cap_pct = cap_pct.min(100);
    let cores: Vec<usize> = g.vcpus.iter().map(|v| v.core).collect();
    cores.iter().enumerate().all(|(v, &c)| configure(h, gid, v, c))
}

/// One balancer step: sample consumption over the `elapsed_us` since the
/// last step, move at most one vCPU.
///
/// A vCPU is *busy* if it used at least half of its fair share of its
/// core (among the vCPUs active there).  Busy
/// vCPUs on one core compete for it, so the balancer evens out their
/// number: one moves from the core with the most to the core with the
/// fewest when that strictly reduces the maximum (a difference of two, or
/// of one if the target core is lightly loaded).
pub fn balance(h: &mut Host, elapsed_us: u64) {
    let cores = h.info.cores;
    if cores < 2 || elapsed_us == 0 {
        return;
    }
    let mut load = alloc::vec![0u64; cores];
    let mut active = alloc::vec![0u64; cores];
    // (guest, vcpu, core, consumed, movable)
    let mut sample = Vec::new();
    let tsc_mhz = (h.info.tsc_hz / 1_000_000).max(1);
    for g in h.guests.iter_mut().flatten() {
        let event_pages: Vec<*mut l4h_abi::EventPage> = (0..g.vcpus.len()).map(|v| g.event_page(v)).collect();
        for (v, vcpu) in g.vcpus.iter_mut().enumerate() {
            let c = consumed(vcpu.sc_k) + consumed(vcpu.sc_u);
            if g.state != State::Running {
                continue;
            }
            vcpu.cooldown = vcpu.cooldown.saturating_sub(1);
            vcpu.usage = (c * 1000 / elapsed_us) as u32;
            // Steal: time the vCPU existed minus what it ran minus what it
            // spent idle (reported by the guest).
            if vcpu.wall_ticks != 0 || c != 0 {
                vcpu.wall_ticks += elapsed_us * tsc_mhz;
                vcpu.run_ticks += c * tsc_mhz;
                let idle = unsafe { core::ptr::addr_of!((*event_pages[v]).idle_ticks).read_volatile() };
                let steal = vcpu.wall_ticks.saturating_sub(vcpu.run_ticks).saturating_sub(idle);
                let ns = steal * 1000 / tsc_mhz;
                if ns > vcpu.steal_ns {
                    vcpu.steal_ns = ns;
                    let page = event_pages[v];
                    unsafe { core::ptr::addr_of_mut!((*page).steal_ns).write_volatile(ns) };
                }
            }
            load[vcpu.core] += c;
            if c * 100 >= elapsed_us * ACTIVE_PCT {
                active[vcpu.core] += 1;
            }
            sample.push((g.id, v, vcpu.core, c, vcpu.cooldown == 0));
        }
    }
    // Busy: at least half of a fair share among the core's active vCPUs
    // (a starved vCPU on a crowded core still counts).
    let mut busy = alloc::vec![0u32; cores];
    let mut cand = Vec::new();
    for &(gid, v, core, c, movable) in &sample {
        if c * 100 >= elapsed_us * ACTIVE_PCT && c * active[core] * 2 >= elapsed_us {
            busy[core] += 1;
            if movable {
                cand.push((gid, v, core, c));
            }
        }
    }
    // Balance the number of busy vCPUs per core.  Ties: by load.
    let src = (0..cores).max_by_key(|&c| (busy[c], load[c])).unwrap();
    let dst = (0..cores).min_by_key(|&c| (busy[c], load[c])).unwrap();
    let light = load[dst] * 100 < elapsed_us * LIGHT_PCT;
    if busy[src] < busy[dst] + 2 && !(busy[src] == busy[dst] + 1 && light && busy[src] >= 2) {
        return;
    }
    let sibling_on = |gid: usize, core: usize| h.guest(gid).is_some_and(|g| g.vcpus.iter().any(|v| v.core == core));
    let pick = cand
        .iter()
        .filter(|&&(_, _, c, _)| c == src)
        .min_by_key(|&&(gid, _, _, used)| (sibling_on(gid, dst), u64::MAX - used));
    let Some(&(gid, v, _, _)) = pick else { return };
    if configure(h, gid, v, dst) {
        sel4::debug_println!(
            "l4h-root: balancer: guest {} vCPU {}: core {} -> {} (busy {:?})",
            gid, v, src, dst, busy
        );
        let vcpu = &mut h.guests[gid].as_mut().unwrap().vcpus[v];
        vcpu.core = dst;
        vcpu.cooldown = COOLDOWN;
    }
}
