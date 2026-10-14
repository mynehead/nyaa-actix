#!/usr/bin/env python3
"""Copies your own torrents from a nyaa site (nyaa.si, sukebei, any site running upstream's
code or this one) to this site, through both sites' JSON API. Standard library only.

    python import_nyaa.py --source-user MyName --target https://our.site --key MyName:nyaa_...

It lists your uploads from the source's RSS feed, oldest first, reads each one's details from
the source's /api/info (which needs your source password; it is only sent to the source),
skips torrents uploaded by someone else or already on the target, and uploads the rest with
the same name, category, information, description and flags. If the target requires its own
tracker in uploaded files, that tracker is added to the file (the info hash stays the same).
See docs/api.md.
"""
import argparse
import base64
import getpass
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


class Site:
    def __init__(self, url, user, secret):
        self.url = url.rstrip("/")
        self.auth = "Basic " + base64.b64encode(f"{user}:{secret}".encode()).decode()

    def request(self, path, body=None, content_type=None, auth=True):
        """(status, body bytes); HTTP errors are answers too."""
        req = urllib.request.Request(self.url + path, data=body, headers={"User-Agent": USER_AGENT})
        if auth:
            req.add_header("Authorization", self.auth)
        if content_type:
            req.add_header("Content-Type", content_type)
        try:
            with urllib.request.urlopen(req, timeout=60) as res:
                return res.status, res.read()
        except urllib.error.HTTPError as e:
            return e.code, e.read()


def source_ids(source, user, delay):
    """Every (id, hash) the source's feed lists for `user`, oldest first."""
    seen, page = {}, 1
    while True:
        status, xml = source.request(f"/?page=rss&u={urllib.parse.quote(user)}&p={page}", auth=False)
        if status != 200 and page == 1:
            sys.exit(f"Source feed answered {status}")
        if status != 200:  # past a page limit (MAX_PAGES here answers 404)
            print(f"Source feed stopped at page {page} ({status}); older uploads are left out")
            return sorted(seen.items())
        new = [item for item in rss_ids(xml) if item[0] not in seen]
        if not new:  # an empty page, or a site that ignores p= and repeats the last one
            return sorted(seen.items())
        seen.update(new)
        page += 1
        time.sleep(delay)


def import_one(source, target, user, tid, delay, dry_run):
    """Copies one torrent; returns a line saying what happened."""
    status, body = source.request(f"/api/info/{tid}")
    if status in (401, 403):
        sys.exit(f"The source refused your password: {body[:200]!r}")
    if status != 200:
        return f"source /api/info answered {status}: {body[:200]!r}"
    info = json.loads(body)
    if (info.get("submitter") or "").lower() != user.lower():
        return f"skipped, uploaded by {info.get('submitter')}"
    if dry_run:
        return f"would import {info['name']}"
    time.sleep(delay)
    status, torrent = source.request(f"/download/{tid}.torrent", auth=False)
    if status != 200:
        return f"source download answered {status}"
    data = json.dumps(
        {
            "name": info["name"],
            "category": f"{info['main_category_id']}_{info['sub_category_id']}",
            "information": info.get("information") or "",
            "description": info.get("description") or "",
            "complete": bool(info.get("is_complete")),
            "remake": bool(info.get("is_remake")),
            "trusted": bool(info.get("is_trusted")),
        }
    )
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
    p.add_argument("--source-user", required=True, help="your username there")
    p.add_argument("--target", required=True, help="this site's address")
    p.add_argument("--key", help="name:nyaa_... API key on this site (or NYAA_API_KEY)")
    p.add_argument("--delay", type=float, default=2, help="seconds between source requests (default 2)")
    p.add_argument("--dry-run", action="store_true", help="only list what would be imported")
    args = p.parse_args()

    key = args.key or os.environ.get("NYAA_API_KEY") or getpass.getpass("API key on the target (name:nyaa_...): ")
    if ":" not in key:
        sys.exit("The key goes with your username: name:nyaa_...")
    password = os.environ.get("NYAA_SOURCE_PASSWORD") or getpass.getpass(f"Password for {args.source_user} on {args.source}: ")
    source = Site(args.source, args.source_user, password)
    target = Site(args.target, *key.split(":", 1))

    torrents = source_ids(source, args.source_user, args.delay)
    print(f"{len(torrents)} torrents listed for {args.source_user}")
    for tid, info_hash in torrents:
        status, _ = target.request(f"/api/info/{info_hash}")
        if status == 200:
            print(f"#{tid}: already on the target")
            continue
        if status in (401, 403):
            sys.exit("The target refused the API key")
        time.sleep(args.delay)
        print(f"#{tid}: {import_one(source, target, args.source_user, tid, args.delay, args.dry_run)}", flush=True)


if __name__ == "__main__":
    main()
