// SPDX-License-Identifier: GPL-2.0
/*
 * hvc0 console of the l4h guest, on the console hypercall.
 */
#include <linux/init.h>
#include <linux/console.h>
#include <linux/err.h>
#include <linux/types.h>
#include <hvc_console.h>
#include <l4h_sys.h>

static ssize_t l4h_hvc_put_chars(uint32_t vt, const u8 *buf, size_t count)
{
	l4h_console_write((const char *)buf, count);
	return count;
}

static ssize_t l4h_hvc_get_chars(uint32_t vt, u8 *buf, size_t count)
{
	return 0;
}

static const struct hv_ops l4h_hvc_ops = {
	.get_chars = l4h_hvc_get_chars,
	.put_chars = l4h_hvc_put_chars,
};

static int __init l4h_hvc_console_init(void)
{
	hvc_instantiate(0, 0, &l4h_hvc_ops);
	return 0;
}
console_initcall(l4h_hvc_console_init);

static int __init l4h_hvc_init(void)
{
	struct hvc_struct *hp = hvc_alloc(0, 0, &l4h_hvc_ops, 256);

	return PTR_ERR_OR_ZERO(hp);
}
device_initcall(l4h_hvc_init);
