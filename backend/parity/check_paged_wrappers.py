"""Catch a hand-written wrapper around paged_list!.

paged_list! emits the whole method, `page: PageRequest` parameter included.
So writing

    pub async fn foo(&self, x: T) -> Result<Page<R>, DbError> {
        paged_list! { pub async fn foo(&self, x: T) -> Result<Page<R>, DbError> { ... } }
    }

shadows the generated method with one whose body returns (). The compiler
catches it, but the error points at the macro expansion rather than at the
mistake, and only after the whole crate is written.

Detection is by indentation, which is crude but has no false negatives for
the real shape and needs no parsing:

  * `paged_list!` at 4 spaces sits directly in an `impl` block -- correct.
  * `paged_list!` at 8 or more spaces is nested inside a function body --
    the wrapper.

The one thing that also matches deeper indentation is a `paged_list!` inside
a macro definition that itself opens an `impl` block (repo/collection.rs's
impl_paged!), where the depth is incidental. Those are skipped by checking
whether the occurrence lies inside a `macro_rules!` body.

Written after two wrong versions of this check. The first matched
signatures by regex and so missed every multi-line signature, reporting
repo/discovery.rs clean while it did not compile. The second walked back to
the nearest `fn` keyword, which is the *preceding* function, and produced 38
false positives on methods like `delete`. Both failures were silent.

A check that quietly says "clean" is worse than no check, so the reasoning
is recorded here rather than left implicit.
"""
import pathlib
import re
import sys

MACRO = re.compile(r"^(\s*)paged_list!\s*\{", re.M)
MACRO_DEFS = re.compile(r"^\s*macro_rules!\s+(\w+)", re.M)


def in_macro_def(text, pos):
    """True when pos falls inside a macro_rules! body in this file."""
    defs = [m for m in MACRO_DEFS.finditer(text) if m.start() < pos]
    if not defs:
        return False
    # Find the last macro_rules! before pos, then look for its closing
    # `macro_name_invocation!(` ... `);` -- a macro body ends where a
    # semicolon follows a balanced brace run at depth 0.
    name = defs[-1].group(1)
    body_start = text.index("{", defs[-1].end())
    depth = 0
    i = body_start
    while i < len(text):
        if text[i] == "{":
            depth += 1
        elif text[i] == "}":
            depth -= 1
            if depth == 0:
                return i >= pos and pos > defs[-1].start()
        i += 1
    return False


root = pathlib.Path(r"C:\\Users\\29789\\Desktop\\sakuramedia\\sakuramedia-rs") / "crates" / "sm-db" / "src"
hits = []
total = 0
for path in sorted(root.rglob("*.rs")):
    text = path.read_text(encoding="utf-8")
    for m in MACRO.finditer(text):
        total += 1
        indent = len(m.group(1))
        if indent < 8:
            continue
        if in_macro_def(text, m.start()):
            continue
        line = text[: m.start()].count("\n") + 1
        after = text[m.end() : m.end() + 400]
        fn = re.search(r"pub\s+(?:async\s+)?fn\s+(\w+)", after)
        hits.append((path.name, line, fn.group(1) if fn else "?"))

if hits:
    print("  %d wrapper(s) out of %d paged_list! uses:" % (len(hits), total))
    for name, line, fn in hits:
        print("    %s:%d  %s" % (name, line, fn))
    sys.exit(1)
print("  0 wrappers out of %d paged_list! uses" % total)