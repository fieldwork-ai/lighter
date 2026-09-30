// The Mac's side of a USB device lighter serves to its guest.
//
// A device is seized from its macOS driver with IOUSBHostDevice's
// DeviceSeize, which is not gated: DeviceCapture needs the
// com.apple.vm.device-access entitlement or root, and lighter runs as the
// user with neither. Apple's own drivers let go when asked (measured with
// the kernel CDC-ACM driver and the DriverKit CH34x one). The device is then
// unconfigured, which tears down macOS's interface drivers, and left so: the
// guest meets it as a device just plugged in and configures it itself.
//
// Everything a device does runs on its own serial dispatch queue: IOUSBHost
// completes transfers there (the interfaces are given the same queue), the
// operations IOUSBHost offers only synchronously (configure, alternate
// settings, clearing a stall, reset) run there as blocks, and closing is the
// queue's last block. Nothing here blocks the caller, and nothing calls back
// after `closed`.

#import <Foundation/Foundation.h>
#import <IOKit/IOKitLib.h>
#import <IOKit/usb/IOUSBLib.h>
#import <IOUSBHost/IOUSBHost.h>
#include <libproc.h>
#include <sys/proc_info.h>
#include <errno.h>
#include "iousb.h"

// Linux URB statuses, as the guest's drivers read them.
static int32_t status_of(IOReturn r) {
    switch (r) {
    case kIOReturnSuccess:
    case kIOReturnUnderrun:  // a short in transfer: data, not a failure
        return 0;
    case kIOUSBPipeStalled:
        return -EPIPE;
    case kIOReturnAborted:
        return -ECONNRESET;
    case kIOReturnNoDevice:
    case kIOReturnNotAttached:
    case kIOReturnNotResponding:
    case kIOReturnOffline:
        return -ESHUTDOWN;
    case kIOReturnTimeout:
    case kIOUSBTransactionTimeout:
        return -ETIMEDOUT;
    case kIOReturnOverrun:
        return -EOVERFLOW;
    case kIOReturnBadArgument:
        return -EINVAL;
    case kIOReturnNoMemory:
        return -ENOMEM;
    default:
        return -EPROTO;
    }
}

static int32_t status_of_error(NSError *e) {
    return e ? status_of((IOReturn)e.code) : -EPROTO;
}

// The handle the caller holds. Every block captures the holder instead,
// which outlives the handle: blocks for requests aborted at close can run
// after the handle is gone, and must find `closed` set, not freed memory.
struct lighter_usb {
    void *holder;
};

@interface LighterUsbHolder : NSObject
@property(assign) void *ctx;
@property(assign) lighter_usb_done_fn done;
@property(assign) lighter_usb_gone_fn gone;
@property(assign) lighter_usb_closed_fn closedFn;
@property(strong) dispatch_queue_t queue;
@property(strong) IOUSBHostDevice *device;
// Interface number to the open interface of the current configuration.
@property(strong) NSMutableDictionary<NSNumber *, IOUSBHostInterface *> *interfaces;
// Endpoint address to its pipe, looked up on first use.
@property(strong) NSMutableDictionary<NSNumber *, IOUSBHostPipe *> *pipes;
@property(assign) BOOL closed;
@property(assign) BOOL goneReported;
@end

@implementation LighterUsbHolder
@end

static LighterUsbHolder *holder_of(lighter_usb *d) { return (__bridge LighterUsbHolder *)d->holder; }

static void finish(LighterUsbHolder *h, uint32_t tag, int32_t status, uint32_t actual, NSData *data) {
    if (h.closed) return;
    h.done(h.ctx, tag, status, actual, data ? (const uint8_t *)data.bytes : NULL);
}

static void report_gone(LighterUsbHolder *h) {
    if (h.closed || h.goneReported) return;
    h.goneReported = YES;
    h.gone(h.ctx);
}

// Closes the interfaces and forgets the pipes: before a configuration or an
// alternate setting changes, and at close.
static void drop_interfaces(LighterUsbHolder *h) {
    for (IOUSBHostInterface *i in h.interfaces.allValues) [i destroy];
    [h.interfaces removeAllObjects];
    [h.pipes removeAllObjects];
}

