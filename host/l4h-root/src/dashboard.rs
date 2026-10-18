//! A read-only status display on the boot framebuffer, when the boot
//! loader set one up (GRUB on UEFI): host resources, VMs, disks and
//! network, refreshed every second.  Only character cells that changed
//! are redrawn.  Nothing on it can be operated: VMs are managed from dom0
//! or the serial monitor.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use sel4::{CapRights, VmAttributes};

use crate::Host;
use crate::devmem::DevMem;
use crate::font;
use crate::guest::State;
use crate::mem::{Alloc, ObjCtx};
use crate::vspace::map_frame;

/// Where the framebuffer is mapped in the root task.
const FB_VA: usize = 0x0000_7200_0000_0000;
/// Refresh period in timer ticks of the balancer (100 ms).
const REFRESH_TICKS: u32 = 10;

const BG: u32 = 0x10141c;
const FG: u32 = 0xd0d4d8;
const DIM: u32 = 0x7c8590;
const HEAD: u32 = 0x80c0ff;
const BAR_BG: u32 = 0x2a5598;
const BAR_FG: u32 = 0xffffff;
const GREEN: u32 = 0x66d26e;
const RED: u32 = 0xe86a6a;
const YELLOW: u32 = 0xe8c86a;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Cell {
    ch: u8,
    fg: u32,
    bg: u32,
}

const BLANK: Cell = Cell { ch: b' ', fg: FG, bg: BG };

pub struct Dashboard {
    fb: *mut u8,
    pitch: usize,
    /// Glyph scale: 8x8 font cells are (8 sx) x (8 sy) pixels.
    sx: usize,
    sy: usize,
    cols: usize,
    rows: usize,
    /// What is on the screen, and what should be.
    shown: Vec<Cell>,
    next: Vec<Cell>,
    ticks: u32,
}

/// struct multiboot2_fb, as seL4 passes it in the bootinfo.
fn parse_fb(c: &[u8]) -> Option<(u64, usize, usize, usize, u8, u8)> {
    if c.len() < 22 {
        return None;
    }
    let u32_at = |o: usize| u32::from_le_bytes(c[o..o + 4].try_into().unwrap()) as usize;
    let addr = u64::from_le_bytes(c[0..8].try_into().unwrap());
    Some((addr, u32_at(8), u32_at(12), u32_at(16), c[20], c[21]))
}

impl Dashboard {
    #[cfg(not(target_arch = "x86_64"))]
    pub fn new(_bootinfo: &sel4::BootInfoPtr, _alloc: &mut Alloc, _devmem: &mut DevMem) -> Option<Self> {
        let _ = (parse_fb, map_frame::<>, FB_VA);
        None
    }

    /// Map the boot framebuffer, if there is a usable one (32-bit RGB).
    #[cfg(target_arch = "x86_64")]
    pub fn new(bootinfo: &sel4::BootInfoPtr, alloc: &mut Alloc, devmem: &mut DevMem) -> Option<Self> {
        let extra = bootinfo.extra().find(|e| e.id == sel4::BootInfoExtraId::X86FrameBuffer)?;
        let (addr, pitch, width, height, bpp, ty) = parse_fb(extra.content())?;
        if addr == 0 || bpp != 32 || ty != 1 || width < 640 || height < 400 {
            sel4::debug_println!("l4h-root: framebuffer {}x{}@{} type {} not supported", width, height, bpp, ty);
            return None;
        }
        // Device frames come in aligned power-of-two blocks.
        let (addr, len) = (addr as usize, pitch * height);
        let mut size = len.next_power_of_two();
        while addr & !(size - 1) < addr + len - size {
            size *= 2;
        }
        let base = addr & !(size - 1);
        let Alloc { slots, pool, .. } = alloc;
        let frames = slots.global(|slots| devmem.frames(slots, base, size))?;
        let first = (addr - base) / 4096;
        let pages = (addr - base + len).div_ceil(4096) - first;
        let wc = VmAttributes::from_inner(sel4::sys::seL4_X86_VMAttributes::seL4_X86_WriteCombining);
        slots.global(|slots| {
            for (i, f) in frames[first..first + pages].iter().enumerate() {
                map_frame(&mut ObjCtx { slots, pool }, sel4::init_thread::slot::VSPACE.cap(), *f, FB_VA + i * 4096, CapRights::read_write(), wc);
            }
        });
        let fb = (FB_VA + (addr & 4095)) as *mut u8;
        let sx = if width >= 1280 { 2 } else { 1 };
        let sy = 2;
        let (cols, rows) = (width / (8 * sx), height / (8 * sy));
        let mut d = Dashboard {
            fb,
            pitch,
            sx,
            sy,
            cols,
            rows,
            shown: vec![Cell { ch: 0, fg: 0, bg: 0 }; cols * rows],
            next: vec![BLANK; cols * rows],
            ticks: REFRESH_TICKS,
        };
        // Clear the margins the cell grid does not cover.
        for y in 0..height {
            for x in 0..width {
                d.put(x, y, BG);
            }
        }
        sel4::debug_println!("l4h-root: dashboard on {}x{} framebuffer ({} columns)", width, height, cols);
        Some(d)
    }

