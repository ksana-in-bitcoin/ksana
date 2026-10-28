#!/usr/bin/env python3
"""Independent reference for Ksana's compact targets (D-025) and ASERT (D-026).

Python integers have arbitrary precision, so the formula of D-026 is evaluated literally,
with no fixed-width integers and no overflow handling:

    exponent = trunc((time_diff - T * (height_diff + 1)) * 65536 / tau)   # C++-style truncation
    shifts   = floor(exponent / 65536)
    frac     = exponent - shifts * 65536
    factor   = 65536 + ((195766423245049*frac + 971821376*frac**2 + 5127*frac**3 + 2**47) >> 48)
    next     = floor(anchor * factor * 2**(shifts - 16)), then 0 -> 1 and > pow_limit -> pow_limit

Usage:
    python3 asert_ref.py check <dir with the official aserti3-2d vectors run01..run12>
        Verifies this reference against the official Bitcoin Cash Node vectors (and shows that
        floor division in the exponent would NOT reproduce them).
    python3 asert_ref.py generate > asert_ref_vectors.txt
        Regenerates the extra vectors used by tests/asert_vectors.rs (deterministic).
"""

import os
import random
import sys

RADIX = 65536
POLY = (195766423245049, 971821376, 5127, 2**47)


def trunc_div(n, d):
    """Integer division rounding toward zero (C/C++/Rust semantics), d > 0."""
    assert d > 0
    q = abs(n) // d
    return q if n >= 0 else -q


def bits_to_target(bits):
    """Compact -> target per D-025. Returns None for sign bit / zero / >= 2**256."""
    if bits & 0x00800000:
        return None
    size = bits >> 24
    mant = bits & 0x007FFFFF
    target = mant >> (8 * (3 - size)) if size <= 3 else mant << (8 * (size - 3))
    if target == 0 or target >= 2**256:
        return None
    return target


def target_to_bits(target):
    """Bitcoin Core's GetCompact (canonical encoding)."""
    size = (target.bit_length() + 7) // 8
    if size <= 3:
        compact = target << (8 * (3 - size))
    else:
        compact = target >> (8 * (size - 3))
    if compact & 0x00800000:
        compact >>= 8
        size += 1
    return compact | (size << 24)


def asert_exponent(spacing, half_life, time_diff, height_diff, division="trunc"):
    num = (time_diff - spacing * (height_diff + 1)) * RADIX
    if division == "trunc":
        return trunc_div(num, half_life)
    return num // half_life  # floor, only used to show the official vectors reject it


def asert_target(anchor, pow_limit, spacing, half_life, time_diff, height_diff, division="trunc"):
    assert half_life > 0 and height_diff >= 0 and 1 <= anchor <= pow_limit
    exponent = asert_exponent(spacing, half_life, time_diff, height_diff, division)
    shifts = exponent >> 16  # Python's >> on negative ints is a floor, like an arithmetic shift
    frac = exponent - shifts * RADIX
    assert 0 <= frac < RADIX
    factor = RADIX + ((POLY[0] * frac + POLY[1] * frac**2 + POLY[2] * frac**3 + POLY[3]) >> 48)
    e = shifts - 16
    if e >= 0:
        # anchor * factor >= 2**16, so for e >= 300 the product is far above any pow limit (< 2**256);
        # skip materialising a gigantic integer.
        nxt = pow_limit + 1 if e >= 300 else (anchor * factor) << e
    else:
        nxt = (anchor * factor) >> (-e)
    if nxt == 0:
        return 1
    return min(nxt, pow_limit)


# --------------------------------------------------------------------------------------------
# Official vectors


def parse_official(path):
    meta = {}
    rows = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line.startswith("##"):
                key, _, value = line[2:].partition(":")
                meta[key.strip()] = value.strip()
            elif line and not line.startswith("#"):
                it, height, time, bits = line.split()
                rows.append((int(it), int(height), int(time), int(bits, 16)))
    return meta, rows


def check(directory):
    bch_limit = 2**224 - 1  # Bitcoin Cash mainnet powLimit, as in BCHN's CalculateASERT
    total = 0
    floor_mismatches = 0
    for name in sorted(os.listdir(directory)):
        meta, rows = parse_official(os.path.join(directory, name))
        anchor_height = int(meta["anchor height"])
        anchor_parent_time = int(meta["anchor parent time"])
        anchor = bits_to_target(int(meta["anchor nBits"], 16))
        assert len(rows) == int(meta["iterations"]), name
        for _, height, time, bits in rows:
            args = (anchor, bch_limit, 600, 172800, time - anchor_parent_time, height - anchor_height)
            got = target_to_bits(asert_target(*args))
            assert got == bits, (name, height, time, hex(got), hex(bits))
            if target_to_bits(asert_target(*args, division="floor")) != bits:
                floor_mismatches += 1
            total += 1
    print(f"{total} official vectors OK; floor division would mismatch {floor_mismatches} of them")
    assert floor_mismatches > 0


# --------------------------------------------------------------------------------------------
# Extra vectors


def h64(x):
    return format(x, "064x")


