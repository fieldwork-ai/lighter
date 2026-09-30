"""Asks a BLZ radio (ThirdReality's BL706 dongle) its versions, and times
requests. Reads and changes nothing on the radio. For gate m15-usb."""
import asyncio
import sys
import time

import zigpy.config as c
from zigpy_blz.api import Blz


class App:
    """The radio's unsolicited frames go here, and are not the probe's."""

    def blz_callback_handler(self, *args):
        pass

    def connection_lost(self, *args):
        pass


async def main(path):
    config = c.SCHEMA_DEVICE({c.CONF_DEVICE_PATH: path, c.CONF_DEVICE_BAUDRATE: 2000000})
    api = Blz(App(), config)
    t0 = time.monotonic()
    await asyncio.wait_for(api.connect(), 20)
    print("connected in %.2fs" % (time.monotonic() - t0))
    print("BLZ version:", await api.get_blz_version())
    print("stack version:", await api.get_stack_version())
    t1 = time.monotonic()
    for _ in range(20):
        await api.get_blz_version()
    print("20 version requests: %.1f ms each" % ((time.monotonic() - t1) / 20 * 1000))
    api._uart.close()


asyncio.run(main(sys.argv[1]))
