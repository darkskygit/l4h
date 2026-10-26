// SPDX-License-Identifier: GPL-2.0
/*
 * Memory hotplug from the l4h host (DESIGN.md §17), through the memory
 * device (L4H_DEV_MEM, struct l4h_mem_page).  dom0 sets a target; we plug
 * and unplug hotplug blocks of 128 MiB (one Linux memory block each) until
 * we have that many.  A plugged block brings its own metadata:
 *
 *   block 0       its memmap (memmap_on_memory)
 *   blocks 1-62   RAM, onlined to ZONE_MOVABLE up to three times boot RAM
 *   block 63      the seL4 objects that map blocks 0-62 (l4h_hp_map()),
 *                 kept PageOffline: never in the page allocator
 *
 * Plug: the CNodes the host copies the Untypeds into, L4H_HC_MEM, map the
 * frames, add_memory.  Unplug: offline_and_remove_memory, unmap, then
 * L4H_HC_MEM, which revokes the Untypeds.  Memory that cannot be offlined
 * (pinned or unmovable pages) stays: the host then sees plugged > target.
 */
#include <linux/bitmap.h>
#include <linux/device.h>
#include <linux/init.h>
#include <linux/interrupt.h>
#include <linux/memory.h>
#include <linux/memory_hotplug.h>
#include <linux/mm.h>
#include <linux/mutex.h>
#include <linux/page_ref.h>
#include <linux/workqueue.h>
#include <irq_kern.h>
#include <l4h_sys.h>
#include <l4h_caps.h>

#define HP_OBJ_BLOCK	(L4H_HP_BLOCKS - 1)
/* Retry after the host was busy, or Linux could not offline enough. */
#define RETRY_BUSY	HZ
#define RETRY_STUCK	(10 * HZ)
/* ZONE_MOVABLE memory, at most, in times boot RAM (as auto-movable). */
#define MOVABLE_RATIO	3
/* Available memory an unplug leaves, at least (can_spare_block()). */
#define HEADROOM_MIN	(64ul << 20)
#define HEADROOM_SHIFT	4

static_assert(L4H_HP_SHIFT == SECTION_SIZE_BITS);

static struct l4h_mem_page *page;
static u64 boot_bytes;
static u64 region_gpa;
static unsigned int region_count;
/* Hotplug blocks in Linux, and those out of Linux and unmapped that the
 * host has not taken back yet. */
static DECLARE_BITMAP(added, L4H_HP_MAX);
static DECLARE_BITMAP(pending, L4H_HP_MAX);
static DEFINE_MUTEX(memhp_mutex);

static void memhp_work_fn(struct work_struct *w);
static DECLARE_DELAYED_WORK(memhp_work, memhp_work_fn);

static u64 block_gpa(unsigned int m)
{
	return region_gpa + (u64)m * L4H_HP_SIZE;
}

static long mem_call(unsigned long op, unsigned int m)
{
	unsigned long mr[2] = { op, m };

	l4h_hypercall(L4H_HC_MEM, mr, 2, 1);
	return (long)mr[0];
}

/* Is `pfn` in the object block of a hotplug block? */
static bool in_obj_block(unsigned long pfn)
{
	u64 gpa = PFN_PHYS(pfn);

	if (gpa < region_gpa || gpa >= block_gpa(region_count))
		return false;
	return ((gpa - region_gpa) >> L4H_BLOCK_SHIFT) % L4H_HP_BLOCKS == HP_OBJ_BLOCK;
}

/* Onlining hands out chunks of at most a block, aligned: the object block's
 * stay PageOffline (as initialized), with a reference. */
static void memhp_online_page(struct page *p, unsigned int order)
{
	if (!in_obj_block(page_to_pfn(p)))
		generic_online_page(p, order);
}

/* Offlining skips PageOffline pages without references: drop ours while it
 * runs, as virtio-mem does for its unplugged ranges. */
