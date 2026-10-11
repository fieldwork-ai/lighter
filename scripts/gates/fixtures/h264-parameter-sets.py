#!/usr/bin/env python3
"""Rewrite an x264 stream's parameter sets without changing its pictures.

Gate m13 decodes what this makes through the device and through software, and
needs streams ffmpeg will not make on request: a new PPS between keyframes, a
switch to a second PPS or SPS id and back, an SPS that stops declaring its
reorder depth, and a second SPS, at another size, that no slice uses. Each edit
leaves the coded pictures as they were, so the gate first checks that software
decodes the edited stream to the same frames as the original: a mistake here
fails that check rather than the device's.

Written by @adamscybot for lighter#78. Bits are handled as
strings of "0" and "1", which is slow and plenty for a few seconds of video.

    h264-parameter-sets.py <src.h264> <dst.h264> [qs|pps-id|sps-id|no-reorder-bound]
    h264-parameter-sets.py <src.h264> <dst.h264> unused-sps <other-size.h264>
"""

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
    _, pos = ue(bits, pos)  # pic_init_qp_minus26, read as its Exp-Golomb code
    start = pos
    # pic_init_qs_minus26, used only by SP and SI slices, which x264 never
    # writes: swapping its code between 0 and 1 changes the PPS and no picture.
    old, pos = ue(bits, pos)
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


def nals(path):
    data = open(path, "rb").read()
    starts = list(re.finditer(b"\x00\x00\x00\x01|\x00\x00\x01", data))
    assert starts, "no Annex B NAL units"
    for index, start in enumerate(starts):
        end = starts[index + 1].start() if index + 1 < len(starts) else len(data)
        yield data[start.end() : end]


def unused_sps(src, other):
    """`src` with `other`'s parameter sets, as SPS 1 and PPS 1, sent before
    its picture 3 and named by none of its slices."""
    sets = [clone_sps_with_id(nal, 1) for nal in nals(other) if nal[0] & 31 == 7]
    sets += [clone_pps_with_ids(nal, 1, 1) for nal in nals(other) if nal[0] & 31 == 8]
    assert len(sets) == 2, "fixture expects one SPS and one PPS in the other stream"
    output = []
    pictures = 0
    for nal in nals(src):
        if nal[0] & 31 in (1, 5):
            pictures += 1
            if pictures == 3:
                output += sets
        output.append(nal)
    assert pictures >= 3, "fewer than three pictures"
    print(f"unused-sps: another size's SPS sent before picture 3 of {pictures}")
    return output


def main(src, dst, mode="qs", second=None):
    assert mode in ("qs", "pps-id", "sps-id", "no-reorder-bound", "unused-sps"), mode
    if mode == "unused-sps":
        output = unused_sps(src, second)
        open(dst, "wb").write(b"".join(b"\x00\x00\x00\x01" + nal for nal in output))
        return
    output = []
    latest_pps = None
    latest_sps = None
    pictures = 0
    inserted = False
    for nal in nals(src):
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
                    output.append(change_unused_qs(latest_pps))
                elif mode == "no-reorder-bound":
                    assert latest_sps is not None
                    output.append(without_main_profile_vui(latest_sps))
                else:
                    if mode == "sps-id":
                        assert latest_sps is not None
                        output.append(clone_sps_with_id(latest_sps, 1))
                    output.append(clone_pps_with_ids(latest_pps, 1, int(mode == "sps-id")))
                inserted = True
            if mode in ("pps-id", "sps-id") and 3 <= pictures < 30:
                output.append(slice_with_pps_id(nal, 1))
                continue
        output.append(nal)
    assert inserted, "fewer than three pictures"
    open(dst, "wb").write(b"".join(b"\x00\x00\x00\x01" + nal for nal in output))
    print(f"{mode}: changed parameter sets before picture 3 of {pictures}")


if __name__ == "__main__":
    main(*sys.argv[1:])