// Opens every interface of the device's current configuration, on the
// device's queue.
static void open_interfaces(LighterUsbHolder *h) {
    io_iterator_t it = IO_OBJECT_NULL;
    if (IORegistryEntryGetChildIterator(h.device.ioService, kIOServicePlane, &it) != KERN_SUCCESS) return;
    io_service_t child;
    while ((child = IOIteratorNext(it))) {
        if (IOObjectConformsTo(child, "IOUSBHostInterface")) {
            NSError *e = nil;
            IOUSBHostInterface *i = [[IOUSBHostInterface alloc] initWithIOService:child
                                                                          options:IOUSBHostObjectInitOptionsNone
                                                                            queue:h.queue
                                                                            error:&e
                                                                  interestHandler:nil];
            if (i) h.interfaces[@(i.interfaceDescriptor->bInterfaceNumber)] = i;
        }
        IOObjectRelease(child);
    }
    IOObjectRelease(it);
}

static IOUSBHostPipe *pipe_for(LighterUsbHolder *h, uint8_t endpoint) {
    IOUSBHostPipe *p = h.pipes[@(endpoint)];
    if (p) return p;
    for (IOUSBHostInterface *i in h.interfaces.allValues) {
        NSError *e = nil;
        p = [i copyPipeWithAddress:endpoint error:&e];
        if (p) {
            h.pipes[@(endpoint)] = p;
            return p;
        }
    }
    return nil;
}

// The object a control request is sent through: the interface it names, for
// a request to an interface or an endpoint, and the device otherwise.
static IOUSBHostObject *control_target(LighterUsbHolder *h, const uint8_t setup[8]) {
    uint8_t recipient = setup[0] & 0x1f;
    uint16_t index = (uint16_t)(setup[4] | (setup[5] << 8));
    if (recipient == 1) {
        IOUSBHostInterface *i = h.interfaces[@(index & 0xff)];
        if (i) return i;
    } else if (recipient == 2) {
        for (IOUSBHostInterface *i in h.interfaces.allValues) {
            NSError *e = nil;
            if ([i copyPipeWithAddress:(index & 0xff) error:&e]) return i;
        }
    }
    return h.device;
}

lighter_usb *lighter_usb_open(uint64_t registry_id, void *ctx, lighter_usb_done_fn done,
                              lighter_usb_gone_fn gone, lighter_usb_closed_fn closed, char *error,
                              size_t error_len) {
    @autoreleasepool {
        io_service_t service =
            IOServiceGetMatchingService(kIOMainPortDefault, IORegistryEntryIDMatching(registry_id));
        if (service == IO_OBJECT_NULL) {
            snprintf(error, error_len, "the device is no longer attached");
            return NULL;
        }
        LighterUsbHolder *h = [LighterUsbHolder new];
        h.ctx = ctx;
        h.done = done;
        h.gone = gone;
        h.closedFn = closed;
        h.queue = dispatch_queue_create("dev.lighter.usb", DISPATCH_QUEUE_SERIAL);
        h.interfaces = [NSMutableDictionary dictionary];
        h.pipes = [NSMutableDictionary dictionary];
        __weak LighterUsbHolder *weak = h;
        NSError *e = nil;
        h.device = [[IOUSBHostDevice alloc]
            initWithIOService:service
                      options:IOUSBHostObjectInitOptionsDeviceSeize
                        queue:h.queue
                        error:&e
              interestHandler:^(IOUSBHostObject *object, uint32_t type, void *argument) {
                  (void)object;
                  (void)argument;
                  LighterUsbHolder *strong = weak;
                  if (strong && type == kIOMessageServiceIsTerminated) report_gone(strong);
              }];
        IOObjectRelease(service);
        if (!h.device) {
            snprintf(error, error_len, "macOS would not let go of the device: %s (0x%08x)",
                     e.localizedDescription.UTF8String, (unsigned)e.code);
            return NULL;
        }
        if (![h.device configureWithValue:0 matchInterfaces:NO error:&e]) {
            snprintf(error, error_len, "the device would not unconfigure: %s (0x%08x)",
                     e.localizedDescription.UTF8String, (unsigned)e.code);
            [h.device destroy];
            return NULL;
        }
        lighter_usb *d = calloc(1, sizeof *d);
        d->holder = (__bridge_retained void *)h;
        return d;
    }
}

