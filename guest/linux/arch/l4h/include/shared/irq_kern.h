/* SPDX-License-Identifier: GPL-2.0 */
#ifndef __IRQ_KERN_H__
#define __IRQ_KERN_H__

#include <linux/interrupt.h>
#include <asm/ptrace.h>

/* Linux IRQ number of l4h event line `event` (from struct l4h_dev_desc). */
int l4h_event_irq(unsigned int event);

#endif
