// SPDX-License-Identifier: GPL-2.0-or-later
// One-head replay of head zero in vino-gpu's interleaved, 16-frame BGRX corpus.
// The supervisor owns CPU/power sampling; stdin gates the measured interval.
#define _POSIX_C_SOURCE 200809L
#include <X11/Xlib.h>
#include <X11/Xatom.h>
#include <X11/Xutil.h>
#include <X11/extensions/XShm.h>
#include <X11/keysym.h>
#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/shm.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t stopped;
static void stop(int sig) { (void)sig; stopped = 1; }
static double seconds(void) {
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) exit(1);
    return (double)t.tv_sec + (double)t.tv_nsec / 1e9;
}
static void until(double target) {
    struct timespec t = {(time_t)target, (long)((target-floor(target))*1e9)};
    while (!stopped && clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &t, NULL)==EINTR) {}
}
static int gate(void) {
    char c;
    return !stopped && read(STDIN_FILENO, &c, 1)==1 && c=='g';
}
static int compare_double(const void *a, const void *b) {
    double x=*(const double *)a, y=*(const double *)b;
    return (x>y)-(x<y);
}

int main(int argc, char **argv) {
    if (argc!=12) {
        fprintf(stderr,"usage: live-replay FILE W H HZ WARMUP SECONDS X Y STATIC LABEL checked-layout\n");
        return 2;
    }
    int width=atoi(argv[2]), height=atoi(argv[3]);
    double rate=strtod(argv[4],NULL), warm=strtod(argv[5],NULL), duration=strtod(argv[6],NULL);
    int xp=atoi(argv[7]), yp=atoi(argv[8]), is_static=atoi(argv[9]);
    if (strcmp(argv[11],"checked-layout") || width<=0 || width>3840 || height<=0 || height>2160
        || !isfinite(rate) || rate<1 || rate>60 || !isfinite(warm) || warm<0 || warm>30
        || !isfinite(duration) || duration<=0 || duration>60 || duration*rate<1
        || (is_static!=0 && is_static!=1)) return 2;
    struct sigaction action={0};
    action.sa_handler=stop;
    sigemptyset(&action.sa_mask);
    sigaction(SIGINT,&action,NULL); sigaction(SIGTERM,&action,NULL); sigaction(SIGALRM,&action,NULL);
    pid_t parent=getppid();
    if (prctl(PR_SET_PDEATHSIG,SIGTERM) || getppid()!=parent) return 1;
    alarm((unsigned)ceil(warm+duration+20));
    setvbuf(stdout,NULL,_IOLBF,0);

    size_t frame_bytes=(size_t)width*height*4, expected=frame_bytes*32;
    int fd=open(argv[1],O_RDONLY);
    struct stat st;
    if (fd<0 || fstat(fd,&st) || st.st_size!=(off_t)expected) {
        fprintf(stderr,"invalid corpus size\n"); return 1;
    }
    unsigned char *frames=mmap(NULL,expected,PROT_READ,MAP_PRIVATE,fd,0);
    close(fd);
    if (frames==MAP_FAILED) return 1;
    volatile unsigned char prefault=0;
    for (size_t i=0;i<expected;i+=4096) prefault ^= frames[i];
    (void)prefault;
    Display *d=XOpenDisplay(NULL);
    if (!d || !XShmQueryExtension(d)) { fprintf(stderr,"X11 MIT-SHM unavailable\n"); return 1; }
    int screen=DefaultScreen(d), depth=DefaultDepth(d,screen);
    if (xp<0 || yp<0 || xp+width>DisplayWidth(d,screen) || yp+height>DisplayHeight(d,screen)) return 2;
    Window root=RootWindow(d,screen);
    XSetWindowAttributes attrs={0}; attrs.override_redirect=True; attrs.background_pixel=0;
    Window window=XCreateWindow(d,root,xp,yp,width,height,0,depth,InputOutput,
        DefaultVisual(d,screen),CWOverrideRedirect|CWBackPixel,&attrs);
    // Prevent the compositor from bypassing composition for this fullscreen window.
    // Both backends must exercise the same KWin path as a normal desktop window.
    Atom bypass=XInternAtom(d,"_NET_WM_BYPASS_COMPOSITOR",False);
    unsigned long never_bypass=2;
    XChangeProperty(d,window,bypass,XA_CARDINAL,32,PropModeReplace,(unsigned char *)&never_bypass,1);
    XStoreName(d,window,argv[10]);
    GC gc=XCreateGC(d,window,0,NULL);
    XShmSegmentInfo shm={0};
    XImage *im=XShmCreateImage(d,DefaultVisual(d,screen),depth,ZPixmap,NULL,&shm,width,height);
    if (!im || im->bits_per_pixel!=32 || im->byte_order!=LSBFirst || im->bytes_per_line!=width*4
        || im->red_mask!=0xff0000 || im->green_mask!=0xff00 || im->blue_mask!=0xff) {
        fprintf(stderr,"unsupported X pixel format\n"); return 1;
    }
    shm.shmid=shmget(IPC_PRIVATE,frame_bytes,IPC_CREAT|0600);
    if (shm.shmid<0) return 1;
    shm.shmaddr=shmat(shm.shmid,NULL,0);
    shmctl(shm.shmid,IPC_RMID,NULL);
    if (shm.shmaddr==(void *)-1) return 1;
    im->data=shm.shmaddr; shm.readOnly=False;
    XShmAttach(d,&shm);
    XMapRaised(d,window);
    memcpy(im->data,frames,frame_bytes);
    XShmPutImage(d,window,gc,im,0,0,0,0,width,height,False);
    XSync(d,False);
    // This checks our X window's input pixels, not the physical monitor's output.
    XImage *check=XGetImage(d,window,0,0,width,height,AllPlanes,ZPixmap);
    if (!check) return 1;
    for (int k=0;k<16;k++) {
        int x=(k*113)%width,y=(k*61)%height;
        const unsigned char *p=frames+((size_t)y*width+x)*4;
        unsigned long want=(unsigned long)p[0]|((unsigned long)p[1]<<8)|((unsigned long)p[2]<<16);
        if ((XGetPixel(check,x,y)&0xffffff)!=want) { fprintf(stderr,"pixel verification failed\n"); return 1; }
    }
    XDestroyImage(check);
    printf("{\"event\":\"ready\",\"pid\":%ld,\"pixel_checks_passed\":16}\n",(long)getpid());
    int escape=XKeysymToKeycode(d,XK_Escape);
    size_t count=(size_t)llround(duration*rate), completed=0, late=0;
    double *times=calloc(count,sizeof(double));
    if (!times) return 1;
    double measured_start=0, measured_end=0, max_lateness=0, max_gap=0;
    if (!gate()) stopped=1;
    for (int phase=0;phase<2 && !stopped;phase++) {
        if (phase) {
            printf("{\"event\":\"warm\"}\n");
            if (!gate()) { stopped=1; break; }
        }
        size_t n=phase?count:(size_t)ceil(warm*rate);
        double start=seconds(), last_submit=0;
        if (phase) measured_start=start;
        for (size_t i=0;i<n && !stopped;i++) {
            until(start+i/rate);
            if (stopped) break;
            double one=seconds();
            // Check Escape at 10 Hz; static holds do not submit repeated image damage.
            if (i % (size_t)fmax(1,round(rate/10))==0) {
                char keys[32]; XQueryKeymap(d,keys);
                if (keys[escape/8]&(1<<(escape%8))) { stopped=1; break; }
            }
            if (!is_static) {
                memcpy(im->data,frames+(i%16)*2*frame_bytes,frame_bytes);
                XShmPutImage(d,window,gc,im,0,0,0,0,width,height,False);
                XSync(d,False);
            }
            if (phase) {
                double done=seconds(), lateness=done-(start+i/rate);
                times[i]=(done-one)*1000;
                if (lateness>1/rate) late++;
                if (lateness>max_lateness) max_lateness=lateness;
                if (last_submit && done-last_submit>max_gap) max_gap=done-last_submit;
                last_submit=done; completed++;
            }
        }
        until(start+n/rate);
        if (phase) measured_end=seconds();
    }
    if (!stopped && completed==count) {
        qsort(times,count,sizeof(double),compare_double);
        printf("{\"event\":\"done\",\"start_monotonic_s\":%.9f,\"end_monotonic_s\":%.9f,"
            "\"seconds\":%.9f,\"static_hold\":%s,\"frames_submitted\":%zu,\"input_frames_per_second\":%.6f,"
            "\"input_deadline_misses\":%zu,\"x_submission_p99_ms\":%.6f,\"max_lateness_ms\":%.6f,"
            "\"max_input_gap_ms\":%.6f}\n",measured_start,measured_end,measured_end-measured_start,
            is_static?"true":"false",is_static?(size_t)0:completed,is_static?0:completed/(measured_end-measured_start),
            late,times[(size_t)llround((count-1)*.99)],max_lateness*1000,max_gap*1000);
        // Stay alive with the same image while the supervisor reads end counters.
        if (!gate()) stopped=1;
    }
    XShmDetach(d,&shm); XSync(d,False);
    im->data=NULL; XDestroyImage(im); shmdt(shm.shmaddr);
    XFreeGC(d,gc); XDestroyWindow(d,window); XCloseDisplay(d);
    munmap(frames,expected); free(times);
    return stopped || completed!=count ? 130 : 0;
}
