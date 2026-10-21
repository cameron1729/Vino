/* SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note */
#ifndef EVDI_SHARED_CAPTURE_H
#define EVDI_SHARED_CAPTURE_H

#include <linux/types.h>
#ifdef __KERNEL__
#include <drm/drm.h>
#else
#include <libdrm/drm.h>
#endif

/* Experimental Vino ABI: not an upstream EVDI interface. */
#define DRM_EVDI_SHARED_CAPTURE_VERSION 1
#define DRM_EVDI_ACQUIRE_FRAME 0x05
#define DRM_EVDI_RELEASE_FRAME 0x06

struct drm_evdi_acquire_frame {
	__u32 version;
	__u32 flags;
	__s32 dmabuf_fd;
	__s32 ready_fence_fd;
	__u32 width;
	__u32 height;
	__u32 pitch;
	__u32 format;
	__u64 offset;
	__u64 modifier;
	__u64 size;
	__u64 token;
	__u64 reserved[2];
};

struct drm_evdi_release_frame {
	__u64 token;
	__u32 flags;
	__u32 reserved;
};

#define DRM_IOCTL_EVDI_ACQUIRE_FRAME DRM_IOWR(DRM_COMMAND_BASE + \
	DRM_EVDI_ACQUIRE_FRAME, struct drm_evdi_acquire_frame)
#define DRM_IOCTL_EVDI_RELEASE_FRAME DRM_IOWR(DRM_COMMAND_BASE + \
	DRM_EVDI_RELEASE_FRAME, struct drm_evdi_release_frame)

#endif
