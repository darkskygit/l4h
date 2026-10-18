/* SPDX-License-Identifier: GPL-2.0 */
#ifndef L4H_ARM64_PTRACE_H
#define L4H_ARM64_PTRACE_H
#include <generated/user_constants.h>
#include <sysdep/faultinfo.h>
#define MAX_REG_OFFSET UM_FRAME_SIZE
#define MAX_REG_NR (UM_FRAME_SIZE / sizeof(unsigned long))
#define REGS_IP(r) ((r)[HOST_IP])
#define REGS_SP(r) ((r)[HOST_SP])
#define REGS_AX(r) ((r)[0])
#define REGS_BP(r) ((r)[29])
#define UPT_IP(r) REGS_IP((r)->gp)
#define UPT_SP(r) REGS_SP((r)->gp)
#define UPT_AX(r) REGS_AX((r)->gp)
#define UPT_BP(r) REGS_BP((r)->gp)
/* x0 is also the return register, initialized to -ENOSYS before dispatch. */
#define UPT_SYSCALL_ARG1(r) ((r)->gp[36])
#define UPT_SYSCALL_ARG2(r) ((r)->gp[1])
#define UPT_SYSCALL_ARG3(r) ((r)->gp[2])
#define UPT_SYSCALL_ARG4(r) ((r)->gp[3])
#define UPT_SYSCALL_ARG5(r) ((r)->gp[4])
#define UPT_SYSCALL_ARG6(r) ((r)->gp[5])
extern unsigned long host_fp_size;
struct uml_pt_regs {
	unsigned long gp[MAX_REG_NR];
	struct faultinfo faultinfo;
	long syscall;
	int is_user;
	unsigned long fp[];
};
#define EMPTY_UML_PT_REGS \
	{                 \
	}
#define UPT_SYSCALL_NR(r) ((r)->syscall)
#define UPT_FAULTINFO(r) (&(r)->faultinfo)
#define UPT_IS_USER(r) ((r)->is_user)
#endif
