#!/usr/bin/env python3
"""Assembles the landing page of iron-oxyde.com (called by `make landing-build`).

Reads the output directory from the LANDING_OUT environment variable (never from a command line,
so no shell ever parses it). It must be a plain relative path strictly under the repository's
dist/ directory: no whitespace, no `~`, no glob characters, and still under dist/ once resolved.

The site is built in a temporary directory next to the output and moved into place only once
complete, so a failed build never leaves a half-built site:
- landing/ is copied, plus schemas/program.schema.json as /program.schema.json;
- programs/ai-prompt.md, the one AI prompt the app shows too, is inserted into index.html between
  the `<!-- prompt:begin -->` and `<!-- prompt:end -->` markers, HTML-escaped (missing or empty:
  the build fails);
- HTML comments and CSS comments (notes for maintainers) are stripped from the shipped files;
- the shipped page must not mention GitHub, open source or a licence: the build fails if it does.
  The fonts' SIL Open Font License files, which must ship with the fonts, and the published JSON
  Schema are not page text and are not checked.
"""
import html
import os
import pathlib
import re
import shutil
import sys
import tempfile

BEGIN, END = "<!-- prompt:begin -->", "<!-- prompt:end -->"
FORBIDDEN = re.compile(r"github|open[\s-]*source|licen[cs]", re.IGNORECASE)
UNSAFE = re.compile(r"[\s~*?\[\]{}$`'\"\\]")
# Shipped text files that are not checked for forbidden words (see the module docstring).
UNCHECKED = re.compile(r"^(fonts/.*-OFL\.txt|program\.schema\.json)$")
TEXT_SUFFIXES = {".html", ".css", ".js", ".txt", ".xml", ".json", ".webmanifest", ""}


def fail(message: str) -> int:
    print(f"make landing-build: {message}", file=sys.stderr)
    return 1


def resolve_output(root: pathlib.Path, value: str) -> pathlib.Path | None:
    """The output directory, or None when LANDING_OUT is not strictly under dist/."""
    if not value or UNSAFE.search(value) or value.startswith("/"):
        return None
    dist = (root / "dist").resolve()
    out = (root / value).resolve()
    if out == dist or dist not in out.parents:
        return None
    return out


def insert_prompt(page: str, prompt_path: pathlib.Path) -> str:
    if page.count(BEGIN) != 1 or page.count(END) != 1 or page.index(BEGIN) > page.index(END):
        raise ValueError(f"landing/index.html: expected one {BEGIN} ... {END} pair")
    if not prompt_path.is_file():
        raise ValueError(f"{prompt_path} not found: the landing page needs the AI prompt")
    prompt = prompt_path.read_text(encoding="utf-8").strip()
    if not prompt:
        raise ValueError(f"{prompt_path} is empty")
    start, end = page.index(BEGIN), page.index(END) + len(END)
    return page[:start] + html.escape(prompt, quote=False) + page[end:]


def main() -> int:
    root = pathlib.Path.cwd()
    value = os.environ.get("LANDING_OUT", "")
    out = resolve_output(root, value)
    if out is None:
        return fail(
            f"LANDING_OUT must be a relative directory strictly under dist/, without spaces, '~' "
            f"or glob characters (got '{value}')."
        )
    out.parent.mkdir(parents=True, exist_ok=True)
    staging = pathlib.Path(tempfile.mkdtemp(prefix=".landing-", dir=out.parent))
    try:
        site = staging / "site"
        shutil.copytree(root / "landing", site)
        shutil.copyfile(root / "schemas/program.schema.json", site / "program.schema.json")

        index = site / "index.html"
        page = insert_prompt(index.read_text(encoding="utf-8"), root / "programs/ai-prompt.md")
        # The prompt is inserted after the comments are gone: it may legitimately contain "<!--".
        index.write_text(page, encoding="utf-8")
        for path in site.rglob("*"):
            if path.suffix == ".html":
                text = path.read_text(encoding="utf-8")
                path.write_text(strip_html_comments(text), encoding="utf-8")
            elif path.suffix == ".css":
                text = path.read_text(encoding="utf-8")
                path.write_text(re.sub(r"/\*.*?\*/\n?", "", text, flags=re.S), encoding="utf-8")

        problems = []
        for path in sorted(site.rglob("*")):
            rel = path.relative_to(site).as_posix()
            if not path.is_file() or path.suffix not in TEXT_SUFFIXES or UNCHECKED.match(rel):
                continue
            for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
                match = FORBIDDEN.search(line)
                if match:
                    problems.append(f"  {rel}:{number}: '{match.group(0)}'")
        if problems:
            return fail(
                "the page must not mention GitHub, open source or a licence:\n" + "\n".join(problems)
            )

        if out.exists():
            shutil.rmtree(out)
        site.rename(out)
    except ValueError as error:
        return fail(str(error))
    finally:
        shutil.rmtree(staging, ignore_errors=True)
    print(f"Landing page: {value}")
    return 0


def strip_html_comments(text: str) -> str:
    """Removes HTML comments, except inside <pre> (the prompt)."""
    parts = re.split(r"(<pre\b.*?</pre>)", text, flags=re.S | re.I)
    for i in range(0, len(parts), 2):
        parts[i] = re.sub(r"[ \t]*<!--.*?-->[ \t]*\n?", "", parts[i], flags=re.S)
    return "".join(parts)


if __name__ == "__main__":
    sys.exit(main())
