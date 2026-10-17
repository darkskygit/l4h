//! Building x86_64 address spaces: mapping frames and creating the paging
//! structures they need.

use sel4::{CapRights, ObjectBlueprint, VmAttributes, cap, cap_type};
use sel4::{ObjectBlueprintArch, ObjectBlueprintX64, ObjectBlueprintX86};

use crate::mem::ObjCtx;

const PDPT_BP: ObjectBlueprint =
    ObjectBlueprint::Arch(ObjectBlueprintX86::SeL4Arch(ObjectBlueprintX64::PDPT));
const PD_BP: ObjectBlueprint = ObjectBlueprint::Arch(ObjectBlueprintX86::PageDirectory);
const PT_BP: ObjectBlueprint = ObjectBlueprint::Arch(ObjectBlueprintX86::PageTable);
pub const PML4_BP: ObjectBlueprint =
    ObjectBlueprint::Arch(ObjectBlueprintX86::SeL4Arch(ObjectBlueprintX64::PML4));
pub const FRAME_BP: ObjectBlueprint = ObjectBlueprint::Arch(ObjectBlueprintX86::_4k);

#[allow(dead_code)]
const _: Option<ObjectBlueprintArch> = None;

fn is_failed_lookup(e: sel4::Error) -> bool {
    e == sel4::Error::FailedLookup
}

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
