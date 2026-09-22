//! Minimal read-only SQLite reader: enough of the file format to walk one
//! table of a database, including its write-ahead log.
//!
//! Only what Firefox's cookie store needs: table b-trees, the record format,
//! overflow pages and WAL frames. No SQL, no indexes, no writing.

use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Real(f) => Some(*f as i64),
            _ => None,
        }
    }
}

pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

impl Table {
    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.eq_ignore_ascii_case(name))
    }

    /// Value of `column` in `row`, or `Null` when the column is missing.
    pub fn get<'a>(&self, row: &'a [Value], column: &str) -> &'a Value {
        self.column(column).and_then(|i| row.get(i)).unwrap_or(&Value::Null)
    }
}

pub struct Database {
    data: Vec<u8>,
    /// Pages replaced by newer versions in the write-ahead log.
    wal: HashMap<u32, Vec<u8>>,
    page_size: usize,
    usable: usize,
}

const HEADER_MAGIC: &[u8] = b"SQLite format 3\0";
/// Guards against corrupt files sending the walk into a loop.
const MAX_PAGES: usize = 200_000;

fn be16(b: &[u8], off: usize) -> usize {
    ((b[off] as usize) << 8) | b[off + 1] as usize
}

fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// Reads a SQLite variable length integer. Returns the value and its length.
fn varint(b: &[u8], off: usize) -> (i64, usize) {
    let mut value: u64 = 0;
    for i in 0..8 {
        let Some(&byte) = b.get(off + i) else { return (value as i64, i.max(1)) };
        value = (value << 7) | (byte & 0x7f) as u64;
        if byte & 0x80 == 0 {
            return (value as i64, i + 1);
        }
    }
    let last = b.get(off + 8).copied().unwrap_or(0);
    (((value << 8) | last as u64) as i64, 9)
}

impl Database {
    /// Opens a database file, applying its `-wal` sidecar when present.
    pub fn open(path: &Path) -> Result<Database, String> {
        let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if data.len() < 100 || !data.starts_with(HEADER_MAGIC) {
            return Err(format!("{} is not a SQLite database", path.display()));
        }
        // A database whose pages all sit in the WAL has an otherwise empty
        // header, so the page size may only be readable from the WAL itself.
        let wal_path = path.with_file_name(format!(
            "{}-wal",
            path.file_name().and_then(|n| n.to_str()).unwrap_or_default()
        ));
        let wal = std::fs::read(&wal_path).ok().filter(|w| w.len() >= 32);

        let page_size = match be16(&data, 16) {
            1 => 65_536,
            n if n >= 512 && n.is_power_of_two() => n,
            _ => match &wal {
                Some(wal) => be32(wal, 8) as usize,
                None => return Err("invalid page size".into()),
            },
        };
        if page_size < 512 || !page_size.is_power_of_two() {
            return Err(format!("invalid page size {page_size}"));
        }
        let mut db = Database { data, wal: HashMap::new(), page_size, usable: page_size };
        if let Some(wal) = wal {
            db.apply_wal(&wal);
        }

        // Read the real header from page 1, which the WAL may have replaced.
        let (reserved, encoding) = {
            let header = db.page(1)?;
            (header[20] as usize, be32(header, 56))
        };
        db.usable = page_size - reserved;
        if encoding != 1 {
            return Err("database is not UTF-8".into());
        }
        Ok(db)
    }

    /// Replays committed WAL frames so cookies written by a running browser
    /// are visible. Frames after the last commit are ignored.
    fn apply_wal(&mut self, wal: &[u8]) {
        if wal.len() < 32 {
            return;
        }
        let magic = be32(wal, 0);
        if magic != 0x377f_0682 && magic != 0x377f_0683 {
            return;
        }
        if be32(wal, 8) as usize != self.page_size {
            return;
        }
        let (salt1, salt2) = (be32(wal, 16), be32(wal, 20));
        let frame_size = 24 + self.page_size;
        let mut pending: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut offset = 32;
        while offset + frame_size <= wal.len() {
            let frame = &wal[offset..offset + frame_size];
            // A frame from an older checkpoint has stale salts: stop there.
            if be32(frame, 8) != salt1 || be32(frame, 12) != salt2 {
                break;
            }
            let page_no = be32(frame, 0);
            let commit = be32(frame, 4) != 0;
            pending.push((page_no, frame[24..].to_vec()));
            if commit {
                for (page, data) in pending.drain(..) {
                    self.wal.insert(page, data);
                }
            }
            offset += frame_size;
        }
    }

