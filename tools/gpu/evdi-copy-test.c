// SPDX-License-Identifier: GPL-2.0-only
// Run the extracted original and patched routines with checked memory/copy hooks.
#include <assert.h>
#include <errno.h>
#include <limits.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "evdi-copy-common.h"

#define __user
#define CONFIG_X86 1
#define EVDI_HAVE_IOSYS_MAP 1
#define READ_ONCE(value) (value)
#define min_t(type, a, b) ((type)(a) < (type)(b) ? (type)(a) : (type)(b))
#define check_mul_overflow(a, b, result) __builtin_mul_overflow(a, b, result)
static unsigned int evdi_copy_batch_rows;

static struct {
    unsigned char *source, *destination, *flushed;
    size_t source_size, destination_size, fault_at;
    unsigned copies, flushes, xe_calls;
} state;

static void drm_clflush_virt_range(void *source, unsigned long length)
{
    ptrdiff_t at = (unsigned char *)source - state.source;
    assert(at >= 0 && (size_t)at <= state.source_size);
    assert(length <= state.source_size - (size_t)at);
    memset(state.flushed + at, 1, length);
    ++state.flushes;
}

static unsigned long copy_to_user(void *destination, const void *source,
                                  unsigned long length)
{
    ptrdiff_t from = (const unsigned char *)source - state.source;
    ptrdiff_t to = (unsigned char *)destination - state.destination;
    assert(from >= 0 && (size_t)from <= state.source_size);
    assert(to >= 0 && (size_t)to <= state.destination_size);
    assert(length <= state.source_size - (size_t)from);
    assert(length <= state.destination_size - (size_t)to);
    for (size_t i = 0; i < length; ++i)
        assert(state.flushed[from + i]);
    size_t copied = length;
    if ((size_t)to >= state.fault_at)
        copied = 0;
    else if (copied > state.fault_at - (size_t)to)
        copied = state.fault_at - (size_t)to;
    memcpy(destination, source, copied);
    ++state.copies;
    return length - copied;
}

static int copy_primary_pixels_on_xe(struct copy_frame *fb, char *buffer,
                                     int stride, int width, int height)
{
    (void)fb; (void)buffer; (void)stride; (void)width; (void)height;
    ++state.xe_calls;
    return -ENOSYS;
}

#include "copy-original.inc"
#include "copy-batched.inc"

static unsigned cases;
static void check(int width, int height, unsigned source_padding,
                  unsigned destination_padding, unsigned offset,
                  struct copy_rect *rectangles, int count, size_t fault_at,
                  unsigned batch, bool xe, int expected_result,
                  int expected_original_calls, int expected_batched_calls)
{
    const unsigned source_stride = (unsigned)width * 4 + source_padding;
    const unsigned destination_stride = (unsigned)width * 4 + destination_padding;
    const size_t source_size = offset + source_stride * (size_t)height;
    const size_t destination_size = destination_stride * (size_t)height;
    unsigned char *source_allocation = malloc(source_size + 128);
    unsigned char *original_allocation = malloc(destination_size + 128);
    unsigned char *batched_allocation = malloc(destination_size + 128);
    unsigned char *original_flushes = calloc(source_size, 1);
    unsigned char *batched_flushes = calloc(source_size, 1);
    assert(source_allocation && original_allocation && batched_allocation && original_flushes && batched_flushes);
    unsigned char *source = source_allocation + 64;
    for (size_t i = 0; i < source_size + 128; ++i)
        source_allocation[i] = (unsigned char)(i * 17 + i / 251);
    memset(original_allocation, 0xa5, destination_size + 128);
    memset(batched_allocation, 0xa5, destination_size + 128);
    struct copy_object object = {.vmapping = source};
    struct copy_frame fb = {.base = {.pitches = {source_stride}, .offsets = {offset}},
                            .obj = &object, .is_from_xe = xe};
    state.source = source;
    state.source_size = source_size;
    state.destination_size = destination_size;
    state.fault_at = fault_at;
    state.destination = original_allocation + 64;
    state.flushed = original_flushes;
    state.copies = state.flushes = state.xe_calls = 0;
    int original = copy_original(&fb, (char *)state.destination, (int)destination_stride,
                                 count, rectangles, width, height);
    unsigned old_copies = state.copies, old_flushes = state.flushes, old_xe = state.xe_calls;
    evdi_copy_batch_rows = batch;
    state.destination = batched_allocation + 64;
    state.flushed = batched_flushes;
    state.copies = state.flushes = state.xe_calls = 0;
    int batched = copy_batched(&fb, (char *)state.destination, (int)destination_stride,
                               count, rectangles, width, height);
    assert(original == expected_result && batched == original);
    assert(memcmp(original_allocation, batched_allocation, destination_size + 128) == 0);
    assert(old_xe == state.xe_calls);
    if (!original)
        assert(memcmp(original_flushes, batched_flushes, source_size) == 0);
    if (batch <= 1) {
        assert(old_copies == state.copies && old_flushes == state.flushes);
    }
    if (expected_original_calls >= 0)
        assert(old_copies == (unsigned)expected_original_calls);
    if (expected_batched_calls >= 0)
        assert(state.copies == (unsigned)expected_batched_calls);
    ++cases;
    free(source_allocation); free(original_allocation); free(batched_allocation);
    free(original_flushes); free(batched_flushes);
}

