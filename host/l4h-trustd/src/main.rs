//! l4h-trustd: attestation for guests (DESIGN.md §9.2).
//!
//! At start, trustd generates a P-256 signing key and has the TPM quote
//! the PCRs, which hold the measurements of what booted (the UEFI
//! application with seL4, root and every host component, then dom0's
//! files), with the hash of the key as the quote's qualifying data.  It
//! then extends PCR 15 with the same hash: the quote shows PCR 15 zero,
//! so it is the first and only key trustd issued in this boot.  The key
//! signs guests' reports; the quote, the attestation key and the EK
//! certificate make the host evidence.  The TPM is only used at start,
//! and for ActivateCredential.

#![no_std]
#![no_main]

extern crate alloc;

mod aml;
mod cbor;
mod tpm;
mod tpm2;

use alloc::vec::Vec;

use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use sha2::{Digest, Sha256};

use l4h_abi as abi;
use l4h_rt::host::trust::{self, GuestRecord, TrustdConfig};
use l4h_rt::{Info, cap, host, info, sel4};
use sel4::cap_type;

use crate::cbor::Enc;
use crate::tpm::{Error, Tpm};

const EINVAL: i64 = 22;
const ENOENT: i64 = 2;
const EIO: i64 = 5;
const ENOSYS: i64 = 38;

/// PCRs in the quote: the static ones (SRTM), 0-15.
const QUOTE_PCRS: u32 = 0xffff;
/// Extended with the key's hash once it is quoted.
const KEY_PCR: u32 = 15;
const KEY_DOMAIN: &[u8] = b"l4h-trustd-key-v1";

struct State {
    tpm: Tpm,
    key: SigningKey,
    /// SHA-256 of the key's SEC1 point.
    key_id: [u8; 32],
    ak: tpm2::Key,
    /// The EK, created on first use (ActivateCredential), RSA unless the
    /// TPM has an ECC EK certificate.
    ek: Option<u32>,
    ek_ecc: bool,
    evidence: Vec<u8>,
}

fn mr(i: usize) -> u64 {
    sel4::with_ipc_buffer(|b| b.msg_regs()[i])
}

fn set_mr(i: usize, v: u64) {
    sel4::with_ipc_buffer_mut(|b| b.msg_regs_mut()[i] = v)
}

/// The P-256 point of `key`, uncompressed (0x04, x, y).
fn point(key: &SigningKey) -> [u8; 65] {
    key.verifying_key().to_encoded_point(false).as_bytes().try_into().unwrap()
}

/// `r`, with what failed.
fn step<T>(what: &'static str, r: Result<T, Error>) -> Result<T, (&'static str, Error)> {
    r.map_err(|e| (what, e))
}

fn init(cfg: &TrustdConfig, hz: u64) -> Result<State, (&'static str, Error)> {
    let t0 = host::ticks::now();
    let mut tpm = step("interface", Tpm::new(cfg, hz))?;
    step("Startup", tpm2::startup(&mut tpm))?;
    let key = loop {
        let mut seed = [0u8; 32];
        step("GetRandom", tpm2::get_random(&mut tpm, &mut seed))?;
        if let Ok(k) = SigningKey::from_bytes(&seed.into()) {
            break k;
        }
    };
    let p = point(&key);
    let key_id: [u8; 32] = Sha256::digest(p).into();
    let bound: [u8; 32] = Sha256::new().chain_update(KEY_DOMAIN).chain_update(p).finalize().into();

    let ak = step("CreatePrimary (AK)", tpm2::create_primary(&mut tpm, tpm2::RH_ENDORSEMENT, &tpm2::ak_template()))?;
    let pcrs = step("PCR_Read", tpm2::pcr_read(&mut tpm, QUOTE_PCRS))?;
    let (quote, sig) = step("Quote", tpm2::quote(&mut tpm, ak.handle, &bound, QUOTE_PCRS))?;
    step("PCR_Extend", tpm2::pcr_extend(&mut tpm, KEY_PCR, &bound))?;
    // The EK certificate, if the manufacturer stored one.
    let (ek_cert, ek_ecc) = match step("NV_Read (EK certificate)", tpm2::nv_read(&mut tpm, tpm2::NV_EK_CERT_ECC))? {
        Some(c) => (Some(c), true),
        None => (step("NV_Read (EK certificate)", tpm2::nv_read(&mut tpm, tpm2::NV_EK_CERT_RSA))?, false),
    };

    // The evidence: keys in the order of their encodings (RFC 8949 §4.2.1).
    let mut e = Enc::new();
    e.map(6 + ek_cert.is_some() as usize);
    e.text("v").uint(1);
    e.text("ak").bytes(&ak.public);
    // A COSE_Key (EC2, P-256).
    e.text("key").map(4).uint(1).uint(2).nint(0).uint(1).nint(1).bytes(&p[1..33]).nint(2).bytes(&p[33..]);
    e.text("pcrs").map(pcrs.len());
    for (i, v) in &pcrs {
        e.uint(*i as u64).bytes(v);
    }
    e.text("quote").bytes(&quote);
    if let Some(c) = &ek_cert {
        e.text("ekcert").bytes(c);
    }
    e.text("quotesig").bytes(&sig);
    info!(
        "l4h-trustd: ready in {} ms: signing key {:02x}{:02x}{:02x}{:02x}..., {} EK certificate",
        (host::ticks::now() - t0) * 1000 / hz.max(1),
        key_id[0], key_id[1], key_id[2], key_id[3],
        match (&ek_cert, ek_ecc) {
            (None, _) => "no",
            (Some(_), true) => "an ECC",
            (Some(_), false) => "an RSA",
        }
    );
    Ok(State { tpm, key, key_id, ak, ek: None, ek_ecc, evidence: e.0 })
}

