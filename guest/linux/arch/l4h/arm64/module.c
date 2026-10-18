// SPDX-License-Identifier: GPL-2.0-only
/*
 * AArch64 loadable module support.
 *
 * Copyright (C) 2012 ARM Limited
 *
 * Author: Will Deacon <will.deacon@arm.com>
 */

/* Native static relocations, with absolute veneers for CALL26/JUMP26.
 * The RAM direct map can place modules more than 128 MiB from the kernel. */
#include <asm/elf.h>
#include <linux/elf.h>
#include <linux/kernel.h>
#include <linux/moduleloader.h>
#include <linux/string.h>

struct l4h_plt_entry {
  __le32 ldr, br;
  __le64 target;
};

int module_frob_arch_sections(Elf_Ehdr *hdr, Elf_Shdr *sechdrs,
                              char *secstrings, struct module *mod) {
  unsigned int i, count = 0, plt = 0;

  for (i = 1; i < hdr->e_shnum; i++) {
    Elf_Shdr *s = &sechdrs[i];
    if (!strcmp(secstrings + s->sh_name, ".plt"))
      plt = i;
    if (s->sh_type == SHT_RELA) {
      Elf_Rela *rel = (void *)hdr + s->sh_offset;
      unsigned int n;
      for (n = 0; n < s->sh_size / sizeof(*rel); n++) {
        unsigned int type = ELF_R_TYPE(rel[n].r_info);
        if (type == R_AARCH64_CALL26 || type == R_AARCH64_JUMP26)
          count++;
      }
    }
  }
  if (!plt)
    return -ENOEXEC;
  mod->arch.plt_section = plt;
  mod->arch.plt_count = 0;
  mod->arch.plt_max = count;
  sechdrs[plt].sh_type = SHT_NOBITS;
  sechdrs[plt].sh_flags = SHF_EXECINSTR | SHF_ALLOC;
  sechdrs[plt].sh_addralign = __alignof__(struct l4h_plt_entry);
  sechdrs[plt].sh_size =
      (unsigned long)(count + 1) * sizeof(struct l4h_plt_entry);
  return 0;
}

static unsigned long module_veneer(struct module *mod, Elf_Shdr *sechdrs,
                                   u64 target) {
  struct l4h_plt_entry *plt = (void *)sechdrs[mod->arch.plt_section].sh_addr;
  unsigned int n;

  for (n = 0; n < mod->arch.plt_count; n++)
    if (le64_to_cpu(plt[n].target) == target)
      return (unsigned long)&plt[n];
  if (n >= mod->arch.plt_max)
    return 0;
  plt[n].ldr = cpu_to_le32(0x58000050); /* ldr x16, .+8 */
  plt[n].br = cpu_to_le32(0xd61f0200);  /* br x16 */
  plt[n].target = cpu_to_le64(target);
  mod->arch.plt_count++;
  return (unsigned long)&plt[n];
}

enum aarch64_insn_imm_type {
  AARCH64_INSN_IMM_26,
  AARCH64_INSN_IMM_19,
  AARCH64_INSN_IMM_16,
  AARCH64_INSN_IMM_14,
  AARCH64_INSN_IMM_12,
  AARCH64_INSN_IMM_ADR
};
static u32 aarch64_insn_encode_immediate(enum aarch64_insn_imm_type type,
                                         u32 insn, u64 value) {
  u32 mask;
  int shift;
  switch (type) {
  case AARCH64_INSN_IMM_ADR:
    mask = (3U << 29) | (0x7ffffU << 5);
    return (insn & ~mask) | ((value & 3) << 29) |
           (((value >> 2) & 0x7ffff) << 5);
  case AARCH64_INSN_IMM_26:
    mask = 0x3ffffff;
    shift = 0;
    break;
  case AARCH64_INSN_IMM_19:
    mask = 0x7ffff;
    shift = 5;
    break;
  case AARCH64_INSN_IMM_16:
    mask = 0xffff;
    shift = 5;
    break;
  case AARCH64_INSN_IMM_14:
    mask = 0x3fff;
    shift = 5;
    break;
  case AARCH64_INSN_IMM_12:
    mask = 0xfff;
    shift = 10;
    break;
  default:
    return insn;
  }
  return (insn & ~(mask << shift)) | ((value & mask) << shift);
}