static int memhp_notify(struct notifier_block *nb, unsigned long action, void *arg)
{
	const struct memory_notify *mn = arg;
	unsigned long pfn, end = mn->start_pfn + mn->nr_pages;

	if (action != MEM_GOING_OFFLINE && action != MEM_CANCEL_OFFLINE)
		return NOTIFY_OK;
	for (pfn = mn->start_pfn; pfn < end; pfn++) {
		if (!in_obj_block(pfn))
			continue;
		if (action == MEM_GOING_OFFLINE)
			page_ref_dec(pfn_to_page(pfn));
		else
			page_ref_inc(pfn_to_page(pfn));
	}
	return NOTIFY_OK;
}

static struct notifier_block memhp_nb = {
	.notifier_call = memhp_notify,
};

/* Did hotplug block m go to ZONE_MOVABLE?  (Its block 0 is its memmap.) */
static bool is_movable(unsigned int m)
{
	struct page *p = pfn_to_page(PFN_DOWN(block_gpa(m) + L4H_BLOCK_SIZE));

	return page_zonenum(p) == ZONE_MOVABLE;
}

static int online_block(struct memory_block *mem, void *arg)
{
	mem->online_type = *(int *)arg;
	return device_online(&mem->dev);
}

/*
 * Online hotplug block m: to ZONE_MOVABLE, which can always be offlined
 * again, while that stays within MOVABLE_RATIO times boot RAM, else to the
 * kernel zone, for the kernel memory a bigger VM's processes need.
 */
static int online(unsigned int m)
{
	unsigned int i, movable = 1;
	int type, ret;

	for_each_set_bit(i, added, region_count)
		movable += is_movable(i);
	type = (u64)movable * L4H_HP_SIZE <= MOVABLE_RATIO * boot_bytes ? MMOP_ONLINE_MOVABLE : MMOP_ONLINE_KERNEL;
	lock_device_hotplug();
	ret = walk_memory_blocks(block_gpa(m), L4H_HP_SIZE, &type, online_block);
	unlock_device_hotplug();
	return ret;
}

static int plug(unsigned int m)
{
	long ret = l4h_hp_prepare(m);

	if (ret)
		return ret;
	ret = mem_call(L4H_MEM_PLUG, m);
	if (ret)
		return ret;
	ret = l4h_hp_map(m);
	if (!ret) {
		/* Added offline (CONFIG_MHP_DEFAULT_ONLINE_TYPE_OFFLINE): we
		 * choose its zone. */
		ret = add_memory_driver_managed(0, block_gpa(m), L4H_HP_SIZE, "System RAM (l4h)",
						MHP_MEMMAP_ON_MEMORY);
		if (!ret) {
			ret = online(m);
			if (ret)
				remove_memory(block_gpa(m), L4H_HP_SIZE);
		}
		if (ret) {
			pr_err("l4h-memhp: block %u: cannot add it: %ld\n", m, ret);
			l4h_hp_unmap(m);
		}
	}
	if (ret) {
		/* The host takes it back; if it is too busy now, later. */
		if (mem_call(L4H_MEM_UNPLUG, m))
			__set_bit(m, pending);
		return ret;
	}
	__set_bit(m, added);
	return 0;
}

/*
 * Would the guest still have room to run without a hotplug block?
 * Offlining moves whatever it can elsewhere, and memory that cannot be
 * reclaimed (tmpfs without swap) then fills what is left: keep available
 * memory for HEADROOM_SHIFT-th of RAM, at least HEADROOM_MIN.
 */
static bool can_spare_block(void)
{
	unsigned long headroom = max(HEADROOM_MIN >> PAGE_SHIFT, totalram_pages() >> HEADROOM_SHIFT);

	return si_mem_available() >= (long)((L4H_HP_SIZE >> PAGE_SHIFT) + headroom);
}

