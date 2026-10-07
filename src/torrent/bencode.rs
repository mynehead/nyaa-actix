use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum BencodeValue {
    Bytes(Vec<u8>),
    Int(i64),
    List(Vec<BencodeValue>),
    Dict(BTreeMap<Vec<u8>, BencodeValue>),
}

#[derive(Debug, thiserror::Error)]
pub enum BencodeError {
    #[error("Unexpected end of input")]
    UnexpectedEnd,
    #[error("Invalid bencode at position {0}")]
    Invalid(usize),
    #[error("Invalid integer")]
    InvalidInt,
    #[error("Invalid string length")]
    InvalidLength,
    #[error("Nesting deeper than {MAX_DEPTH} levels")]
    TooDeep,
}

/// Real torrents nest a handful of levels; the cap stops crafted input
/// from overflowing the stack and aborting the server.
const MAX_DEPTH: usize = 64;

pub fn decode(data: &[u8]) -> Result<BencodeValue, BencodeError> {
    let (val, _) = decode_value(data, 0, 0)?;
    Ok(val)
}

fn decode_value(data: &[u8], pos: usize, depth: usize) -> Result<(BencodeValue, usize), BencodeError> {
    if pos >= data.len() {
        return Err(BencodeError::UnexpectedEnd);
    }
    if depth > MAX_DEPTH {
        return Err(BencodeError::TooDeep);
    }
    match data[pos] {
        b'i' => decode_int(data, pos),
        b'l' => decode_list(data, pos, depth),
        b'd' => decode_dict(data, pos, depth),
        b'0'..=b'9' => decode_bytes(data, pos),
        _ => Err(BencodeError::Invalid(pos)),
    }
}

fn decode_int(data: &[u8], pos: usize) -> Result<(BencodeValue, usize), BencodeError> {
    // i<int>e
    let end = data[pos..].iter().position(|&b| b == b'e').ok_or(BencodeError::UnexpectedEnd)?;
    let end = pos + end;
    let s = std::str::from_utf8(&data[pos + 1..end]).map_err(|_| BencodeError::InvalidInt)?;
    let n: i64 = s.parse().map_err(|_| BencodeError::InvalidInt)?;
    Ok((BencodeValue::Int(n), end + 1))
}

fn decode_bytes(data: &[u8], pos: usize) -> Result<(BencodeValue, usize), BencodeError> {
    // <len>:<data>
    let colon = data[pos..].iter().position(|&b| b == b':').ok_or(BencodeError::UnexpectedEnd)?;
    let colon = pos + colon;
    let len_str = std::str::from_utf8(&data[pos..colon]).map_err(|_| BencodeError::InvalidLength)?;
    let len: usize = len_str.parse().map_err(|_| BencodeError::InvalidLength)?;
    let start = colon + 1;
    let end = start.checked_add(len).ok_or(BencodeError::InvalidLength)?;
    if end > data.len() {
        return Err(BencodeError::UnexpectedEnd);
    }
    Ok((BencodeValue::Bytes(data[start..end].to_vec()), end))
}

fn decode_list(data: &[u8], pos: usize, depth: usize) -> Result<(BencodeValue, usize), BencodeError> {
    let mut items = Vec::new();
    let mut cur = pos + 1;
    while cur < data.len() && data[cur] != b'e' {
        let (val, next) = decode_value(data, cur, depth + 1)?;
        items.push(val);
        cur = next;
    }
    if cur >= data.len() {
        return Err(BencodeError::UnexpectedEnd);
    }
    Ok((BencodeValue::List(items), cur + 1))
}

fn decode_dict(data: &[u8], pos: usize, depth: usize) -> Result<(BencodeValue, usize), BencodeError> {
    let mut map = BTreeMap::new();
    let mut cur = pos + 1;
    while cur < data.len() && data[cur] != b'e' {
        let (key, next) = decode_bytes(data, cur)?;
        let key_bytes = match key {
            BencodeValue::Bytes(b) => b,
            _ => return Err(BencodeError::Invalid(cur)),
        };
        let (val, next2) = decode_value(data, next, depth + 1)?;
        map.insert(key_bytes, val);
        cur = next2;
    }
    if cur >= data.len() {
        return Err(BencodeError::UnexpectedEnd);
    }
    Ok((BencodeValue::Dict(map), cur + 1))
}

