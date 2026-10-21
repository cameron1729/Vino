// SPDX-License-Identifier: GPL-2.0-only
/* Compile the actual ioctl implementation with fault-injectable kernel stubs.
 * These check lifecycle/control flow, not real DRM/GPU synchronization.
 */
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <libdrm/drm_fourcc.h>
#include "evdi-shared-capture.h"

typedef uint64_t u64;
typedef uint32_t u32;
#define PAGE_SHIFT 12
#define PAGE_SIZE (1UL << PAGE_SHIFT)
#define ARRAY_SIZE(a) (sizeof(a)/sizeof(*(a)))
#define IS_ALIGNED(n,a) (!((n)&((a)-1)))
#define ERR_PTR(n) ((void *)(intptr_t)(n))
#define PTR_ERR(p) ((int)(intptr_t)(p))
#define IS_ERR(p) ((uintptr_t)(p) >= (uintptr_t)-4095)
typedef unsigned spinlock_t;
#define U32_MAX UINT32_MAX
#define U64_MAX UINT64_MAX
#define GFP_KERNEL 0
#define THIS_MODULE NULL
#define DMA_RESV_USAGE_WRITE 1
#define DMA_RESV_USAGE_READ 2
#define DMA_RESV_USAGE_BOOKKEEP 3
#define check_mul_overflow __builtin_mul_overflow
#define check_add_overflow __builtin_add_overflow
#define timer_container_of(name, pointer, member) \
 ((typeof(name))((char *)(pointer) - offsetof(typeof(*name), member)))