    fn page(&self, number: u32) -> Result<&[u8], String> {
        if let Some(page) = self.wal.get(&number) {
            return Ok(page);
        }
        let start = (number as usize)
            .checked_sub(1)
            .map(|n| n * self.page_size)
            .ok_or("page 0 requested")?;
        self.data
            .get(start..start + self.page_size)
            .ok_or_else(|| format!("page {number} is past the end of the file"))
    }

    /// Reads the table named `name` (its columns come from its CREATE TABLE).
    pub fn table(&self, name: &str) -> Result<Table, String> {
        let schema = self.read_table(1)?;
        // sqlite_schema: type, name, tbl_name, rootpage, sql
        let entry = schema
            .iter()
            .find(|row| {
                row.first().and_then(Value::as_str) == Some("table")
                    && row.get(1).and_then(Value::as_str) == Some(name)
            })
            .ok_or_else(|| format!("table {name} not found"))?;
        let root = entry.get(3).and_then(Value::as_int).ok_or("table has no root page")? as u32;
        let sql = entry.get(4).and_then(Value::as_str).unwrap_or_default();
        Ok(Table { columns: parse_columns(sql), rows: self.read_table(root)? })
    }

    fn read_table(&self, root: u32) -> Result<Vec<Vec<Value>>, String> {
        let mut rows = Vec::new();
        let mut stack = vec![root];
        let mut budget = MAX_PAGES;
        while let Some(number) = stack.pop() {
            budget = budget.checked_sub(1).ok_or("database walk is too long")?;
            let page = self.page(number)?;
            // Page 1 starts with the 100 byte file header.
            let base = if number == 1 { 100 } else { 0 };
            let kind = page[base];
            let cells = be16(page, base + 3);
            let content = base + if kind == 5 { 12 } else { 8 };
            match kind {
                13 => {
                    for i in 0..cells {
                        let pointer = be16(page, content + i * 2);
                        if pointer >= page.len() {
                            return Err("cell pointer out of range".into());
                        }
                        rows.push(self.read_record(page, pointer)?);
                    }
                }
                5 => {
                    stack.push(be32(page, base + 8));
                    for i in 0..cells {
                        let pointer = be16(page, content + i * 2);
                        if pointer + 4 > page.len() {
                            return Err("cell pointer out of range".into());
                        }
                        stack.push(be32(page, pointer));
                    }
                }
                _ => return Err(format!("unexpected b-tree page type {kind}")),
            }
        }
        Ok(rows)
    }

    /// Reads one table leaf cell into a row of values.
    fn read_record(&self, page: &[u8], offset: usize) -> Result<Vec<Value>, String> {
        let (size, n1) = varint(page, offset);
        let (_rowid, n2) = varint(page, offset + n1);
        let start = offset + n1 + n2;
        let size = size.max(0) as usize;

        // Payload beyond the page spills into a chain of overflow pages.
        let max_local = self.usable - 35;
        let payload = if size <= max_local {
            page.get(start..start + size).ok_or("truncated cell")?.to_vec()
        } else {
            let min_local = ((self.usable - 12) * 32 / 255) - 23;
            let k = min_local + (size - min_local) % (self.usable - 4);
            let local = if k <= max_local { k } else { min_local };
            let mut payload = page.get(start..start + local).ok_or("truncated cell")?.to_vec();
            let mut next = be32(page, start + local);
            let mut budget = MAX_PAGES;
            while next != 0 && payload.len() < size {
                budget = budget.checked_sub(1).ok_or("overflow chain is too long")?;
                let page = self.page(next)?;
                let take = (size - payload.len()).min(self.usable - 4);
                payload.extend_from_slice(&page[4..4 + take]);
                next = be32(page, 0);
            }
            payload
        };

        let (header_size, n) = varint(&payload, 0);
        let header_size = (header_size.max(0) as usize).min(payload.len());
        let mut types = Vec::new();
        let mut at = n;
        while at < header_size {
            let (serial, used) = varint(&payload, at);
            types.push(serial);
            at += used;
        }

        let mut values = Vec::with_capacity(types.len());
        let mut body = header_size;
        for serial in types {
            let (value, size) = decode(&payload, body, serial)?;
            values.push(value);
            body += size;
        }
        Ok(values)
    }
}

