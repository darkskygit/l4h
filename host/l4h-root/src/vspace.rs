//! Building address spaces: mapping frames and creating the paging
//! structures they need.

use sel4::{CapRights, ObjectBlueprint, VmAttributes, cap, cap_type};

use crate::mem::ObjCtx;

#[cfg(target_arch = "aarch64")]
pub use aarch64::*;
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;

fn is_failed_lookup(e: sel4::Error) -> bool {
    e == sel4::Error::FailedLookup
}

/// aarch64: a VSpace root and page tables at every level below it.
#[cfg(target_arch = "aarch64")]
mod aarch64 {
    use super::*;
    use sel4::{ObjectBlueprintAArch64, ObjectBlueprintArm};

    const PT_BP: ObjectBlueprint = ObjectBlueprint::Arch(ObjectBlueprintArm::PT);
    /// The root of an address space (named after x86_64's top level).
    pub const PML4_BP: ObjectBlueprint =
        ObjectBlueprint::Arch(ObjectBlueprintArm::SeL4Arch(ObjectBlueprintAArch64::VSpace));
    pub const FRAME_BP: ObjectBlueprint = ObjectBlueprint::Arch(ObjectBlueprintArm::SmallPage);

    /// Map `frame` at `vaddr` in `vspace`, creating page tables from `ctx`
    /// as needed (each failed lookup adds the missing level).
    pub fn map_frame(
        ctx: &mut ObjCtx,
        vspace: cap::VSpace,
        frame: cap::Granule,
        vaddr: usize,
        rights: CapRights,
        attrs: VmAttributes,
    ) {
        loop {
            match frame.frame_map(vspace, vaddr, rights.clone(), attrs) {
                Ok(()) => return,
                Err(e) if is_failed_lookup(e) => loop {
                    let pt = ctx.cap::<cap_type::PT>(PT_BP);
                    match pt.pt_map(vspace, vaddr, VmAttributes::DEFAULT) {
                        Ok(()) => break,
                        Err(e) if is_failed_lookup(e) => continue,
                        Err(e) => panic!("pt_map({:#x}) failed: {:?}", vaddr, e),
                    }
                },
                Err(e) => panic!("frame_map({:#x}) failed: {:?}", vaddr, e),
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
mod x86_64 {
use super::*;
use sel4::{ObjectBlueprintArch, ObjectBlueprintX64, ObjectBlueprintX86};

const PDPT_BP: ObjectBlueprint =
    ObjectBlueprint::Arch(ObjectBlueprintX86::SeL4Arch(ObjectBlueprintX64::PDPT));
const PD_BP: ObjectBlueprint = ObjectBlueprint::Arch(ObjectBlueprintX86::PageDirectory);
const PT_BP: ObjectBlueprint = ObjectBlueprint::Arch(ObjectBlueprintX86::PageTable);
pub const PML4_BP: ObjectBlueprint =
    ObjectBlueprint::Arch(ObjectBlueprintX86::SeL4Arch(ObjectBlueprintX64::PML4));
pub const FRAME_BP: ObjectBlueprint = ObjectBlueprint::Arch(ObjectBlueprintX86::_4k);

#[allow(dead_code)]
const _: Option<ObjectBlueprintArch> = None;

/// Map `frame` at `vaddr` in `vspace`, creating intermediate tables from
/// `ctx` as needed.
pub fn map_frame(
    ctx: &mut ObjCtx,
    vspace: cap::VSpace,
    frame: cap::Granule,
    vaddr: usize,
    rights: CapRights,
    attrs: VmAttributes,
) {
    loop {
        match frame.frame_map(vspace, vaddr, rights.clone(), attrs) {
            Ok(()) => return,
            Err(e) if is_failed_lookup(e) => ensure_pt(ctx, vspace, vaddr),
            Err(e) => panic!("frame_map({:#x}) failed: {:?}", vaddr, e),
        }
    }
}

fn ensure_pt(ctx: &mut ObjCtx, vspace: cap::VSpace, vaddr: usize) {
    let pt = ctx.cap::<cap_type::PageTable>(PT_BP);
    loop {
        match pt.page_table_map(vspace, vaddr, VmAttributes::DEFAULT) {
            Ok(()) => return,
            Err(e) if is_failed_lookup(e) => ensure_pd(ctx, vspace, vaddr),
            Err(e) => panic!("page_table_map({:#x}) failed: {:?}", vaddr, e),
        }
    }
}

fn ensure_pd(ctx: &mut ObjCtx, vspace: cap::VSpace, vaddr: usize) {
    let pd = ctx.cap::<cap_type::PageDirectory>(PD_BP);
    loop {
        match pd.page_directory_map(vspace, vaddr, VmAttributes::DEFAULT) {
            Ok(()) => return,
            Err(e) if is_failed_lookup(e) => {
                let pdpt = ctx.cap::<cap_type::PDPT>(PDPT_BP);
                pdpt.pdpt_map(vspace, vaddr, VmAttributes::DEFAULT)
                    .unwrap_or_else(|e| panic!("pdpt_map({:#x}) failed: {:?}", vaddr, e));
            }
            Err(e) => panic!("page_directory_map({:#x}) failed: {:?}", vaddr, e),
        }
    }
}
}
