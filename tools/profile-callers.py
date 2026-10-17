#!/usr/bin/env python3
"""Call chains above the innermost frame matching PATTERN, from a Time Profiler
time-profile export. usage: profile-callers.py tp.xml PATTERN [DEPTH]"""
import collections, re, sys
import xml.etree.ElementTree as ET
pat = re.compile(sys.argv[2]); depth = int(sys.argv[3]) if len(sys.argv) > 3 else 6
frames, stacks = {}, {}
chains = collections.Counter(); other = collections.Counter()
for _, el in ET.iterparse(sys.argv[1], events=("end",)):
    if el.tag == "frame" and "id" in el.attrib: frames[el.attrib["id"]] = el.attrib.get("name", "?")
    elif el.tag == "backtrace" and "id" in el.attrib:
        stacks[el.attrib["id"]] = [frames.get(f.attrib.get("id") or f.attrib.get("ref"), "?") for f in el.findall("frame")]
    elif el.tag == "row":
        bt = el.find("tagged-backtrace/backtrace")
        names = stacks.get(bt.attrib.get("id") or bt.attrib.get("ref"), []) if bt is not None else []
        for i, n in enumerate(names):
            if pat.search(n):
                above = [re.sub(r"^_R\w*?\d+(\w+?)\d", "", x)[:70] for x in names[i:i+depth]]
                chains[" <- ".join(re.sub(r"Cs\w{8,13}_", "", x) for x in above)] += 1
                break
        el.clear()
for c, n in chains.most_common(12): print(n, c, "\n")
