// SPDX-License-Identifier: GPL-2.0-or-later
// Read framebuffer metadata only. Never acquire DRM master or change a mode.
#include <errno.h>
#include <fcntl.h>
#include <glob.h>
#include <inttypes.h>
#include <stdbool.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <xf86drm.h>
#include <xf86drmMode.h>

static int inspect(const char *path)
{
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        fprintf(stderr, "%s: %s\n", path, strerror(errno));
        return 1;
    }
    drmVersionPtr version = drmGetVersion(fd);
    if (!version) {
        fprintf(stderr, "%s: DRM version: %s\n", path, strerror(errno));
        close(fd);
        return 1;
    }
    bool evdi = version->name_len == 4 && !memcmp(version->name, "evdi", 4);
    drmFreeVersion(version);
    if (!evdi) {
        close(fd);
        return 0;
    }
    drmModeResPtr resources = drmModeGetResources(fd);
    if (!resources) {
        fprintf(stderr, "%s: resources: %s\n", path, strerror(errno));
        close(fd);
        return 1;
    }
    int result = 0;
    for (int i = 0; i < resources->count_crtcs; ++i) {
        drmModeCrtcPtr crtc = drmModeGetCrtc(fd, resources->crtcs[i]);
        if (!crtc) {
            fprintf(stderr, "%s: CRTC: %s\n", path, strerror(errno));
            result = 1;
            continue;
        }
        printf("{\"card\":\"%s\",\"driver\":\"evdi\",\"crtc\":%u,\"fb_id\":%u",
               path, crtc->crtc_id, crtc->buffer_id);
        if (crtc->buffer_id) {
            drmModeFB2Ptr fb = drmModeGetFB2(fd, crtc->buffer_id);
            if (!fb) {
                printf(",\"metadata_errno\":%d", errno);
                result = 1;
            } else {
                printf(",\"width\":%u,\"height\":%u,\"fourcc\":%u,\"pitch\":%u,"
                       "\"offset\":%u,\"modifier_valid\":%s,\"modifier\":\"0x%016" PRIx64 "\","
                       "\"packed_32bit_rows\":%s",
                       fb->width, fb->height, fb->pixel_format, fb->pitches[0],
                       fb->offsets[0], (fb->flags & DRM_MODE_FB_MODIFIERS) ? "true" : "false",
                       fb->modifier, ((uint64_t)fb->width * 4 == fb->pitches[0]) ? "true" : "false");
                // Privileged GETFB2 may create handles. Release each distinct handle once.
                for (unsigned plane = 0; plane < 4; ++plane) {
                    bool duplicate = false;
                    for (unsigned earlier = 0; earlier < plane; ++earlier)
                        duplicate |= fb->handles[plane] == fb->handles[earlier];
                    if (fb->handles[plane] && !duplicate)
                        drmCloseBufferHandle(fd, fb->handles[plane]);
                }
                drmModeFreeFB2(fb);
            }
        }
        puts("}");
        drmModeFreeCrtc(crtc);
    }
    drmModeFreeResources(resources);
    close(fd);
    return result;
}

int main(int argc, char **argv)
{
    glob_t cards = {0};
    if (argc > 2) {
        fprintf(stderr, "usage: %s [/dev/dri/cardN]\n", argv[0]);
        return 2;
    }
    if (argc == 2)
        return inspect(argv[1]);
    if (glob("/dev/dri/card[0-9]*", 0, NULL, &cards)) {
        fputs("No DRM cards found\n", stderr);
        return 1;
    }
    int result = 0;
    for (size_t i = 0; i < cards.gl_pathc; ++i)
        result |= inspect(cards.gl_pathv[i]);
    globfree(&cards);
    return result;
}
