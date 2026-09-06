// Read Activity Monitor's task ledger without walking every VM region.
#include <errno.h>
#include <limits.h>
#include <mach/mach.h>
#include <mach/task_info.h>
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    char *end;
    errno = 0;
    long pid = strtol(argv[1], &end, 10);
    if (errno || !*argv[1] || *end || pid <= 0 || pid > INT_MAX) return 2;
    mach_port_t task = MACH_PORT_NULL;
    kern_return_t result = task_name_for_pid(mach_task_self(), (int)pid, &task);
    if (result != KERN_SUCCESS) return 1;
    task_vm_info_data_t info = {0};
    mach_msg_type_number_t count = TASK_VM_INFO_COUNT;
    result = task_info(task, TASK_VM_INFO, (task_info_t)&info, &count);
    mach_port_deallocate(mach_task_self(), task);
    if (result != KERN_SUCCESS || count < TASK_VM_INFO_REV1_COUNT) return 1;
    printf("%llu\n", (unsigned long long)info.phys_footprint);
    return 0;
}
