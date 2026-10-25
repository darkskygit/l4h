# SPDX-License-Identifier: GPL-2.0
#
# Copyright (C) 2002 - 2007 Jeff Dike (jdike@{addtoit,linux.intel}.com)
#

obj-y = bugs_64.o delay.o fpu.o ptrace.o ptrace_64.o setjmp_64.o signal.o \
	sys_call_table_64.o sysrq_64.o tls_64.o mem_64.o syscalls_64.o \
	lib/ vdso/
obj-$(CONFIG_BPF_JIT) += bpf_jit.o
obj-$(CONFIG_KEMU) += kemu.o

quiet_cmd_kemu_syscalls = GEN     $@
      cmd_kemu_syscalls = $(PYTHON3) $(src)/kemu-syscalls.py $< > $@

$(obj)/kemu-syscalls.h: $(srctree)/arch/x86/entry/syscalls/syscall_64.tbl $(src)/kemu-syscalls.py FORCE
	$(call if_changed,kemu_syscalls)

targets += kemu-syscalls.h
$(obj)/kemu.o: $(obj)/kemu-syscalls.h

USER_OBJS :=

$(obj)/user-offsets.s: c_flags = -Wp,-MD,$(depfile) $(USER_CFLAGS) \
	-Iarch/x86/include/generated
targets += user-offsets.s

include/generated/user_constants.h: $(obj)/user-offsets.s FORCE
	$(call filechk,offsets,__USER_CONSTANT_H__)

include $(srctree)/arch/l4h/scripts/Makefile.rules
