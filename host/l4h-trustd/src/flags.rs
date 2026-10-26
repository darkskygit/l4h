//! Fixed-size, single-writer flags service. No heap, TPM, AML or calls
//! back to root: a caller donates only the time for this one request.

use l4h_rt::{cap, host::trust::{self, Flags, FlagsEntry}, sel4};
use sel4::cap_type;

pub fn read(id: usize, instance: u64) -> Flags {
    unsafe { &*(trust::FLAGS_VA as *const FlagsEntry).add(id) }.read().for_instance(instance)
}

pub fn call(op: u64, id: usize, instance: u64, bits: [u64; 4], flags: u64) -> Result<Flags, i64> {
    sel4::with_ipc_buffer_mut(|b| {
        b.msg_regs_mut()[..8].copy_from_slice(&[op, id as u64, instance, bits[0], bits[1], bits[2], bits[3], flags]);
    });
    let reply = cap::<cap_type::Endpoint>(trust::SLOT_FLAGS_CALL).call(sel4::MessageInfo::new(0, 0, 0, 8));
    if reply.length() != 10 { return Err(5); }
    sel4::with_ipc_buffer(|b| {
        let m = b.msg_regs();
        if m[0] != 0 { return Err(-(m[0] as i64)); }
        Ok(Flags { instance: m[1], taint: m[2..6].try_into().unwrap(), seal: m[6..10].try_into().unwrap() })
    })
}

extern "C" fn serve() -> ! {
    unsafe { sel4::set_ipc_buffer(&mut *(trust::FLAGS_IPCBUF_VA as *mut sel4::IpcBuffer)) };
    let ep = cap::<cap_type::Endpoint>(trust::SLOT_FLAGS_EP);
    let reply = cap::<cap_type::Reply>(trust::SLOT_FLAGS_REPLY);
    let (mut info, mut badge) = ep.recv(reply);
    loop {
        let req: [u64; 8] = sel4::with_ipc_buffer(|b| b.msg_regs()[..8].try_into().unwrap());
        let result = if info.length() != 8 {
            Err(22)
        } else if badge == trust::FLAGS_ROOT && req[0] == trust::FLAGS_READY {
            Ok(Flags::default())
        } else if req[1] >= trust::MAX_RECORDS as u64 {
            Err(22)
        } else {
            // Only this thread has a writable reference to the entries.
            let entry = unsafe { &*(trust::FLAGS_WRITE_VA as *const FlagsEntry).add(req[1] as usize) };
            unsafe { entry.update(badge, req[0], req[2], req[3..7].try_into().unwrap(), req[7]) }
        };
        sel4::with_ipc_buffer_mut(|b| {
            let m = &mut b.msg_regs_mut()[..10];
            m.fill(0);
            match result {
                Err(e) => m[0] = (-e) as u64,
                Ok(f) => {
                    m[1] = f.instance;
                    m[2..6].copy_from_slice(&f.taint);
                    m[6..10].copy_from_slice(&f.seal);
                }
            }
        });
        (info, badge) = ep.reply_recv(sel4::MessageInfo::new(0, 0, 0, 10), reply);
    }
}

pub fn start() {
    unsafe { l4h_rt::start_secondary(trust::SLOT_FLAGS_TCB, trust::FLAGS_STACK_TOP, serve) };
}
