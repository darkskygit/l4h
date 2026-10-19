// SPDX-License-Identifier: GPL-2.0
/*
 * /dev/l4hctl: dom0's access to the l4h control API (DESIGN.md §9.1).
 *
 * ioctl(L4HCTL_IOC_CALL) passes a struct l4h_ctl_req to l4h-root and
 * returns the struct l4h_ctl_resp. For VM_LOAD and BLK_WRITE, source bytes
 * come from a user buffer: they are staged in the device's data area, a
 * host-owned region (l4h-root never reads guest RAM). VM_LOAD uses offset
 * zero; BLK_WRITE uses the staged bytes only when its zero flag is clear.
 */
#include <linux/fs.h>
#include <linux/init.h>
#include <linux/miscdevice.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/slab.h>
#include <linux/uaccess.h>
#include <linux/gfp.h>
#include <asm/page.h>
#include <l4h_sys.h>

struct l4hctl_call {
	__u64 req;      /* struct l4h_ctl_req * */
	__u64 resp;     /* struct l4h_ctl_resp * */
	__u64 buf;      /* VM_LOAD / BLK_WRITE: source bytes */
	__u64 len;
};

#define L4HCTL_IOC_CALL _IOWR('L', 1, struct l4hctl_call)

static void *ctl_page;
static void *stage;
static unsigned long stage_size;
static DEFINE_MUTEX(ctl_lock);

static long l4hctl_ioctl(struct file *f, unsigned int cmd, unsigned long arg)
{
	struct l4h_ctl_req *req = ctl_page;
	struct l4h_ctl_resp *resp = ctl_page + L4H_CTL_RESP_OFF;
	struct l4hctl_call call;
	unsigned long mr = 0;
	long ret = 0;

	if (cmd != L4HCTL_IOC_CALL)
		return -ENOTTY;
	if (copy_from_user(&call, (void __user *)arg, sizeof(call)))
		return -EFAULT;

	mutex_lock(&ctl_lock);
	if (copy_from_user(req, u64_to_user_ptr(call.req), sizeof(*req))) {
		ret = -EFAULT;
		goto out;
	}
	if (req->op == L4H_CTL_VM_LOAD) {
		if (call.len > stage_size ||
		    copy_from_user(stage, u64_to_user_ptr(call.buf), call.len)) {
			ret = -EFAULT;
			goto out;
		}
		req->args[2] = 0;
		req->args[3] = call.len;
	} else if (req->op == L4H_CTL_BLK_WRITE && !req->args[3]) {
		if (call.len > stage_size || call.len != req->args[2]) {
			ret = -EINVAL;
			goto out;
		}
		if (copy_from_user(stage, u64_to_user_ptr(call.buf), call.len)) {
			ret = -EFAULT;
			goto out;
		}
	}
	memset(resp, 0, sizeof(*resp));
	l4h_hypercall(L4H_HC_CTL, &mr, 0, 1);
	if (copy_to_user(u64_to_user_ptr(call.resp), resp, sizeof(*resp)))
		ret = -EFAULT;
out:
	mutex_unlock(&ctl_lock);
	return ret;
}

static const struct file_operations l4hctl_fops = {
	.owner = THIS_MODULE,
	.unlocked_ioctl = l4hctl_ioctl,
};

static struct miscdevice l4hctl_dev = {
	.minor = MISC_DYNAMIC_MINOR,
	.name = "l4hctl",
	.fops = &l4hctl_fops,
	.mode = 0600,
};

static int __init l4hctl_init(void)
{
	const struct l4h_bootinfo *bi = l4h_bootinfo;
	unsigned int i;

	for (i = 0; i < bi->nr_devices && i < L4H_MAX_DEVICES; i++) {
		if (bi->devices[i].type == L4H_DEV_CTL) {
			ctl_page = (void *)bi->devices[i].page_va;
			stage = (void *)bi->devices[i].data_va;
			stage_size = min_t(unsigned long, bi->devices[i].data_size, L4H_CTL_LOAD_MAX);
		}
	}
	if (!ctl_page)
		return 0;
	if (!stage || !stage_size)
		return -ENODEV;

	pr_info("l4hctl: control device present\n");
	return misc_register(&l4hctl_dev);
}
device_initcall(l4hctl_init);
