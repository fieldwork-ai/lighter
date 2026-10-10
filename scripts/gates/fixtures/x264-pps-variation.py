#!/usr/bin/env python3
"""Make H.264 streams that change parameter sets without changing pictures."""

import re
import sys


def ue(bits, pos):
    zeros = 0
    while bits[pos] == "0":
        zeros += 1
        pos += 1
    pos += 1
    return (1 << zeros) - 1 + int(bits[pos : pos + zeros] or "0", 2), pos + zeros


def encode_ue(value):
    binary = format(value + 1, "b")
    return "0" * (len(binary) - 1) + binary


def rbsp_bits(nal):
    raw = bytearray()
    zeros = 0
    for byte in nal[1:]:
        if zeros >= 2 and byte == 3:
            zeros = 0
            continue
        raw.append(byte)
        zeros = zeros + 1 if byte == 0 else 0
    return "".join(f"{byte:08b}" for byte in raw)


def pack_nal(header, bits):
    bits = bits[: bits.rfind("1") + 1]  # rbsp_stop_one_bit, without byte padding
    bits += "0" * (-len(bits) % 8)
    escaped = bytearray()
    for index in range(0, len(bits), 8):
        byte = int(bits[index : index + 8], 2)
        if len(escaped) >= 2 and escaped[-2:] == b"\0\0" and byte <= 3:
            escaped.append(3)
        escaped.append(byte)
    return bytes([header]) + escaped


def replace_ue(bits, pos, value):
    old, end = ue(bits, pos)
    return bits[:pos] + encode_ue(value) + bits[end:], old


def change_unused_qs(nal):
    assert nal[0] & 31 == 8, "expected PPS"
    bits = rbsp_bits(nal)
    pos = 0
    _, pos = ue(bits, pos)  # pic_parameter_set_id
    _, pos = ue(bits, pos)  # seq_parameter_set_id
    pos += 2  # entropy_coding_mode_flag, bottom_field_pic_order_in_frame_present_flag
    groups, pos = ue(bits, pos)
    assert groups == 0, "slice groups need a different PPS parser"
    _, pos = ue(bits, pos)  # num_ref_idx_l0_default_active_minus1
    _, pos = ue(bits, pos)  # num_ref_idx_l1_default_active_minus1
    pos += 3  # weighted_pred_flag, weighted_bipred_idc
    _, pos = ue(bits, pos)  # pic_init_qp_minus26 (signed Exp-Golomb)
    start = pos
    old, pos = ue(bits, pos)  # pic_init_qs_minus26 (signed Exp-Golomb)
    new = 1 if old == 0 else 0
    print(f"PPS SP/SI-only QS code: {old} -> {new}")
    bits = bits[:start] + encode_ue(new) + bits[pos:]
    return pack_nal(nal[0], bits)


def clone_sps_with_id(nal, new_id):
    assert nal[0] & 31 == 7, "expected SPS"
    bits = rbsp_bits(nal)
    bits, old_id = replace_ue(bits, 24, new_id)  # profile, constraints, level
    assert old_id == 0, "fixture expects SPS 0"
    return pack_nal(nal[0], bits)


def clone_pps_with_ids(nal, new_pps_id, new_sps_id):
    assert nal[0] & 31 == 8, "expected PPS"
    bits = rbsp_bits(nal)
    bits, old_pps_id = replace_ue(bits, 0, new_pps_id)
    _, pos = ue(bits, 0)
    bits, old_sps_id = replace_ue(bits, pos, new_sps_id)
    assert (old_pps_id, old_sps_id) == (0, 0), "fixture expects PPS 0 -> SPS 0"
    return pack_nal(nal[0], bits)


def slice_with_pps_id(nal, new_id):
    assert nal[0] & 31 in (1, 5), "expected coded slice"
    bits = rbsp_bits(nal)
    _, pos = ue(bits, 0)  # first_mb_in_slice
    _, pos = ue(bits, pos)  # slice_type
    bits, old_id = replace_ue(bits, pos, new_id)
    assert old_id == 0, "fixture expects slices using PPS 0"
    return pack_nal(nal[0], bits)


def without_main_profile_vui(nal):
    bits = rbsp_bits(nal)
    assert int(bits[:8], 2) == 77, "fixture requires Main profile"
    pos = 24
    _, pos = ue(bits, pos)  # seq_parameter_set_id
    _, pos = ue(bits, pos)  # log2_max_frame_num_minus4
    poc_type, pos = ue(bits, pos)
    assert poc_type in (0, 2), "fixture does not use POC type 1"
    if poc_type == 0:
        _, pos = ue(bits, pos)  # log2_max_pic_order_cnt_lsb_minus4
    _, pos = ue(bits, pos)  # max_num_ref_frames
    pos += 1  # gaps_in_frame_num_value_allowed_flag
    _, pos = ue(bits, pos)  # pic_width_in_mbs_minus1
    _, pos = ue(bits, pos)  # pic_height_in_map_units_minus1
    assert bits[pos] == "1", "fixture requires progressive frames"
    pos += 2  # frame_mbs_only_flag, direct_8x8_inference_flag
    cropped = bits[pos] == "1"
    pos += 1
    if cropped:
        for _ in range(4):
            _, pos = ue(bits, pos)
    assert bits[pos] == "1", "fixture must start with a VUI"
    # Remove VUI, including its reorder bound, and terminate the RBSP.
    return pack_nal(nal[0], bits[:pos] + "01")


def main(src, dst, mode="qs"):
    assert mode in ("qs", "pps-id", "sps-id", "no-reorder-bound"), mode
    data = open(src, "rb").read()
    starts = list(re.finditer(b"\x00\x00\x00\x01|\x00\x00\x01", data))
    assert starts, "no Annex B NAL units"
    output = bytearray(data[: starts[0].start()])
    latest_pps = None
    latest_sps = None
    pictures = 0
    inserted = False
    for index, start in enumerate(starts):
        end = starts[index + 1].start() if index + 1 < len(starts) else len(data)
        nal = data[start.end() : end]
        kind = nal[0] & 31
        if kind == 8:
            latest_pps = nal
        if kind == 7:
            latest_sps = nal
        if kind in (1, 5):
            pictures += 1
            if pictures == 3:
                assert latest_pps is not None
                if mode == "qs":
                    output += b"\x00\x00\x00\x01" + change_unused_qs(latest_pps)
                elif mode == "no-reorder-bound":
                    assert latest_sps is not None
                    output += b"\x00\x00\x00\x01" + without_main_profile_vui(latest_sps)
                else:
                    if mode == "sps-id":
                        assert latest_sps is not None
                        output += b"\x00\x00\x00\x01" + clone_sps_with_id(latest_sps, 1)
                    output += b"\x00\x00\x00\x01" + clone_pps_with_ids(
                        latest_pps, 1, int(mode == "sps-id")
                    )
                inserted = True
            if mode in ("pps-id", "sps-id") and 3 <= pictures < 30:
                output += b"\x00\x00\x00\x01" + slice_with_pps_id(nal, 1)
                continue
        output += data[start.start() : end]
    assert inserted, "fewer than three pictures"
    open(dst, "wb").write(output)
    print(f"{mode}: changed parameter sets before picture 3 of {pictures}")


if __name__ == "__main__":
    main(*sys.argv[1:])
