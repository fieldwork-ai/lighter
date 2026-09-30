"""Brings up a Zigbee network on a BLZ radio (ThirdReality's BL706 dongle)
the way ZHA does, through zigpy: formed on first start, resumed after. Prints
the network as one JSON line. For gate m15-usb part two."""
import asyncio
import json
import sys

import zigpy.config as c
from zigpy_blz.zigbee.application import ControllerApplication


async def main(path, database):
    # Raw: `new` validates it, and a validated config does not validate twice.
    config = {
        c.CONF_DEVICE: {c.CONF_DEVICE_PATH: path, c.CONF_DEVICE_BAUDRATE: 2000000},
        c.CONF_DATABASE: database,
    }
    app = await asyncio.wait_for(ControllerApplication.new(config, auto_form=True, start_radio=True), 120)
    try:
        network, node = app.state.network_info, app.state.node_info
        print(
            json.dumps(
                {
                    "ieee": str(node.ieee),
                    "pan_id": f"0x{network.pan_id:04x}",
                    "extended_pan_id": str(network.extended_pan_id),
                    "channel": network.channel,
                }
            )
        )
    finally:
        await app.shutdown()


asyncio.run(main(sys.argv[1], sys.argv[2]))
