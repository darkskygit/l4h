// SPDX-License-Identifier: GPL-2.0
#include <linux/init.h>
#include <linux/module.h>
#include <linux/slab.h>

static const unsigned long expected[] = { 0x12345678, 0xabcdef01 };

static int __init modtest_init(void)
{
	unsigned long *p = kmemdup(expected, sizeof(expected), GFP_KERNEL);

	if (!p)
		return -ENOMEM;
	if (p[0] != expected[0] || p[1] != expected[1]) {
		kfree(p);
		return -EINVAL;
	}
	kfree(p);
	pr_info("MODTEST: loaded\n");
	return 0;
}

static void __exit modtest_exit(void)
{
	pr_info("MODTEST: unloaded\n");
}

module_init(modtest_init);
module_exit(modtest_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("l4h native module relocation and lifecycle test");
