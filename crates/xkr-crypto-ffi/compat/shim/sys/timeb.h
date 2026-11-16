/* Fallback <sys/timeb.h> for C libraries that no longer ship it (oaes_lib.c includes it on
   Linux). Searched with -idirafter, so a system header wins when one exists. */
#ifndef XKR_COMPAT_SHIM_SYS_TIMEB_H
#define XKR_COMPAT_SHIM_SYS_TIMEB_H

#include <sys/time.h>
#include <time.h>

struct timeb
{
    time_t time;
    unsigned short millitm;
    short timezone;
    short dstflag;
};

static inline int ftime(struct timeb *tp)
{
    struct timeval tv;
    gettimeofday(&tv, 0);
    tp->time = tv.tv_sec;
    tp->millitm = (unsigned short)(tv.tv_usec / 1000);
    tp->timezone = 0;
    tp->dstflag = 0;
    return 0;
}

#endif
