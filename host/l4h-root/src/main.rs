#![no_std]
#![no_main]

use sel4_root_task::root_task;

#[root_task]
fn main(bootinfo: &sel4::BootInfoPtr) -> ! {
    sel4::debug_println!("l4h-root: hello, {} untypeds", bootinfo.untyped_list().len());
    sel4::init_thread::suspend_self()
}
