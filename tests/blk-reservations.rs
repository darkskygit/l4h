//! Production reservation selection and detach, with mocked driver/IOMMU calls.
#![allow(dead_code)]
use l4h_host::blk::{self, DiskWindow};

mod ctl { pub const EBUSY: i64 = 16; pub const ENOSPC: i64 = 28; }

include!(env!("L4H_BLK_RESERVATIONS_TEST_SRC"));

pub struct Pci;
#[derive(Clone, Copy, PartialEq, Eq)]
struct Domain(u8);
impl Domain { const NONE: Self = Self(0); }
pub struct Iommu { unmapped: Vec<(usize, usize)> }
impl Iommu {
    fn unmap(&mut self, _: Domain, pa: usize, len: usize) { self.unmapped.push((pa, len)); }
}
struct Slot;
impl Slot { fn delete(&self) -> Result<(), ()> { Ok(()) } }
struct Component { service_ep: u64 }
impl Component { fn slot(&self, _: u64) -> Slot { Slot } }
struct Driver { comp: Component }
pub struct Controller {
    clients: Vec<Option<Attachment>>, driver: Option<Driver>, failed: bool, dom: Domain,
}
fn call(ep: u64, _: u64, _: &[u64]) -> u64 { ep }
fn quarantine(c: &mut Controller, _: &Pci) { c.failed = true; }

fn reservation() -> Attachment {
    Attachment { vm: 0, disk: 0, window: DiskWindow::new(8, 16, 1024).unwrap(),
        read_only: false, reserved: true, pool: None }
}

#[test]
fn only_dom0_can_claim_the_exact_reserved_range_even_when_table_is_full() {
    let clients = [Some(reservation())];
    let w = clients[0].as_ref().unwrap().window;
    assert_eq!(attachment_slot(&clients, 0, w, false, true), Ok(0));
    for ro in [false, true] {
        assert_eq!(attachment_slot(&clients, 0, w, ro, false), Err(ctl::EBUSY));
        for other in [DiskWindow::new(8, 8, 1024).unwrap(), DiskWindow::new(0, 32, 1024).unwrap()] {
            assert_eq!(attachment_slot(&clients, 0, other, ro, true), Err(ctl::EBUSY));
        }
    }
    assert_eq!(attachment_slot(&clients, 1, w, false, true), Err(ctl::ENOSPC));
}

#[test]
fn an_active_dom0_window_cannot_be_claimed_twice() {
    let mut a = reservation();
    let w = a.window;
    a.pool = Some((0x100000, 0x400000));
    assert_eq!(attachment_slot(&[Some(a)], 0, w, false, true), Err(ctl::EBUSY));
}

#[test]
fn detach_keeps_boot_reservations_and_releases_normal_attachments() {
    for reserved in [false, true] {
        for fails in [false, true] {
            let mut a = reservation();
            let w = a.window;
            a.reserved = reserved;
            a.pool = Some((0x100000, 0x400000));
            let mut c = Controller { clients: vec![Some(a)], failed: false, dom: Domain(1),
                driver: Some(Driver { comp: Component { service_ep: if fails { u64::MAX } else { 0 } } }) };
            let mut iommu = Iommu { unmapped: Vec::new() };
            assert!(remove(&mut c, 0, &Pci, &mut iommu));
            assert_eq!(c.failed, fails);
            assert_eq!(iommu.unmapped, [(0x100000, 0x400000)]);
            if reserved {
                assert!(c.clients[0].as_ref().unwrap().pool.is_none());
                assert_eq!(attachment_slot(&c.clients, 0, w, false, false), Err(ctl::EBUSY));
                assert_eq!(attachment_slot(&c.clients, 0, w, false, true), Ok(0));
            } else {
                assert!(c.clients[0].is_none());
            }
        }
    }
}
