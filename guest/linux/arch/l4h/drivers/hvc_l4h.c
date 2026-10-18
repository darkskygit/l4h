// SPDX-License-Identifier: GPL-2.0
/*
 * hvc0 console of the l4h guest: output through the console hypercall,
 * input from the ring in the L4H_DEV_CONSOLE page (polled by hvc).
 */
#include <linux/init.h>
#include <linux/console.h>
#include <linux/err.h>
#include <linux/types.h>
#include <asm/barrier.h>
#include <linux/interrupt.h>
#include <linux/reboot.h>
#include <irq_kern.h>
#include <hvc_console.h>
#include <l4h_sys.h>

static ssize_t l4h_hvc_put_chars(uint32_t vt, const u8 *buf, size_t count)
{
	l4h_console_write((const char *)buf, count);
	return count;
}

static struct l4h_console_page *l4h_console_in;

static ssize_t l4h_hvc_get_chars(uint32_t vt, u8 *buf, size_t count)
{
	struct l4h_console_page *p = l4h_console_in;
	u32 head, tail;
	size_t n = 0;

	if (!p)
		return 0;
	tail = p->in_tail;
	head = smp_load_acquire(&p->in_head);
	while (tail != head && n < count)
		buf[n++] = p->in[tail++ % L4H_CONSOLE_IN_SIZE];
	smp_store_release(&p->in_tail, tail);
	return n;
}

static unsigned int l4h_console_event;

static void l4h_hvc_find_input(void)
{
	const struct l4h_bootinfo *bi = l4h_bootinfo;
	unsigned int i;

	for (i = 0; i < bi->nr_devices && i < L4H_MAX_DEVICES; i++)
		if (bi->devices[i].type == L4H_DEV_CONSOLE) {
			l4h_console_in = (struct l4h_console_page *)bi->devices[i].page_va;
			l4h_console_event = bi->devices[i].event;
		}
}

/* The host asks for a clean shutdown or reboot (l4hctl shutdown). */
static irqreturn_t l4h_power_interrupt(int irq, void *data)
{
	u32 req = xchg(&l4h_console_in->power_request, 0);

	if (req == L4H_POWER_OFF + 1) {
		pr_info("l4h: shutdown requested by the host\n");
		orderly_poweroff(true);
	} else if (req == L4H_POWER_REBOOT + 1) {
		pr_info("l4h: reboot requested by the host\n");
		orderly_reboot();
	}
	return req ? IRQ_HANDLED : IRQ_NONE;
}

static const struct hv_ops l4h_hvc_ops = {
	.get_chars = l4h_hvc_get_chars,
	.put_chars = l4h_hvc_put_chars,
};

static int __init l4h_hvc_console_init(void)
{
	l4h_hvc_find_input();
	hvc_instantiate(0, 0, &l4h_hvc_ops);
	return 0;
}
console_initcall(l4h_hvc_console_init);

static int __init l4h_hvc_init(void)
{
	struct hvc_struct *hp = hvc_alloc(0, 0, &l4h_hvc_ops, 256);

	if (l4h_console_in) {
		int irq = l4h_event_irq(l4h_console_event);

		if (irq < 0 || request_irq(irq, l4h_power_interrupt, 0, "l4h-power", NULL))
			pr_warn("l4h: no power request interrupt\n");
	}
	return PTR_ERR_OR_ZERO(hp);
}
device_initcall(l4h_hvc_init);
