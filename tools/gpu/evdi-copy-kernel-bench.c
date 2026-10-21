// SPDX-License-Identifier: GPL-2.0-only
// Synthetic copy benchmark: private vmalloc memory only, no DRM device ownership.
#include <linux/fs.h>
#include <linux/ktime.h>
#include <linux/miscdevice.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/overflow.h>
#include <linux/sched/signal.h>
#include <linux/slab.h>
#include <linux/uaccess.h>
#include <linux/vmalloc.h>
#include <drm/drm_cache.h>
#include "evdi-copy-common.h"

#define WIDTH 1920
#define HEIGHT 1080
#define FRAME_BYTES (WIDTH * HEIGHT * 4UL)

struct copy_request {
	__u64 address;
	__u32 rows;
	__u32 iterations;
	__u32 method;
	__u32 reserved;
	__u64 elapsed_ns;
};
#define COPY_BENCH _IOWR('V', 0x91, struct copy_request)

/* Serialize changes to the prototype's per-call batch-size input. */
static DEFINE_MUTEX(copy_mutex);
static unsigned int evdi_copy_batch_rows;

#include "copy-original.inc"
#include "copy-batched.inc"

static int bench_open(struct inode *inode, struct file *file)
{
	u8 *source;
	size_t i;
	int result;

	result = nonseekable_open(inode, file);
	if (result)
		return result;
	source = vmalloc(FRAME_BYTES);
	if (!source)
		return -ENOMEM;
	for (i = 0; i < FRAME_BYTES; ++i)
		source[i] = (u8)i;
	file->private_data = source;
	return 0;
}

static int bench_release(struct inode *inode, struct file *file)
{
	(void)inode;
	vfree(file->private_data);
	return 0;
}

static long bench_ioctl(struct file *file, unsigned int command, unsigned long argument)
{
	struct copy_request request;
	struct copy_object object = { .vmapping = file->private_data };
	struct copy_frame fb = { .base.pitches = { WIDTH * 4 }, .obj = &object };
	struct copy_rect rectangle = { 0, 0, WIDTH, HEIGHT };
	u64 started;
	unsigned int i;
	int result;

	if (command != COPY_BENCH)
		return -ENOTTY;
	if (copy_from_user(&request, (void __user *)argument, sizeof(request)))
		return -EFAULT;
	if (request.reserved || request.method > 1 || request.rows > HEIGHT ||
	    !request.iterations || request.iterations > 128)
		return -EINVAL;
	result = mutex_lock_interruptible(&copy_mutex);
	if (result)
		return result;
	evdi_copy_batch_rows = request.rows;
	started = ktime_get_ns();
	for (i = 0; i < request.iterations; ++i) {
		if (signal_pending(current)) {
			result = -EINTR;
			break;
		}
		if (request.method)
			result = copy_batched(&fb, u64_to_user_ptr(request.address),
				WIDTH * 4, 1, &rectangle, WIDTH, HEIGHT);
		else
			result = copy_original(&fb, u64_to_user_ptr(request.address),
				WIDTH * 4, 1, &rectangle, WIDTH, HEIGHT);
		if (result)
			break;
		cond_resched();
	}
	request.elapsed_ns = ktime_get_ns() - started;
	mutex_unlock(&copy_mutex);
	if (result)
		return result;
	if (copy_to_user((void __user *)argument, &request, sizeof(request)))
		return -EFAULT;
	return 0;
}

static const struct file_operations bench_fops = {
	.owner = THIS_MODULE,
	.open = bench_open,
	.release = bench_release,
	.unlocked_ioctl = bench_ioctl,
};

static struct miscdevice bench_device = {
	.minor = MISC_DYNAMIC_MINOR,
	.name = "vino-evdi-copy-bench",
	.fops = &bench_fops,
	.mode = 0600,
};

static int __init bench_init(void)
{
	BUILD_BUG_ON(sizeof(struct copy_request) != 32);
	return misc_register(&bench_device);
}

static void __exit bench_exit(void)
{
	misc_deregister(&bench_device);
}

module_init(bench_init);
module_exit(bench_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Isolated EVDI capture-copy benchmark using private RAM only");
MODULE_VERSION("0.1");
