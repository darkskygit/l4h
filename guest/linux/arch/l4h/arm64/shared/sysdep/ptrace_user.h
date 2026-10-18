/* SPDX-License-Identifier: GPL-2.0 */
#include <generated/user_constants.h>
#define PT_SYSCALL_NR(r) ((r)[HOST_ORIG_AX])
#define PT_SYSCALL_NR_OFFSET (HOST_ORIG_AX * sizeof(long))
#define PT_SYSCALL_RET_OFFSET 0
#define REGS_IP_INDEX HOST_IP
#define REGS_SP_INDEX HOST_SP