static int unplug(unsigned int m)
{
	int ret;

	if (!can_spare_block())
		return -ENOMEM;
	ret = offline_and_remove_memory(block_gpa(m), L4H_HP_SIZE);
	if (ret)
		return ret;
	__clear_bit(m, added);
	l4h_hp_unmap(m);
	if (mem_call(L4H_MEM_UNPLUG, m))
		__set_bit(m, pending);
	return 0;
}

/* Bring the plugged hotplug blocks to the host's target. */
static void memhp_work_fn(struct work_struct *w)
{
	unsigned long delay = 0;
	unsigned int m, n, target;

	mutex_lock(&memhp_mutex);
	/* First what the host has yet to take back. */
	for_each_set_bit(m, pending, region_count) {
		if (mem_call(L4H_MEM_UNPLUG, m)) {
			delay = RETRY_BUSY;
			goto out;
		}
		__clear_bit(m, pending);
	}
	target = min(READ_ONCE(page->target), region_count);
	n = bitmap_weight(added, region_count);
	while (n < target) {
		long ret;

		m = find_first_zero_bit(added, region_count);
		ret = m < region_count ? plug(m) : -ENOSPC;
		if (ret) {
			/* -ENOSPC: the target has just gone down. */
			if (ret != -ENOSPC) {
				pr_info_ratelimited("l4h-memhp: block %u: cannot plug: %ld\n", m, ret);
				delay = ret == -EBUSY ? RETRY_BUSY : RETRY_STUCK;
			}
			goto out;
		}
		n++;
	}
	while (n > target) {
		bool done = false;
		int pass;

		/* ZONE_MOVABLE first, whose pages can always move, highest
		 * first; then the others. */
		for (pass = 0; pass < 2 && !done; pass++) {
			for (m = region_count; m-- > 0 && !done;) {
				if (test_bit(m, added) && is_movable(m) == !pass && !unplug(m))
					done = true;
			}
		}
		if (!done) {
			pr_info_ratelimited("l4h-memhp: %u blocks plugged, target %u: cannot offline more\n",
					    n, target);
			delay = RETRY_STUCK;
			goto out;
		}
		n--;
	}
out:
	mutex_unlock(&memhp_mutex);
	if (delay)
		schedule_delayed_work(&memhp_work, delay);
}

static irqreturn_t memhp_interrupt(int irq, void *data)
{
	mod_delayed_work(system_wq, &memhp_work, 0);
	return IRQ_HANDLED;
}

static int __init l4h_memhp_init(void)
{
	const struct l4h_bootinfo *bi = l4h_bootinfo;
	unsigned int i;
	int irq, ret;

	for (i = 0; i < bi->nr_devices && i < L4H_MAX_DEVICES; i++) {
		if (bi->devices[i].type == L4H_DEV_MEM)
			break;
	}
	if (i == bi->nr_devices || i == L4H_MAX_DEVICES || !bi->hp_count)
		return 0;
	page = (struct l4h_mem_page *)bi->devices[i].page_va;
	region_gpa = bi->hp_gpa;
	region_count = min_t(u64, bi->hp_count, L4H_HP_MAX);
	/* Before any plug: the memory the kernel booted with. */
	boot_bytes = (u64)totalram_pages() << PAGE_SHIFT;
	if (!mhp_supports_memmap_on_memory())
		pr_warn("l4h-memhp: no memmap_on_memory: plugged memory takes its memmap from boot RAM\n");
	ret = set_online_page_callback(memhp_online_page);
	if (ret)
		return ret;
	ret = register_memory_notifier(&memhp_nb);
	if (ret)
		return ret;
	irq = l4h_event_irq(bi->devices[i].event);
	if (irq < 0)
		return irq;
	ret = request_irq(irq, memhp_interrupt, 0, "l4h-memhp", NULL);
	if (ret)
		return ret;
	pr_info("l4h-memhp: %u hotplug blocks of %llu MiB from %#llx\n", region_count,
		L4H_HP_SIZE >> 20, region_gpa);
	schedule_delayed_work(&memhp_work, 0);
	return 0;
}
late_initcall(l4h_memhp_init);
