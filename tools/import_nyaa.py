#!/usr/bin/env python3
"""Copies your own torrents from a nyaa site (nyaa.si, sukebei, any site running upstream's
code or this one) to this site. Standard library only.

    python import_nyaa.py --source-user MyName --target https://our.site --key MyName:nyaa_...

It lists your uploads, oldest first: the public ones from the source's RSS feed, or with
--cookie (your signed-in browser cookie there) all of them from your own user page, anonymous
and hidden ones included. With --search "[Tag]" it takes that search's results instead
(anonymous uploads included), keeping names that contain the search text. It reads each one's details from its page, skips torrents already on
the target or uploaded by someone else, and uploads the rest through the target's API with the
same name, category, information, description and anonymous/hidden/remake/trusted flags.
No password for the source is needed. If the target requires its own
tracker in uploaded files, that tracker is added to the file (the info hash stays the same).
See docs/api.md.
"""
import argparse
import base64
import getpass
import html
import json
import os
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
import xml.etree.ElementTree as ET

USER_AGENT = "nyaa-import/1"


def bdecode(data, i=0):
    """One bencoded value at data[i:]; returns (value, end). Strings stay bytes."""
    c = data[i : i + 1]
    if c == b"i":
        end = data.index(b"e", i)
        return int(data[i + 1 : end]), end + 1
    if c in (b"l", b"d"):
        items, i = [], i + 1
        while data[i : i + 1] != b"e":
            value, i = bdecode(data, i)
            items.append(value)
        return (items if c == b"l" else dict(zip(items[::2], items[1::2]))), i + 1
    colon = data.index(b":", i)
    end = colon + 1 + int(data[i:colon])
    return data[colon + 1 : end], end


def bencode(value):
    if isinstance(value, int):
        return b"i%de" % value
    if isinstance(value, bytes):
        return b"%d:%s" % (len(value), value)
    if isinstance(value, list):
        return b"l" + b"".join(map(bencode, value)) + b"e"
    return b"d" + b"".join(bencode(k) + bencode(v) for k, v in sorted(value.items())) + b"e"


def add_tracker(torrent, url):
    """The .torrent with `url` as the first tracker tier. Every top-level value other than the
    tracker list is copied byte for byte, so the info dict and its hash do not change."""
    if torrent[:1] != b"d":
        raise ValueError("not a torrent file")
    raw, i = {}, 1
    while torrent[i : i + 1] != b"e":
        key, i = bdecode(torrent, i)
        _, end = bdecode(torrent, i)
        raw[key], i = torrent[i:end], end
    tiers = bdecode(raw[b"announce-list"])[0] if b"announce-list" in raw else []
    if not tiers and b"announce" in raw:
        tiers = [[bdecode(raw[b"announce"])[0]]]  # clients ignore `announce` once a list exists
    raw[b"announce-list"] = bencode([[url.encode()]] + tiers)
    return b"d" + b"".join(bencode(k) + raw[k] for k in sorted(raw)) + b"e"


def multipart(fields, files):
    """(content type, body) for a multipart/form-data POST; files are (name, filename, bytes)."""
    boundary = uuid.uuid4().hex
    body = b""
    for name, value in fields.items():
        body += f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"\r\n\r\n{value}\r\n'.encode()
    for name, filename, data in files:
        body += (
            f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"; filename="{filename}"\r\n'
            "Content-Type: application/x-bittorrent\r\n\r\n"
        ).encode() + data + b"\r\n"
    return f"multipart/form-data; boundary={boundary}", body + f"--{boundary}--\r\n".encode()


def rss_ids(xml):
    """(torrent id, hex info hash) of each item in an RSS page."""
    items = []
    for item in ET.fromstring(xml).iter("item"):
        view = item.findtext("guid") or ""
        # The nyaa namespace is each site's own URL (…/xmlns/nyaa), so match the local name
        info_hash = next(e.text for e in item if e.tag.endswith("}infoHash"))
        items.append((int(view.rstrip("/").rsplit("/", 1)[-1]), info_hash.strip().lower()))
    return items


