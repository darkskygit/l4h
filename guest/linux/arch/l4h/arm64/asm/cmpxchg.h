/* SPDX-License-Identifier: GPL-2.0 */
#ifndef L4H_ARM64_CMPXCHG_H
#define L4H_ARM64_CMPXCHG_H
/* Export relaxed primitives. Linux's atomic fallback adds the full/acquire/
 * release barriers required by LKMM, which is stronger than C11 seq_cst. */
#define arch_xchg_relaxed(p, v) __atomic_exchange_n((p), (v), __ATOMIC_RELAXED)
#define arch_cmpxchg_relaxed(p, o, n)                            \
	({                                                       \
		__typeof__(*(p)) __old = (o);                    \
		__atomic_compare_exchange_n((p), &__old, (n), 0, \
					    __ATOMIC_RELAXED,    \
					    __ATOMIC_RELAXED);   \
		__old;                                           \
	})
#define arch_cmpxchg64_relaxed arch_cmpxchg_relaxed
#define arch_cmpxchg_local arch_cmpxchg_relaxed
#define arch_cmpxchg64_local arch_cmpxchg_relaxed
#endif