enum aarch64_reloc_op {
  RELOC_OP_NONE,
  RELOC_OP_ABS,
  RELOC_OP_PREL,
  RELOC_OP_PAGE,
};

static u64 do_reloc(enum aarch64_reloc_op reloc_op, __le32 *place, u64 val) {
  switch (reloc_op) {
  case RELOC_OP_ABS:
    return val;
  case RELOC_OP_PREL:
    return val - (u64)place;
  case RELOC_OP_PAGE:
    return (val & ~0xfff) - ((u64)place & ~0xfff);
  case RELOC_OP_NONE:
    return 0;
  }

  pr_err("do_reloc: unknown relocation operation %d\n", reloc_op);
  return 0;
}

#define WRITE_PLACE(place, val, mod)                                           \
  do {                                                                         \
    __typeof__(*(place)) __v = (val);                                          \
    memcpy((place), &__v, sizeof(__v));                                        \
  } while (0)

static int reloc_data(enum aarch64_reloc_op op, void *place, u64 val, int len,
                      struct module *me) {
  s64 sval = do_reloc(op, place, val);

  /*
   * The ELF psABI for AArch64 documents the 16-bit and 32-bit place
   * relative and absolute relocations as having a range of [-2^15, 2^16)
   * or [-2^31, 2^32), respectively. However, in order to be able to
   * detect overflows reliably, we have to choose whether we interpret
   * such quantities as signed or as unsigned, and stick with it.
   * The way we organize our address space requires a signed
   * interpretation of 32-bit relative references, so let's use that
   * for all R_AARCH64_PRELxx relocations. This means our upper
   * bound for overflow detection should be Sxx_MAX rather than Uxx_MAX.
   */

  switch (len) {
  case 16:
    WRITE_PLACE((s16 *)place, sval, me);
    switch (op) {
    case RELOC_OP_ABS:
      if (sval < 0 || sval > U16_MAX)
        return -ERANGE;
      break;
    case RELOC_OP_PREL:
      if (sval < S16_MIN || sval > S16_MAX)
        return -ERANGE;
      break;
    default:
      pr_err("Invalid 16-bit data relocation (%d)\n", op);
      return 0;
    }
    break;
  case 32:
    WRITE_PLACE((s32 *)place, sval, me);
    switch (op) {
    case RELOC_OP_ABS:
      if (sval < 0 || sval > U32_MAX)
        return -ERANGE;
      break;
    case RELOC_OP_PREL:
      if (sval < S32_MIN || sval > S32_MAX)
        return -ERANGE;
      break;
    default:
      pr_err("Invalid 32-bit data relocation (%d)\n", op);
      return 0;
    }
    break;
  case 64:
    WRITE_PLACE((s64 *)place, sval, me);
    break;
  default:
    pr_err("Invalid length (%d) for data relocation\n", len);
    return 0;
  }
  return 0;
}

enum aarch64_insn_movw_imm_type {
  AARCH64_INSN_IMM_MOVNZ,
  AARCH64_INSN_IMM_MOVKZ,
};

