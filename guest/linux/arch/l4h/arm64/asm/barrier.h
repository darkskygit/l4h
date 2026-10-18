/* SPDX-License-Identifier: GPL-2.0 */
#ifndef L4H_ARM64_BARRIER_H
#define L4H_ARM64_BARRIER_H
#define mb() asm volatile("dmb ish" ::: "memory")
#define rmb() asm volatile("dmb ishld" ::: "memory")
#define wmb() asm volatile("dmb ishst" ::: "memory")
#define dma_rmb() asm volatile("dmb oshld" ::: "memory")
#define dma_wmb() asm volatile("dmb oshst" ::: "memory")
#include <asm-generic/barrier.h>
#endif