void lighter_usb_control(lighter_usb *d, uint32_t tag, const uint8_t setup[8], int is_in,
                         const uint8_t *out, uint32_t out_len, uint32_t in_len) {
    // A struct, since a block cannot capture an array.
    struct { uint8_t b[8]; } s;
    memcpy(s.b, setup, 8);
    NSData *payload = out_len ? [NSData dataWithBytes:out length:out_len] : nil;
    LighterUsbHolder *h = holder_of(d);
    dispatch_async(h.queue, ^{
        @autoreleasepool {
            if (h.closed) return;
            IOUSBDeviceRequest request = {
                .bmRequestType = s.b[0],
                .bRequest = s.b[1],
                .wValue = (uint16_t)(s.b[2] | (s.b[3] << 8)),
                .wIndex = (uint16_t)(s.b[4] | (s.b[5] << 8)),
                .wLength = (uint16_t)(s.b[6] | (s.b[7] << 8)),
            };
            NSMutableData *data = nil;
            if (is_in) data = in_len ? [NSMutableData dataWithLength:in_len] : nil;
            else if (payload) data = [payload mutableCopy];
            NSError *e = nil;
            // No timeout: the guest's driver times a request out itself, by
            // unlinking it, as it would on a Linux host.
            BOOL queued = [control_target(h, s.b) enqueueDeviceRequest:request
                                                                data:data
                                                   completionTimeout:0
                                                               error:&e
                                                   completionHandler:^(IOReturn r, NSUInteger n) {
                                                       finish(h, tag, status_of(r), (uint32_t)n,
                                                              is_in ? data : nil);
                                                   }];
            if (!queued) finish(h, tag, status_of_error(e), 0, nil);
        }
    });
}

void lighter_usb_transfer(lighter_usb *d, uint32_t tag, uint8_t endpoint, const uint8_t *out,
                          uint32_t out_len, uint32_t in_len, int zero_packet) {
    NSData *payload = out_len ? [NSData dataWithBytes:out length:out_len] : nil;
    LighterUsbHolder *h = holder_of(d);
    dispatch_async(h.queue, ^{
        @autoreleasepool {
            if (h.closed) return;
            IOUSBHostPipe *pipe = pipe_for(h, endpoint);
            if (!pipe) {
                finish(h, tag, -EPIPE, 0, nil);
                return;
            }
            BOOL is_in = (endpoint & 0x80) != 0;
            NSMutableData *data = is_in ? [NSMutableData dataWithLength:in_len]
                                        : (payload ? [payload mutableCopy] : nil);
            uint16_t max_packet = OSSwapLittleToHostInt16(pipe.descriptors->descriptor.wMaxPacketSize) & 0x7ff;
            BOOL zlp = !is_in && zero_packet && out_len && max_packet && out_len % max_packet == 0;
            NSError *e = nil;
            if (!zlp) {
                BOOL queued = [pipe enqueueIORequestWithData:data
                                           completionTimeout:0
                                                       error:&e
                                           completionHandler:^(IOReturn r, NSUInteger n) {
                                               finish(h, tag, status_of(r), (uint32_t)n, is_in ? data : nil);
                                           }];
                if (!queued) finish(h, tag, status_of_error(e), 0, nil);
                return;
            }
            // Linux's URB_ZERO_PACKET: the data, then a zero-length packet
            // queued straight behind it, so nothing can come between them.
            // A pipe completes in order, on this queue: the data's handler
            // runs first, and the URB is answered when the packet has gone.
            __block IOReturn sent = kIOReturnSuccess;
            __block NSUInteger sent_n = 0;
            // Set if the packet could not be queued: the data's own handler
            // answers. Neither handler can run before this block returns.
            __block BOOL alone = NO;
            BOOL queued = [pipe enqueueIORequestWithData:data
                                       completionTimeout:0
                                                   error:&e
                                       completionHandler:^(IOReturn r, NSUInteger n) {
                                           sent = r;
                                           sent_n = n;
                                           if (alone) finish(h, tag, status_of(r), (uint32_t)n, nil);
                                       }];
            if (!queued) {
                finish(h, tag, status_of_error(e), 0, nil);
                return;
            }
            queued = [pipe enqueueIORequestWithData:nil
                                  completionTimeout:0
                                              error:&e
                                  completionHandler:^(IOReturn r, NSUInteger n) {
                                      (void)n;
                                      IOReturn status = sent != kIOReturnSuccess ? sent : r;
                                      finish(h, tag, status_of(status), (uint32_t)sent_n, nil);
                                  }];
            if (!queued) alone = YES;
        }
    });
}

