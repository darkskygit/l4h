/* SPDX-License-Identifier: GPL-2.0 */
#include <asm/unistd.h>
#define SECCOMP_ARCH_NATIVE AUDIT_ARCH_AARCH64
#define SECCOMP_ARCH_NATIVE_NR __NR_syscalls
#define SECCOMP_ARCH_NATIVE_NAME "aarch64"
#include <asm-generic/seccomp.h>