def listing_ids(page):
    """(torrent id, hex info hash or None) of each row of an HTML torrent listing (a user's
    page). Upstream's rows carry the magnet link; this site's link to it instead."""
    items = []
    for row in page.split("<tr")[1:]:
        if view := re.search(r'href="/view/(\d+)"', row):
            info_hash = re.search(r"btih:([0-9a-fA-F]{40})", row)
            items.append((int(view[1]), info_hash and info_hash[1].lower()))
    return items


def text(fragment):
    return " ".join(html.unescape(re.sub(r"<[^>]+>", "", fragment)).split())


def page_details(page):
    """A torrent's details from its page (upstream's view.html markup, which this site keeps)."""
    found = {
        key: re.search(pattern, page, re.S)
        for key, pattern in {
            "panel": r'<div class="panel panel-(\w+)">\s*<div class="panel-heading"([^>]*)>',
            "name": r'<h3 class="panel-title">(.*?)</h3>',
            "hash": r"<kbd>([0-9a-fA-F]{40})</kbd>",
            "category": r'Category:</div>\s*<div class="col-md-5">(.*?)</div>',
            "submitter": r'Submitter:</div>\s*<div class="col-md-5">(.*?)</div>',
            "information": r'Information:</div>\s*<div class="col-md-5">(.*?)</div>',
            "description": r'id="torrent-description">(.*?)</div>',
        }.items()
    }
    if not all(found.values()):
        raise ValueError("unexpected page layout")
    information = text(found["information"][1])
    description = html.unescape(found["description"][1]).strip()
    submitter = text(found["submitter"][1])
    return {
        "name": text(found["name"][1]),
        "hash": found["hash"][1].lower(),
        "category": re.findall(r"c=(\d+_\d+)", found["category"][1])[-1],
        # "name", or "Anonymous (name)" when the viewer is the uploader
        "uploader": re.sub(r"^Anonymous \((.*)\)$", r"\1", submitter),
        "anonymous": submitter.startswith("Anonymous"),
        "hidden": "darkgray" in found["panel"][2],
        "remake": found["panel"][1] == "danger",
        "trusted": found["panel"][1] == "success",
        "information": "" if information == "No information." else information,
        "description": "" if description == "#### No description." else description,
    }


class Site:
    def __init__(self, url, user=None, secret=None, cookie=None):
        self.url = url.rstrip("/")
        self.auth = user and "Basic " + base64.b64encode(f"{user}:{secret}".encode()).decode()
        self.cookie = cookie

    def request(self, path, body=None, content_type=None):
        """(status, body bytes); HTTP errors are answers too, an unreachable site ends the run."""
        req = urllib.request.Request(self.url + path, data=body, headers={"User-Agent": USER_AGENT})
        if self.auth:
            req.add_header("Authorization", self.auth)
        if self.cookie:
            req.add_header("Cookie", self.cookie)
        if content_type:
            req.add_header("Content-Type", content_type)
        try:
            with urllib.request.urlopen(req, timeout=60) as res:
                return res.status, res.read()
        except urllib.error.HTTPError as e:
            return e.code, e.read()
        except urllib.error.URLError as e:
            sys.exit(f"Cannot reach {self.url}: {e.reason} (http or https?)")


def source_ids(source, user, search, delay):
    """Every (id, hash) the source lists, oldest first: a search's results (anonymous uploads
    included) from the feed, or `user`'s uploads, signed in (with a cookie) from their own page,
    which also shows their anonymous and hidden uploads, otherwise from the feed."""
    name = urllib.parse.quote(user or "")
    path, parse = (f"/user/{name}?p=", lambda b: listing_ids(b.decode("utf-8", "replace")))
    if search:
        path, parse = f"/?page=rss&q={urllib.parse.quote(search)}&p=", rss_ids
    elif not source.cookie:
        path, parse = f"/?page=rss&u={name}&p=", rss_ids
    seen, page = {}, 1
    while True:
        status, body = source.request(f"{path}{page}")
        if status != 200 and page == 1:
            sys.exit(f"Source listing answered {status}")
        if status != 200:  # past a page limit (MAX_PAGES here answers 404)
            print(f"Source listing stopped at page {page} ({status}); older uploads are left out")
            return sorted(seen.items())
        new = [item for item in parse(body) if item[0] not in seen]
        if not new:  # an empty page, or a site that repeats its last page
            return sorted(seen.items())
        seen.update(new)
        page += 1
        time.sleep(delay)


