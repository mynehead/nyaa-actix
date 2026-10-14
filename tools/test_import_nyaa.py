"""python3 -m unittest discover tools"""
import hashlib
import unittest

from import_nyaa import add_tracker, bdecode, bencode, rss_ids


class ImportTest(unittest.TestCase):
    def test_add_tracker_keeps_the_info_hash(self):
        # Unsorted info keys: re-encoding a decoded dict would change these bytes
        info = b"d4:name3:abc6:lengthi3e12:piece lengthi16384e6:pieces20:" + b"x" * 20 + b"e"
        torrent = b"d8:announce10:http://old4:info" + info + b"e"
        out = add_tracker(torrent, "http://new")
        top, end = bdecode(out)
        self.assertEqual(end, len(out))
        self.assertEqual(top[b"announce-list"], [[b"http://new"], [b"http://old"]])
        self.assertEqual(top[b"announce"], b"http://old")
        start = out.index(b"4:info") + 6
        self.assertEqual(hashlib.sha1(out[start : start + len(info)]).digest(), hashlib.sha1(info).digest())

    def test_add_tracker_goes_first_in_an_existing_list(self):
        torrent = bencode({b"announce-list": [[b"a"], [b"b"]], b"info": {b"name": b"x"}})
        self.assertEqual(bdecode(add_tracker(torrent, "n"))[0][b"announce-list"], [[b"n"], [b"a"], [b"b"]])

    def test_rss_ids(self):
        xml = b"""<rss xmlns:nyaa="https://sukebei.nyaa.si/xmlns/nyaa" version="2.0"><channel>
            <item><guid isPermaLink="true">https://nyaa.si/view/12</guid><nyaa:infoHash>ABCD</nyaa:infoHash></item>
            <item><guid isPermaLink="true">https://nyaa.si/view/9</guid><nyaa:infoHash> ef01</nyaa:infoHash></item>
            </channel></rss>"""
        self.assertEqual(rss_ids(xml), [(12, "abcd"), (9, "ef01")])


if __name__ == "__main__":
    unittest.main()
