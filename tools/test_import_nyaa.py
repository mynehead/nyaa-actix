"""python3 -m unittest discover tools"""
import hashlib
import unittest

from import_nyaa import add_tracker, bdecode, bencode, listing_ids, page_details, rss_ids


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

    def test_listing_ids(self):
        row = """<tr class="warning"><td><a href="/?c=1_2"></a></td><td colspan="2">
            <a href="/view/5#comments" class="comments">2</a><a href="/view/5" title="x">x</a></td>
            <td><a href="/download/5.torrent"></a><a href="magnet:?xt=urn:btih:ABCDEF0123456789ABCDEF0123456789ABCDEF01&dn=x"></a></td></tr>"""
        ours = '<tr class="default"><td><a href="/view/7" title="y">y</a><a href="/view/7/magnet"></a></td></tr>'
        self.assertEqual(listing_ids("<table>" + row + ours), [(5, "abcdef0123456789abcdef0123456789abcdef01"), (7, None)])

    def test_page_details(self):
        # Upstream's view.html as the uploader sees an anonymous, hidden remake
        page = """<div class="panel panel-danger">
	<div class="panel-heading" style="background-color: darkgray;">
		<h3 class="panel-title">
			<a href="/view/5/edit" title="Edit torrent"><i class="fa fa-fw fa-pencil"></i></a>
			[Grp] Show &amp; Tell - 01
		</h3>
	</div>
	<div class="panel-body">
		<div class="row">
			<div class="col-md-1">Category:</div>
			<div class="col-md-5">
				<a href="/?c=1_0">Anime</a> - <a href="/?c=1_2">English-translated</a>
			</div>
		</div>
		<div class="row">
			<div class="col-md-1">Submitter:</div>
			<div class="col-md-5">Anonymous (<a class="text-default" href="/user/Grp" title="User">Grp</a>)
			</div>
		</div>
		<div class="row">
			<div class="col-md-1">Info hash:</div>
			<div class="col-md-5"><kbd>ABCDEF0123456789ABCDEF0123456789ABCDEF01</kbd></div>
		</div>
		<div class="row">
			<div class="col-md-1">Information:</div>
			<div class="col-md-5">
				<a href="https://grp.example/?a=1&amp;b=2">https://grp.example/?a=1&amp;b=2</a>
			</div>
		</div>
	</div>
</div>
<div class="panel panel-default">
	<div markdown-text class="panel-body" id="torrent-description">**Bold** &lt;3&#10;line two</div>
</div>"""
        self.assertEqual(
            page_details(page),
            {
                "name": "[Grp] Show & Tell - 01",
                "hash": "abcdef0123456789abcdef0123456789abcdef01",
                "category": "1_2",
                "uploader": "Grp",
                "anonymous": True,
                "hidden": True,
                "remake": True,
                "trusted": False,
                "information": "https://grp.example/?a=1&b=2",
                "description": "**Bold** <3\nline two",
            },
        )
        public = page.replace("panel-danger", "panel-success").replace(' style="background-color: darkgray;"', "")
        public = public.replace("Anonymous (<a", "<a").replace("</a>)", "</a>")
        public = public.replace("**Bold** &lt;3&#10;line two", "#### No description.")
        details = page_details(public)
        self.assertEqual((details["uploader"], details["anonymous"], details["hidden"]), ("Grp", False, False))
        self.assertEqual((details["trusted"], details["remake"], details["description"]), (True, False, ""))


if __name__ == "__main__":
    unittest.main()
