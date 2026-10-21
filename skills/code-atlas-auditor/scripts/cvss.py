#!/usr/bin/env python3
"""CVSS 3.1 base score from a vector string (FIRST specification, section 7).

    cvss.py CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H
    -> 9.8 Critical

Prints the score and the qualitative rating; exits 2 on a malformed vector.
"""
import math
import sys

WEIGHTS = {
    "AV": {"N": 0.85, "A": 0.62, "L": 0.55, "P": 0.2},
    "AC": {"L": 0.77, "H": 0.44},
    "UI": {"N": 0.85, "R": 0.62},
    "C": {"H": 0.56, "L": 0.22, "N": 0.0},
    "I": {"H": 0.56, "L": 0.22, "N": 0.0},
    "A": {"H": 0.56, "L": 0.22, "N": 0.0},
}
PR = {"U": {"N": 0.85, "L": 0.62, "H": 0.27}, "C": {"N": 0.85, "L": 0.68, "H": 0.5}}


def roundup(x):
    i = round(x * 100000)
    return i / 100000.0 if i % 10000 == 0 else (math.floor(i / 10000) + 1) / 10.0


def rating(score):
    if score == 0:
        return "None"
    return "Low" if score < 4 else "Medium" if score < 7 else "High" if score < 9 else "Critical"


def base_score(vector):
    parts = vector.strip().split("/")
    if parts[0] != "CVSS:3.1":
        raise ValueError("vector must start with CVSS:3.1/")
    m = dict(p.split(":", 1) for p in parts[1:])
    need = ["AV", "AC", "PR", "UI", "S", "C", "I", "A"]
    if sorted(m) != sorted(need) or m["S"] not in ("U", "C"):
        raise ValueError(f"need exactly the base metrics {need}")
    iss = 1 - (1 - WEIGHTS["C"][m["C"]]) * (1 - WEIGHTS["I"][m["I"]]) * (1 - WEIGHTS["A"][m["A"]])
    if m["S"] == "U":
        impact = 6.42 * iss
    else:
        impact = 7.52 * (iss - 0.029) - 3.25 * (iss - 0.02) ** 15
    expl = 8.22 * WEIGHTS["AV"][m["AV"]] * WEIGHTS["AC"][m["AC"]] * PR[m["S"]][m["PR"]] * WEIGHTS["UI"][m["UI"]]
    if impact <= 0:
        return 0.0
    if m["S"] == "U":
        return roundup(min(impact + expl, 10))
    return roundup(min(1.08 * (impact + expl), 10))


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    try:
        s = base_score(sys.argv[1])
    except (ValueError, KeyError) as e:
        print(f"invalid vector: {e}")
        sys.exit(2)
    print(f"{s} {rating(s)}")


if __name__ == "__main__":
    main()