static unsigned random_state = 0x51682173;
static unsigned next_random(void)
{
    random_state ^= random_state << 13;
    random_state ^= random_state >> 17;
    random_state ^= random_state << 5;
    return random_state;
}

int main(void)
{
    struct copy_rect full = {0, 0, 1920, 1080};
    for (unsigned batch_index = 0; batch_index < 8; ++batch_index) {
        const unsigned batches[] = {0, 1, 2, 7, 32, 128, 1080, UINT_MAX};
        unsigned batch = batches[batch_index];
        unsigned calls = batch <= 1 ? 1080 : 1080 / batch + (1080 % batch != 0);
        check(1920, 1080, 0, 0, 0, &full, 1, SIZE_MAX, batch, false, 0, 1080, (int)calls);
        check(1920, 1080, 0, 0, 37, &full, 1, SIZE_MAX, batch, false, 0, 1080, (int)calls);
        check(1920, 1080, 256, 0, 0, &full, 1, SIZE_MAX, batch, false, 0, 1080, 1080);
        check(1920, 1080, 0, 64, 0, &full, 1, SIZE_MAX, batch, false, 0, 1080, 1080);
        check(1920, 1080, 256, 128, 37, &full, 1, SIZE_MAX, batch, false, 0, 1080, 1080);
        check(1920, 1080, 0, 0, 0, &full, 1, 0, batch, false, -EFAULT, 1, 1);
        check(1920, 1080, 0, 0, 37, &full, 1, 7680 * 5 + 123, batch, false, -EFAULT, 6, -1);
        check(1920, 1080, 0, 0, 0, &full, 1, SIZE_MAX, batch, true, -ENOSYS, 0, 0);
    }
    struct copy_rect partial = {3, 5, 80, 63};
    check(128, 64, 0, 0, 0, &partial, 1, SIZE_MAX, 1080, false, 0, 58, 58);
    struct copy_rect band = {0, 7, 128, 55};
    check(128, 64, 0, 0, 13, &band, 1, SIZE_MAX, 32, false, 0, 48, 2);
    struct copy_rect clips[] = {{0, 0, 128, 20}, {4, 5, 20, 10}, {0, 17, 128, 64}};
    check(128, 64, 0, 0, 0, clips, 3, SIZE_MAX, 32, false, 0, 72, 8);
    struct copy_rect too_wide = {0, 0, 129, 64}, too_tall = {0, 0, 128, 65};
    check(128, 64, 0, 0, 0, &too_wide, 1, SIZE_MAX, 32, false, -EFAULT, 0, 0);
    check(128, 64, 0, 0, 0, &too_tall, 1, SIZE_MAX, 32, false, -EFAULT, 0, 0);
    check(128, 64, 0, 0, 0, &full, 0, SIZE_MAX, 32, false, 0, 0, 0);
    struct copy_rect empty[] = {{0, 8, 0, 12}, {0, 8, 128, 8}};
    check(128, 64, 0, 0, 0, empty, 2, SIZE_MAX, 32, false, 0, 4, 4);
    for (unsigned i = 0; i < 500; ++i) {
        unsigned width = 1 + next_random() % 256, height = 1 + next_random() % 128;
        struct copy_rect rectangles[4];
        int count = 1 + (int)(next_random() % 4);
        for (int j = 0; j < count; ++j) {
            unsigned x = next_random() % width, y = next_random() % height;
            rectangles[j] = (struct copy_rect){x, y, x + 1 + next_random() % (width - x),
                                               y + 1 + next_random() % (height - y)};
        }
        if (i % 3 == 0)
            rectangles[0] = (struct copy_rect){0, 0, width, height};
        check((int)width, (int)height, i % 4 == 0 ? 64 : 0, i % 5 == 0 ? 128 : 0,
              next_random() % 64, rectangles, count, SIZE_MAX,
              i % 7 == 0 ? 0 : 2 + next_random() % 128, false, 0, -1, -1);
    }
    printf("{\"passed\":true,\"cases\":%u,\"full_frame_row_calls\":1080,\"calls_at_32_rows\":34}\n", cases);
    return 0;
}
