pub mod bencode;
pub mod magnet;

use bencode::BencodeValue;
use sha1::{Sha1, Digest};

#[derive(Debug)]
pub struct TorrentMeta {
    pub info_hash: Vec<u8>,
    pub display_name: String,
    pub filesize: i64,
    pub encoding: String,
    pub bencoded_info: Vec<u8>,
}

pub fn parse_torrent(data: &[u8]) -> Result<TorrentMeta, bencode::BencodeError> {
    let val = bencode::decode(data)?;
    let dict = val.as_dict().ok_or(bencode::BencodeError::Invalid(0))?;

    let info = dict.get(b"info".as_slice()).ok_or(bencode::BencodeError::Invalid(0))?;
    let info_dict = info.as_dict().ok_or(bencode::BencodeError::Invalid(0))?;

    let bencoded_info = bencode::encode(info);
    let mut hasher = Sha1::new();
    hasher.update(&bencoded_info);
    let info_hash = hasher.finalize().to_vec();

    let encoding = dict.get(b"encoding".as_slice())
        .and_then(|v| v.as_str())
        .unwrap_or("utf-8")
        .to_lowercase();

    let name = info_dict.get(b"name".as_slice())
        .and_then(|v| v.as_bytes())
        .and_then(|b| std::str::from_utf8(b).ok())
        .unwrap_or("Unknown")
        .to_string();

    let filesize = if let Some(length) = info_dict.get(b"length".as_slice()).and_then(|v| v.as_int()) {
        length
    } else if let Some(files) = info_dict.get(b"files".as_slice()).and_then(|v| v.as_list()) {
        files.iter()
            .filter_map(|f| f.get(b"length".as_slice()).and_then(|l| l.as_int()))
            .sum()
    } else {
        0
    };

    Ok(TorrentMeta {
        info_hash,
        display_name: name,
        filesize,
        encoding,
        bencoded_info,
    })
}

pub fn rebuild_torrent(torrent: &crate::models::Torrent, bencoded_info: &[u8], trackers: &[&str], site_url: &str) -> Vec<u8> {
    let mut dict = std::collections::BTreeMap::new();

    if let Ok(info_val) = bencode::decode(bencoded_info) {
        dict.insert(b"info".to_vec(), info_val);
    }

    if !trackers.is_empty() {
        dict.insert(b"announce".to_vec(), BencodeValue::Bytes(trackers[0].as_bytes().to_vec()));
        if trackers.len() > 1 {
            let list = trackers.iter()
                .map(|t| BencodeValue::List(vec![BencodeValue::Bytes(t.as_bytes().to_vec())]))
                .collect();
            dict.insert(b"announce-list".to_vec(), BencodeValue::List(list));
        }
    }

    dict.insert(b"encoding".to_vec(), BencodeValue::Bytes(torrent.encoding.as_bytes().to_vec()));
    dict.insert(b"comment".to_vec(), BencodeValue::Bytes(
        format!("{}/view/{}", site_url, torrent.id).into_bytes()
    ));

    bencode::encode(&BencodeValue::Dict(dict))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const INFO: &[u8] = b"d6:lengthi5e4:name5:a.txt12:piece lengthi16384e6:pieces20:AAAAAAAAAAAAAAAAAAAAe";

    fn torrent_file(info: &[u8]) -> Vec<u8> {
        let mut data = b"d8:announce14:http://x/annou4:info".to_vec();
        data.extend_from_slice(info);
        data.push(b'e');
        data
    }

    pub(crate) fn sample_torrent() -> crate::models::Torrent {
        let now = chrono::NaiveDateTime::default();
        crate::models::Torrent {
            id: 7, info_hash: vec![0xab; 20], display_name: "Test & Co".into(),
            torrent_name: "a.txt".into(), information: String::new(), description: String::new(),
            filesize: 5, encoding: "utf-8".into(), flags: 0, uploader_id: None, uploader_ip: None,
            has_torrent: 1, comment_count: 0, created_time: now, updated_time: now,
            main_category_id: 1, sub_category_id: 1, group_id: None,
        }
    }

    #[test]
    fn parses_single_file_torrent() {
        let meta = parse_torrent(&torrent_file(INFO)).unwrap();
        assert_eq!(meta.info_hash, Sha1::digest(INFO).to_vec());
        assert_eq!(meta.display_name, "a.txt");
        assert_eq!(meta.filesize, 5);
        assert_eq!(meta.encoding, "utf-8");
        assert_eq!(meta.bencoded_info, INFO);
    }

    #[test]
    fn sums_multi_file_sizes() {
        let info = b"d5:filesld6:lengthi3e4:pathl1:aeed6:lengthi4e4:pathl1:beee4:name3:dir12:piece lengthi16384e6:pieces20:AAAAAAAAAAAAAAAAAAAAe";
        let meta = parse_torrent(&torrent_file(info)).unwrap();
        assert_eq!(meta.filesize, 7);
        assert_eq!(meta.display_name, "dir");
    }

    #[test]
    fn rejects_file_without_info() {
        assert!(parse_torrent(b"d8:announce1:xe").is_err());
        assert!(parse_torrent(b"not bencode").is_err());
    }

    #[test]
    fn rebuild_writes_trackers_and_keeps_info_hash() {
        let trackers = ["udp://t1/announce", "http://t2/announce"];
        let out = rebuild_torrent(&sample_torrent(), INFO, &trackers, "https://site.test");
        let meta = parse_torrent(&out).unwrap();
        assert_eq!(meta.info_hash, Sha1::digest(INFO).to_vec());

        let root = bencode::decode(&out).unwrap();
        assert_eq!(root.get(b"announce").and_then(|v| v.as_str()), Some("udp://t1/announce"));
        let tiers: Vec<&str> = root.get(b"announce-list").and_then(|v| v.as_list()).unwrap()
            .iter().map(|tier| tier.as_list().unwrap()[0].as_str().unwrap()).collect();
        assert_eq!(tiers, trackers);
        assert_eq!(root.get(b"comment").and_then(|v| v.as_str()), Some("https://site.test/view/7"));
    }

    #[test]
    fn rebuild_without_trackers_omits_announce() {
        let root = bencode::decode(&rebuild_torrent(&sample_torrent(), INFO, &[], "https://site.test")).unwrap();
        assert!(root.get(b"announce").is_none());
        assert!(root.get(b"announce-list").is_none());
    }

    #[test]
    fn rebuild_with_one_tracker_has_no_announce_list() {
        let root = bencode::decode(&rebuild_torrent(&sample_torrent(), INFO, &["udp://t1/announce"], "https://site.test")).unwrap();
        assert_eq!(root.get(b"announce").and_then(|v| v.as_str()), Some("udp://t1/announce"));
        assert!(root.get(b"announce-list").is_none());
    }
}
