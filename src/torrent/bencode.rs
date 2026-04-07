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
}

pub fn decode(data: &[u8]) -> Result<BencodeValue, BencodeError> {
    let (val, _) = decode_value(data, 0)?;
    Ok(val)
}

fn decode_value(data: &[u8], pos: usize) -> Result<(BencodeValue, usize), BencodeError> {
    if pos >= data.len() {
        return Err(BencodeError::UnexpectedEnd);
    }
    match data[pos] {
        b'i' => decode_int(data, pos),
        b'l' => decode_list(data, pos),
        b'd' => decode_dict(data, pos),
        b'0'..=b'9' => decode_bytes(data, pos),
        _ => Err(BencodeError::Invalid(pos)),
    }
}

fn decode_int(data: &[u8], pos: usize) -> Result<(BencodeValue, usize), BencodeError> {
    // i<int>e
    let end = data[pos..].iter().position(|&b| b == b'e')
        .ok_or(BencodeError::UnexpectedEnd)?;
    let end = pos + end;
    let s = std::str::from_utf8(&data[pos+1..end]).map_err(|_| BencodeError::InvalidInt)?;
    let n: i64 = s.parse().map_err(|_| BencodeError::InvalidInt)?;
    Ok((BencodeValue::Int(n), end + 1))
}

fn decode_bytes(data: &[u8], pos: usize) -> Result<(BencodeValue, usize), BencodeError> {
    // <len>:<data>
    let colon = data[pos..].iter().position(|&b| b == b':')
        .ok_or(BencodeError::UnexpectedEnd)?;
    let colon = pos + colon;
    let len_str = std::str::from_utf8(&data[pos..colon]).map_err(|_| BencodeError::InvalidLength)?;
    let len: usize = len_str.parse().map_err(|_| BencodeError::InvalidLength)?;
    let start = colon + 1;
    let end = start + len;
    if end > data.len() {
        return Err(BencodeError::UnexpectedEnd);
    }
    Ok((BencodeValue::Bytes(data[start..end].to_vec()), end))
}

fn decode_list(data: &[u8], pos: usize) -> Result<(BencodeValue, usize), BencodeError> {
    let mut items = Vec::new();
    let mut cur = pos + 1;
    while cur < data.len() && data[cur] != b'e' {
        let (val, next) = decode_value(data, cur)?;
        items.push(val);
        cur = next;
    }
    if cur >= data.len() {
        return Err(BencodeError::UnexpectedEnd);
    }
    Ok((BencodeValue::List(items), cur + 1))
}

fn decode_dict(data: &[u8], pos: usize) -> Result<(BencodeValue, usize), BencodeError> {
    let mut map = BTreeMap::new();
    let mut cur = pos + 1;
    while cur < data.len() && data[cur] != b'e' {
        let (key, next) = decode_bytes(data, cur)?;
        let key_bytes = match key {
            BencodeValue::Bytes(b) => b,
            _ => return Err(BencodeError::Invalid(cur)),
        };
        let (val, next2) = decode_value(data, next)?;
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
        match self { BencodeValue::Bytes(b) => Some(b), _ => None }
    }
    pub fn as_str(&self) -> Option<&str> {
        self.as_bytes().and_then(|b| std::str::from_utf8(b).ok())
    }
    pub fn as_int(&self) -> Option<i64> {
        match self { BencodeValue::Int(n) => Some(*n), _ => None }
    }
    pub fn as_list(&self) -> Option<&[BencodeValue]> {
        match self { BencodeValue::List(l) => Some(l), _ => None }
    }
    pub fn as_dict(&self) -> Option<&BTreeMap<Vec<u8>, BencodeValue>> {
        match self { BencodeValue::Dict(d) => Some(d), _ => None }
    }
    pub fn get(&self, key: &[u8]) -> Option<&BencodeValue> {
        self.as_dict()?.get(key)
    }
}
