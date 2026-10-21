/* SPDX-License-Identifier: GPL-2.0-only */
/* Test-only stand-ins for the fields read by the extracted copy routine. */
#ifndef VINO_EVDI_COPY_COMMON_H
#define VINO_EVDI_COPY_COMMON_H
#ifdef __KERNEL__
#include <linux/types.h>
#else
#include <stdbool.h>
#include <stdint.h>
typedef uint32_t u32;
typedef uint16_t u16;
#endif

/* Keep synthetic geometry from becoming compile-time constants. Otherwise GCC
 * can remove hardened-usercopy checks from the original routine only, biasing
 * the comparison. The production EVDI function receives runtime geometry.
 */
#define COPY_ROUTINE_ATTRIBUTES __attribute__((noipa))

struct copy_framebuffer {
    u32 pitches[4];
    u32 offsets[4];
};
struct copy_object { void *vmapping; };
struct copy_frame {
    struct copy_framebuffer base;
    struct copy_object *obj;
    bool is_from_xe;
};
struct copy_rect { u16 x1, y1, x2, y2; };

#define EVDI_CHECKPT() ((void)0)
#define EVDI_VERBOSE(...) ((void)0)
#define EVDI_WARN(...) ((void)0)

#endif
