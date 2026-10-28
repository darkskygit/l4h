/* SPDX-License-Identifier: GPL-2.0-only */
/* The actual guest writer, with only its vCPU and notification mocked. */
#include <l4h/abi.h>

struct l4h_vcpu {
	struct l4h_event_page *evt;
	int in_console;
};
static struct l4h_vcpu vcpu;
static unsigned int kicks;
static struct l4h_vcpu *l4h_this_vcpu(void) { return &vcpu; }
static void seL4_Signal(unsigned long cap) { (void)cap; kicks++; }

#include "console-writer.inc"

void console_test_select(struct l4h_event_page *page) { vcpu.evt = page; }
unsigned int console_test_kicks(void) { return kicks; }
void console_test_reset(void) { kicks = console_seq = vcpu.in_console = 0; }
