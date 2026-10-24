/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * arch/arm64/net/bpf_jit_comp.c and arch/arm64/lib/insn.c (the BPF JIT)
 * include this for the BRK encodings, which are in insn-def.h.  The
 * upstream header pulls in arm64 kernel internals that l4h does not have.
 */
#ifndef L4H_ARM64_DEBUG_MONITORS_H
#define L4H_ARM64_DEBUG_MONITORS_H
#include <asm/insn-def.h>
#endif
