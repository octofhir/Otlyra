#!/usr/bin/env python3
"""Inclusive samples under frames matching each pattern, from a Time Profiler
time-profile export. usage: profile-inclusive.py tp.xml PATTERN..."""
import collections, re, sys
import xml.etree.ElementTree as ET
pats = [(p, re.compile(p)) for p in sys.argv[2:]]
frames, stacks = {}, {}; hits = collections.Counter(); total = 0
for _, el in ET.iterparse(sys.argv[1], events=("end",)):
    if el.tag == "frame" and "id" in el.attrib: frames[el.attrib["id"]] = el.attrib.get("name", "?")
    elif el.tag == "backtrace" and "id" in el.attrib:
        stacks[el.attrib["id"]] = [frames.get(f.attrib.get("id") or f.attrib.get("ref"), "?") for f in el.findall("frame")]
    elif el.tag == "row":
        total += 1
        bt = el.find("tagged-backtrace/backtrace")
        names = stacks.get(bt.attrib.get("id") or bt.attrib.get("ref"), []) if bt is not None else []
        joined = "\n".join(names)
        for p, rx in pats:
            if rx.search(joined): hits[p] += 1
        el.clear()
print("total", total)
for p, _ in pats: print(f"{hits[p]:6d}  {p}")
