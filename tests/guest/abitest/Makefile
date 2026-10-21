# SPDX-License-Identifier: GPL-2.0
# The l4h ABI test (main.c), a module built against the guest kernel's tree.
obj-m += l4h_abitest.o
l4h_abitest-y := main.o sel4.o user.o

SEL4_OBJS := sel4.o
include $(srctree)/arch/l4h/scripts/Makefile.rules