static int reloc_insn_movw(enum aarch64_reloc_op op, __le32 *place, u64 val,
                           int lsb, enum aarch64_insn_movw_imm_type imm_type,
                           struct module *me) {
  u64 imm;
  s64 sval;
  u32 insn = le32_to_cpu(*place);

  sval = do_reloc(op, place, val);
  imm = sval >> lsb;

  if (imm_type == AARCH64_INSN_IMM_MOVNZ) {
    /*
     * For signed MOVW relocations, we have to manipulate the
     * instruction encoding depending on whether or not the
     * immediate is less than zero.
     */
    insn &= ~(3 << 29);
    if (sval >= 0) {
      /* >=0: Set the instruction to MOVZ (opcode 10b). */
      insn |= 2 << 29;
    } else {
      /*
       * <0: Set the instruction to MOVN (opcode 00b).
       *     Since we've masked the opcode already, we
       *     don't need to do anything other than
       *     inverting the new immediate field.
       */
      imm = ~imm;
    }
  }

  /* Update the instruction with the new encoding. */
  insn = aarch64_insn_encode_immediate(AARCH64_INSN_IMM_16, insn, imm);
  WRITE_PLACE(place, cpu_to_le32(insn), me);

  if (imm > U16_MAX)
    return -ERANGE;

  return 0;
}

static int reloc_insn_imm(enum aarch64_reloc_op op, __le32 *place, u64 val,
                          int lsb, int len, enum aarch64_insn_imm_type imm_type,
                          struct module *me) {
  u64 imm, imm_mask;
  s64 sval;
  u32 insn = le32_to_cpu(*place);

  /* Calculate the relocation value. */
  sval = do_reloc(op, place, val);
  sval >>= lsb;

  /* Extract the value bits and shift them to bit 0. */
  imm_mask = (BIT(lsb + len) - 1) >> lsb;
  imm = sval & imm_mask;

  /* Update the instruction's immediate field. */
  insn = aarch64_insn_encode_immediate(imm_type, insn, imm);
  WRITE_PLACE(place, cpu_to_le32(insn), me);

  /*
   * Extract the upper value bits (including the sign bit) and
   * shift them to bit 0.
   */
  sval = (s64)(sval & ~(imm_mask >> 1)) >> (len - 1);

  /*
   * Overflow has occurred if the upper bits are not all equal to
   * the sign bit of the value.
   */
  if ((u64)(sval + 1) >= 2)
    return -ERANGE;

  return 0;
}

static int reloc_insn_adrp(struct module *mod, Elf64_Shdr *sechdrs,
                           __le32 *place, u64 val, struct module *me) {
  return reloc_insn_imm(RELOC_OP_PAGE, place, val, 12, 21, AARCH64_INSN_IMM_ADR,
                        me);
}