pub fn encode(val: &BencodeValue) -> Vec<u8> {
    let mut out = Vec::new();
    encode_value(val, &mut out);
    out
}

fn encode_value(val: &BencodeValue, out: &mut Vec<u8>) {
    match val {
        BencodeValue::Bytes(b) => {
            out.extend_from_slice(b.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(b);
        }
        BencodeValue::Int(n) => {
            out.push(b'i');
            out.extend_from_slice(n.to_string().as_bytes());
            out.push(b'e');
        }
        BencodeValue::List(items) => {
            out.push(b'l');
            for item in items {
                encode_value(item, out);
            }
            out.push(b'e');
        }
        BencodeValue::Dict(map) => {
            out.push(b'd');
            for (k, v) in map {
                out.extend_from_slice(k.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(k);
                encode_value(v, out);
            }
            out.push(b'e');
        }
    }
}

impl BencodeValue {
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            BencodeValue::Bytes(b) => Some(b),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        self.as_bytes().and_then(|b| std::str::from_utf8(b).ok())
    }
    pub fn as_int(&self) -> Option<i64> {
        match self {
            BencodeValue::Int(n) => Some(*n),
            _ => None,
        }
    }
    pub fn as_list(&self) -> Option<&[BencodeValue]> {
        match self {
            BencodeValue::List(l) => Some(l),
            _ => None,
        }
    }
    pub fn as_dict(&self) -> Option<&BTreeMap<Vec<u8>, BencodeValue>> {
        match self {
            BencodeValue::Dict(d) => Some(d),
            _ => None,
        }
    }
    pub fn get(&self, key: &[u8]) -> Option<&BencodeValue> {
        self.as_dict()?.get(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_each_type() {
        assert_eq!(decode(b"i42e").unwrap(), BencodeValue::Int(42));
        assert_eq!(decode(b"i-7e").unwrap(), BencodeValue::Int(-7));
        assert_eq!(decode(b"4:spam").unwrap(), BencodeValue::Bytes(b"spam".to_vec()));
        assert_eq!(decode(b"0:").unwrap(), BencodeValue::Bytes(vec![]));
        assert_eq!(
            decode(b"l4:spami1ee").unwrap(),
            BencodeValue::List(vec![BencodeValue::Bytes(b"spam".to_vec()), BencodeValue::Int(1)])
        );
        let dict = decode(b"d3:bar4:spam3:fooi42ee").unwrap();
        assert_eq!(dict.get(b"bar").and_then(|v| v.as_str()), Some("spam"));
        assert_eq!(dict.get(b"foo").and_then(|v| v.as_int()), Some(42));
    }

    #[test]
    fn encode_round_trips() {
        let input: &[u8] = b"d4:infod6:lengthi5e4:name5:a.txte4:listl1:ai-1eee";
        assert_eq!(encode(&decode(input).unwrap()), input);
    }

    #[test]
    fn encodes_dict_keys_sorted() {
        let mut map = BTreeMap::new();
        map.insert(b"zz".to_vec(), BencodeValue::Int(1));
        map.insert(b"aa".to_vec(), BencodeValue::Int(2));
        assert_eq!(encode(&BencodeValue::Dict(map)), b"d2:aai2e2:zzi1ee");
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [&b""[..], b"x", b"i12", b"iabce", b"5:abc", b"l4:spam", b"d3:foo", b"di1ei2ee", b"3abc"] {
            assert!(decode(bad).is_err(), "expected error for {:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn rejects_huge_length_without_panicking() {
        assert!(decode(b"18446744073709551615:x").is_err());
    }

    #[test]
    fn rejects_deep_nesting_without_overflowing_the_stack() {
        let mut deep = vec![b'l'; 1_000_000];
        deep.extend(vec![b'e'; 1_000_000]);
        assert!(matches!(decode(&deep), Err(BencodeError::TooDeep)));

        let mut ok = vec![b'l'; MAX_DEPTH];
        ok.extend(vec![b'e'; MAX_DEPTH]);
        assert!(decode(&ok).is_ok());
    }
}