    fn put(&mut self, x: usize, y: usize, rgb: u32) {
        unsafe { (self.fb.add(y * self.pitch + x * 4) as *mut u32).write_volatile(rgb) };
    }

    /// Called on every balancer tick.
    pub fn tick(h: &mut Host) {
        let Some(mut d) = h.dashboard.take() else { return };
        d.ticks += 1;
        if d.ticks >= REFRESH_TICKS {
            d.ticks = 0;
            d.compose(h);
            d.flush();
        }
        h.dashboard = Some(d);
    }

    fn text(&mut self, row: usize, col: usize, s: &str, fg: u32, bg: u32) -> usize {
        if row >= self.rows {
            return col;
        }
        let mut c = col;
        for b in s.bytes() {
            if c >= self.cols {
                break;
            }
            let ch = if (0x20..0x7f).contains(&b) { b } else { b'?' };
            self.next[row * self.cols + c] = Cell { ch, fg, bg };
            c += 1;
        }
        c
    }

    fn fill_row(&mut self, row: usize, bg: u32) {
        if row < self.rows {
            for c in 0..self.cols {
                self.next[row * self.cols + c] = Cell { ch: b' ', fg: FG, bg };
            }
        }
    }

    fn compose(&mut self, h: &Host) {
        self.next.fill(BLANK);
        let hz = h.info.tsc_hz.max(1);
        let up = crate::host::ticks::now().wrapping_sub(h.info.epoch_ticks) / hz;
        // Title bar.
        self.fill_row(0, BAR_BG);
        self.text(0, 1, "l4h", BAR_FG, BAR_BG);
        let mut right = format!("up {}", duration(up));
        if h.info.epoch_ns != 0 {
            right = format!("{}   {}", utc(h.info.epoch_ns / 1_000_000_000 + up), right);
        }
        let col = self.cols.saturating_sub(right.len() + 1);
        self.text(0, col, &right, BAR_FG, BAR_BG);

        let guests: Vec<_> = h.guests.iter().flatten().collect();
        let running = guests.iter().filter(|g| g.state == State::Running).count();
        let total = h.total_bytes >> 20;
        let free = h.alloc.free_bytes() >> 20;
        let mut r = 2;
        self.text(r, 1, "Host", HEAD, BG);
        r += 1;
        let line = format!(
            "{} cores   memory {} of {} MiB in use ({} MiB free)   {} VMs, {} running",
            h.info.cores, total - free.min(total), total, free, guests.len(), running
        );
        self.text(r, 3, &line, FG, BG);
        r += 1;
        self.bar(r, 3, (total - free.min(total)) as u64, total as u64);
        r += 2;

        // VMs.
        self.text(r, 1, "Virtual machines", HEAD, BG);
        r += 1;
        let head = format!("{:>3}  {:<16} {:<8} {:>5} {:>8} {:>6} {:>5}  {}", "ID", "NAME", "STATE", "VCPUS", "MEMORY", "CPU%", "CAP%", "CORES");
        self.text(r, 3, &head, DIM, BG);
        r += 1;
        for (n, g) in guests.iter().enumerate() {
            if r + 8 >= self.rows {
                self.text(r, 3, &format!("... and {} more", guests.len() - n), DIM, BG);
                r += 1;
                break;
            }
            let (state, color) = match g.state {
                State::Running => ("running", GREEN),
                State::Stopped => ("stopped", RED),
                State::Created => ("created", YELLOW),
            };
            let cpu: u64 = g.vcpus.iter().map(|v| v.usage as u64).sum();
            let cores: Vec<String> = g.vcpus.iter().take(8).map(|v| format!("{}", v.core)).collect();
            let cap = if g.cpu_cap_pct == 0 { String::from("-") } else { format!("{}", g.cpu_cap_pct) };
            let name: String = g.name.chars().take(16).collect();
            let c = self.text(r, 3, &format!("{:>3}  {:<16} ", g.id, name), FG, BG);
            let c = self.text(r, c, &format!("{:<8}", state), color, BG);
            let more = if g.vcpus.len() > 8 { " ..." } else { "" };
            let rest = format!(
                " {:>5} {:>6}M {:>5}.{} {:>5}  {}{}{}",
                g.vcpus.len(), g.ram_size() >> 20, cpu / 10, cpu % 10, cap, cores.join(","), more,
                if g.dom0 { "   dom0" } else { "" }
            );
            self.text(r, c, &rest, FG, BG);
            r += 1;
        }
        r += 1;

        // Disks.
        self.text(r, 1, "Disks", HEAD, BG);
        r += 1;
        if h.disks.is_empty() {
            self.text(r, 3, "none", DIM, BG);
            r += 1;
        }
        for (i, d) in h.disks.iter().enumerate() {
            if r + 4 >= self.rows {
                break;
            }
            let kind = if d.kind == crate::host::blk::BLKD_NVME { "nvme" } else { "virtio-blk" };
            let owner = match d.owner {
                Some(o) => format!("vm {}{}", o, if d.read_only { ", read-only" } else { "" }),
                None => String::from("free"),
            };
            let line = format!("disk {:<2} pci {:02x}:{:02x}.{}  {:<10}  {}", i, d.pci.bus, d.pci.dev, d.pci.func, kind, owner);
            self.text(r, 3, &line, if d.owner.is_some() { FG } else { DIM }, BG);
            r += 1;
        }
        r += 1;

        // Network: switches with their uplink and ports.
        self.text(r, 1, "Network", HEAD, BG);
        r += 1;
        for sw in 0..h.netd.switches {
            if r + 2 >= self.rows {
                break;
            }
            let uplink = match h.netd.nics.iter().position(|n| n.switch == Some(sw)) {
                Some(n) => format!("uplink nic {}", n),
                None => String::from("internal"),
            };
            let ports: Vec<String> = h
                .netd
                .ports
                .iter()
                .enumerate()
                .filter_map(|(p, i)| i.as_ref().filter(|i| i.switch == sw).map(|i| (p, i.vm)))
                .map(|(p, vm)| {
                    let (rx, tx, _) = h.netd.port_stats(p);
                    format!("vm {} ({}/{})", vm, rx, tx)
                })
                .collect();
            let line = format!("switch {:<2} {:<12} {}", sw, uplink, if ports.is_empty() { String::from("no ports") } else { ports.join("  ") });
            self.text(r, 3, &line, FG, BG);
            r += 1;
        }

        // Footer.
        let last = self.rows - 1;
        self.fill_row(last, BAR_BG);
        self.text(last, 1, "Read-only. Manage VMs from dom0 (l4hctl) or the serial monitor.  Port counters: rx/tx frames.", BAR_FG, BAR_BG);
    }

