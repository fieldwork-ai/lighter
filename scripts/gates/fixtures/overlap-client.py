# A raw TCP client for the stream gate's overlap check, run inside a
# container with NET_ADMIN: a handshake, sixteen bytes, and on "next" from
# stdin sixteen more starting eight bytes back, then the FIN. The kernel's
# own stack would answer the unexpected SYN-ACK with a reset, so resets
# from the client's port are dropped first.
import socket, struct, subprocess, sys, time
host, port = sys.argv[1], int(sys.argv[2]); sport = 40123
u = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); u.connect((host, port)); src = u.getsockname()[0]; u.close()
subprocess.run(['iptables', '-I', 'OUTPUT', '-p', 'tcp', '--tcp-flags', 'RST', 'RST', '--sport', str(sport), '-j', 'DROP'], check=True)
def checksum(b):
    if len(b) % 2: b += b'\0'
    n = sum(struct.unpack('!%dH' % (len(b) // 2), b)); n = (n >> 16) + (n & 65535); n += n >> 16
    return ~n & 65535
rx = socket.socket(socket.AF_INET, socket.SOCK_RAW, socket.IPPROTO_TCP); rx.settimeout(5)
tx = socket.socket(socket.AF_INET, socket.SOCK_RAW, socket.IPPROTO_RAW)
def send(seq, ack, flags, data=b''):
    tcp = struct.pack('!HHIIBBHHH', sport, port, seq, ack, 5 << 4, flags, 65535, 0, 0)
    pseudo = socket.inet_aton(src) + socket.inet_aton(host) + struct.pack('!BBH', 0, 6, len(tcp) + len(data))
    tcp = tcp[:16] + struct.pack('!H', checksum(pseudo + tcp + data)) + tcp[18:]
    ip = struct.pack('!BBHHHBBH4s4s', 0x45, 0, 40 + len(data), 42, 0, 64, 6, 0, socket.inet_aton(src), socket.inet_aton(host))
    ip = ip[:10] + struct.pack('!H', checksum(ip)) + ip[12:]
    tx.sendto(ip + tcp + data, (host, port))
send(1000, 0, 2)  # SYN
while True:
    b = rx.recv(65535); off = (b[0] & 15) * 4
    if len(b) < off + 20: continue
    sp, dp, seq, ack, do, flags, win, cs, urg = struct.unpack('!HHIIBBHHH', b[off:off + 20])
    if sp == port and dp == sport and flags & 18 == 18:  # SYN-ACK
        server = seq + 1; break
send(1001, server, 16)  # ACK
send(1001, server, 24, b'ABCDEFGHIJKLMNOP')  # PSH-ACK, 1001..1016
assert sys.stdin.readline().strip() == 'next'  # the Mac has those sixteen
send(1009, server, 24, b'IJKLMNOPQRSTUVWX')  # 1009..1024: eight already delivered
time.sleep(0.1)
send(1025, server, 17)  # FIN-ACK
time.sleep(0.5)
