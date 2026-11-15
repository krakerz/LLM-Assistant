#!/usr/bin/env python3
"""Build assets/danbooru-tags.sqlite, the tag list shipped with the app.

Usage:
    scripts/build-danbooru-db.py <csv-dir> [--out assets/danbooru-tags.sqlite]
                                 [--top 40] [--min-ratio 0.30] [--max-mb 6]

<csv-dir> holds `danbooru_tags.csv` (tag,category,count,alias) and
`danbooru_tags_cooccurrence.csv` (tag_a,tag_b,count). The ComfyUI node
comfyui-autocomplete-plus downloads both into its `data/` folder.

The tag and alias tables are kept whole. Co-occurrence is pruned to pairs
of one character tag (category 4) and one general tag (category 0), keeping
per character the `--top` highest pairs with count >= min_ratio * count of
the character. The output is deterministic: same input, same bytes.
Python standard library only. Exits non-zero when the file exceeds --max-mb.
"""
import argparse
import csv
import datetime
import os
import sqlite3
import sys

SCHEMA = """
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
CREATE TABLE tags(id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE,
                  category INTEGER NOT NULL, count INTEGER NOT NULL);
CREATE TABLE aliases(alias TEXT PRIMARY KEY, tag_id INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE cooccurrence(tag_id INTEGER NOT NULL, other_id INTEGER NOT NULL,
                          count INTEGER NOT NULL, PRIMARY KEY(tag_id, other_id)) WITHOUT ROWID;
"""
TAGS_FILE = "danbooru_tags.csv"
COOC_FILE = "danbooru_tags_cooccurrence.csv"


def to_int(text):
    try:
        return int(text)
    except ValueError:
        return int(float(text))


def read_tags(path):
    tags = []  # (name, category, count)
    aliases = []  # (alias, tag index)
    seen = set()
    with open(path, newline="", encoding="utf-8") as f:
        for row in csv.reader(f):
            if len(row) < 3 or row[0].strip().lower() in ("tag", "name"):
                continue
            try:
                category, count = to_int(row[1]), to_int(row[2])
            except ValueError:
                continue
            name = row[0].strip()
            if not name or name in seen:
                continue
            seen.add(name)
            tags.append((name, category, count))
            if len(row) > 3 and row[3]:
                for a in row[3].split(","):
                    a = "_".join(a.strip().lower().split())
                    if a:
                        aliases.append((a, len(tags)))  # id = index + 1
    return tags, aliases


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("csv_dir")
    ap.add_argument("--out", default="assets/danbooru-tags.sqlite")
    ap.add_argument("--top", type=int, default=40)
    ap.add_argument("--min-ratio", type=float, default=0.30)
    ap.add_argument("--max-mb", type=float, default=6.0)
    args = ap.parse_args()

    tags_path = os.path.join(args.csv_dir, TAGS_FILE)
    cooc_path = os.path.join(args.csv_dir, COOC_FILE)
    tags, raw_aliases = read_tags(tags_path)
    ids = {name: i + 1 for i, (name, _, _) in enumerate(tags)}
    info = {name: (cat, cnt) for name, cat, cnt in tags}

    # First alias wins (the CSV is sorted by count); never equal to a tag name.
    aliases = {}
    for alias, tag_id in raw_aliases:
        if alias not in ids and alias not in aliases:
            aliases[alias] = tag_id

    per_char = {}
    with open(cooc_path, newline="", encoding="utf-8") as f:
        for row in csv.reader(f):
            if len(row) < 3:
                continue
            a, b = info.get(row[0]), info.get(row[1])
            if a is None or b is None:
                continue
            if a[0] == 4 and b[0] == 0:
                char, other = row[0], row[1]
            elif b[0] == 4 and a[0] == 0:
                char, other = row[1], row[0]
            else:
                continue
            try:
                count = to_int(row[2])
            except ValueError:
                continue
            if count < args.min_ratio * info[char][1]:
                continue
            per_char.setdefault(char, []).append((count, other))

    pairs = []
    for char, items in per_char.items():
        items.sort(key=lambda x: (-x[0], x[1]))
        for count, other in items[: args.top]:
            pairs.append((ids[char], ids[other], count))
    pairs.sort()

    if os.path.exists(args.out):
        os.remove(args.out)
    os.makedirs(os.path.dirname(os.path.abspath(args.out)), exist_ok=True)
    conn = sqlite3.connect(args.out)
    conn.execute("PRAGMA page_size=4096")
    conn.executescript(SCHEMA)
    built = datetime.datetime.fromtimestamp(
        os.path.getmtime(tags_path), datetime.timezone.utc
    ).strftime("%Y-%m-%d")
    meta = [
        ("schema_version", "1"),
        ("built", built),
        ("source", f"{TAGS_FILE}, {COOC_FILE}"),
        ("tags", str(len(tags))),
        ("aliases", str(len(aliases))),
        ("pairs", str(len(pairs))),
        ("top", str(args.top)),
        ("min_ratio", str(args.min_ratio)),
    ]
    conn.executemany("INSERT INTO meta VALUES(?,?)", meta)
    conn.executemany(
        "INSERT INTO tags VALUES(?,?,?,?)",
        [(i + 1, n, c, k) for i, (n, c, k) in enumerate(tags)],
    )
    conn.executemany("INSERT INTO aliases VALUES(?,?)", sorted(aliases.items()))
    conn.executemany("INSERT INTO cooccurrence VALUES(?,?,?)", pairs)
    conn.commit()
    conn.execute("VACUUM")
    conn.close()

    size = os.path.getsize(args.out)
    print(
        f"tags={len(tags)} aliases={len(aliases)} pairs={len(pairs)} "
        f"top={args.top} min_ratio={args.min_ratio} size={size} bytes "
        f"({size / 1048576:.2f} MB)"
    )
    if size > args.max_mb * 1048576:
        print(f"too big: over {args.max_mb} MB", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
