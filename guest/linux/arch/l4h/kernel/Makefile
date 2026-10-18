# SPDX-License-Identifier: GPL-2.0
#
# Copyright (C) 2002 - 2007 Jeff Dike (jdike@{addtoit,linux,intel}.com)
#

KCOV_INSTRUMENT := n

CPPFLAGS_vmlinux.lds := -DSTART=$(LDS_START) -DELF_ARCH=$(LDS_ELF_ARCH) \
			-DELF_FORMAT=$(LDS_ELF_FORMAT)
always-$(KBUILD_BUILTIN) := vmlinux.lds

obj-y = exec.o irq.o mem.o physmem.o process.o ptrace.o reboot.o \
	signal.o sysrq.o time.o tlb.o trap.o um_arch.o kmsg_dump.o \
	caps.o vspace.o uctx.o skas/

obj-$(CONFIG_BLK_DEV_INITRD) += initrd.o
obj-$(CONFIG_EARLY_PRINTK) += early_printk.o
obj-$(CONFIG_STACKTRACE) += stacktrace.o
obj-$(CONFIG_SMP) += smp.o

obj-$(CONFIG_UML_X86) += capflags.o

targets := capflags.c

quiet_cmd_mkcapflags = MKCAP   $@
      cmd_mkcapflags = $(CONFIG_SHELL) $(srctree)/arch/x86/kernel/cpu/mkcapflags.sh $@ $^

cpufeature = $(srctree)/arch/x86/include/asm/cpufeatures.h
vmxfeature = $(srctree)/arch/x86/include/asm/vmxfeatures.h

$(obj)/capflags.c: $(cpufeature) $(vmxfeature) $(srctree)/arch/x86/kernel/cpu/mkcapflags.sh FORCE
	$(call if_changed,mkcapflags)
