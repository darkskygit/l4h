/* SPDX-License-Identifier: GPL-2.0 */
#ifndef L4H_ARM64_ATOMIC_H
#define L4H_ARM64_ATOMIC_H
#include <linux/types.h>
#include <asm/cmpxchg.h>
/* Compile with -mno-outline-atomics: all operations use unprivileged LL/SC.
 * Linux supplies the ordering variants around these relaxed primitives. */
#define ATOMIC_INIT(i) { (i) }
#define arch_atomic_read(v) __atomic_load_n(&(v)->counter, __ATOMIC_RELAXED)
#define arch_atomic_set(v, i) \
	__atomic_store_n(&(v)->counter, (i), __ATOMIC_RELAXED)
#define arch_atomic_xchg_relaxed(v, n) arch_xchg_relaxed(&(v)->counter, n)
#define arch_atomic_cmpxchg_relaxed(v, o, n) \
	arch_cmpxchg_relaxed(&(v)->counter, o, n)
static inline void arch_atomic_add(int i, atomic_t *v)
{
	__atomic_fetch_add(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic_add arch_atomic_add
static inline int arch_atomic_fetch_add_relaxed(int i, atomic_t *v)
{
	return __atomic_fetch_add(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic_fetch_add_relaxed arch_atomic_fetch_add_relaxed
static inline int arch_atomic_add_return_relaxed(int i, atomic_t *v)
{
	return __atomic_add_fetch(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic_add_return_relaxed arch_atomic_add_return_relaxed
static inline void arch_atomic_sub(int i, atomic_t *v)
{
	__atomic_fetch_sub(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic_sub arch_atomic_sub
static inline int arch_atomic_fetch_sub_relaxed(int i, atomic_t *v)
{
	return __atomic_fetch_sub(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic_fetch_sub_relaxed arch_atomic_fetch_sub_relaxed
static inline int arch_atomic_sub_return_relaxed(int i, atomic_t *v)
{
	return __atomic_sub_fetch(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic_sub_return_relaxed arch_atomic_sub_return_relaxed
static inline void arch_atomic_and(int i, atomic_t *v)
{
	__atomic_fetch_and(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic_and arch_atomic_and
static inline int arch_atomic_fetch_and_relaxed(int i, atomic_t *v)
{
	return __atomic_fetch_and(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic_fetch_and_relaxed arch_atomic_fetch_and_relaxed
static inline void arch_atomic_or(int i, atomic_t *v)
{
	__atomic_fetch_or(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic_or arch_atomic_or
static inline int arch_atomic_fetch_or_relaxed(int i, atomic_t *v)
{
	return __atomic_fetch_or(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic_fetch_or_relaxed arch_atomic_fetch_or_relaxed
static inline void arch_atomic_xor(int i, atomic_t *v)
{
	__atomic_fetch_xor(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic_xor arch_atomic_xor
static inline int arch_atomic_fetch_xor_relaxed(int i, atomic_t *v)
{
	return __atomic_fetch_xor(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic_fetch_xor_relaxed arch_atomic_fetch_xor_relaxed
#define ATOMIC64_INIT(i) { (i) }
#define arch_atomic64_read(v) __atomic_load_n(&(v)->counter, __ATOMIC_RELAXED)
#define arch_atomic64_set(v, i) \
	__atomic_store_n(&(v)->counter, (i), __ATOMIC_RELAXED)
#define arch_atomic64_xchg_relaxed(v, n) arch_xchg_relaxed(&(v)->counter, n)
#define arch_atomic64_cmpxchg_relaxed(v, o, n) \
	arch_cmpxchg_relaxed(&(v)->counter, o, n)
static inline void arch_atomic64_add(s64 i, atomic64_t *v)
{
	__atomic_fetch_add(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic64_add arch_atomic64_add
static inline s64 arch_atomic64_fetch_add_relaxed(s64 i, atomic64_t *v)
{
	return __atomic_fetch_add(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic64_fetch_add_relaxed arch_atomic64_fetch_add_relaxed
static inline s64 arch_atomic64_add_return_relaxed(s64 i, atomic64_t *v)
{
	return __atomic_add_fetch(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic64_add_return_relaxed arch_atomic64_add_return_relaxed
static inline void arch_atomic64_sub(s64 i, atomic64_t *v)
{
	__atomic_fetch_sub(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic64_sub arch_atomic64_sub
static inline s64 arch_atomic64_fetch_sub_relaxed(s64 i, atomic64_t *v)
{
	return __atomic_fetch_sub(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic64_fetch_sub_relaxed arch_atomic64_fetch_sub_relaxed
static inline s64 arch_atomic64_sub_return_relaxed(s64 i, atomic64_t *v)
{
	return __atomic_sub_fetch(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic64_sub_return_relaxed arch_atomic64_sub_return_relaxed
static inline void arch_atomic64_and(s64 i, atomic64_t *v)
{
	__atomic_fetch_and(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic64_and arch_atomic64_and
static inline s64 arch_atomic64_fetch_and_relaxed(s64 i, atomic64_t *v)
{
	return __atomic_fetch_and(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic64_fetch_and_relaxed arch_atomic64_fetch_and_relaxed
static inline void arch_atomic64_or(s64 i, atomic64_t *v)
{
	__atomic_fetch_or(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic64_or arch_atomic64_or
static inline s64 arch_atomic64_fetch_or_relaxed(s64 i, atomic64_t *v)
{
	return __atomic_fetch_or(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic64_fetch_or_relaxed arch_atomic64_fetch_or_relaxed
static inline void arch_atomic64_xor(s64 i, atomic64_t *v)
{
	__atomic_fetch_xor(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic64_xor arch_atomic64_xor
static inline s64 arch_atomic64_fetch_xor_relaxed(s64 i, atomic64_t *v)
{
	return __atomic_fetch_xor(&v->counter, i, __ATOMIC_RELAXED);
}
#define arch_atomic64_fetch_xor_relaxed arch_atomic64_fetch_xor_relaxed
#endif
