/* Stand-in for the C library header videodev2.h includes: the two structs it needs, as x86_64 Linux lays them out. */
#ifndef LIGHTER_SYS_TIME_H
#define LIGHTER_SYS_TIME_H
struct timeval { long tv_sec; long tv_usec; };
struct timespec { long tv_sec; long tv_nsec; };
#endif
