// SPDX-License-Identifier: GPL-2.0
/*
 * Attestation by the l4h host (l4h-trustd, DESIGN.md §9.2), when the boot
 * info has L4H_BOOT_ATTEST:
 *
 * - configfs-tsm reports (/sys/kernel/config/tsm/report, provider "l4h"):
 *   inblob is the report data (up to 64 bytes, zero padded), outblob the
 *   report (COSE_Sign1), auxblob the host evidence (CBOR).
 * - /dev/l4h-attest: ioctl(L4H_ATTEST_IOC) runs any attestation call,
 *   ActivateCredential included, for tools such as l4hctl.
 */
#include <linux/fs.h>
#include <linux/init.h>
#include <linux/irqflags.h>
#include <linux/miscdevice.h>
#include <linux/mm.h>
#include <linux/mutex.h>
#include <linux/slab.h>
#include <linux/tsm.h>
#include <linux/uaccess.h>
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
#define MAX_MRS 120

static DEFINE_MUTEX(attest_lock);

/*
 * Run attestation call `label` with argument registers in[0, n_in) and
 * collect its result, which may take several replies: the offset is
 * argument register off_mr (-1 for a call with a single reply).  Returns
 * a kvmalloc'ed buffer in *out.
 */
static int attest_call(unsigned long label, const unsigned long *in, int n_in, int off_mr,
		       u8 **out, size_t *out_len)
{
	unsigned long *mrs;
	size_t total = 0, got = 0;
	u8 *buf = NULL;
	int ret = 0;

	mrs = kmalloc_array(MAX_MRS, sizeof(*mrs), GFP_KERNEL);
	if (!mrs)
		return -ENOMEM;
	mutex_lock(&attest_lock);
	do {
		unsigned long flags, chunk;
		int n;

		memcpy(mrs, in, n_in * sizeof(*mrs));
		if (off_mr >= 0)
			mrs[off_mr] = got;
		/* The message registers are this vCPU's: stay on it. */
		local_irq_save(flags);
		n = l4h_call(L4H_CPTR(L4H_SLOT_ATTEST_EP), label, mrs, n_in, MAX_MRS);
		local_irq_restore(flags);
		if (n < 1) {
			ret = -EIO;
			break;
		}
		if ((long)mrs[0] < 0) {
			ret = (long)mrs[0];
			break;
		}
		chunk = mrs[2];
		if (n < 3 || chunk > L4H_ATTEST_CHUNK || n < 3 + DIV_ROUND_UP(chunk, 8) ||
		    (buf && mrs[1] != total) || got + chunk > mrs[1] || (!chunk && got < mrs[1])) {
			ret = -EIO;
			break;
		}
		if (!buf) {
			total = mrs[1];
			buf = kvmalloc(max_t(size_t, total, 1), GFP_KERNEL);
			if (!buf) {
				ret = -ENOMEM;
				break;
			}
		}
		memcpy(buf + got, &mrs[3], chunk);
		got += chunk;
	} while (got < total && off_mr >= 0);
	mutex_unlock(&attest_lock);
	kfree(mrs);
	if (!ret && got != total)
		ret = -EIO;
	if (ret) {
		kvfree(buf);
		return ret;
	}
	*out = buf;
	*out_len = total;
	return 0;
}

static void pack(unsigned long *mrs, const u8 *b, size_t n)
{
	memset(mrs, 0, DIV_ROUND_UP(n, 8) * sizeof(*mrs));
	memcpy(mrs, b, n);
}

static int attest_report(const u8 *data, size_t len, u8 **out, size_t *out_len)
{
	unsigned long mrs[9] = {};

	pack(mrs, data, min_t(size_t, len, L4H_ATTEST_DATA_SIZE));
	return attest_call(L4H_ATTEST_REPORT, mrs, 9, 8, out, out_len);
}

static int attest_evidence(u8 **out, size_t *out_len)
{
	unsigned long off = 0;

	return attest_call(L4H_ATTEST_EVIDENCE, &off, 1, 0, out, out_len);
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
		/* Two length registers and the bytes, in one message. */
		if (in_len > 8 * (MAX_MRS - 2))
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
	if (c.op == L4H_ATTEST_REPORT) {
		ret = attest_report(in, in_len, &out, &len);
	} else if (c.op == L4H_ATTEST_EVIDENCE) {
		ret = attest_evidence(&out, &len);
	} else {
		unsigned long *mrs = kcalloc(MAX_MRS, sizeof(*mrs), GFP_KERNEL);

		ret = -ENOMEM;
		if (mrs) {
			mrs[0] = c.blob_len;
			mrs[1] = c.secret_len;
			pack(mrs + 2, in, in_len);
			ret = attest_call(L4H_ATTEST_ACTIVATE, mrs, 2 + DIV_ROUND_UP(in_len, 8), -1, &out, &len);
			kfree(mrs);
		}
	}
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
	int ret;

	if (!(l4h_bootinfo->flags & L4H_BOOT_ATTEST))
		return 0;
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
