use anyhow::{anyhow, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct TorrentFile {
    pub path: String,
    pub length: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TorrentMeta {
    pub name: String,
    pub files: Vec<TorrentFile>,
}

enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dict(Vec<(Vec<u8>, Value)>),
}

impl Value {
    fn as_dict(&self) -> Option<&Vec<(Vec<u8>, Value)>> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }
    fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }
    fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }
    fn as_list(&self) -> Option<&Vec<Value>> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }
    fn get(&self, key: &str) -> Option<&Value> {
        self.as_dict()?.iter().find(|(k, _)| k == key.as_bytes()).map(|(_, v)| v)
    }
}

fn parse_bytes(data: &[u8], pos: &mut usize) -> Result<Vec<u8>> {
    let start = *pos;
    while data.get(*pos).is_some_and(u8::is_ascii_digit) {
        *pos += 1;
    }
    if data.get(*pos) != Some(&b':') {
        return Err(anyhow!("expected ':' in bencode string length"));
    }
    let len: usize = std::str::from_utf8(&data[start..*pos])?.parse()?;
    *pos += 1;
    let bytes = data
        .get(*pos..*pos + len)
        .ok_or_else(|| anyhow!("truncated bencode string"))?
        .to_vec();
    *pos += len;
    Ok(bytes)
}

fn parse_value(data: &[u8], pos: &mut usize) -> Result<Value> {
    match data.get(*pos) {
        Some(b'i') => {
            *pos += 1;
            let start = *pos;
            while data.get(*pos).is_some() && data.get(*pos) != Some(&b'e') {
                *pos += 1;
            }
            let n: i64 = std::str::from_utf8(&data[start..*pos])?.parse()?;
            *pos += 1;
            Ok(Value::Int(n))
        }
        Some(b'l') => {
            *pos += 1;
            let mut items = Vec::new();
            while data.get(*pos) != Some(&b'e') {
                if data.get(*pos).is_none() {
                    return Err(anyhow!("unterminated list"));
                }
                items.push(parse_value(data, pos)?);
            }
            *pos += 1;
            Ok(Value::List(items))
        }
        Some(b'd') => {
            *pos += 1;
            let mut items = Vec::new();
            while data.get(*pos) != Some(&b'e') {
                if data.get(*pos).is_none() {
                    return Err(anyhow!("unterminated dict"));
                }
                let key = parse_bytes(data, pos)?;
                let val = parse_value(data, pos)?;
                items.push((key, val));
            }
            *pos += 1;
            Ok(Value::Dict(items))
        }
        Some(c) if c.is_ascii_digit() => Ok(Value::Bytes(parse_bytes(data, pos)?)),
        _ => Err(anyhow!("invalid bencode at byte {pos}")),
    }
}

/// Parse a `.torrent` file's bytes and extract its name and file list.
/// Torrent files are self-describing, so this needs no network access.
pub fn parse_torrent_file(data: &[u8]) -> Result<TorrentMeta> {
    let mut pos = 0;
    let root = parse_value(data, &mut pos)?;
    let info = root.get("info").ok_or_else(|| anyhow!("missing 'info' dict"))?;
    let name = info
        .get("name")
        .and_then(Value::as_bytes)
        .map(|b| String::from_utf8_lossy(b).to_string())
        .ok_or_else(|| anyhow!("missing 'name' in info dict"))?;

    let files = if let Some(file_list) = info.get("files").and_then(Value::as_list) {
        file_list
            .iter()
            .filter_map(|f| {
                let length = f.get("length")?.as_int()? as u64;
                let path_parts = f.get("path")?.as_list()?;
                let path = path_parts
                    .iter()
                    .filter_map(Value::as_bytes)
                    .map(|b| String::from_utf8_lossy(b).to_string())
                    .collect::<Vec<_>>()
                    .join("/");
                Some(TorrentFile { path, length })
            })
            .collect()
    } else {
        let length = info.get("length").and_then(Value::as_int).unwrap_or(0) as u64;
        vec![TorrentFile { path: name.clone(), length }]
    };

    Ok(TorrentMeta { name, files })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_file_torrent() {
        // d8:announce3:xyz4:infod6:lengthi1024e4:name8:file.mkv12:piece lengthi16384e6:pieces0:ee
        let bencode = b"d8:announce3:xyz4:infod6:lengthi1024e4:name8:file.mkv12:piece lengthi16384e6:pieces0:ee";
        let meta = parse_torrent_file(bencode).unwrap();
        assert_eq!(meta.name, "file.mkv");
        assert_eq!(meta.files, vec![TorrentFile { path: "file.mkv".to_string(), length: 1024 }]);
    }

    #[test]
    fn parses_multi_file_torrent() {
        // info.files = [ {length: 100, path: ["a.txt"]}, {length: 200, path: ["sub", "b.txt"]} ]
        let bencode = b"d4:infod5:filesld6:lengthi100e4:pathl5:a.txteed6:lengthi200e4:pathl3:sub5:b.txteee4:name6:MyPack12:piece lengthi16384e6:pieces0:ee";
        let meta = parse_torrent_file(bencode).unwrap();
        assert_eq!(meta.name, "MyPack");
        assert_eq!(meta.files.len(), 2);
        assert_eq!(meta.files[0], TorrentFile { path: "a.txt".to_string(), length: 100 });
        assert_eq!(meta.files[1], TorrentFile { path: "sub/b.txt".to_string(), length: 200 });
    }

    #[test]
    fn rejects_garbage_input() {
        assert!(parse_torrent_file(b"not bencode at all").is_err());
    }
}
