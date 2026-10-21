// SPDX-License-Identifier: GPL-2.0
/*
 * Attestation by the l4h host (l4h-trustd, DESIGN.md §9.2), through the
 * attestation device (L4H_DEV_ATTEST, struct l4h_attest_page) on hosts
 * with a TPM:
 *
 * - configfs-tsm reports (/sys/kernel/config/tsm/report, provider "l4h"):
 *   inblob is the report data (up to 64 bytes, zero padded), outblob the
 *   report (COSE_Sign1), auxblob the host evidence (CBOR).
 * - /dev/l4h-attest: ioctl(L4H_ATTEST_IOC) makes any request,
 *   ActivateCredential included, for tools such as l4hctl.
 *
 * Requests are asynchronous: the caller sleeps until the host's answer
 * raises the device's event, and the vCPU runs other tasks meanwhile.
 */
#include <linux/fs.h>
#include <linux/init.h>
#include <linux/interrupt.h>
#include <linux/miscdevice.h>
#include <linux/mm.h>
#include <linux/mutex.h>
#include <linux/slab.h>
#include <linux/tsm.h>
#include <linux/uaccess.h>
#include <linux/wait.h>
#include <irq_kern.h>
#include <l4h_sys.h>

struct l4h_attest_ioc {
	__u32 op;		/* L4H_ATTEST_* */
	__u32 blob_len;		/* ACTIVATE: credential blob bytes */
	__u32 secret_len;	/* ACTIVATE: encrypted secret bytes */
	__u32 out_len;		/* in: room at out; out: the result's length */
	__u64 data;		/* REPORT: 64 bytes; ACTIVATE: blob, then secret */
	__u64 out;
};

#define L4H_ATTEST_IOC _IOWR('L', 2, struct l4h_attest_ioc)
/* ActivateCredential creates the EK on first use: seconds on a slow TPM. */
#define ATTEST_TIMEOUT (150 * HZ)

static struct l4h_attest_page *page;
static unsigned long kick_cap;
static DECLARE_WAIT_QUEUE_HEAD(answered);
static DEFINE_MUTEX(attest_lock);
static u32 seq;

static irqreturn_t attest_interrupt(int irq, void *data)
{
	wake_up(&answered);
	return IRQ_HANDLED;
}

/*
 * Run request `op` with `in_len` bytes of `in` (`blob_len` of them the
 * credential blob) and collect its result, which may take several
 * replies.  Returns a kvmalloc'ed buffer in *out.
 */
static int attest_call(u32 op, const u8 *in, size_t in_len, size_t blob_len, u8 **out, size_t *out_len)
{
	size_t total = 0, got = 0;
	u8 *buf = NULL;
	int ret = 0;

	if (in_len > L4H_ATTEST_PAGE_DATA)
		return -EINVAL;
	mutex_lock(&attest_lock);
	do {
		u32 s = ++seq, n;

		if (READ_ONCE(page->state) == L4H_ATTEST_FAILED) {
			ret = -EIO;
			break;
		}
		WRITE_ONCE(page->op, op);
		WRITE_ONCE(page->offset, got);
		WRITE_ONCE(page->in_len, in_len);
		WRITE_ONCE(page->blob_len, blob_len);
		if (in_len)
			memcpy(page->data, in, in_len);
		smp_wmb();
		WRITE_ONCE(page->seq, s);
		l4h_signal(kick_cap);
		if (!wait_event_timeout(answered, READ_ONCE(page->done) == s, ATTEST_TIMEOUT)) {
			ret = -ETIMEDOUT;
			break;
		}
		smp_rmb();
		if (READ_ONCE(page->status)) {
			ret = READ_ONCE(page->status);
			break;
		}
		n = READ_ONCE(page->out_len);
		if (!buf) {
			total = READ_ONCE(page->total);
			buf = kvmalloc(max_t(size_t, total, 1), GFP_KERNEL);
			if (!buf) {
				ret = -ENOMEM;
				break;
			}
		}
		if (n > L4H_ATTEST_PAGE_DATA || READ_ONCE(page->total) != total || got + n > total ||
		    (!n && got < total)) {
			ret = -EIO;
			break;
		}
		memcpy(buf + got, page->data, n);
		got += n;
	} while (got < total);
	mutex_unlock(&attest_lock);
	if (ret) {
		kvfree(buf);
		return ret;
	}
	*out = buf;
	*out_len = total;
	return 0;
}

static int attest_report(const u8 *data, size_t len, u8 **out, size_t *out_len)
{
	u8 d[L4H_ATTEST_DATA_SIZE] = {};

	memcpy(d, data, min_t(size_t, len, sizeof(d)));
	return attest_call(L4H_ATTEST_REPORT, d, sizeof(d), 0, out, out_len);
}

static int attest_evidence(u8 **out, size_t *out_len)
{
	return attest_call(L4H_ATTEST_EVIDENCE, NULL, 0, 0, out, out_len);
}

