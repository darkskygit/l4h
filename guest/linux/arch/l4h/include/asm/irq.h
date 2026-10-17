/* SPDX-License-Identifier: GPL-2.0 */
#ifndef __L4H_IRQ_H
#define __L4H_IRQ_H

/*
 * Linux IRQs of the l4h guest: fixed per-CPU interrupts for the timer and
 * IPIs, then a linear domain for the l4h event lines (event page bits).
 */
#define TIMER_IRQ		0
#define IPI_IRQ			1
#define L4H_FIRST_EVENT_IRQ	16
#define NR_IRQS			(L4H_FIRST_EVENT_IRQ + 512)

#include <asm-generic/irq.h>
#endif
