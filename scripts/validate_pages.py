#!/usr/bin/env python3
from __future__ import annotations

import json
import sys
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urlsplit


class DocumentParser(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.ids: set[str] = set()
        self.duplicate_ids: set[str] = set()
        self.references: list[tuple[str, str, str]] = []
        self.meta_names: dict[str, str] = {}
        self.meta_properties: dict[str, str] = {}
        self.canonical_urls: list[str] = []
        self.title_parts: list[str] = []
        self.structured_data: list[str] = []
        self._in_title = False
        self._structured_data_parts: list[str] | None = None

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        attributes = {key: value or "" for key, value in attrs}
        element_id = attributes.get("id")
        if element_id:
            if element_id in self.ids:
                self.duplicate_ids.add(element_id)
            self.ids.add(element_id)

        for attribute in ("href", "src"):
            value = attributes.get(attribute)
            if value:
                self.references.append((tag, attribute, value))

        if tag == "meta":
            if attributes.get("name"):
                self.meta_names[attributes["name"].lower()] = attributes.get("content", "")
            if attributes.get("property"):
                self.meta_properties[attributes["property"].lower()] = attributes.get(
                    "content", ""
                )
        elif tag == "link" and "canonical" in attributes.get("rel", "").lower().split():
            self.canonical_urls.append(attributes.get("href", ""))
        elif tag == "title":
            self._in_title = True
        elif tag == "script" and attributes.get("type", "").lower() == "application/ld+json":
            self._structured_data_parts = []

    def handle_endtag(self, tag: str) -> None:
        if tag == "title":
            self._in_title = False
        elif tag == "script" and self._structured_data_parts is not None:
            self.structured_data.append("".join(self._structured_data_parts))
            self._structured_data_parts = None

    def handle_data(self, data: str) -> None:
        if self._in_title:
            self.title_parts.append(data)
        if self._structured_data_parts is not None:
            self._structured_data_parts.append(data)

    @property
    def title(self) -> str:
        return "".join(self.title_parts).strip()


def parse_document(path: Path) -> DocumentParser:
    parser = DocumentParser()
    parser.feed(path.read_text(encoding="utf-8"))
    parser.close()
    return parser


def local_target(root: Path, source: Path, raw_url: str) -> tuple[Path, str] | None:
    parsed = urlsplit(raw_url)
    if parsed.scheme or parsed.netloc or raw_url.startswith("//"):
        return None
    if parsed.scheme.lower() == "javascript":
        raise ValueError("javascript URLs are not allowed")

    relative = unquote(parsed.path)
    if relative.startswith("/"):
        raise ValueError("root-relative URLs break project Pages deployments")
    target = source if not relative else source.parent / relative
    if target.is_dir():
        target = target / "index.html"
    target = target.resolve()
    try:
        target.relative_to(root)
    except ValueError as error:
        raise ValueError("local URL escapes the Pages artifact") from error
    return target, unquote(parsed.fragment)


def validate_site(root: Path) -> list[str]:
    errors: list[str] = []
    root = root.resolve()
    index = root / "index.html"
    if not index.is_file():
        return [f"missing Pages entry point: {index}"]

    documents = {path.resolve(): parse_document(path) for path in root.rglob("*.html")}
    main = documents[index.resolve()]

    required_meta = {
        "viewport": main.meta_names,
        "description": main.meta_names,
        "robots": main.meta_names,
    }
    if not main.title:
        errors.append("index.html: missing non-empty <title>")
    for name, values in required_meta.items():
        if not values.get(name, "").strip():
            errors.append(f"index.html: missing non-empty meta[name={name!r}]")
    for property_name in ("og:title", "og:description", "og:url"):
        if not main.meta_properties.get(property_name, "").strip():
            errors.append(f"index.html: missing non-empty meta[property={property_name!r}]")
    if main.canonical_urls != ["https://siliconstate.github.io/Dext/"]:
        errors.append("index.html: canonical URL must be https://siliconstate.github.io/Dext/")
    if not main.structured_data:
        errors.append("index.html: missing application/ld+json metadata")
    for position, payload in enumerate(main.structured_data, start=1):
        try:
            json.loads(payload)
        except json.JSONDecodeError as error:
            errors.append(f"index.html: malformed JSON-LD block {position}: {error}")

    for path, document in documents.items():
        display = path.relative_to(root)
        for duplicate in sorted(document.duplicate_ids):
            errors.append(f"{display}: duplicate id #{duplicate}")
        for tag, attribute, raw_url in document.references:
            if raw_url.lower().startswith("javascript:"):
                errors.append(f"{display}: {tag}[{attribute}] uses a javascript URL")
                continue
            try:
                target_data = local_target(root, path, raw_url)
            except ValueError as error:
                errors.append(f"{display}: {tag}[{attribute}]={raw_url!r}: {error}")
                continue
            if target_data is None:
                continue
            target, fragment = target_data
            if not target.is_file():
                errors.append(
                    f"{display}: {tag}[{attribute}]={raw_url!r} targets missing {target.relative_to(root)}"
                )
                continue
            if fragment and target.suffix.lower() == ".html":
                target_document = documents.get(target)
                if target_document is None:
                    target_document = parse_document(target)
                    documents[target] = target_document
                if fragment not in target_document.ids:
                    errors.append(
                        f"{display}: {tag}[{attribute}]={raw_url!r} targets missing #{fragment}"
                    )

    if not (root / "favicon.svg").is_file():
        errors.append("missing required Pages asset: favicon.svg")
    return errors


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: validate_pages.py <site-root>", file=sys.stderr)
        return 2
    root = Path(sys.argv[1])
    if not root.is_dir():
        print(f"site root is not a directory: {root}", file=sys.stderr)
        return 2
    errors = validate_site(root)
    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1
    html_count = sum(1 for _ in root.rglob("*.html"))
    print(f"validated GitHub Pages site: {html_count} HTML document(s), local links and metadata OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