int apply_relocate_add(Elf64_Shdr *sechdrs, const char *strtab,
                       unsigned int symindex, unsigned int relsec,
                       struct module *me) {
  unsigned int i;
  int ovf;
  bool overflow_check;
  Elf64_Sym *sym;
  void *loc;
  u64 val;
  Elf64_Rela *rel = (void *)sechdrs[relsec].sh_addr;

  for (i = 0; i < sechdrs[relsec].sh_size / sizeof(*rel); i++) {
    /* loc corresponds to P in the AArch64 ELF document. */
    loc = (void *)sechdrs[sechdrs[relsec].sh_info].sh_addr + rel[i].r_offset;

    /* sym is the ELF symbol we're referring to. */
    sym = (Elf64_Sym *)sechdrs[symindex].sh_addr + ELF64_R_SYM(rel[i].r_info);

    /* val corresponds to (S + A) in the AArch64 ELF document. */
    val = sym->st_value + rel[i].r_addend;

    /* Check for overflow by default. */
    overflow_check = true;

    /* Perform the static relocation. */
    switch (ELF64_R_TYPE(rel[i].r_info)) {
    /* Null relocations. */
    case R_ARM_NONE:
    case R_AARCH64_NONE:
      ovf = 0;
      break;

    /* Data relocations. */
    case R_AARCH64_ABS64:
      overflow_check = false;
      ovf = reloc_data(RELOC_OP_ABS, loc, val, 64, me);
      break;
    case R_AARCH64_ABS32:
      ovf = reloc_data(RELOC_OP_ABS, loc, val, 32, me);
      break;
    case R_AARCH64_ABS16:
      ovf = reloc_data(RELOC_OP_ABS, loc, val, 16, me);
      break;
    case R_AARCH64_PREL64:
      overflow_check = false;
      ovf = reloc_data(RELOC_OP_PREL, loc, val, 64, me);
      break;
    case R_AARCH64_PREL32:
      ovf = reloc_data(RELOC_OP_PREL, loc, val, 32, me);
      break;
    case R_AARCH64_PREL16:
      ovf = reloc_data(RELOC_OP_PREL, loc, val, 16, me);
      break;

    /* MOVW instruction relocations. */
    case R_AARCH64_MOVW_UABS_G0_NC:
      overflow_check = false;
      fallthrough;
    case R_AARCH64_MOVW_UABS_G0:
      ovf = reloc_insn_movw(RELOC_OP_ABS, loc, val, 0, AARCH64_INSN_IMM_MOVKZ,
                            me);
      break;
    case R_AARCH64_MOVW_UABS_G1_NC:
      overflow_check = false;
      fallthrough;
    case R_AARCH64_MOVW_UABS_G1:
      ovf = reloc_insn_movw(RELOC_OP_ABS, loc, val, 16, AARCH64_INSN_IMM_MOVKZ,
                            me);
      break;
    case R_AARCH64_MOVW_UABS_G2_NC:
      overflow_check = false;
      fallthrough;
    case R_AARCH64_MOVW_UABS_G2:
      ovf = reloc_insn_movw(RELOC_OP_ABS, loc, val, 32, AARCH64_INSN_IMM_MOVKZ,
                            me);
      break;
    case R_AARCH64_MOVW_UABS_G3:
      /* We're using the top bits so we can't overflow. */
      overflow_check = false;
      ovf = reloc_insn_movw(RELOC_OP_ABS, loc, val, 48, AARCH64_INSN_IMM_MOVKZ,
                            me);
      break;
    case R_AARCH64_MOVW_SABS_G0:
      ovf = reloc_insn_movw(RELOC_OP_ABS, loc, val, 0, AARCH64_INSN_IMM_MOVNZ,
                            me);
      break;
    case R_AARCH64_MOVW_SABS_G1:
      ovf = reloc_insn_movw(RELOC_OP_ABS, loc, val, 16, AARCH64_INSN_IMM_MOVNZ,
                            me);
      break;
    case R_AARCH64_MOVW_SABS_G2:
      ovf = reloc_insn_movw(RELOC_OP_ABS, loc, val, 32, AARCH64_INSN_IMM_MOVNZ,
                            me);
      break;
    case R_AARCH64_MOVW_PREL_G0_NC:
      overflow_check = false;
      ovf = reloc_insn_movw(RELOC_OP_PREL, loc, val, 0, AARCH64_INSN_IMM_MOVKZ,
                            me);
      break;
    case R_AARCH64_MOVW_PREL_G0:
      ovf = reloc_insn_movw(RELOC_OP_PREL, loc, val, 0, AARCH64_INSN_IMM_MOVNZ,
                            me);
      break;
    case R_AARCH64_MOVW_PREL_G1_NC:
      overflow_check = false;
      ovf = reloc_insn_movw(RELOC_OP_PREL, loc, val, 16, AARCH64_INSN_IMM_MOVKZ,
                            me);
      break;
    case R_AARCH64_MOVW_PREL_G1:
      ovf = reloc_insn_movw(RELOC_OP_PREL, loc, val, 16, AARCH64_INSN_IMM_MOVNZ,
                            me);
      break;
    case R_AARCH64_MOVW_PREL_G2_NC:
      overflow_check = false;
      ovf = reloc_insn_movw(RELOC_OP_PREL, loc, val, 32, AARCH64_INSN_IMM_MOVKZ,
                            me);
      break;
    case R_AARCH64_MOVW_PREL_G2:
      ovf = reloc_insn_movw(RELOC_OP_PREL, loc, val, 32, AARCH64_INSN_IMM_MOVNZ,
                            me);
      break;
    case R_AARCH64_MOVW_PREL_G3:
      /* We're using the top bits so we can't overflow. */
      overflow_check = false;
      ovf = reloc_insn_movw(RELOC_OP_PREL, loc, val, 48, AARCH64_INSN_IMM_MOVNZ,
                            me);
      break;

    /* Immediate instruction relocations. */
    case R_AARCH64_LD_PREL_LO19:
      ovf = reloc_insn_imm(RELOC_OP_PREL, loc, val, 2, 19, AARCH64_INSN_IMM_19,
                           me);
      break;
    case R_AARCH64_ADR_PREL_LO21:
      ovf = reloc_insn_imm(RELOC_OP_PREL, loc, val, 0, 21, AARCH64_INSN_IMM_ADR,
                           me);
      break;
    case R_AARCH64_ADR_PREL_PG_HI21_NC:
      overflow_check = false;
      fallthrough;
    case R_AARCH64_ADR_PREL_PG_HI21:
      ovf = reloc_insn_adrp(me, sechdrs, loc, val, me);
      if (ovf && ovf != -ERANGE)
        return ovf;
      break;
    case R_AARCH64_ADD_ABS_LO12_NC:
    case R_AARCH64_LDST8_ABS_LO12_NC:
      overflow_check = false;
      ovf = reloc_insn_imm(RELOC_OP_ABS, loc, val, 0, 12, AARCH64_INSN_IMM_12,
                           me);
      break;
    case R_AARCH64_LDST16_ABS_LO12_NC:
      overflow_check = false;
      ovf = reloc_insn_imm(RELOC_OP_ABS, loc, val, 1, 11, AARCH64_INSN_IMM_12,
                           me);
      break;
    case R_AARCH64_LDST32_ABS_LO12_NC:
      overflow_check = false;
      ovf = reloc_insn_imm(RELOC_OP_ABS, loc, val, 2, 10, AARCH64_INSN_IMM_12,
                           me);
      break;
    case R_AARCH64_LDST64_ABS_LO12_NC:
      overflow_check = false;
      ovf =
          reloc_insn_imm(RELOC_OP_ABS, loc, val, 3, 9, AARCH64_INSN_IMM_12, me);
      break;
    case R_AARCH64_LDST128_ABS_LO12_NC:
      overflow_check = false;
      ovf =
          reloc_insn_imm(RELOC_OP_ABS, loc, val, 4, 8, AARCH64_INSN_IMM_12, me);
      break;
    case R_AARCH64_TSTBR14:
      ovf = reloc_insn_imm(RELOC_OP_PREL, loc, val, 2, 14, AARCH64_INSN_IMM_14,
                           me);
      break;
    case R_AARCH64_CONDBR19:
      ovf = reloc_insn_imm(RELOC_OP_PREL, loc, val, 2, 19, AARCH64_INSN_IMM_19,
                           me);
      break;
    case R_AARCH64_JUMP26:
    case R_AARCH64_CALL26:
      ovf = reloc_insn_imm(RELOC_OP_PREL, loc, val, 2, 26, AARCH64_INSN_IMM_26,
                           me);

      if (ovf == -ERANGE) {
        val = module_veneer(me, sechdrs, val);
        if (!val)
          return -ENOEXEC;
        ovf = reloc_insn_imm(RELOC_OP_PREL, loc, val, 2, 26,
                             AARCH64_INSN_IMM_26, me);
      }
      break;

    default:
      pr_err("module %s: unsupported RELA relocation: %llu\n", me->name,
             ELF64_R_TYPE(rel[i].r_info));
      return -ENOEXEC;
    }

    if (overflow_check && ovf == -ERANGE)
      goto overflow;
  }

  return 0;

overflow:
  pr_err("module %s: overflow in relocation type %d val %Lx\n", me->name,
         (int)ELF64_R_TYPE(rel[i].r_info), val);
  return -ENOEXEC;
}
