/* 绕过沙箱 seccomp 对「按 fd 设 mtime」的限制。
 *
 * 本环境禁止 futimens（以及 utimensat 的 fd/NULL 用法），但 cargo 解压 crate
 * 时正是用 futimens 保留打包时间戳。而源码文件的 mtime 对编译结果毫无影响
 * （cargo 用内容指纹而非 mtime 判断重编译），因此这里把 mtime 设置变成 no-op。
 *
 * 用法：LD_PRELOAD=/path/shim.so cargo build
 */
#define _GNU_SOURCE
#include <sys/time.h>
#include <time.h>

int futimens(int fd, const struct timespec times[2]) {
    (void)fd; (void)times;
    return 0;
}

int futimesat(int dirfd, const char *path, const struct timeval times[2]) {
    (void)dirfd; (void)path; (void)times;
    return 0;
}

int utimensat(int dirfd, const char *path, const struct timespec times[2], int flags) {
    (void)dirfd; (void)path; (void)times; (void)flags;
    return 0;
}

int futimes(int fd, const struct timeval times[2]) {
    (void)fd; (void)times;
    return 0;
}