def import_one(source, target, user, search, tid, known_hash, delay, dry_run):
    """Copies one torrent; returns a line saying what happened."""
    status, page = source.request(f"/view/{tid}")
    if status != 200:
        return f"source page answered {status}"
    try:
        t = page_details(page.decode("utf-8", "replace"))
    except ValueError as e:
        return f"not imported: source page: {e}"
    if not known_hash and target.request(f"/api/info/{t['hash']}")[0] == 200:
        return "already on the target"
    # A search matches words anywhere; only names that contain the search text count
    if search and search.lower() not in t["name"].lower():
        return f"skipped, name doesn't contain {search}: {t['name']}"
    if user and t["uploader"].lower() != user.lower():
        return f"skipped, uploaded by {t['uploader']}"
    if dry_run:
        return f"would import {t['name']}" + " (anonymous)" * t["anonymous"] + " (hidden)" * t["hidden"]
    time.sleep(delay)
    status, torrent = source.request(f"/download/{tid}.torrent")
    if status != 200:
        return f"source download answered {status}"
    data = json.dumps({k: v for k, v in t.items() if k not in ("uploader", "hash")})
    added_tracker = False
    while True:
        content_type, body = multipart({"torrent_data": data}, [("torrent", f"{tid}.torrent", torrent)])
        status, answer = target.request("/api/upload", body, content_type)
        if status == 200:
            return f"imported as {json.loads(answer)['url']}"
        try:
            errors = json.loads(answer)["errors"]
        except (ValueError, KeyError):
            errors = answer[:200]
        if not isinstance(errors, dict):
            return f"target answered {status}: {errors}"
        message = " ".join(sum(errors.values(), []))
        # Both messages are the target's own (src/handlers/torrents.rs, create_torrent)
        if wait := re.search(r"upload again in (\d+) minute", message):
            print(f"  upload limit, waiting {wait[1]} min", flush=True)
            time.sleep(int(wait[1]) * 60 + 5)
        elif (need := re.search(r"Please include (\S+) in the trackers", message)) and not added_tracker:
            torrent, added_tracker = add_tracker(torrent, need[1]), True
        else:
            return f"not imported: {message}"


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--source", default="https://nyaa.si", help="site to copy from (default https://nyaa.si)")
    p.add_argument("--source-user", help="the username there whose uploads to copy")
    p.add_argument("--search", help='copy the results of this search there instead, e.g. "[Tag]"')
    p.add_argument("--target", required=True, help="this site's address")
    p.add_argument("--key", help="name:nyaa_... API key on this site (or NYAA_API_KEY)")
    p.add_argument(
        "--cookie",
        help="your signed-in Cookie header for the source (or NYAA_SOURCE_COOKIE), to include "
        "your anonymous and hidden uploads",
    )
    p.add_argument("--delay", type=float, default=2, help="seconds between source requests (default 2)")
    p.add_argument("--dry-run", action="store_true", help="only list what would be imported")
    p.add_argument("--limit", type=int, help="stop after this many torrents not yet on the target")
    args = p.parse_args()
    if not args.source_user and not args.search:
        p.error("give --source-user, --search or both")

    key = args.key or os.environ.get("NYAA_API_KEY") or getpass.getpass("API key on the target (name:nyaa_...): ")
    if ":" not in key:
        sys.exit("The key goes with your username: name:nyaa_...")
    source = Site(args.source, cookie=args.cookie or os.environ.get("NYAA_SOURCE_COOKIE"))
    target = Site(args.target, *key.split(":", 1))

    if target.request("/api/info/0")[0] in (401, 403):
        sys.exit("The target refused the API key")
    torrents = source_ids(source, args.source_user, args.search, args.delay)
    print(f"{len(torrents)} torrents listed for {args.search or args.source_user}")
    done = 0
    for tid, info_hash in torrents:
        if done == args.limit:
            print(f"Stopped after {done} (--limit)")
            break
        if info_hash and target.request(f"/api/info/{info_hash}")[0] == 200:
            print(f"#{tid}: already on the target")
            continue
        time.sleep(args.delay)
        result = import_one(source, target, args.source_user, args.search, tid, bool(info_hash), args.delay, args.dry_run)
        done += result != "already on the target"
        print(f"#{tid}: {result}", flush=True)


if __name__ == "__main__":
    main()