#define spin_lock_init(lock) (*(lock) = 0)
#define spin_lock_irqsave(lock, flags) ((void)(lock), (flags) = 0)
#define spin_unlock_irqrestore(lock, flags) ((void)(lock), (void)(flags))
#define msecs_to_jiffies(x) (x)
static unsigned long jiffies;
static const char *fault;
static unsigned allocation_attempt, fd_attempt, acknowledgements;
static unsigned locks, reservation_locks;
static bool trip(const char *name) { return fault && !strcmp(fault, name); }
static void *kzalloc(size_t size, int flags) {
 (void)flags;
 ++allocation_attempt;
 if ((trip("alloc1") && allocation_attempt == 1) ||
     (trip("alloc2") && allocation_attempt == 2) ||
     (trip("alloc3") && allocation_attempt == 3)) return NULL;
 return calloc(1, size);
}
#define kfree free
#define kvfree free
static void *kvmalloc_array(size_t count, size_t size, int flags) { (void)flags; return trip("pages") ? NULL : calloc(count,size); }
#define round_down(n,a) ((n)&~((a)-1))
static unsigned dma_get_max_seg_size(void *dev) { (void)dev; return UINT_MAX; }
struct dma_fence;
struct dma_fence_ops {
 const char *(*get_driver_name)(struct dma_fence *);
 const char *(*get_timeline_name)(struct dma_fence *);
 void (*release)(struct dma_fence *);
 void *enable_signaling, *signaled, *wait, *set_deadline;
};
struct dma_fence { const struct dma_fence_ops *ops; unsigned refs; int error; bool signaled; };
static struct dma_fence_ops core_ops;
static struct dma_fence stub = {.ops=&core_ops, .refs=1, .signaled=true};
static void dma_fence_init(struct dma_fence *f, const struct dma_fence_ops *ops,
 spinlock_t *lock, u64 context, u64 seqno) {
 (void)lock; (void)context; (void)seqno; f->ops=ops; f->refs=1;
}
static u64 dma_fence_context_alloc(unsigned count) { static u64 next; next+=count; return next; }
static struct dma_fence *dma_fence_get(struct dma_fence *f) { if(f) ++f->refs; return f; }
static void dma_fence_put(struct dma_fence *f) {
 if(!f) return;
 assert(f->refs);
 if (!--f->refs) { assert(f != &stub); if(f->ops && f->ops->release) f->ops->release(f); else free(f); }
}
static struct dma_fence *dma_fence_get_stub(void) { core_ops.signaled=trip("coreops") ? (void *)1 : NULL; return dma_fence_get(&stub); }
static bool dma_fence_is_signaled_locked(struct dma_fence *f) { return f->signaled; }
static void dma_fence_signal_locked(struct dma_fence *f) { assert(!f->signaled); f->signaled=true; }
static int dma_fence_get_status(struct dma_fence *f) { return f->signaled ? (f->error ? f->error : 1) : 0; }
struct timer_list { void (*callback)(struct timer_list *); bool active, shutdown; };
static void timer_setup(struct timer_list *t, void (*cb)(struct timer_list *), int flags) { (void)flags; t->callback=cb; }
static void mod_timer(struct timer_list *t, unsigned long expires) { (void)expires; assert(!t->shutdown); t->active=true; }
static void timer_shutdown_sync(struct timer_list *t) { t->active=false; t->shutdown=true; }
static void fire_timer(struct timer_list *t) { assert(t->active); t->active=false; t->callback(t); }
struct dma_resv { struct dma_fence *reader; bool snapshot, proxy, pending; };
static bool dma_resv_trylock(struct dma_resv *r) { (void)r; if(trip("trylock")) return false; ++reservation_locks; return true; }
static void dma_resv_unlock(struct dma_resv *r) { (void)r; assert(reservation_locks); --reservation_locks; }
static int dma_resv_reserve_fences(struct dma_resv *r, unsigned n) { (void)r; assert(n==1 && reservation_locks); return trip("reserve") ? -ENOMEM : 0; }
static int dma_resv_get_singleton(struct dma_resv *r, int usage, struct dma_fence **f) {
 assert(reservation_locks && usage==DMA_RESV_USAGE_WRITE && !r->reader);
 r->snapshot=true; *f=NULL; return trip("writers") ? -ENOMEM : 0;
}
static void dma_resv_add_fence(struct dma_resv *r, struct dma_fence *f, int usage) {
 assert(reservation_locks);
 if(r->proxy) assert(usage==DMA_RESV_USAGE_WRITE);
 else assert(r->snapshot && !r->reader && usage==DMA_RESV_USAGE_READ);
 dma_fence_put(r->reader); r->reader=dma_fence_get(f);
}
static bool dma_resv_test_signaled(struct dma_resv *r, int usage) { assert(reservation_locks && usage==DMA_RESV_USAGE_BOOKKEEP); return !r->pending && (!r->reader || r->reader->signaled); }
struct file { unsigned refs; void (*release)(struct file *); void *owner; };
static void fput(struct file *file) { assert(file && file->refs); if(!--file->refs && file->release) file->release(file); }
struct module { const char *name; };
struct dma_buf_ops;
struct dma_buf { struct file *file; struct module *owner; struct dma_resv *resv; u64 size; void *priv; const struct dma_buf_ops *ops; };
static void get_dma_buf(struct dma_buf *b) { ++b->file->refs; }
static void dma_buf_put(struct dma_buf *b) { fput(b->file); }
struct sync_file { struct file *file; struct dma_fence *fence; };
static void release_sync_file(struct file *file) {
 struct sync_file *sync=file->owner;
 dma_fence_put(sync->fence); free(sync); free(file);
}
static struct sync_file *sync_file_create(struct dma_fence *fence) {
 if(trip("sync")) return NULL;
 struct sync_file *s=calloc(1,sizeof(*s)); assert(s);
 s->file=calloc(1,sizeof(*s->file)); assert(s->file);
 s->file->refs=1; s->file->owner=s; s->file->release=release_sync_file;
 s->fence=dma_fence_get(fence); return s;
}
static bool reserved[64];
static struct file *installed[64];
static int get_unused_fd_flags(int flags) {
 assert(flags==O_CLOEXEC); ++fd_attempt;
 if ((trip("fd1") && fd_attempt==1) || (trip("fd2") && fd_attempt==2)) return -EMFILE;
 for(int i=3;i<64;++i) if(!reserved[i] && !installed[i]) { reserved[i]=true; return i; }
 return -EMFILE;
}
static void put_unused_fd(int fd) { assert(fd>=3 && reserved[fd]); reserved[fd]=false; }
static void fd_install(int fd, struct file *file) { assert(reserved[fd] && !installed[fd]); reserved[fd]=false; installed[fd]=file; }
struct drm_crtc { int unused; };
struct drm_pending_vblank_event { int unused; };
static void evdi_send_vblank(struct drm_crtc *crtc, struct drm_pending_vblank_event *event) {
 assert((crtc!=NULL)==(event!=NULL)); if(event) ++acknowledgements;
}
struct drm_format_info { unsigned format, num_planes; };
struct drm_framebuffer { const struct drm_format_info *format; u64 modifier; unsigned width,height,pitches[4],offsets[4],refs; };
static void drm_framebuffer_get(struct drm_framebuffer *f) { ++f->refs; }
static void drm_framebuffer_put(struct drm_framebuffer *f) { assert(f->refs>1); --f->refs; }
struct dma_buf_attachment { struct dma_buf *dmabuf; void *dev; };
struct drm_device;
static unsigned device_refs;
static void drm_dev_get(struct drm_device *dev) { (void)dev; ++device_refs; }
static void drm_dev_put(struct drm_device *dev) { (void)dev; assert(device_refs); --device_refs; }
struct drm_gem_object { struct drm_device *dev; struct dma_buf_attachment *import_attach; size_t size; unsigned refs; };
static void drm_gem_object_get(struct drm_gem_object *g) { ++g->refs; }
static void drm_gem_object_put(struct drm_gem_object *g) { assert(g->refs>1); --g->refs; }
struct page { bool device; };
static bool is_zone_device_page(struct page *p) { return p->device; }
struct scatterlist { unsigned offset,length; };
struct sg_table { struct scatterlist *sgl; unsigned orig_nents; };
#define for_each_sg(start,sg,n,i) for((i)=0,(sg)=(start); (i)<(n); ++(i),++(sg))
struct evdi_gem_object { struct drm_gem_object base; struct page **pages; struct sg_table *sg; };
enum dma_data_direction { DMA_BIDIRECTIONAL };
static struct page sg_ram;
static int drm_prime_sg_to_page_array(struct sg_table *sg, struct page **pages, int count) {
 assert(sg->sgl->length/PAGE_SIZE==(unsigned)count);
 if(trip("sgpages")) return -EINVAL;
 sg_ram.device=trip("devicepage");
 for(int i=0;i<count;++i) pages[i]=&sg_ram;
 return 0;
}
static int sg_alloc_table_from_pages_segment(struct sg_table *sg, struct page **pages,
 unsigned count, unsigned offset, size_t size, unsigned maxseg, int flags) {
 (void)pages; (void)flags; assert(offset==0 && count*PAGE_SIZE==size && maxseg>=PAGE_SIZE);
 if(trip("sgmap")) return -ENOMEM;
 sg->sgl=calloc(1,sizeof(*sg->sgl)); assert(sg->sgl); sg->orig_nents=1; sg->sgl->length=size; return 0;
}
static void sg_free_table(struct sg_table *sg) { free(sg->sgl); }
static unsigned mappings;
static int dma_map_sgtable(void *dev, struct sg_table *sg, enum dma_data_direction dir, int attrs) {
 (void)dev; (void)sg; (void)dir; (void)attrs; if(trip("map")) return -ENOMEM; ++mappings; return 0;
}
static void dma_unmap_sgtable(void *dev, struct sg_table *sg, enum dma_data_direction dir, int attrs) {
 (void)dev; (void)sg; (void)dir; (void)attrs; assert(mappings); --mappings;
}
struct dma_buf_ops {
 struct sg_table *(*map_dma_buf)(struct dma_buf_attachment *,enum dma_data_direction);
 void (*unmap_dma_buf)(struct dma_buf_attachment *,struct sg_table *,enum dma_data_direction);
 void (*release)(struct dma_buf *);
};
struct dma_buf_export_info { const struct dma_buf_ops *ops; size_t size; int flags; void *priv; };
#define DEFINE_DMA_BUF_EXPORT_INFO(name) struct dma_buf_export_info name={0}
static void exported_file_release(struct file *file) {
 struct dma_buf *b=file->owner; b->ops->release(b); dma_fence_put(b->resv->reader); free(b->resv); free(b); free(file);
}
static struct dma_buf *dma_buf_export(const struct dma_buf_export_info *i) {
 if(trip("export")) return ERR_PTR(-ENOMEM);
 struct dma_buf *b=calloc(1,sizeof(*b)); assert(b);
 b->file=calloc(1,sizeof(*b->file)); assert(b->file); b->file->refs=1; b->file->owner=b; b->file->release=exported_file_release;
 b->resv=calloc(1,sizeof(*b->resv)); assert(b->resv); b->resv->proxy=true; b->ops=i->ops; b->priv=i->priv; b->size=i->size; return b;
}
struct evdi_framebuffer { struct drm_framebuffer base; struct evdi_gem_object *obj; };
struct drm_file { int unused; };
struct evdi_shared_capture;
struct evdi_painter {
 bool is_connected,shared_powered;
 struct dma_buf *shared_proxies[4]; unsigned shared_proxy_next;
 struct drm_file *drm_filp;
 struct evdi_framebuffer *scanout_fb;
 struct evdi_shared_capture *shared_capture;
 u64 shared_next_token;
 int num_dirts;
 struct drm_crtc *crtc;
 struct drm_pending_vblank_event *vblank;
};
static void painter_lock(struct evdi_painter *p) { (void)p; assert(!locks); ++locks; }
static void painter_unlock(struct evdi_painter *p) { (void)p; assert(locks==1); --locks; }
struct evdi_device { struct evdi_painter *painter; bool cursor_events_enabled; };
struct drm_device { struct evdi_device *dev_private; };
#include "evdi_shared_capture_impl.inc"

