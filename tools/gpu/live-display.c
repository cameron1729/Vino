// SPDX-License-Identifier: GPL-2.0-or-later
// Replay precomputed benchmark pixels through the existing X11 desktop.
// No driver restart, USB access, keyboard grab, or display-mode changes.
#define _POSIX_C_SOURCE 200809L
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/extensions/XShm.h>
#include <X11/keysym.h>
#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <signal.h>
#include <stdint.h>
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
static double seconds(clockid_t clock) {
    struct timespec t;
    if (clock_gettime(clock, &t)) { perror("clock_gettime"); exit(1); }
    return (double)t.tv_sec + (double)t.tv_nsec / 1e9;
}
static void until(double target) {
    struct timespec t = {(time_t)target, (long)((target - floor(target)) * 1e9)};
    while (!stopped && clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &t, NULL) == EINTR) {}
}
struct cpu { double total, user, system; };
static struct cpu service_cpu(void) {
    FILE *f = fopen("/sys/fs/cgroup/system.slice/displaylink-driver.service/cpu.stat", "r");
    if (!f) { perror("DisplayLink cpu.stat"); exit(1); }
    struct cpu result = {-1, -1, -1};
    char key[80]; unsigned long long value;
    while (fscanf(f, "%79s %llu", key, &value) == 2) {
        if (!strcmp(key,"usage_usec")) result.total = (double)value / 1e6;
        if (!strcmp(key,"user_usec")) result.user = (double)value / 1e6;
        if (!strcmp(key,"system_usec")) result.system = (double)value / 1e6;
    }
    fclose(f);
    if (result.total < 0 || result.user < 0 || result.system < 0) exit(1);
    return result;
}
static double process_cpu(int pid) {
    char path[100], line[4096];
    snprintf(path, sizeof(path), "/proc/%d/stat", pid);
    FILE *f = fopen(path, "r");
    if (!f || !fgets(line, sizeof(line), f)) { perror(path); exit(1); }
    fclose(f);
    char *p = strrchr(line, ')');
    if (!p) exit(1);
    char *state = NULL, *field = strtok_r(p + 2, " ", &state);
    double ticks = 0;
    for (int n = 3; field; n++, field = strtok_r(NULL, " ", &state)) {
        if (n == 14 || n == 15) ticks += strtod(field, NULL);
        if (n == 15) break;
    }
    return ticks / (double)sysconf(_SC_CLK_TCK);
}
static int compare_double(const void *a, const void *b) {
    double x = *(const double *)a, y = *(const double *)b;
    return (x > y) - (x < y);
}
int main(int argc, char **argv) {
    if (argc != 15) {
        fprintf(stderr,"usage: live-display FILE W H HZ WARMUP SECONDS X0 Y0 X1 Y1 XORG_PID KWIN_PID STATIC checked-layout\n");
        return 2;
    }
    int width=atoi(argv[2]), height=atoi(argv[3]);
    double rate=strtod(argv[4], NULL), warm=strtod(argv[5], NULL), duration=strtod(argv[6], NULL);
    int xp[2]={atoi(argv[7]),atoi(argv[9])}, yp[2]={atoi(argv[8]),atoi(argv[10])};
    int xorg=atoi(argv[11]), kwin=atoi(argv[12]), is_static=atoi(argv[13]);
    // Last argument is an explicit confirmation that the caller checked monitor geometry.
    if (strcmp(argv[14], "checked-layout") || width<=0 || width>3840 || height<=0 || height>2160
        || !isfinite(rate) || rate<=0 || rate>120 || !isfinite(warm) || warm<0 || warm>30
        || !isfinite(duration) || duration<=0 || duration>60 || rate*duration<0.5 || xorg<=0 || kwin<=0) return 2;
    signal(SIGINT,stop); signal(SIGTERM,stop); signal(SIGALRM,stop);
    prctl(PR_SET_PDEATHSIG,SIGTERM);
    alarm((unsigned)(warm+duration+15));
    size_t frame_bytes=(size_t)width*height*4, expected=frame_bytes*32;
    int fd=open(argv[1],O_RDONLY);
    struct stat st;
    if (fd<0 || fstat(fd,&st) || st.st_size!=(off_t)expected) { fprintf(stderr,"invalid corpus size\n"); return 1; }
    unsigned char *frames=mmap(NULL,expected,PROT_READ,MAP_PRIVATE,fd,0);
    close(fd);
    if (frames==MAP_FAILED) { perror("mmap"); return 1; }
    // Read the complete corpus before timing to avoid page faults during replay.
    volatile unsigned char prefault=0;
    for (size_t i=0;i<expected;i+=4096) prefault ^= frames[i];
    (void)prefault;
    Display *d=XOpenDisplay(NULL);
    if (!d || !XShmQueryExtension(d)) { fprintf(stderr,"X11 MIT-SHM unavailable\n"); return 1; }
    int screen=DefaultScreen(d), depth=DefaultDepth(d,screen);
    Window root=RootWindow(d,screen), window[2];
    GC gc[2]; XImage *im[2]; XShmSegmentInfo shm[2];
    for (int h=0;h<2;h++) {
        if (xp[h]<0 || yp[h]<0 || xp[h]+width>DisplayWidth(d,screen) || yp[h]+height>DisplayHeight(d,screen)) return 2;
        XSetWindowAttributes attrs={0};
        attrs.override_redirect=True; attrs.background_pixel=0;
        window[h]=XCreateWindow(d,root,xp[h],yp[h],width,height,0,depth,InputOutput,
            DefaultVisual(d,screen),CWOverrideRedirect|CWBackPixel,&attrs);
        XStoreName(d,window[h],"Vino DisplayLink comparison — Escape stops test");
        gc[h]=XCreateGC(d,window[h],0,NULL);
        memset(&shm[h],0,sizeof(shm[h]));
        im[h]=XShmCreateImage(d,DefaultVisual(d,screen),depth,ZPixmap,NULL,&shm[h],width,height);
        if (!im[h] || im[h]->bits_per_pixel!=32 || im[h]->byte_order!=LSBFirst
            || im[h]->bytes_per_line!=width*4 || im[h]->red_mask!=0xff0000
            || im[h]->green_mask!=0xff00 || im[h]->blue_mask!=0xff) { fprintf(stderr,"unsupported X pixel format\n"); return 1; }
        shm[h].shmid=shmget(IPC_PRIVATE,frame_bytes,IPC_CREAT|0600);
        if (shm[h].shmid<0) { perror("shmget"); return 1; }
        shm[h].shmaddr=shmat(shm[h].shmid,NULL,0);
        // Mark immediately: the kernel removes it even after interruption/error.
        shmctl(shm[h].shmid,IPC_RMID,NULL);
        if (shm[h].shmaddr==(void*)-1) { perror("shmat"); return 1; }
        im[h]->data=shm[h].shmaddr; shm[h].readOnly=False;
        XShmAttach(d,&shm[h]);
        XMapRaised(d,window[h]);
        memcpy(im[h]->data,frames+h*frame_bytes,frame_bytes);
        XShmPutImage(d,window[h],gc[h],im[h],0,0,0,0,width,height,False);
    }
    XSync(d,False);
    // Verify pixels from our own windows only. No desktop screenshot is captured.
    for(int h=0;h<2;h++) {
        XImage *check=XGetImage(d,window[h],0,0,width,height,AllPlanes,ZPixmap);
        if(!check) return 1;
        for(int k=0;k<16;k++) {
            int x=(k*113)%width,y=(k*61)%height;
            const unsigned char *p=frames+(size_t)h*frame_bytes+((size_t)y*width+x)*4;
            unsigned long want=(unsigned long)p[0]|((unsigned long)p[1]<<8)|((unsigned long)p[2]<<16);
            if((XGetPixel(check,x,y)&0xffffff)!=want) { fprintf(stderr,"replay pixel verification failed\n"); return 1; }
        }
        XDestroyImage(check);
    }
    int escape=XKeysymToKeycode(d,XK_Escape);
    size_t warm_frames=(size_t)ceil(warm*rate/16)*16;
    size_t count=(size_t)llround(rate*duration);
    if(!warm_frames) warm_frames=16;
    double *times=calloc(count,sizeof(double));
    if(!times) return 1;
    struct cpu before={0},after={0};
    double start=0,cpu_start=0,xorg_start=0,kwin_start=0,total=0;
    size_t completed=0,late=0;
    for(int phase=0;phase<2 && !stopped;phase++) {
        size_t n=phase?count:warm_frames;
        if(phase) {
            before=service_cpu(); xorg_start=process_cpu(xorg); kwin_start=process_cpu(kwin);
            cpu_start=seconds(CLOCK_PROCESS_CPUTIME_ID);
        }
        start=seconds(CLOCK_MONOTONIC);
        for(size_t i=0;i<n && !stopped;i++) {
            double one=seconds(CLOCK_MONOTONIC);
            char keys[32]; XQueryKeymap(d,keys);
            if(keys[escape/8]&(1<<(escape%8))) { stopped=1; break; }
            if(!is_static) {
                for(int h=0;h<2;h++) {
                    memcpy(im[h]->data,frames+((i%16)*2+h)*frame_bytes,frame_bytes);
                    XShmPutImage(d,window[h],gc[h],im[h],0,0,0,0,width,height,False);
                }
                XSync(d,False);
            }
            if(phase) {
                times[i]=(seconds(CLOCK_MONOTONIC)-one)*1000;
                if(seconds(CLOCK_MONOTONIC)>start+(i+1)/rate) late++;
                completed++;
            }
            until(start+(i+1)/rate);
        }
        if(phase) total=seconds(CLOCK_MONOTONIC)-start;
    }
    double cpu_end=seconds(CLOCK_PROCESS_CPUTIME_ID);
    double xorg_end=process_cpu(xorg),kwin_end=process_cpu(kwin);
    after=service_cpu();
    for(int h=0;h<2;h++) {
        XShmDetach(d,&shm[h]); XSync(d,False);
        im[h]->data=NULL; XDestroyImage(im[h]); shmdt(shm[h].shmaddr);
        XFreeGC(d,gc[h]); XDestroyWindow(d,window[h]);
    }
    XCloseDisplay(d); munmap(frames,expected);
    if(stopped || completed!=count || total<=0 || after.total<before.total) {
        fprintf(stderr,"replay stopped or invalid measurement\n"); free(times); return 130;
    }
    qsort(times,count,sizeof(double),compare_double);
    printf("{\"scope\":\"live official service; output delivery unverified\",\"frame_pairs_submitted\":%zu,"
        "\"seconds\":%.9f,\"input_pairs_per_second\":%.6f,\"static_hold\":%s,"
        "\"displaylink_cpu_seconds\":%.9f,\"displaylink_user_seconds\":%.9f,\"displaylink_system_seconds\":%.9f,"
        "\"displaylink_cpu_cores\":%.9f,\"xorg_cpu_cores\":%.9f,\"kwin_cpu_cores\":%.9f,\"player_cpu_cores\":%.9f,"
        "\"input_deadline_misses\":%zu,\"x_submission_p99_ms\":%.6f,\"pixel_checks_passed\":32}\n",
        is_static?(size_t)0:completed,total,is_static?0:completed/total,is_static?"true":"false",
        after.total-before.total,after.user-before.user,after.system-before.system,
        (after.total-before.total)/total,(xorg_end-xorg_start)/total,(kwin_end-kwin_start)/total,
        (cpu_end-cpu_start)/total,late,times[(size_t)llround((count-1)*.99)]);
    free(times); return 0;
}
