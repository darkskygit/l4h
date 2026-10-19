// SPDX-License-Identifier: GPL-2.0-only
/*
 * Copyright (C) 2011 Richard Weinberger <richrd@nod.at>
 */

#include <linux/slab.h>
#include <linux/sched.h>
#include <linux/mm.h>
#include <asm/page.h>
#include <asm/elf.h>
#include <linux/init.h>
#include <vdso/datapage.h>
#include <asm/vdso/vsyscall.h>

static unsigned int __read_mostly vdso_enabled = 1;
unsigned long um_vdso_addr;

extern unsigned long task_size;
extern char vdso_start[], vdso_end[];

/* The vDSO text, and the data pages below it: the time page the kernel
 * keeps current (update_vsyscall()), then unused zero pages. */
static unsigned int vdso_pages;
static struct page **vdso_text;
static struct page *vdso_data[__VDSO_PAGES + 1];

static int __init init_vdso(void)
{
	struct page *zero;
	int i;

	vdso_pages = DIV_ROUND_UP(vdso_end - vdso_start, PAGE_SIZE);
	/* The stack starts below L4H_VDSO_AREA (asm/processor-generic.h). */
	if ((vdso_pages + __VDSO_PAGES) * PAGE_SIZE > L4H_VDSO_AREA) {
		printk(KERN_ERR "vdso: %u pages do not fit below the top of user space\n",
		       vdso_pages + __VDSO_PAGES);
		vdso_enabled = 0;
		return -ENOMEM;
	}
	um_vdso_addr = task_size - vdso_pages * PAGE_SIZE;

	vdso_text = kcalloc(vdso_pages + 1, sizeof(*vdso_text), GFP_KERNEL);
	zero = alloc_page(GFP_KERNEL | __GFP_ZERO);
	if (!vdso_text || !zero)
		goto oom;
	for (i = 0; i < vdso_pages; i++) {
		vdso_text[i] = alloc_page(GFP_KERNEL | __GFP_ZERO);
		if (!vdso_text[i])
			goto oom;
		memcpy(page_address(vdso_text[i]), vdso_start + i * PAGE_SIZE,
		       min_t(long, PAGE_SIZE, vdso_end - vdso_start - i * PAGE_SIZE));
	}
	vdso_data[VDSO_TIME_PAGE_OFFSET] = virt_to_page(vdso_k_time_data);
	for (i = 1; i < __VDSO_PAGES; i++)
		vdso_data[i] = zero;

	return 0;

oom:
	printk(KERN_ERR "Cannot allocate vdso\n");
	vdso_enabled = 0;

	return -ENOMEM;
}
subsys_initcall(init_vdso);

int arch_setup_additional_pages(struct linux_binprm *bprm, int uses_interp)
{
	struct vm_area_struct *vma;
	struct mm_struct *mm = current->mm;
	static struct vm_special_mapping vvar_mapping = {
		.name = "[vvar]",
		.pages = vdso_data,
	};
	static struct vm_special_mapping vdso_mapping = {
		.name = "[vdso]",
	};

	if (!vdso_enabled)
		return 0;

	vdso_mapping.pages = vdso_text;
	if (mmap_write_lock_killable(mm))
		return -EINTR;

	vma = _install_special_mapping(mm, um_vdso_addr - __VDSO_PAGES * PAGE_SIZE,
		__VDSO_PAGES * PAGE_SIZE, VM_READ | VM_MAYREAD, &vvar_mapping);
	if (!IS_ERR(vma))
		vma = _install_special_mapping(mm, um_vdso_addr, vdso_pages * PAGE_SIZE,
			VM_READ|VM_EXEC|
			VM_MAYREAD|VM_MAYWRITE|VM_MAYEXEC,
			&vdso_mapping);

	mmap_write_unlock(mm);

	return IS_ERR(vma) ? PTR_ERR(vma) : 0;
}