    /// A usage bar of `used` out of `total`.
    fn bar(&mut self, row: usize, col: usize, used: u64, total: u64) {
        let width = (self.cols.saturating_sub(col + 2)).min(60);
        let filled = if total == 0 { 0 } else { (used * width as u64 / total) as usize };
        for i in 0..width {
            let bg = if i < filled { GREEN } else { 0x2a3038 };
            if row < self.rows && col + i < self.cols {
                self.next[row * self.cols + col + i] = Cell { ch: b' ', fg: FG, bg };
            }
        }
    }

    /// Draw the cells that changed.
    fn flush(&mut self) {
        for i in 0..self.next.len() {
            let c = self.next[i];
            if c != self.shown[i] {
                self.draw(i % self.cols, i / self.cols, c);
                self.shown[i] = c;
            }
        }
    }

    fn draw(&mut self, col: usize, row: usize, c: Cell) {
        let glyph = &font::GLYPHS[(c.ch - font::FIRST) as usize];
        let (x0, y0) = (col * 8 * self.sx, row * 8 * self.sy);
        for (gy, bits) in glyph.iter().enumerate() {
            for dy in 0..self.sy {
                let y = y0 + gy * self.sy + dy;
                for gx in 0..8 {
                    let rgb = if bits & (1 << gx) != 0 { c.fg } else { c.bg };
                    for dx in 0..self.sx {
                        self.put(x0 + gx * self.sx + dx, y, rgb);
                    }
                }
            }
        }
    }
}

/// "3d 04:05:06" or "04:05:06".
fn duration(s: u64) -> String {
    let (d, h, m, sec) = (s / 86400, s / 3600 % 24, s / 60 % 60, s % 60);
    if d > 0 { format!("{}d {:02}:{:02}:{:02}", d, h, m, sec) } else { format!("{:02}:{:02}:{:02}", h, m, sec) }
}

/// Seconds since the epoch as "YYYY-MM-DD HH:MM:SS UTC".
fn utc(t: u64) -> String {
    let (days, s) = ((t / 86400) as i64, t % 86400);
    // Civil from days (proleptic Gregorian).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC", y, m, d, s / 3600, s / 60 % 60, s % 60)
}
