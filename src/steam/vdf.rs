//! Steam's binary VDF, the format of `shortcuts.vdf`: nested maps of named
//! values, each a type byte, a NUL-terminated name, then the value. Read and
//! written whole, so entries rommd doesn't know about survive a rewrite.

use anyhow::{Context, Result, bail};

const MAP: u8 = 0x00;
const STRING: u8 = 0x01;
const INT32: u8 = 0x02;
const FLOAT32: u8 = 0x03;
const UINT64: u8 = 0x07;
const END: u8 = 0x08;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Map(Map),
    Str(String),
    Int(i32),
    /// A float32 or uint64, kept as read.
    Other(u8, Vec<u8>),
}

/// Named values in file order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Map(pub Vec<(String, Value)>);

impl Map {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn int(&self, key: &str) -> Option<i32> {
        match self.get(key)? {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn map(&self, key: &str) -> Option<&Map> {
        match self.get(key)? {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }

    /// Set `key`, in place when it is there (matching its case), else at the end.
    pub fn set(&mut self, key: &str, value: Value) {
        match self.0.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(key)) {
            Some((_, v)) => *v = value,
            None => self.0.push((key.to_owned(), value)),
        }
    }
}

pub fn parse(data: &[u8]) -> Result<Map> {
    let mut at = 0;
    let map = parse_map(data, &mut at, true).context("not a binary VDF file")?;
    if at != data.len() {
        bail!(
            "not a binary VDF file: {} bytes after its end",
            data.len() - at
        );
    }
    Ok(map)
}

/// The root map may end with the file instead of an end byte, as some
/// writers leave it.
fn parse_map(data: &[u8], at: &mut usize, root: bool) -> Result<Map> {
    let mut map = Map::default();
    loop {
        if root && *at == data.len() {
            return Ok(map);
        }
        let kind = *data.get(*at).context("ends inside a map")?;
        *at += 1;
        if kind == END {
            return Ok(map);
        }
        let name = c_string(data, at)?;
        let value = match kind {
            MAP => Value::Map(parse_map(data, at, false)?),
            STRING => Value::Str(c_string(data, at)?),
            INT32 => Value::Int(i32::from_le_bytes(take(data, at, 4)?.try_into().unwrap())),
            FLOAT32 => Value::Other(kind, take(data, at, 4)?.to_vec()),
            UINT64 => Value::Other(kind, take(data, at, 8)?.to_vec()),
            other => bail!("unknown value type {other:#04x}"),
        };
        map.0.push((name, value));
    }
}

fn take<'a>(data: &'a [u8], at: &mut usize, n: usize) -> Result<&'a [u8]> {
    let bytes = data.get(*at..*at + n).context("ends inside a value")?;
    *at += n;
    Ok(bytes)
}

fn c_string(data: &[u8], at: &mut usize) -> Result<String> {
    let len = data[*at..]
        .iter()
        .position(|&b| b == 0)
        .context("ends inside a string")?;
    let s = String::from_utf8_lossy(&data[*at..*at + len]).into_owned();
    *at += len + 1;
    Ok(s)
}

/// The file's bytes: `map`'s values, then its end byte.
pub fn write(map: &Map) -> Vec<u8> {
    let mut out = Vec::new();
    write_map(map, &mut out);
    out
}

fn write_map(map: &Map, out: &mut Vec<u8>) {
    for (name, value) in &map.0 {
        let kind = match value {
            Value::Map(_) => MAP,
            Value::Str(_) => STRING,
            Value::Int(_) => INT32,
            Value::Other(kind, _) => *kind,
        };
        out.push(kind);
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        match value {
            Value::Map(m) => write_map(m, out),
            Value::Str(s) => {
                out.extend_from_slice(s.as_bytes());
                out.push(0);
            }
            Value::Int(i) => out.extend_from_slice(&i.to_le_bytes()),
            Value::Other(_, bytes) => out.extend_from_slice(bytes),
        }
    }
    out.push(END);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `shortcuts.vdf` with one shortcut, as Steam writes it.
    fn sample() -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(b"\x00shortcuts\x00");
        d.extend_from_slice(b"\x000\x00");
        d.extend_from_slice(b"\x02appid\x00");
        d.extend_from_slice(&(-123456i32).to_le_bytes());
        d.extend_from_slice(b"\x01AppName\x00Some Game\x00");
        d.extend_from_slice(b"\x01Exe\x00\"/usr/bin/game\"\x00");
        d.extend_from_slice(b"\x07Devkit\x00\x01\x02\x03\x04\x05\x06\x07\x08");
        d.extend_from_slice(b"\x00tags\x00\x010\x00favorite\x00\x08");
        d.extend_from_slice(b"\x08\x08\x08");
        d
    }

    #[test]
    fn reads_and_writes_back_byte_for_byte() {
        let data = sample();
        let root = parse(&data).unwrap();
        let shortcut = root.map("shortcuts").unwrap().map("0").unwrap();
        assert_eq!(shortcut.int("appid"), Some(-123456));
        assert_eq!(shortcut.str("appname"), Some("Some Game"));
        assert_eq!(shortcut.map("tags").unwrap().str("0"), Some("favorite"));
        assert_eq!(write(&root), data);
    }

    #[test]
    fn refuses_what_isnt_vdf() {
        assert!(parse(b"\x00shortcuts\x00\x05x\x00").is_err());
        assert!(parse(b"\x00shortcuts\x00").is_err());
        let mut no_root_end = sample();
        no_root_end.pop();
        assert_eq!(parse(&no_root_end).unwrap(), parse(&sample()).unwrap());
        let mut trailing = sample();
        trailing.push(0x42);
        assert!(parse(&trailing).is_err());
    }
}
