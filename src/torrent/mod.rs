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

pub fn rebuild_torrent(torrent: &crate::models::Torrent, bencoded_info: &[u8], trackers: &[&str]) -> Vec<u8> {
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
        format!("https://nyaa.si/view/{}", torrent.id).into_bytes()
    ));

    bencode::encode(&BencodeValue::Dict(dict))
}
