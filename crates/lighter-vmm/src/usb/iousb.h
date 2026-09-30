// The Mac's side of a USB device lighter serves to its guest: IOUSBHost,
// behind a C interface for iousb.rs. See iousb.m.
#pragma once
#include <stddef.h>
#include <stdint.h>

typedef struct lighter_usb lighter_usb;

// A finished operation. `status` is a Linux URB status (0, or a negative
// errno); `data` is what an in transfer received, `actual` bytes of it (for
// an out transfer, `actual` is what was sent and `data` is NULL).
typedef void (*lighter_usb_done_fn)(void *ctx, uint32_t tag, int32_t status, uint32_t actual,
                                    const uint8_t *data);
// The device went away: unplugged, or terminated by macOS.
typedef void (*lighter_usb_gone_fn)(void *ctx);
// The device's last callback: after it, nothing refers to `ctx`.
typedef void (*lighter_usb_closed_fn)(void *ctx);

typedef struct {
    uint64_t registry_id;
    uint32_t location_id;
    uint16_t vendor_id;
    uint16_t product_id;
    uint16_t bcd_device;
    uint8_t device_class;
    // IOUSBHost's speed: 0 low, 1 full, 2 high, 3 super, 4 super+.
    uint8_t speed;
    // Bit n set: an interface of class n (n < 64); bits for the classes above
    // 63 lighter cares about: 62 wireless controller (0xe0), 63 vendor (0xff).
    uint64_t interface_classes;
    char vendor[128];
    char product[128];
    char serial[128];
    // The macOS driver bound to the device's first interface, if any.
    char driver[128];
    // The device's serial port on the Mac (`/dev/cu.*`), if it has one.
    char callout[128];
} lighter_usb_info;

// Fills up to `max` entries; returns how many USB devices there are.
int lighter_usb_list(lighter_usb_info *out, int max);

// Seizes the device from its macOS driver (as the user: no entitlement, no
// root) and unconfigures it, so the guest meets it as a device just plugged
// in. NULL and a message in `error` if it cannot be had.
lighter_usb *lighter_usb_open(uint64_t registry_id, void *ctx, lighter_usb_done_fn done,
                              lighter_usb_gone_fn gone, lighter_usb_closed_fn closed, char *error,
                              size_t error_len);

// `is_in`: a device-to-host request, `in_len` bytes back; otherwise `out`.
void lighter_usb_control(lighter_usb *d, uint32_t tag, const uint8_t setup[8], int is_in,
                         const uint8_t *out, uint32_t out_len, uint32_t in_len);
// `zero_packet`: an out transfer that is a whole number of packets ends with
// a zero-length one, and completes when that has gone.
void lighter_usb_transfer(lighter_usb *d, uint32_t tag, uint8_t endpoint, const uint8_t *out,
                          uint32_t out_len, uint32_t in_len, int zero_packet);
void lighter_usb_set_configuration(lighter_usb *d, uint32_t tag, uint8_t value);
void lighter_usb_set_interface(lighter_usb *d, uint32_t tag, uint8_t interface, uint8_t alternate);
void lighter_usb_clear_halt(lighter_usb *d, uint32_t tag, uint8_t endpoint);
void lighter_usb_reset(lighter_usb *d, uint32_t tag);
// Stops every transfer queued on `endpoint` (0: the default pipe).
void lighter_usb_abort(lighter_usb *d, uint8_t endpoint);
// Gives the device back to macOS and frees it; `closed` is called last.
void lighter_usb_close(lighter_usb *d);

// Whether a process other than this one holds `path` open, and its pid.
int lighter_usb_port_holder(const char *path, int32_t *pid, char *name, size_t name_len);

// Calls `arrived(ctx)` whenever a USB device appears on the Mac, from a
// queue of its own, for as long as the process runs.
void lighter_usb_watch(void *ctx, void (*arrived)(void *ctx));

// Gives a device back to macOS that a lighter which is no longer running
// left unconfigured (it crashed holding it): seized again, then configured
// with macOS's drivers matched. Synchronous. 0 on success, or when the
// device is no longer attached; -1 and a message otherwise.
int lighter_usb_restore(uint64_t registry_id, char *error, size_t error_len);
