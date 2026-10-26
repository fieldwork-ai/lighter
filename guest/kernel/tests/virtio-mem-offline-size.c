/* virtio-mem's offline_size across many plugs and unplugs of blocks that
 * carry their own page array (patch 0024): what the notifier adds on
 * MEM_OFFLINE and takes on MEM_ONLINE, as patch 0059 writes it, with its
 * helper and 0024's page-array lookup. The mocks are a memory block with an
 * altmap and the driver's add and remove accounting as virtio_mem.c does
 * it; the accounting before 0059 is written out here as the control. This
 * checks the arithmetic the plug path's threshold sees, not the core's
 * hotplug. */
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>

#define PAGE_SHIFT 12
#define PFN_PHYS(x) ((uint64_t)(x) << PAGE_SHIFT)
#define MIB (1024ull * 1024)
#define BLOCK (128 * MIB)
#define BLOCK_PAGES (BLOCK >> PAGE_SHIFT)
/* 64 bytes of struct page per 4 KiB page: 2 MiB for a 128 MiB block. */
#define VMEMMAP_PAGES ((BLOCK_PAGES * 64) >> PAGE_SHIFT)
/* VIRTIO_MEM_DEFAULT_OFFLINE_THRESHOLD, and max(2 * bb_size, it) in BBM. */
#define OFFLINE_THRESHOLD (1024 * MIB)
#define SECTION_PAGES BLOCK_PAGES

struct memory_notify { unsigned long start_pfn; unsigned long nr_pages; };
struct vmem_altmap { unsigned long free; };
struct device { int refs; };
struct memory_block { struct vmem_altmap *altmap; struct device dev; };

static struct vmem_altmap altmap = { .free = VMEMMAP_PAGES };
static struct memory_block block = { .altmap = &altmap };
static unsigned long pfn_to_section_nr(unsigned long pfn) { return pfn / SECTION_PAGES; }
static struct memory_block *find_memory_block(unsigned long section)
{
	(void)section;
	block.dev.refs++;
	return &block;
}
static void put_device(struct device *dev) { dev->refs--; }

@VMEMMAP_FUNCTION@

@NOTIFIED_FUNCTION@

/* The driver's accounting around the notifier, from virtio_mem.c. */
static uint64_t offline_size;
static bool fixed = true;

/* The notifier's own expressions, `mhp` its argument. */
static uint64_t onlined(struct memory_notify *mhp)
{
	return fixed ? (uint64_t)(@ONLINE_SIZE@) : PFN_PHYS(mhp->nr_pages);
}

static uint64_t offlined(struct memory_notify *mhp)
{
	return fixed ? (uint64_t)(@OFFLINE_SIZE@) : PFN_PHYS(mhp->nr_pages);
}

static bool could_add_memory(uint64_t size)
{
	return offline_size + size <= OFFLINE_THRESHOLD;
}

/* The core onlines a memmap_on_memory block past its page array, and tells
 * the notifier only that part (memory_block_online). */
static struct memory_notify past_the_array(unsigned long first_pfn)
{
	return (struct memory_notify){
		.start_pfn = first_pfn + VMEMMAP_PAGES,
		.nr_pages = BLOCK_PAGES - VMEMMAP_PAGES,
	};
}

/* virtio_mem_add_memory, and the auto-online that follows it. */
static bool plug(unsigned long index)
{
	if (!could_add_memory(BLOCK))
		return false; /* -ENOSPC: the main loop does not retry it */
	offline_size += BLOCK;
	struct memory_notify mhp = past_the_array(index * BLOCK_PAGES);
	offline_size -= onlined(&mhp); /* MEM_ONLINE */
	return true;
}

/* MEM_OFFLINE, then virtio_mem_offline_and_remove_memory. */
static void unplug(unsigned long index)
{
	struct memory_notify mhp = past_the_array(index * BLOCK_PAGES);
	offline_size += offlined(&mhp); /* MEM_OFFLINE */
	offline_size -= BLOCK;
}

/* How many blocks plug before the threshold refuses one, up to `limit`. */
static unsigned long plugs_until_refused(unsigned long limit)
{
	unsigned long n = 0;
	while (n < limit && plug(n))
		n++;
	return n;
}

int main(void)
{
	assert(VMEMMAP_PAGES * (1ul << PAGE_SHIFT) == 2 * MIB);

	/* Without 0059: 2 MiB stays behind per block, and the 450th block
	 * (the 2026-10-26 guest's, at 449 plugged) is refused for good. */
	fixed = false;
	offline_size = 0;
	assert(plugs_until_refused(2048) == 449);
	assert(offline_size == 449 * 2 * MIB);

	/* With it: a whole range of 1008 blocks (126 GiB, twice a 64 GB Mac
	 * less the base) plugs, nothing stays counted offline, and the lookup
	 * gives back every reference it takes. */
	fixed = true;
	offline_size = 0;
	assert(plugs_until_refused(1008) == 1008);
	assert(offline_size == 0);
	assert(block.dev.refs == 0);

	/* Shrinking and growing again leaves nothing behind either. */
	for (unsigned long round = 0; round < 50; round++) {
		for (unsigned long i = 0; i < 500; i++)
			unplug(1007 - i);
		assert(offline_size == 0);
		for (unsigned long i = 0; i < 500; i++)
			assert(plug(508 + i));
		assert(offline_size == 0);
	}
	assert(block.dev.refs == 0);

	puts("virtio-mem offline_size: ok");
	return 0;
}
