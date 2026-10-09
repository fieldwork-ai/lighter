// A vmnet interface, bridged to one of the Mac's cards or host-only,
// relayed to a datagram socket.
//
// Both directions run on one serial dispatch queue: vmnet's "packets
// available" event drains the interface into the socket, and a read source
// on the socket drains it into the interface. One frame is one datagram, so
// the far end needs no framing, and a full socket drops a frame as a full
// NIC ring does. The far end is the VMM's LAN or link card, in this process
// when lighter holds com.apple.vm.networking, or, for the LAN, in another
// when a root helper runs this for it.
#include <dispatch/dispatch.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>
#include <vmnet/vmnet.h>

#define BATCH 32

struct lighter_bridge {
	interface_ref iface;
	dispatch_queue_t queue;
	dispatch_source_t from_guest;
	int fd;
	size_t max_packet;
	uint8_t *in_bufs;  // BATCH * max_packet, for the interface's side
	uint8_t *out_bufs; // BATCH * max_packet, for the socket's side
	uint64_t to_guest, to_lan, dropped;
};

static void set_error(char *err, size_t len, const char *what, long code) {
	if (err && len) snprintf(err, len, "%s (%ld)", what, code);
}

// The interface's frames, into the socket. Runs on the queue.
static void drain_interface(struct lighter_bridge *b) {
	for (;;) {
		struct iovec iov[BATCH];
		struct vmpktdesc pkts[BATCH];
		for (int i = 0; i < BATCH; i++) {
			iov[i].iov_base = b->in_bufs + (size_t)i * b->max_packet;
			iov[i].iov_len = b->max_packet;
			pkts[i].vm_pkt_size = b->max_packet;
			pkts[i].vm_pkt_iov = &iov[i];
			pkts[i].vm_pkt_iovcnt = 1;
			pkts[i].vm_flags = 0;
		}
		int count = BATCH;
		if (vmnet_read(b->iface, pkts, &count) != VMNET_SUCCESS || count <= 0) return;
		for (int i = 0; i < count; i++) {
			if (send(b->fd, iov[i].iov_base, pkts[i].vm_pkt_size, MSG_DONTWAIT) < 0)
				b->dropped++;
			else
				b->to_guest++;
		}
		if (count < BATCH) return;
	}
}

// The socket's frames, into the interface. Runs on the queue.
static void drain_socket(struct lighter_bridge *b) {
	for (;;) {
		struct iovec iov[BATCH];
		struct vmpktdesc pkts[BATCH];
		int count = 0;
		while (count < BATCH) {
			uint8_t *buf = b->out_bufs + (size_t)count * b->max_packet;
			ssize_t n = recv(b->fd, buf, b->max_packet, MSG_DONTWAIT);
			if (n <= 0) break;
			iov[count].iov_base = buf;
			iov[count].iov_len = (size_t)n;
			pkts[count].vm_pkt_size = (size_t)n;
			pkts[count].vm_pkt_iov = &iov[count];
			pkts[count].vm_pkt_iovcnt = 1;
			pkts[count].vm_flags = 0;
			count++;
		}
		if (count == 0) return;
		int written = count;
		if (vmnet_write(b->iface, pkts, &written) != VMNET_SUCCESS) written = 0;
		b->to_lan += (uint64_t)written;
		b->dropped += (uint64_t)(count - written);
		if (count < BATCH) return;
	}
}

// Starts the interface `desc` describes and relays it to `fd`. Takes `desc`.
static struct lighter_bridge *start(xpc_object_t desc, const char *what, int fd, uint32_t *mtu_out,
                                    uint32_t *max_packet_out, char *err, size_t errlen) {
	struct lighter_bridge *b = calloc(1, sizeof *b);
	if (!b) {
		xpc_release(desc);
		set_error(err, errlen, "out of memory", 0);
		return NULL;
	}
	b->fd = fd;
	b->queue = dispatch_queue_create("dev.lighter.bridge", DISPATCH_QUEUE_SERIAL);
	dispatch_semaphore_t started = dispatch_semaphore_create(0);
	__block vmnet_return_t status = VMNET_FAILURE;
	__block uint64_t mtu = 0, max_packet = 0;
	b->iface = vmnet_start_interface(desc, b->queue, ^(vmnet_return_t s, xpc_object_t params) {
		status = s;
		if (s == VMNET_SUCCESS && params) {
			mtu = xpc_dictionary_get_uint64(params, vmnet_mtu_key);
			max_packet = xpc_dictionary_get_uint64(params, vmnet_max_packet_size_key);
		}
		dispatch_semaphore_signal(started);
	});
	xpc_release(desc);
	if (!b->iface) {
		char why[160];
		snprintf(why, sizeof why, "vmnet refused the %s interface (root, or com.apple.vm.networking, is needed)", what);
		set_error(err, errlen, why, 0);
		dispatch_release(b->queue);
		free(b);
		return NULL;
	}
	if (dispatch_semaphore_wait(started, dispatch_time(DISPATCH_TIME_NOW, 10 * NSEC_PER_SEC)) != 0 ||
	    status != VMNET_SUCCESS) {
		char why[160];
		snprintf(why, sizeof why, "vmnet could not start the %s interface", what);
		set_error(err, errlen, why, (long)status);
		dispatch_release(started);
		dispatch_release(b->queue);
		free(b);
		return NULL;
	}
	dispatch_release(started);
	b->max_packet = max_packet ? (size_t)max_packet : 1514;
	b->in_bufs = malloc((size_t)BATCH * b->max_packet);
	b->out_bufs = malloc((size_t)BATCH * b->max_packet);
	int flags = fcntl(fd, F_GETFL);
	fcntl(fd, F_SETFL, flags | O_NONBLOCK);
	vmnet_interface_set_event_callback(b->iface, VMNET_INTERFACE_PACKETS_AVAILABLE, b->queue,
	                                   ^(interface_event_t e, xpc_object_t event) {
		                                   (void)e;
		                                   (void)event;
		                                   drain_interface(b);
	                                   });
	b->from_guest = dispatch_source_create(DISPATCH_SOURCE_TYPE_READ, (uintptr_t)fd, 0, b->queue);
	dispatch_source_set_event_handler(b->from_guest, ^{
		drain_socket(b);
	});
	dispatch_resume(b->from_guest);
	if (mtu_out) *mtu_out = (uint32_t)mtu;
	if (max_packet_out) *max_packet_out = (uint32_t)b->max_packet;
	return b;
}

