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

/// seL4's minimum budget is a few times the kernel WCET; stay well above.
const MIN_BUDGET_US: u64 = 200;
/// Size of a vCPU's scheduling contexts: room for 1 + EXTRA_REFILLS
/// refills (the smallest holds only seL4's minimum of 2).  With few
/// refills, a budget used in many short pieces, as a vCPU that is
/// preempted and blocks often uses it, is merged into later refills and
/// lost to the vCPU.
pub const SC_BITS: usize = 9;
const EXTRA_REFILLS: u64 = 14;
/// SC_K's initial part of a capped vCPU's budget (permille).
pub const K_SHARE_INIT: u32 = 500;

/// Scheduling parameters of a vCPU, in µs: (period, SC_K budget, SC_U
/// budget).  Uncapped (`cap_pct` 0 or 100), both SCs may use the whole
/// period.  Under a cap the two SCs share one budget, `cap_pct` of the
/// period, so that the vCPU as a whole never exceeds its cap; SC_K gets
/// `k_share` permille of it.  Each SC needs at least MIN_BUDGET_US, so a
/// small cap stretches the period instead (1% runs 400 µs every 40 ms).
/// ARM virtual platforms use at least 40 ms under caps to amortize GIC
/// timer delivery latency.
pub fn budgets(timeslice_us: u64, cap_pct: u32, k_share: u32) -> (u64, u64, u64) {
    if cap_pct == 0 || cap_pct >= 100 {
        return (timeslice_us, timeslice_us, timeslice_us);
    }
    let cap = cap_pct as u64;
    // ARM virtual platforms deliver the GIC timer through a userspace
    // interrupt controller. Short periods lose useful runtime to that
    // delivery latency; retain a useful quota even after splitting it.
    let min_period = if cfg!(target_arch = "aarch64") { 40_000 } else { 0 };
    let period = timeslice_us.max(min_period).max((2 * MIN_BUDGET_US * 100).div_ceil(cap));
    let total = period * cap / 100;
    let k = (total * k_share as u64 / 1000).clamp(MIN_BUDGET_US, total - MIN_BUDGET_US);
    (period, k, total - k)
}

/// A capped vCPU used `ck` and `cu` µs of SC_K and SC_U over `elapsed_us`:
/// the new SC_K share, if its budget should move.
///
/// Once either SC used (nearly) all of its budget, the budget is split in
/// proportion to what each used.  The other SC's use is not a measure of
/// its demand: a vCPU alternates between its kernel and user threads
/// (each syscall or fault runs the one, then the other), so when one SC
/// runs out the other stalls too.  Giving the saturated SC "what the other
/// used plus headroom" therefore starved the other one, which saturated in
/// the next period: the split flipped every period, and a syscall-heavy
/// guest capped at 50% used about 10% of a core.  The ratio of the two
/// uses survives that throttling, so it is what the split follows.
fn adapt_share(timeslice_us: u64, cap_pct: u32, k_share: u32, ck: u64, cu: u64, elapsed_us: u64) -> Option<u32> {
    // More than the elapsed time: consumption from an earlier period
    // reported late (the root task ran late), not a measure of this one.
    if cap_pct == 0 || cap_pct >= 100 || ck + cu > elapsed_us || ck + cu == 0 {
        return None;
    }
    let (period, bk, bu) = budgets(timeslice_us, cap_pct, k_share);
    let total = bk + bu;
    // Use per period.
    let (uk, uu) = (ck * period / elapsed_us, cu * period / elapsed_us);
    if uk * 10 < bk * 9 && uu * 10 < bu * 9 {
        return None;
    }
    let new_k = (total * ck / (ck + cu)).clamp(MIN_BUDGET_US, total - MIN_BUDGET_US);
    // Reconfigure only for a real change.
    if new_k.abs_diff(bk) * 16 < total {
        return None;
    }
    Some((new_k * 1000).div_ceil(total) as u32)
}

/// (Re)configure vCPU `v` of guest `g` on `core` with the guest's cap.
pub fn configure(h: &Host, gid: usize, v: usize, core: usize) -> bool {
    let Some(g) = h.guest(gid) else { return false };
    let vcpu = &g.vcpus[v];
    let (period, budget_k, budget_u) = budgets(h.info.timeslice_us, g.cpu_cap_pct, vcpu.k_share);
    let sc = h.info.sched_control(core);
    let r = sc
        .sched_control_configure_flags(vcpu.sc_k, budget_k, period, EXTRA_REFILLS, 0, 0)
        .and_then(|_| sc.sched_control_configure_flags(vcpu.sc_u, budget_u, period, EXTRA_REFILLS, 0, 0));
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
    if elapsed_us == 0 {
        return;
    }
    let mut load = alloc::vec![0u64; cores];
    let mut active = alloc::vec![0u64; cores];
    // (guest, vcpu, core, consumed, movable)
    let mut sample = Vec::new();
    // Capped vCPUs whose budget split changed: (guest, vcpu, core).
    let mut resplit = Vec::new();
    let tsc_mhz = (h.info.tsc_hz / 1_000_000).max(1);
    let timeslice_us = h.info.timeslice_us;
    for g in h.guests.iter_mut().flatten() {
        let event_pages: Vec<*mut l4h_abi::EventPage> = (0..g.vcpus.len()).map(|v| g.event_page(v)).collect();
        for (v, vcpu) in g.vcpus.iter_mut().enumerate() {
            let (ck, cu) = (consumed(vcpu.sc_k), consumed(vcpu.sc_u));
            let c = ck + cu;
            if g.state != State::Running {
                continue;
            }
            if let Some(share) = adapt_share(timeslice_us, g.cpu_cap_pct, vcpu.k_share, ck, cu, elapsed_us) {
                vcpu.k_share = share;
                resplit.push((g.id, v, vcpu.core));
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
    for &(gid, v, core) in &resplit {
        configure(h, gid, v, core);
    }
    if cores < 2 {
        return;
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
        info!(
            "l4h-root: balancer: guest {} vCPU {}: core {} -> {} (busy {:?})",
            gid, v, src, dst, busy
        );
        let vcpu = &mut h.guests[gid].as_mut().unwrap().vcpus[v];
        vcpu.core = dst;
        vcpu.cooldown = COOLDOWN;
    }
}
