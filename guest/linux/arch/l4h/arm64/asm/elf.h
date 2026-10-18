/* SPDX-License-Identifier: GPL-2.0 */
#ifndef L4H_ARM64_ELF_H
#define L4H_ARM64_ELF_H
#include <asm/ptrace.h>
#include <asm/hwcap.h>
/* Miscellaneous. */
#define R_ARM_NONE 0
#define R_AARCH64_NONE 256

/* Data. */
#define R_AARCH64_ABS64 257
#define R_AARCH64_ABS32 258
#define R_AARCH64_ABS16 259
#define R_AARCH64_PREL64 260
#define R_AARCH64_PREL32 261
#define R_AARCH64_PREL16 262

/* Instructions. */
#define R_AARCH64_MOVW_UABS_G0 263
#define R_AARCH64_MOVW_UABS_G0_NC 264
#define R_AARCH64_MOVW_UABS_G1 265
#define R_AARCH64_MOVW_UABS_G1_NC 266
#define R_AARCH64_MOVW_UABS_G2 267
#define R_AARCH64_MOVW_UABS_G2_NC 268
#define R_AARCH64_MOVW_UABS_G3 269

#define R_AARCH64_MOVW_SABS_G0 270
#define R_AARCH64_MOVW_SABS_G1 271
#define R_AARCH64_MOVW_SABS_G2 272

#define R_AARCH64_LD_PREL_LO19 273
#define R_AARCH64_ADR_PREL_LO21 274
#define R_AARCH64_ADR_PREL_PG_HI21 275
#define R_AARCH64_ADR_PREL_PG_HI21_NC 276
#define R_AARCH64_ADD_ABS_LO12_NC 277
#define R_AARCH64_LDST8_ABS_LO12_NC 278

#define R_AARCH64_TSTBR14 279
#define R_AARCH64_CONDBR19 280
#define R_AARCH64_JUMP26 282
#define R_AARCH64_CALL26 283
#define R_AARCH64_LDST16_ABS_LO12_NC 284
#define R_AARCH64_LDST32_ABS_LO12_NC 285
#define R_AARCH64_LDST64_ABS_LO12_NC 286
#define R_AARCH64_LDST128_ABS_LO12_NC 299

#define R_AARCH64_MOVW_PREL_G0 287
#define R_AARCH64_MOVW_PREL_G0_NC 288
#define R_AARCH64_MOVW_PREL_G1 289
#define R_AARCH64_MOVW_PREL_G1_NC 290
#define R_AARCH64_MOVW_PREL_G2 291
#define R_AARCH64_MOVW_PREL_G2_NC 292
#define R_AARCH64_MOVW_PREL_G3 293

#define R_AARCH64_RELATIVE 1027

#define CORE_DUMP_USE_REGSET
#define elf_check_arch(x) ((x)->e_machine == EM_AARCH64)
#define ELF_CLASS ELFCLASS64
#define ELF_DATA ELFDATA2LSB
#define ELF_ARCH EM_AARCH64
#define ELF_PLATFORM "aarch64"
#define ELF_EXEC_PAGESIZE 4096
#define ELF_ET_DYN_BASE (TASK_SIZE / 3 * 2)
extern long elf_aux_hwcap;
#define ELF_HWCAP (elf_aux_hwcap)
#define SET_PERSONALITY(ex) \
	do {                \
	} while (0)
#define ELF_PLAT_INIT(r, addr) memset((r)->regs.gp, 0, sizeof((r)->regs.gp))
#define ELF_NGREG 34
typedef unsigned long elf_greg_t;
typedef elf_greg_t elf_gregset_t[ELF_NGREG];
typedef struct user_fpsimd_state elf_fpregset_t;
#define ELF_CORE_COPY_REGS(p, r) memcpy(p, (r)->regs.gp, 34 * sizeof(long));
#define FIXADDR_USER_START 0
#define FIXADDR_USER_END 0
#define ARCH_HAS_SETUP_ADDITIONAL_PAGES 1
struct linux_binprm;
int arch_setup_additional_pages(struct linux_binprm *bprm, int uses_interp);
extern unsigned long um_vdso_addr;
#define ARCH_DLINFO NEW_AUX_ENT(AT_SYSINFO_EHDR, um_vdso_addr)
#endif