def generate():
    rng = random.Random(20260930)
    max256 = 2**256 - 1
    devnet_limit = bits_to_target(0x207FFFFF)
    param_sets = [
        # (name, spacing, half_life, pow_limit)
        ("devnet", 5, 600, devnet_limit),
        ("bch", 600, 172800, 2**224 - 1),
        ("bch-bits", 600, 172800, bits_to_target(0x1D00FFFF)),
        ("unit-tau", 1, 65536, max256),  # exponent == time_diff - (height_diff + 1)
        ("tiny-tau", 7, 1, devnet_limit),
        ("max-limit", 5, 600, max256),
        ("huge", 2**64 - 1, 2**64 - 1, bits_to_target(0x2100FFFF)),
        ("spacing-0", 0, 3600, devnet_limit),
    ]

    def anchors(limit):
        out = [limit, 1, limit - 1, (limit >> 7) + 12345]
        for _ in range(2):
            out.append(rng.randint(1, limit))
        for _ in range(2):  # compact-representable anchors, like real block targets
            while True:
                t = bits_to_target(rng.randint(0x01000000, 0x22FFFFFF) & 0xFF7FFFFF)
                if t is not None and t <= limit:
                    out.append(t)
                    break
        return out

    def cases(spacing, half_life):
        """(time_diff, height_diff) pairs covering the interesting regions of the exponent."""
        out = []
        for h in (0, 1, 2, 7, 1000, 123456789, 2**32, 2**63, 2**64 - 1):
            on_schedule = spacing * (h + 1)
            out.append((on_schedule, h))  # exponent 0
            for k in (-3, -1, 1, 2, 17):  # whole half-lives off schedule: frac 0
                out.append((on_schedule + k * half_life, h))
            for d in (-1, 1, -half_life // 2, half_life // 3):
                out.append((on_schedule + d, h))
        # exponent close to the boundaries of frac (0 / 65535) and of the saturation region
        for target_exp in (
            -1, 1, 65535, 65536, 65537, -65535, -65536, -65537,
            16 * RADIX - 1, 16 * RADIX, 200 * RADIX, 240 * RADIX + 12345, 255 * RADIX, 256 * RADIX,
            257 * RADIX, -200 * RADIX, -239 * RADIX - 1, -240 * RADIX, -255 * RADIX, -256 * RADIX,
            -257 * RADIX, -272 * RADIX, -273 * RADIX, -274 * RADIX, 2**24, -(2**24), 2**40, -(2**40),
        ):
            # smallest |num| whose truncated exponent reaches target_exp; neighbours as well
            num = -((-abs(target_exp) * half_life) // RADIX)
            num = num if target_exp >= 0 else -num
            h = rng.choice((0, 5, 99999))
            for delta in (-1, 0, 1):
                out.append((num + delta + spacing * (h + 1), h))
        # random walks around the schedule and random extremes
        for _ in range(12):
            h = rng.randint(0, 10**7)
            out.append((spacing * (h + 1) + rng.randint(-50 * half_life, 50 * half_life), h))
        for _ in range(6):
            out.append((rng.randint(-(2**127), 2**127 - 1), rng.randint(0, 2**64 - 1)))
        for t in (-(2**127), 2**127 - 1, -(2**100), 2**100, 0, -1):
            for h in (0, 2**64 - 1):
                out.append((t, h))
        # the Rust API takes time_diff as i128 and height_diff as u64
        return [(t, h) for t, h in out if -(2**127) <= t < 2**127 and 0 <= h < 2**64]

    lines = [
        "# Generated by asert_ref.py (python3 asert_ref.py generate); do not edit by hand.",
        "# P <name> <spacing> <half_life> <pow_limit hex>   -- parameter set",
        "# A <anchor target hex>                           -- anchor target for the following cases",
        "# <time_diff> <height_diff> <expected target hex>",
    ]
    count = 0
    for name, spacing, half_life, limit in param_sets:
        lines.append(f"P {name} {spacing} {half_life} {h64(limit)}")
        for anchor in anchors(limit):
            lines.append(f"A {h64(anchor)}")
            chosen = cases(spacing, half_life)
            # keep the file small: the pow-limit anchor gets every case, other anchors a random subset
            if anchor != limit:
                chosen = rng.sample(chosen, 20)
            for time_diff, height_diff in chosen:
                expected = asert_target(anchor, limit, spacing, half_life, time_diff, height_diff)
                lines.append(f"{time_diff} {height_diff} {format(expected, 'x')}")
                count += 1

    # Genesis-anchored devnet ASERT (D-026): anchor parent time = genesis time - T, anchor height 0.
    lines.append("# G <genesis timestamp> <pow_limit_bits hex> <spacing> <half_life>")
    lines.append("# <parent height> <parent timestamp> <expected bits hex>")
    for genesis_time in (1_700_000_000, 0, 3):
        lines.append(f"G {genesis_time} 207fffff 5 600")
        anchor = devnet_limit
        pairs = [(0, genesis_time), (1, genesis_time + 5), (0, genesis_time + 1000), (120, genesis_time)]
        for _ in range(40):
            h = rng.randint(0, 2_000_000)
            pairs.append((h, max(0, genesis_time + 5 * h + rng.randint(-40000, 2000))))
        for h, t in pairs:
            time_diff = t - (genesis_time - 5)
            target = asert_target(anchor, devnet_limit, 5, 600, time_diff, h)
            lines.append(f"{h} {t} {target_to_bits(target):08x}")
            count += 1
    lines.append(f"# {count} cases")
    print("\n".join(lines))


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "check":
        check(sys.argv[2])
    elif len(sys.argv) == 2 and sys.argv[1] == "generate":
        generate()
    else:
        sys.exit(__doc__)