void lighter_usb_set_configuration(lighter_usb *d, uint32_t tag, uint8_t value) {
    LighterUsbHolder *h = holder_of(d);
    dispatch_async(h.queue, ^{
        @autoreleasepool {
            if (h.closed) return;
            drop_interfaces(h);
            NSError *e = nil;
            if (![h.device configureWithValue:value matchInterfaces:NO error:&e]) {
                finish(h, tag, status_of_error(e), 0, nil);
                return;
            }
            if (value) open_interfaces(h);
            finish(h, tag, 0, 0, nil);
        }
    });
}

void lighter_usb_set_interface(lighter_usb *d, uint32_t tag, uint8_t interface, uint8_t alternate) {
    LighterUsbHolder *h = holder_of(d);
    dispatch_async(h.queue, ^{
        @autoreleasepool {
            if (h.closed) return;
            IOUSBHostInterface *i = h.interfaces[@(interface)];
            if (!i) {
                finish(h, tag, -EINVAL, 0, nil);
                return;
            }
            NSError *e = nil;
            if (![i selectAlternateSetting:alternate error:&e]) {
                finish(h, tag, status_of_error(e), 0, nil);
                return;
            }
            // The interface's endpoints change with its setting.
            [h.pipes removeAllObjects];
            finish(h, tag, 0, 0, nil);
        }
    });
}

void lighter_usb_clear_halt(lighter_usb *d, uint32_t tag, uint8_t endpoint) {
    LighterUsbHolder *h = holder_of(d);
    dispatch_async(h.queue, ^{
        @autoreleasepool {
            if (h.closed) return;
            IOUSBHostPipe *pipe = pipe_for(h, endpoint);
            NSError *e = nil;
            if (!pipe) finish(h, tag, -EINVAL, 0, nil);
            else if (![pipe clearStallWithError:&e]) finish(h, tag, status_of_error(e), 0, nil);
            else finish(h, tag, 0, 0, nil);
        }
    });
}

void lighter_usb_reset(lighter_usb *d, uint32_t tag) {
    LighterUsbHolder *h = holder_of(d);
    dispatch_async(h.queue, ^{
        @autoreleasepool {
            if (h.closed) return;
            drop_interfaces(h);
            NSError *e = nil;
            if (![h.device resetWithError:&e]) finish(h, tag, status_of_error(e), 0, nil);
            else finish(h, tag, 0, 0, nil);
        }
    });
}

void lighter_usb_abort(lighter_usb *d, uint8_t endpoint) {
    LighterUsbHolder *h = holder_of(d);
    dispatch_async(h.queue, ^{
        @autoreleasepool {
            if (h.closed) return;
            NSError *e = nil;
            if (endpoint == 0) {
                [h.device abortDeviceRequestsWithOption:IOUSBHostAbortOptionAsynchronous error:&e];
                for (IOUSBHostInterface *i in h.interfaces.allValues)
                    [i abortDeviceRequestsWithOption:IOUSBHostAbortOptionAsynchronous error:&e];
            } else {
                [pipe_for(h, endpoint) abortWithOption:IOUSBHostAbortOptionAsynchronous error:&e];
            }
        }
    });
}

