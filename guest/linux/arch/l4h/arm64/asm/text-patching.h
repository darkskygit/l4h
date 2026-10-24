/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * Code patching for the BPF JIT (arch/arm64/net), in arm64/bpf_jit.c: the
 * subset of arch/arm64/include/asm/text-patching.h that it uses.
 */
#ifndef L4H_ARM64_TEXT_PATCHING_H
#define L4H_ARM64_TEXT_PATCHING_H

#include <linux/types.h>

int aarch64_insn_read(void *addr, u32 *insnp);
int aarch64_insn_write(void *addr, u32 insn);
void *aarch64_insn_set(void *dst, u32 insn, size_t len);
void *aarch64_insn_copy(void *dst, void *src, size_t len);
int aarch64_insn_patch_text_nosync(void *addr, u32 insn);

#endif