static struct module exporter={.name="amdgpu"};
static struct drm_format_info format={DRM_FORMAT_XRGB8888,1};
static struct drm_file owner, stranger;
static struct drm_crtc crtc;
static struct drm_pending_vblank_event vblank;
static struct file buffer_file;
static struct dma_resv reservation;
static struct dma_buf buffer;
static struct dma_buf_attachment attachment;
static struct evdi_gem_object gem;
static struct page ram;
static struct page *page_array[16];
static struct scatterlist original_sg={.length=16*PAGE_SIZE};
static struct sg_table original_sgt={.sgl=&original_sg,.orig_nents=1};
static struct evdi_framebuffer fb;
static struct evdi_painter painter;
static struct evdi_device evdi;
static struct drm_device device;

static void setup(void) {
 assert(!device_refs && stub.refs==1 && !locks && !reservation_locks);
 allocation_attempt=fd_attempt=acknowledgements=0; fault=NULL;
 memset(&reservation,0,sizeof(reservation));
 buffer_file=(struct file){.refs=1};
 exporter.name="amdgpu"; format=(struct drm_format_info){DRM_FORMAT_XRGB8888,1};
 buffer=(struct dma_buf){.file=&buffer_file,.owner=&exporter,.resv=&reservation,.size=4096*16};
 attachment.dmabuf=&buffer;
 for(unsigned i=0;i<16;++i) page_array[i]=&ram;
 gem=(struct evdi_gem_object){.base={.import_attach=&attachment,.size=buffer.size,.refs=1},.pages=page_array,.sg=&original_sgt};
 fb=(struct evdi_framebuffer){.base={.format=&format,.width=64,.height=32,.pitches={256},.refs=1},.obj=&gem};
 painter=(struct evdi_painter){.is_connected=true,.shared_powered=true,.drm_filp=&owner,.scanout_fb=&fb,.num_dirts=2,.crtc=&crtc,.vblank=&vblank};
 evdi=(struct evdi_device){&painter,true}; device.dev_private=&evdi;
}
static struct drm_evdi_acquire_frame request(void) { return (struct drm_evdi_acquire_frame){.version=1}; }
static void cleanup(void) {
 painter_lock(&painter); evdi_shared_capture_cancel(&painter,-ESTALE); evdi_shared_proxies_clear(&painter); painter_unlock(&painter);
 for(int i=3;i<64;++i) { assert(!reserved[i]); if(installed[i]) { fput(installed[i]); installed[i]=NULL; } }
 dma_fence_put(reservation.reader); reservation.reader=NULL;
 assert(buffer_file.refs==1 && fb.base.refs==1 && gem.base.refs==1 && device_refs==0 && stub.refs==1 && !mappings);
 assert(!locks && !reservation_locks);
}
static void unchanged(void) { assert(painter.num_dirts==2 && painter.crtc==&crtc && painter.vblank==&vblank && !painter.shared_capture && !acknowledgements); }
static unsigned cases;
#define CHECK(condition) do { assert(condition); ++cases; } while(0)
int main(void) {
 _Static_assert(sizeof(struct drm_evdi_acquire_frame)==80,"acquire ABI");
 _Static_assert(sizeof(struct drm_evdi_release_frame)==16,"release ABI");
 _Static_assert(offsetof(struct drm_evdi_acquire_frame,offset)==32,"offset ABI");
 _Static_assert(offsetof(struct drm_evdi_acquire_frame,token)==56,"token ABI");
 _Static_assert(DRM_IOCTL_EVDI_ACQUIRE_FRAME==0xc0506445UL,"acquire ioctl");
 _Static_assert(DRM_IOCTL_EVDI_RELEASE_FRAME==0xc0106446UL,"release ioctl");
 const char *failures[]={"alloc1","alloc2","alloc3","pages","sgpages","devicepage","export","coreops","fd1","fd2","trylock","reserve","writers","sync"};
 for(unsigned i=0;i<sizeof(failures)/sizeof(*failures);++i) {
  setup(); fault=failures[i]; struct drm_evdi_acquire_frame cmd=request();
  CHECK(evdi_painter_acquire_frame_ioctl(&device,&cmd,&owner)<0); unchanged(); cleanup();
 }
 setup(); struct drm_evdi_acquire_frame cmd=request();
 CHECK(evdi_painter_acquire_frame_ioctl(&device,&cmd,&stranger)==-EACCES); unchanged(); cleanup();
 setup(); cmd=request(); cmd.reserved[1]=1;
 CHECK(evdi_painter_acquire_frame_ioctl(&device,&cmd,&owner)==-EINVAL); unchanged(); cleanup();
 setup(); cmd=request(); cmd.version=2;
 CHECK(evdi_painter_acquire_frame_ioctl(&device,&cmd,&owner)==-EINVAL); unchanged(); cleanup();
 setup(); cmd=request(); exporter.name="other";
 CHECK(evdi_painter_acquire_frame_ioctl(&device,&cmd,&owner)==-EOPNOTSUPP); unchanged(); cleanup();
 setup(); cmd=request(); evdi.cursor_events_enabled=false;
 CHECK(evdi_painter_acquire_frame_ioctl(&device,&cmd,&owner)==-EOPNOTSUPP); unchanged(); cleanup();
 setup(); cmd=request(); fb.base.modifier=1;
 CHECK(evdi_painter_acquire_frame_ioctl(&device,&cmd,&owner)==-EOPNOTSUPP); unchanged(); cleanup();
 setup(); cmd=request(); buffer.size=16;
 CHECK(evdi_painter_acquire_frame_ioctl(&device,&cmd,&owner)==-EINVAL); unchanged(); cleanup();
 setup(); cmd=request(); painter.shared_next_token=U64_MAX;
 CHECK(evdi_painter_acquire_frame_ioctl(&device,&cmd,&owner)==-EOVERFLOW); unchanged(); cleanup();
 setup(); cmd=request(); gem.pages=NULL;
 CHECK(evdi_painter_acquire_frame_ioctl(&device,&cmd,&owner)==0);
 CHECK(cmd.token==1 && cmd.width==64 && cmd.height==32 && cmd.pitch==256 && cmd.format==DRM_FORMAT_XRGB8888);
 CHECK(installed[cmd.dmabuf_fd]!=&buffer_file && installed[cmd.ready_fence_fd]);
 struct dma_buf *alias=installed[cmd.dmabuf_fd]->owner;
 CHECK(alias->resv != buffer.resv && alias->resv->reader != reservation.reader);
 struct dma_buf_attachment alias_attach={.dmabuf=alias};
 struct sg_table *mapped=alias->ops->map_dma_buf(&alias_attach,DMA_BIDIRECTIONAL);
 CHECK(!IS_ERR(mapped) && mappings==1);
 alias->ops->unmap_dma_buf(&alias_attach,mapped,DMA_BIDIRECTIONAL);
 fault="map"; CHECK(IS_ERR(alias->ops->map_dma_buf(&alias_attach,DMA_BIDIRECTIONAL))); fault=NULL;
 CHECK(painter.num_dirts==0 && !painter.crtc && !acknowledgements && fb.base.refs==2);
 struct drm_evdi_acquire_frame again=request();
 CHECK(evdi_painter_acquire_frame_ioctl(&device,&again,&owner)==-EBUSY);
 struct drm_evdi_release_frame release={.token=2};
 CHECK(evdi_painter_release_frame_ioctl(&device,&release,&owner)==-ESTALE);
 CHECK(painter.shared_capture && !acknowledgements);
 release.token=cmd.token;
 alias->resv->pending=true;
 CHECK(evdi_painter_release_frame_ioctl(&device,&release,&owner)==-EBUSY);
 CHECK(painter.shared_capture && painter.shared_capture->guard->timeout.active && !reservation.reader->signaled);
 alias->resv->pending=false;
 CHECK(evdi_painter_release_frame_ioctl(&device,&release,&stranger)==-EACCES);
 CHECK(evdi_painter_release_frame_ioctl(&device,&release,&owner)==0);
 CHECK(!painter.shared_capture && acknowledgements==1 && dma_fence_get_status(reservation.reader)==1);
 CHECK(evdi_painter_release_frame_ioctl(&device,&release,&owner)==-ESTALE); cleanup();
 setup(); cmd=request(); CHECK(evdi_painter_acquire_frame_ioctl(&device,&cmd,&owner)==0);
 fire_timer(&painter.shared_capture->guard->timeout);
 CHECK(acknowledgements==1 && dma_fence_get_status(reservation.reader)==0 && painter.shared_capture->guard->lease_error==-ETIMEDOUT);
 release=(struct drm_evdi_release_frame){.token=cmd.token};
 CHECK(evdi_painter_release_frame_ioctl(&device,&release,&owner)==-ESTALE);
 CHECK(acknowledgements==1 && !painter.shared_capture && !reservation.reader->signaled);
 CHECK(!painter.shared_proxies[0]);
 fput(installed[cmd.dmabuf_fd]); installed[cmd.dmabuf_fd]=NULL;
 CHECK(reservation.reader->signaled); cleanup();
 setup(); cmd=request(); CHECK(evdi_painter_acquire_frame_ioctl(&device,&cmd,&owner)==0);
 painter_lock(&painter); evdi_shared_capture_cancel(&painter,-ESTALE); painter_unlock(&painter);
 CHECK(acknowledgements==1 && !painter.shared_capture && dma_fence_get_status(reservation.reader)==0);
 release=(struct drm_evdi_release_frame){.token=cmd.token};
 CHECK(evdi_painter_release_frame_ioctl(&device,&release,&owner)==-ESTALE); cleanup();
 printf("{\"passed\":true,\"checks\":%u,\"scope\":\"actual ioctl control flow with mock kernel primitives; no real DMA synchronization tested\"}\n",cases);
 return 0;
}