void lighter_usb_close(lighter_usb *d) {
    // The handle goes now; the holder, with the device, when the queue has
    // run this block and any the device's aborts queue after it.
    LighterUsbHolder *h = CFBridgingRelease(d->holder);
    free(d);
    dispatch_async(h.queue, ^{
        @autoreleasepool {
            h.closed = YES;
            // Back to macOS: unconfigured, then configured with its drivers
            // matched again, as a device just plugged in would be. A seized
            // device is not reset when it is released, so this is ours to do.
            drop_interfaces(h);
            NSError *e = nil;
            if (!h.goneReported) {
                const IOUSBDeviceDescriptor *dd = h.device.deviceDescriptor;
                [h.device configureWithValue:0 matchInterfaces:YES error:&e];
                const IOUSBConfigurationDescriptor *cd =
                    dd && dd->bNumConfigurations ? [h.device configurationDescriptorWithIndex:0 error:&e] : NULL;
                if (cd) [h.device configureWithValue:cd->bConfigurationValue matchInterfaces:YES error:&e];
            }
            [h.device destroy];
            h.device = nil;
            h.closedFn(h.ctx);
        }
    });
}

static void copy_string(io_service_t s, CFStringRef key, char *out, size_t len) {
    out[0] = 0;
    CFTypeRef v = IORegistryEntryCreateCFProperty(s, key, kCFAllocatorDefault, 0);
    if (v && CFGetTypeID(v) == CFStringGetTypeID()) CFStringGetCString(v, out, (CFIndex)len, kCFStringEncodingUTF8);
    if (v) CFRelease(v);
}

static uint64_t number(io_service_t s, CFStringRef key) {
    uint64_t n = 0;
    CFTypeRef v = IORegistryEntryCreateCFProperty(s, key, kCFAllocatorDefault, 0);
    if (v && CFGetTypeID(v) == CFNumberGetTypeID()) CFNumberGetValue(v, kCFNumberSInt64Type, &n);
    if (v) CFRelease(v);
    return n;
}

int lighter_usb_list(lighter_usb_info *out, int max) {
    @autoreleasepool {
        io_iterator_t it = IO_OBJECT_NULL;
        if (IOServiceGetMatchingServices(kIOMainPortDefault, IOServiceMatching("IOUSBHostDevice"), &it) != KERN_SUCCESS)
            return 0;
        int count = 0;
        io_service_t s;
        while ((s = IOIteratorNext(it))) {
            if (count < max) {
                lighter_usb_info *i = &out[count];
                memset(i, 0, sizeof *i);
                IORegistryEntryGetRegistryEntryID(s, &i->registry_id);
                i->location_id = (uint32_t)number(s, CFSTR("locationID"));
                i->vendor_id = (uint16_t)number(s, CFSTR("idVendor"));
                i->product_id = (uint16_t)number(s, CFSTR("idProduct"));
                i->bcd_device = (uint16_t)number(s, CFSTR("bcdDevice"));
                i->device_class = (uint8_t)number(s, CFSTR("bDeviceClass"));
                i->speed = (uint8_t)number(s, CFSTR("Device Speed"));
                copy_string(s, CFSTR("USB Vendor Name"), i->vendor, sizeof i->vendor);
                copy_string(s, CFSTR("USB Product Name"), i->product, sizeof i->product);
                copy_string(s, CFSTR("USB Serial Number"), i->serial, sizeof i->serial);
                CFTypeRef callout = IORegistryEntrySearchCFProperty(s, kIOServicePlane, CFSTR("IOCalloutDevice"),
                                                                    kCFAllocatorDefault, kIORegistryIterateRecursively);
                if (callout && CFGetTypeID(callout) == CFStringGetTypeID())
                    CFStringGetCString(callout, i->callout, sizeof i->callout, kCFStringEncodingUTF8);
                if (callout) CFRelease(callout);
                io_iterator_t children = IO_OBJECT_NULL;
                if (IORegistryEntryGetChildIterator(s, kIOServicePlane, &children) == KERN_SUCCESS) {
                    io_service_t c;
                    while ((c = IOIteratorNext(children))) {
                        if (IOObjectConformsTo(c, "IOUSBHostInterface")) {
                            uint64_t cls = number(c, CFSTR("bInterfaceClass"));
                            if (cls < 62) i->interface_classes |= 1ull << cls;
                            else if (cls == 0xe0) i->interface_classes |= 1ull << 62;
                            else if (cls == 0xff) i->interface_classes |= 1ull << 63;
                            if (!i->driver[0]) {
                                io_iterator_t drivers = IO_OBJECT_NULL;
                                if (IORegistryEntryGetChildIterator(c, kIOServicePlane, &drivers) == KERN_SUCCESS) {
                                    io_service_t drv = IOIteratorNext(drivers);
                                    if (drv) {
                                        copy_string(drv, CFSTR("CFBundleIdentifier"), i->driver, sizeof i->driver);
                                        IOObjectRelease(drv);
                                    }
                                    IOObjectRelease(drivers);
                                }
                            }
                        }
                        IOObjectRelease(c);
                    }
                    IOObjectRelease(children);
                }
            }
            count++;
            IOObjectRelease(s);
        }
        IOObjectRelease(it);
        return count;
    }
}

