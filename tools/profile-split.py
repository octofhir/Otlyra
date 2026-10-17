#!/usr/bin/env python3
"""Split a Time Profiler export (time-profile table XML) by crate.

usage: profile_split.py tp.xml
Each running sample is charged to the innermost frame that names a known
crate; Otter's are split further into parse/compile, interpreter, JIT, GC.
"""
import collections, re, sys
import xml.etree.ElementTree as ET

# (category, pattern on the frame name), first match from the leaf outwards wins.
RULES = [
    ("otter: parse", r"otter_syntax|oxc_parser|oxc_ast|oxc_allocator|oxc_span|otter_parser|9otter_ast"),
    ("otter: compile", r"otter_compiler|otter_bytecode|otter_codegen|oxc_semantic"),
    ("otter: gc", r"otter_gc"),
    ("otter: jit", r"otter_jit|cranelift|otter_vm.*[Jj]it"),
    ("otter: interpreter", r"otter_vm.*interp|dispatch_loop"),
    ("otter: vm other", r"otter_vm|otter_runtime|otter_web|otter_node|otter_macros"),
    ("glue: otlyra-script", r"otlyra_script"),
    ("style: stylo", r"5style|8selectors|otlyra_css|cssparser"),
    ("layout", r"otlyra_layout|taffy"),
    ("text", r"otlyra_text|parley|skrifa|harfrust|swash|fontique|icu_|read_fonts"),
    ("paint/gfx", r"otlyra_paint|otlyra_gfx|skia|Sk[A-Z]|png_|deflate|longest_match|resvg|usvg|tiny_skia"),
    ("html/dom", r"otlyra_html|html5ever|markup5ever|otlyra_dom"),
    ("net", r"otlyra_net|reqwest|hyper|rustls|h2|tokio|aws_lc|ring|brotli|flate2|zstd"),
    ("app", r"otlyra_app|otlyra_platform|otlyra"),
    ("malloc", r"malloc|_xzm|free|memmove|memcpy|memset|_platform_mem"),
]
COMPILED = [(c, re.compile(p)) for c, p in RULES]


def main():
    frames = {}
    stacks = {}
    threads = {}
    per_cat = collections.Counter()
    per_thread = collections.Counter()
    leaves = collections.Counter()
    total = 0
    for _, el in ET.iterparse(sys.argv[1], events=("end",)):
        tag = el.tag
        if tag == "frame" and "id" in el.attrib:
            frames[el.attrib["id"]] = el.attrib.get("name", "?")
        elif tag == "thread" and "id" in el.attrib:
            threads[el.attrib["id"]] = el.attrib.get("fmt", "?")
        elif tag == "backtrace" and "id" in el.attrib:
            names = []
            for f in el.findall("frame"):
                names.append(frames.get(f.attrib.get("id") or f.attrib.get("ref"), "?"))
            stacks[el.attrib["id"]] = names
        elif tag == "row":
            state = el.find("thread-state")
            th = el.find("thread")
            tname = threads.get(th.attrib.get("id") or th.attrib.get("ref"), "?") if th is not None else "?"
            bt = el.find("tagged-backtrace/backtrace")
            if bt is None:
                bt = el.find("backtrace")
            names = []
            if bt is not None:
                names = stacks.get(bt.attrib.get("id") or bt.attrib.get("ref"), [])
            total += 1
            cat = "other"
            for name in names:
                hit = next((c for c, rx in COMPILED if rx.search(name)), None)
                if hit and hit != "malloc":
                    cat = hit
                    break
                if hit == "malloc" and cat == "other":
                    cat = "malloc"
            per_cat[cat] += 1
            tname = re.sub(r" \(0x[0-9a-f]+\).*", "", tname)
            per_thread[tname] += 1
            if names:
                leaves[(cat, names[0][:90])] += 1
            el.clear()
    print(f"samples (1 ms each): {total}")
    print("\nby category:")
    for c, n in per_cat.most_common():
        print(f"  {c:24s} {n:6d}  {100*n/total:5.1f}%")
    print("\nby thread:")
    for t, n in per_thread.most_common(8):
        print(f"  {t:40s} {n:6d}")
    print("\nhottest leaves:")
    for (c, l), n in leaves.most_common(25):
        print(f"  {n:5d}  {c:20s} {l}")


main()
