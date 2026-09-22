/* Stand-in for the stub headers_install writes. It strips the kernel-only
 * address-space annotations from the UAPI headers; here they are defined
 * away instead. */
#ifndef LIGHTER_LINUX_COMPILER_H
#define LIGHTER_LINUX_COMPILER_H
#define __user
#define __force
#define __iomem
#define __kernel
#endif