int lighter_usb_port_holder(const char *path, int32_t *pid, char *name, size_t name_len) {
    int n = proc_listpidspath(PROC_ALL_PIDS, 0, path, PROC_LISTPIDSPATH_EXCLUDE_EVTONLY, NULL, 0);
    if (n <= 0) return 0;
    pid_t *pids = calloc((size_t)n, 1);
    n = proc_listpidspath(PROC_ALL_PIDS, 0, path, PROC_LISTPIDSPATH_EXCLUDE_EVTONLY, pids, n);
    int found = 0;
    for (int k = 0; k < n / (int)sizeof(pid_t); k++) {
        if (pids[k] > 0 && pids[k] != getpid()) {
            *pid = pids[k];
            name[0] = 0;
            proc_name(pids[k], name, (uint32_t)name_len);
            found = 1;
            break;
        }
    }
    free(pids);
    return found;
}

static void drain(void *refcon, io_iterator_t it) {
    void **pair = refcon;
    io_service_t s;
    int any = 0;
    while ((s = IOIteratorNext(it))) {
        IOObjectRelease(s);
        any = 1;
    }
    if (any) ((void (*)(void *))pair[1])(pair[0]);
}

void lighter_usb_watch(void *ctx, void (*arrived)(void *ctx)) {
    IONotificationPortRef port = IONotificationPortCreate(kIOMainPortDefault);
    IONotificationPortSetDispatchQueue(port, dispatch_queue_create("dev.lighter.usb.watch", DISPATCH_QUEUE_SERIAL));
    void **pair = calloc(2, sizeof(void *));
    pair[0] = ctx;
    pair[1] = (void *)arrived;
    io_iterator_t it = IO_OBJECT_NULL;
    if (IOServiceAddMatchingNotification(port, kIOFirstMatchNotification, IOServiceMatching("IOUSBHostDevice"),
                                         drain, pair, &it) == KERN_SUCCESS) {
        // Arming the notification means draining what matches now; those
        // are not arrivals.
        io_service_t s;
        while ((s = IOIteratorNext(it))) IOObjectRelease(s);
    }
}

int lighter_usb_restore(uint64_t registry_id, char *error, size_t error_len) {
    @autoreleasepool {
        io_service_t service =
            IOServiceGetMatchingService(kIOMainPortDefault, IORegistryEntryIDMatching(registry_id));
        if (service == IO_OBJECT_NULL) return 0;
        NSError *e = nil;
        IOUSBHostDevice *device = [[IOUSBHostDevice alloc] initWithIOService:service
                                                                      options:IOUSBHostObjectInitOptionsDeviceSeize
                                                                        queue:nil
                                                                        error:&e
                                                              interestHandler:nil];
        IOObjectRelease(service);
        if (!device) {
            snprintf(error, error_len, "%s (0x%08x)", e.localizedDescription.UTF8String, (unsigned)e.code);
            return -1;
        }
        const IOUSBDeviceDescriptor *dd = device.deviceDescriptor;
        [device configureWithValue:0 matchInterfaces:YES error:&e];
        const IOUSBConfigurationDescriptor *cd =
            dd && dd->bNumConfigurations ? [device configurationDescriptorWithIndex:0 error:&e] : NULL;
        BOOL ok = cd ? [device configureWithValue:cd->bConfigurationValue matchInterfaces:YES error:&e] : NO;
        [device destroy];
        if (!ok) {
            snprintf(error, error_len, "%s (0x%08x)", e.localizedDescription.UTF8String, (unsigned)e.code);
            return -1;
        }
        return 0;
    }
}