/// Decodes one value of the given serial type. Returns it and its byte size.
fn decode(payload: &[u8], at: usize, serial: i64) -> Result<(Value, usize), String> {
    let int = |n: usize| -> Result<i64, String> {
        let bytes = payload.get(at..at + n).ok_or("truncated record")?;
        let mut value = if bytes[0] & 0x80 != 0 { -1i64 } else { 0 };
        for &b in bytes {
            value = (value << 8) | b as i64;
        }
        Ok(value)
    };
    Ok(match serial {
        0 => (Value::Null, 0),
        1..=4 => (Value::Int(int(serial as usize)?), serial as usize),
        5 => (Value::Int(int(6)?), 6),
        6 => (Value::Int(int(8)?), 8),
        7 => {
            let bytes = payload.get(at..at + 8).ok_or("truncated record")?;
            (Value::Real(f64::from_be_bytes(bytes.try_into().unwrap())), 8)
        }
        8 => (Value::Int(0), 0),
        9 => (Value::Int(1), 0),
        10 | 11 => (Value::Null, 0),
        n if n % 2 == 0 => {
            let size = (n as usize - 12) / 2;
            let bytes = payload.get(at..at + size).ok_or("truncated record")?;
            (Value::Blob(bytes.to_vec()), size)
        }
        n => {
            let size = (n as usize - 13) / 2;
            let bytes = payload.get(at..at + size).ok_or("truncated record")?;
            (Value::Text(String::from_utf8_lossy(bytes).into_owned()), size)
        }
    })
}

/// Pulls column names out of a CREATE TABLE statement.
fn parse_columns(sql: &str) -> Vec<String> {
    let Some(start) = sql.find('(') else { return Vec::new() };
    let body = &sql[start + 1..sql.rfind(')').unwrap_or(sql.len())];
    let mut columns = Vec::new();
    let mut depth = 0;
    let mut current = String::new();
    for c in body.chars() {
        match c {
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => {
                columns.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    columns.push(current);

    const CONSTRAINTS: [&str; 6] = ["constraint", "primary", "unique", "check", "foreign", "key"];
    columns
        .iter()
        .filter_map(|definition| {
            let name = definition.trim().split([' ', '\t', '\n', '(']).next()?;
            let name = name.trim_matches(['"', '`', '[', ']', '\'']);
            let keyword = name.to_lowercase();
            (!name.is_empty() && !CONSTRAINTS.contains(&keyword.as_str())).then(|| name.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints() {
        assert_eq!(varint(&[0x00], 0), (0, 1));
        assert_eq!(varint(&[0x7f], 0), (127, 1));
        assert_eq!(varint(&[0x81, 0x00], 0), (128, 2));
        assert_eq!(varint(&[0x82, 0x21], 0), (289, 2));
    }

    #[test]
    fn columns_from_sql() {
        let sql = "CREATE TABLE moz_cookies (id INTEGER PRIMARY KEY, \
                   originAttributes TEXT NOT NULL DEFAULT '', name TEXT, value TEXT, \
                   host TEXT, expiry INTEGER, CONSTRAINT moz_uniqueid UNIQUE (name, host))";
        assert_eq!(
            parse_columns(sql),
            ["id", "originAttributes", "name", "value", "host", "expiry"]
        );
    }
}
