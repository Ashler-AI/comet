#!/usr/bin/env python3
"""List Crew changes that an upstream port no longer contains.

A port resolves conflicts file by file, and taking upstream's side silently
drops whatever main had shipped there (0.2.130 lost #145, #146 and #148 that
way). This compares every line main added since it diverged from upstream with
the port's result, ignoring formatting and code moved to another file, and
groups what is missing by the main commit that added it. Each group needs a decision: restore it, or say in the
port's PR why upstream supersedes it.

  audit-upstream-port.py --main MAIN --upstream UPSTREAM --result PORT
  audit-upstream-port.py --main MAIN --base BASE --result PORT

MAIN is main before the port, UPSTREAM the upstream commit being ported (BASE,
their merge base, may be given instead) and PORT the port's head. Exits 1 when
anything is missing.
"""

import argparse
import collections
import re
import subprocess
import sys

TOKEN = re.compile(r'"(?:\\.|[^"\\])*"|//[^\n]*|/\*.*?\*/|[A-Za-z_][A-Za-z0-9_]*|\d[\w.]*|::|->|=>|\S', re.S)
# Lines that carry no behavior of their own.
TRIVIAL = re.compile(r"^[\s{}()\[\];,]*$|^\s*(//|#|/\*|\*)")
# Vendored sources and lockfiles follow their own upgrade paths.
SKIPPED = re.compile(r"^vendor/|(^|/)[^/]*\.lock$|(^|/)package-lock\.json$")


def git(*args):
    return subprocess.run(["git", *args], check=True, capture_output=True, text=True, errors="replace").stdout


def tokens(text):
    # Comments and trailing commas come and go with rustfmt's line breaks.
    found = [t for t in TOKEN.findall(text) if not t.startswith(("//", "/*"))]
    return [t for i, t in enumerate(found) if not (t == "," and i + 1 < len(found) and found[i + 1] in ")]}")]


def added_lines(base, main):
    """path -> (line number in MAIN, text) for each text line main added."""
    added, path, line = collections.defaultdict(list), None, 0
    for row in git("diff", "--unified=0", "--no-renames", "--diff-filter=AM", base, main).splitlines():
        if row.startswith("+++ "):
            path = row[len("+++ b/"):] if row.startswith("+++ b/") else None
        elif row.startswith("@@"):
            line = int(re.match(r"@@ -\S+ \+(\d+)", row).group(1))
        elif row.startswith("+") and path and not SKIPPED.search(path):
            added[path].append((line, row[1:]))
            line += 1
    return added


def texts(rev):
    """path -> text for every text file in `rev`, read through one git process."""
    entries = [row.split("\t", 1) for row in git("ls-tree", "-r", rev).splitlines()]
    wanted = [(meta.split()[2], path) for meta, path in entries
              if meta.split()[1] == "blob" and not SKIPPED.search(path)]
    batch = subprocess.run(["git", "cat-file", "--batch"], input="\n".join(sha for sha, _ in wanted).encode() + b"\n",
                           capture_output=True, check=True).stdout
    found, offset = {}, 0
    for _, path in wanted:
        end = batch.index(b"\n", offset)
        size = int(batch[offset:end].split()[2])
        body = batch[end + 1:end + 1 + size]
        offset = end + 2 + size
        if b"\0" not in body[:8192]:
            found[path] = body.decode("utf-8", "replace")
    return found


def blame(main, path):
    """Line number in MAIN -> 'sha subject' of the commit that last changed it."""
    owners, subjects, current = {}, {}, None
    for row in git("blame", "--line-porcelain", main, "--", path).splitlines():
        head = re.match(r"^([0-9a-f]{40}) \d+ (\d+)", row)
        if head:
            current = (head.group(1), int(head.group(2)))
        elif row.startswith("summary ") and current:
            subjects[current[0]] = row[len("summary "):]
        elif row.startswith("\t") and current:
            owners[current[1]] = current[0]
    return {line: f"{sha[:10]} {subjects.get(sha, '')}" for line, sha in owners.items()}


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--main", required=True)
    parser.add_argument("--result", required=True)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--upstream")
    source.add_argument("--base")
    parser.add_argument("--show", type=int, default=3, help="missing lines shown per commit and file")
    args = parser.parse_args()
    base = git("rev-parse", args.base).strip() if args.base else git("merge-base", args.main, args.upstream).strip()
    result = texts(args.result)
    # Every line of the result once, so code a port moved between files still counts.
    anywhere = {" ".join(tokens(line)) for text in result.values() for line in text.splitlines()}
    missing = collections.defaultdict(lambda: collections.defaultdict(list))
    for path, lines in added_lines(base, args.main).items():
        lines = [(n, text) for n, text in lines if not TRIVIAL.match(text) and len(tokens(text)) >= 2]
        if path in result:
            stream = f" {' '.join(tokens(result[path]))} "
            lines = [(n, text) for n, text in lines if f" {' '.join(tokens(text))} " not in stream]
        lines = [(n, text) for n, text in lines if " ".join(tokens(text)) not in anywhere]
        if lines:
            owners = blame(args.main, path)
            for n, text in lines:
                missing[owners.get(n, "unknown")][path].append((n, text))
    if not missing:
        print(f"Every line main added since {base[:10]} is still in {args.result}.")
        return 0
    print(f"Lines main added since {base[:10]} that {args.result} no longer contains:\n")
    for owner, files in sorted(missing.items(), key=lambda item: -sum(map(len, item[1].values()))):
        print(f"{owner}: {sum(map(len, files.values()))} lines")
        for path, rows in sorted(files.items()):
            print(f"  {path}: {len(rows)}")
            for n, text in rows[: args.show]:
                print(f"    {n}: {text.strip()[:110]}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