#if IS_ENABLED(CONFIG_TSM_REPORTS)
static int l4h_report_new(struct tsm_report *report, void *data)
{
	struct tsm_report_desc *desc = &report->desc;
	u8 *out, *aux;
	size_t len, aux_len;
	int ret;

	ret = attest_report(desc->inblob, desc->inblob_len, &out, &len);
	if (ret)
		return ret;
	ret = attest_evidence(&aux, &aux_len);
	if (ret) {
		kvfree(out);
		return ret;
	}
	report->outblob = out;
	report->outblob_len = len;
	report->auxblob = aux;
	report->auxblob_len = aux_len;
	return 0;
}

static bool l4h_report_attr_visible(int n)
{
	return n == TSM_REPORT_GENERATION || n == TSM_REPORT_PROVIDER;
}

static bool l4h_report_bin_attr_visible(int n)
{
	return n == TSM_REPORT_INBLOB || n == TSM_REPORT_OUTBLOB || n == TSM_REPORT_AUXBLOB;
}

static const struct tsm_report_ops l4h_tsm_ops = {
	.name = "l4h",
	.report_new = l4h_report_new,
	.report_attr_visible = l4h_report_attr_visible,
	.report_bin_attr_visible = l4h_report_bin_attr_visible,
};
#endif

static long attest_ioctl(struct file *f, unsigned int cmd, unsigned long arg)
{
	struct l4h_attest_ioc c;
	u8 *in = NULL, *out = NULL;
	size_t in_len, len;
	long ret;

	if (cmd != L4H_ATTEST_IOC)
		return -ENOTTY;
	if (copy_from_user(&c, (void __user *)arg, sizeof(c)))
		return -EFAULT;
	switch (c.op) {
	case L4H_ATTEST_REPORT:
		in_len = L4H_ATTEST_DATA_SIZE;
		break;
	case L4H_ATTEST_ACTIVATE:
		in_len = (size_t)c.blob_len + c.secret_len;
		if (in_len > L4H_ATTEST_PAGE_DATA)
			return -EINVAL;
		break;
	case L4H_ATTEST_EVIDENCE:
		in_len = 0;
		break;
	default:
		return -EINVAL;
	}
	if (in_len) {
		in = memdup_user(u64_to_user_ptr(c.data), in_len);
		if (IS_ERR(in))
			return PTR_ERR(in);
	}
	if (c.op == L4H_ATTEST_REPORT)
		ret = attest_report(in, in_len, &out, &len);
	else if (c.op == L4H_ATTEST_EVIDENCE)
		ret = attest_evidence(&out, &len);
	else
		ret = attest_call(L4H_ATTEST_ACTIVATE, in, in_len, c.blob_len, &out, &len);
	kfree(in);
	if (ret)
		return ret;
	if (len > c.out_len)
		ret = -ENOSPC;
	else if (copy_to_user(u64_to_user_ptr(c.out), out, len))
		ret = -EFAULT;
	c.out_len = len;
	if (ret != -EFAULT && copy_to_user((void __user *)arg, &c, sizeof(c)))
		ret = -EFAULT;
	kvfree(out);
	return ret;
}

static const struct file_operations attest_fops = {
	.owner = THIS_MODULE,
	.unlocked_ioctl = attest_ioctl,
};

static struct miscdevice attest_dev = {
	.minor = MISC_DYNAMIC_MINOR,
	.name = "l4h-attest",
	.fops = &attest_fops,
	.mode = 0600,
};

static int __init l4h_attest_init(void)
{
	const struct l4h_bootinfo *bi = l4h_bootinfo;
	unsigned int i;
	int irq, ret;

	for (i = 0; i < bi->nr_devices && i < L4H_MAX_DEVICES; i++) {
		if (bi->devices[i].type == L4H_DEV_ATTEST)
			break;
	}
	if (i == bi->nr_devices || i == L4H_MAX_DEVICES)
		return 0;
	page = (struct l4h_attest_page *)bi->devices[i].page_va;
	kick_cap = L4H_CPTR2(L4H_SLOT_DEV_CNODE, bi->devices[i].kick_slot);
	irq = l4h_event_irq(bi->devices[i].event);
	if (irq < 0)
		return irq;
	ret = request_irq(irq, attest_interrupt, 0, "l4h-attest", NULL);
	if (ret)
		return ret;
	seq = READ_ONCE(page->seq);
	ret = misc_register(&attest_dev);
	if (ret)
		return ret;
#if IS_ENABLED(CONFIG_TSM_REPORTS)
	ret = tsm_report_register(&l4h_tsm_ops, NULL);
	if (ret)
		pr_warn("l4h-attest: no configfs-tsm reports: %d\n", ret);
#endif
	pr_info("l4h-attest: host attestation available\n");
	return 0;
}
device_initcall(l4h_attest_init);
