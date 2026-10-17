# SPDX-License-Identifier: GPL-2.0
#
# Copyright (C) 2002 - 2007 Jeff Dike (jdike@{addtoit,linux.intel}.com)
#

obj-y = bugs_64.o delay.o ptrace.o ptrace_64.o setjmp_64.o signal.o \
	sys_call_table_64.o sysrq_64.o tls_64.o mem_64.o syscalls_64.o \
	lib/ vdso/

USER_OBJS :=

$(obj)/user-offsets.s: c_flags = -Wp,-MD,$(depfile) $(USER_CFLAGS) \
	-Iarch/x86/include/generated
targets += user-offsets.s

include/generated/user_constants.h: $(obj)/user-offsets.s FORCE
	$(call filechk,offsets,__USER_CONSTANT_H__)

include $(srctree)/arch/l4h/scripts/Makefile.rules