/// Guest `id`'s record, if it runs.
fn record(id: usize) -> Option<GuestRecord> {
    if id >= trust::MAX_RECORDS {
        return None;
    }
    let p = (trust::REGISTRY_VA as *const GuestRecord).wrapping_add(id);
    loop {
        let r = unsafe { p.read_volatile() };
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        let again = unsafe { core::ptr::addr_of!((*p).instance).read_volatile() };
        if again == r.instance {
            return (r.instance != 0).then_some(r);
        }
    }
}

impl State {
    /// The COSE_Sign1 report on guest `id`, whose record is `r`, with the
    /// guest's `data`.
    fn report(&self, id: usize, r: &GuestRecord, data: &[u8; abi::ATTEST_DATA_SIZE]) -> Vec<u8> {
        let mut c = Enc::new();
        c.map(9);
        c.text("v").uint(1);
        c.text("ram").uint(r.ram);
        c.text("data").bytes(data);
        c.text("dom0").bool(r.flags & trust::RECORD_DOM0 != 0);
        c.text("host").bytes(&self.key_id);
        c.text("guest").uint(id as u64);
        c.text("vcpus").uint(r.vcpus as u64);
        c.text("launch").bytes(&r.launch);
        c.text("instance").uint(r.instance);
        let payload = c.0;
        // Protected header {1: -7}: alg ES256.
        let protected = [0xa1, 0x01, 0x26];
        let mut s = Enc::new();
        s.array(4).text("Signature1").bytes(&protected).bytes(&[]).bytes(&payload);
        let sig: Signature = self.key.sign(&s.0);
        let mut out = Enc::new();
        out.tag(18).array(4).bytes(&protected).map(0).bytes(&payload).bytes(&sig.to_bytes());
        out.0
    }

    fn activate(&mut self, blob: &[u8], secret: &[u8]) -> Result<Vec<u8>, Error> {
        let ek = match self.ek {
            Some(h) => h,
            None => {
                let k = tpm2::create_primary(&mut self.tpm, tpm2::RH_ENDORSEMENT, &tpm2::ek_template(self.ek_ecc))?;
                self.ek = Some(k.handle);
                k.handle
            }
        };
        tpm2::activate_credential(&mut self.tpm, self.ak.handle, ek, blob, secret)
    }
}

/// Reply with bytes [off, off + ATTEST_CHUNK) of `data`.
fn reply_data(data: &[u8], off: usize) -> usize {
    let n = data.len().saturating_sub(off).min(abi::ATTEST_CHUNK);
    set_mr(0, 0);
    set_mr(1, data.len() as u64);
    set_mr(2, n as u64);
    for (w, chunk) in data[off.min(data.len())..off.min(data.len()) + n].chunks(8).enumerate() {
        let mut b = [0u8; 8];
        b[..chunk.len()].copy_from_slice(chunk);
        set_mr(3 + w, u64::from_le_bytes(b));
    }
    3 + n.div_ceil(8)
}

/// `n` bytes packed in message registers from `first`.
fn mr_bytes(first: usize, n: usize) -> Vec<u8> {
    (0..n).map(|i| (mr(first + i / 8) >> (8 * (i % 8))) as u8).collect()
}

fn main(info: &'static Info) -> ! {
    let cfg: &TrustdConfig = unsafe { info.config() };
    let mut state = match init(cfg, info.header().tsc_hz) {
        Ok(s) => Some(s),
        Err((what, e)) => {
            sel4::debug_println!("l4h-trustd: TPM unusable, no attestation: {}: {}", what, e);
            None
        }
    };
    let ep = cap::<cap_type::Endpoint>(host::SLOT_SERVICE_EP);
    let reply = cap::<cap_type::Reply>(host::SLOT_REPLY);
    let (mut msg, mut badge) = ep.recv(reply);
    loop {
        let id = (badge & 0xffff) as usize;
        let rec = (badge & trust::BADGE_GUEST != 0).then(|| record(id)).flatten();
        let words = match (&mut state, rec) {
            (None, _) => {
                set_mr(0, (-EIO) as u64);
                1
            }
            (_, None) => {
                set_mr(0, (-ENOENT) as u64);
                1
            }
            (Some(s), Some(r)) => match msg.label() {
                abi::ATTEST_REPORT => {
                    let data: [u8; abi::ATTEST_DATA_SIZE] = mr_bytes(0, abi::ATTEST_DATA_SIZE).try_into().unwrap();
                    let off = mr(8) as usize;
                    reply_data(&s.report(id, &r, &data), off)
                }
                abi::ATTEST_EVIDENCE => reply_data(&s.evidence, mr(0) as usize),
                abi::ATTEST_ACTIVATE => {
                    let (nb, ns) = (mr(0) as usize, mr(1) as usize);
                    if nb.checked_add(ns).is_none_or(|n| 2 + n.div_ceil(8) > msg.length()) {
                        set_mr(0, (-EINVAL) as u64);
                        1
                    } else {
                        let b = mr_bytes(2, nb + ns);
                        match s.activate(&b[..nb], &b[nb..]) {
                            Ok(cred) => reply_data(&cred, 0),
                            Err(e) => {
                                info!("l4h-trustd: guest {}: ActivateCredential failed: {}", id, e);
                                set_mr(0, (-EINVAL) as u64);
                                1
                            }
                        }
                    }
                }
                _ => {
                    set_mr(0, (-ENOSYS) as u64);
                    1
                }
            },
        };
        (msg, badge) = ep.reply_recv(sel4::MessageInfo::new(0, 0, 0, words), reply);
    }
}

l4h_rt::component!(main);