struct lighter_bridge *lighter_bridge_start(const char *ifname, const uint8_t mac[6], int fd, uint32_t *mtu_out,
                                            uint32_t *max_packet_out, char *err, size_t errlen) {
	(void)mac; // the guest's frames carry its own; vmnet bridges them as they are
	xpc_object_t desc = xpc_dictionary_create(NULL, NULL, 0);
	xpc_dictionary_set_uint64(desc, vmnet_operation_mode_key, VMNET_BRIDGED_MODE);
	xpc_dictionary_set_string(desc, vmnet_shared_interface_name_key, ifname);
	xpc_dictionary_set_bool(desc, vmnet_allocate_mac_address_key, false);
	return start(desc, "bridged", fd, mtu_out, max_packet_out, err, errlen);
}

// A network between the Mac and the guest alone: the Mac gets an interface
// (bridge100 and on) at `host_ip`/`mask`, with a route to the subnet, and
// nothing is shared with any other network. `network` names it, so a VM
// started again with the same identifier joins the same network.
struct lighter_bridge *lighter_host_link_start(const char *host_ip, const char *mask, const uint8_t network[16], int fd,
                                               uint32_t *mtu_out, uint32_t *max_packet_out, char *err,
                                               size_t errlen) {
	xpc_object_t desc = xpc_dictionary_create(NULL, NULL, 0);
	xpc_dictionary_set_uint64(desc, vmnet_operation_mode_key, VMNET_HOST_MODE);
	xpc_dictionary_set_uuid(desc, vmnet_network_identifier_key, network);
	xpc_dictionary_set_string(desc, vmnet_host_ip_address_key, host_ip);
	xpc_dictionary_set_string(desc, vmnet_host_subnet_mask_key, mask);
	xpc_dictionary_set_bool(desc, vmnet_allocate_mac_address_key, false);
	return start(desc, "host", fd, mtu_out, max_packet_out, err, errlen);
}

void lighter_bridge_counters(struct lighter_bridge *b, uint64_t out[3]) {
	__block uint64_t a = 0, c = 0, d = 0;
	dispatch_sync(b->queue, ^{
		a = b->to_guest;
		c = b->to_lan;
		d = b->dropped;
	});
	out[0] = a;
	out[1] = c;
	out[2] = d;
}

void lighter_bridge_stop(struct lighter_bridge *b) {
	if (!b) return;
	dispatch_source_cancel(b->from_guest);
	vmnet_interface_set_event_callback(b->iface, VMNET_INTERFACE_PACKETS_AVAILABLE, NULL, NULL);
	dispatch_semaphore_t stopped = dispatch_semaphore_create(0);
	vmnet_return_t r = vmnet_stop_interface(b->iface, b->queue, ^(vmnet_return_t s) {
		(void)s;
		dispatch_semaphore_signal(stopped);
	});
	if (r == VMNET_SUCCESS) dispatch_semaphore_wait(stopped, dispatch_time(DISPATCH_TIME_NOW, 3 * NSEC_PER_SEC));
	dispatch_release(stopped);
	// Anything the queue still has to run finishes before the buffers go.
	dispatch_sync(b->queue, ^{
	});
	dispatch_release(b->from_guest);
	dispatch_release(b->queue);
	free(b->in_bufs);
	free(b->out_bufs);
	free(b);
}

// The interfaces vmnet can bridge, newline-separated, into buf. Returns the
// count, or -1.
int lighter_bridge_interfaces(char *buf, size_t len) {
	xpc_object_t list = vmnet_copy_shared_interface_list();
	if (!list) return -1;
	size_t used = 0;
	int count = (int)xpc_array_get_count(list);
	if (len) buf[0] = 0;
	for (int i = 0; i < count; i++) {
		const char *name = xpc_array_get_string(list, (size_t)i);
		if (!name) continue;
		int n = snprintf(buf + used, len > used ? len - used : 0, "%s\n", name);
		if (n > 0) used += (size_t)n;
	}
	xpc_release(list);
	return count;
}
