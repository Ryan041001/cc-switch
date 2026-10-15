//! Actual Antigravity ACP generation tokens, integrated into session sync.
//! No HTTP/OAuth or prompt export. Like every other importer (Pi/Claude/Codex),
//! USD cost is not returned upstream: it is computed locally from CC Switch's own
//! `model_pricing` table, and stays unpriced while the model has no pricing row.
//! Request/semantic ids are part of the on-disk dedup contract: changing them
//! would re-import already-recorded calls, so they are frozen as-is.

use crate::database::Database;
use crate::error::AppError;
use crate::proxy::usage::calculator::CostCalculator;
use crate::proxy::usage::parser::TokenUsage;
use crate::services::session_usage::SessionSyncResult;
use crate::services::sql_helpers::INPUT_TOKEN_SEMANTICS_FRESH;
use crate::services::usage_stats::find_model_pricing;
use rusqlite::{params, Connection, OpenFlags, TransactionBehavior};
use rust_decimal::Decimal;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const PROVIDER: &str = "antigravity-acp";
// Keep this ledger namespace stable across upgrades: it is part of dedup identity.
const SOURCE: &str = "pi_session";
const ROW_SOURCE: &str = "antigravity_native";
/// Session importers all bill under `pi`; Antigravity's outer turn is a Pi agent
/// session, and `pi` keeps the fresh-input semantics our native counts use.
const APP_TYPE: &str = "pi";
const MARKER: &str = "antigravity_native_tokens_v1";
const PREFIX: &str = "antigravity_native:";

const MAX_BLOB: usize = 32 * 1024 * 1024;
type SafeResult<T> = Result<T, &'static str>;

#[derive(Clone, Copy)]
enum Value<'a> {
    Int(u64),
    Bytes(&'a [u8]),
    Fixed,
}
struct Proto<'a>(Vec<(u32, Value<'a>)>);

fn varint(data: &[u8], offset: &mut usize) -> SafeResult<u64> {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let byte = *data.get(*offset).ok_or("truncated protobuf varint")?;
        *offset += 1;
        if shift == 63 && byte > 1 {
            return Err("overflowing protobuf varint");
        }
        value |= u64::from(byte & 127) << shift;
        if byte < 128 {
            return Ok(value);
        }
    }
    Err("invalid protobuf varint")
}

impl<'a> Proto<'a> {
    fn parse(data: &'a [u8]) -> SafeResult<Self> {
        if data.len() > MAX_BLOB {
            return Err("oversized native metadata");
        }
        let mut fields = Vec::new();
        let mut offset = 0;
        while offset < data.len() {
            let tag = varint(data, &mut offset)?;
            let number = tag >> 3;
            if !(1..1 << 29).contains(&number) {
                return Err("invalid protobuf field number");
            }
            let value = match tag & 7 {
                0 => Value::Int(varint(data, &mut offset)?),
                wire @ (1 | 5) => {
                    offset += if wire == 1 { 8 } else { 4 };
                    if offset > data.len() {
                        return Err("truncated fixed protobuf field");
                    }
                    Value::Fixed
                }
                2 => {
                    let length = usize::try_from(varint(data, &mut offset)?)
                        .map_err(|_| "invalid protobuf length")?;
                    let end = offset
                        .checked_add(length)
                        .ok_or("invalid protobuf length")?;
                    let bytes = data.get(offset..end).ok_or("truncated protobuf field")?;
                    offset = end;
                    Value::Bytes(bytes)
                }
                _ => return Err("unsupported protobuf wire type"),
            };
            fields.push((number as u32, value));
        }
        Ok(Self(fields))
    }
    fn one(&self, field: u32) -> SafeResult<Option<Value<'a>>> {
        let mut values = self.0.iter().filter(|(n, _)| *n == field).map(|(_, v)| *v);
        let value = values.next();
        if values.next().is_some() {
            return Err("repeated scalar metadata field");
        }
        Ok(value)
    }
    fn has(&self, field: u32) -> bool {
        self.0.iter().any(|(n, _)| *n == field)
    }
    fn int(&self, field: u32) -> SafeResult<u64> {
        match self.one(field)? {
            None => Ok(0),
            Some(Value::Int(n)) => Ok(n),
            _ => Err("unexpected integer wire type"),
        }
    }
    fn bytes(&self, field: u32) -> SafeResult<&'a [u8]> {
        match self.one(field)? {
            None => Ok(&[]),
            Some(Value::Bytes(bytes)) => Ok(bytes),
            _ => Err("unexpected message wire type"),
        }
    }
    fn sub(&self, field: u32) -> SafeResult<Proto<'a>> {
        Proto::parse(self.bytes(field)?)
    }
    fn text(&self, field: u32) -> SafeResult<&'a str> {
        let data = self.bytes(field)?;
        if data.len() > 1024 {
            return Err("oversized native metadata label");
        }
        let text = std::str::from_utf8(data).map_err(|_| "invalid metadata label encoding")?;
        if text.chars().any(|c| c < ' ') {
            return Err("unsafe metadata label");
        }
        Ok(text)
    }
    fn repeated_messages(&self, field: u32) -> SafeResult<Vec<Proto<'a>>> {
        self.0
            .iter()
            .filter(|(n, _)| *n == field)
            .map(|(_, v)| match v {
                Value::Bytes(bytes) => Proto::parse(bytes),
                _ => Err("invalid descriptor message type"),
            })
            .collect()
    }
}

// Read the embedded ZIP without loading/copying the whole official executable.
struct Section {
    file: File,
    start: u64,
    length: u64,
    position: u64,
}
impl Read for Section {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let size = buffer
            .len()
            .min(self.length.saturating_sub(self.position) as usize);
        self.file
            .seek(SeekFrom::Start(self.start + self.position))?;
        let read = self.file.read(&mut buffer[..size])?;
        self.position += read as u64;
        Ok(read)
    }
}
impl Seek for Section {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let position = match from {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::Current(n) => i128::from(self.position) + i128::from(n),
            SeekFrom::End(n) => i128::from(self.length) + i128::from(n),
        };
        self.position = u64::try_from(position).map_err(|_| std::io::ErrorKind::InvalidInput)?;
        Ok(self.position)
    }
}
fn le32(data: &[u8], offset: usize) -> SafeResult<u32> {
    Ok(u32::from_le_bytes(
        data.get(offset..offset + 4)
            .ok_or("truncated Mach-O")?
            .try_into()
            .map_err(|_| "invalid Mach-O integer")?,
    ))
}
fn official_section(path: &Path) -> SafeResult<Section> {
    let mut file = File::open(path).map_err(|_| "official ACP server unavailable")?;
    let length = file
        .metadata()
        .map_err(|_| "official ACP metadata unavailable")?
        .len();
    let mut header = [0u8; 32];
    file.read_exact(&mut header)
        .map_err(|_| "invalid official ACP header")?;
    if header[..4] != [0xcf, 0xfa, 0xed, 0xfe] {
        return Err("unsupported official ACP executable format");
    }
    let count = le32(&header, 16)?;
    if count > 1000 {
        return Err("invalid official ACP command count");
    }
    for _ in 0..count {
        let mut header = [0u8; 8];
        file.read_exact(&mut header)
            .map_err(|_| "invalid official ACP load command")?;
        let command = le32(&header, 0)?;
        let size = le32(&header, 4)? as usize;
        if !(8..=1024 * 1024).contains(&size) {
            return Err("invalid official ACP load size");
        }
        let mut data = vec![0; size - 8];
        file.read_exact(&mut data)
            .map_err(|_| "truncated official ACP load command")?;
        if command == 0x19 && data.len() >= 64 {
            let count = le32(&data, 56)? as usize;
            if count > (data.len() - 64) / 80 {
                return Err("invalid official ACP section count");
            }
            for section in data[64..].chunks_exact(80).take(count) {
                if &section[..9] == b".par_data" && section[9..16].iter().all(|n| *n == 0) {
                    let size = u64::from_le_bytes(
                        section[40..48]
                            .try_into()
                            .map_err(|_| "invalid archive size")?,
                    );
                    let start = u64::from(le32(section, 48)?);
                    if start.checked_add(size).is_none_or(|end| end > length) {
                        return Err("invalid official ACP archive bounds");
                    }
                    return Ok(Section {
                        file,
                        start,
                        length: size,
                        position: 0,
                    });
                }
            }
        }
    }
    Err("official ACP descriptor archive not found")
}

// Decode ONLY a generated Python bytes literal; never evaluate vendor code.
fn descriptor_literal(code: &[u8]) -> SafeResult<Vec<u8>> {
    let marker = b".AddSerializedFile(";
    let mut offset = code
        .windows(marker.len())
        .position(|b| b == marker)
        .ok_or("official descriptor constant not found")?
        + marker.len();
    while code.get(offset).is_some_and(u8::is_ascii_whitespace) {
        offset += 1;
    }
    if code.get(offset) != Some(&b'b') {
        return Err("unsupported official descriptor literal");
    }
    offset += 1;
    let quote = *code.get(offset).ok_or("missing descriptor quote")?;
    if quote != b'\'' && quote != b'"' {
        return Err("unsupported descriptor quote");
    }
    offset += 1;
    let mut bytes = Vec::new();
    loop {
        let byte = *code.get(offset).ok_or("unterminated descriptor literal")?;
        offset += 1;
        if byte == quote {
            break;
        }
        if byte != b'\\' {
            if !byte.is_ascii() || byte == b'\n' || byte == b'\r' {
                return Err("invalid descriptor literal");
            }
            bytes.push(byte);
            continue;
        }
        let escape = *code.get(offset).ok_or("truncated descriptor escape")?;
        offset += 1;
        let value = match escape {
            b'\\' | b'\'' | b'"' => escape,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'a' => 7,
            b'b' => 8,
            b'f' => 12,
            b'v' => 11,
            b'x' => {
                let hex = code
                    .get(offset..offset + 2)
                    .ok_or("truncated descriptor hex escape")?;
                offset += 2;
                let text = std::str::from_utf8(hex).map_err(|_| "invalid descriptor hex")?;
                u8::from_str_radix(text, 16).map_err(|_| "invalid descriptor hex")?
            }
            _ => return Err("unsupported descriptor escape"),
        };
        bytes.push(value);
    }
    while code.get(offset).is_some_and(u8::is_ascii_whitespace) {
        offset += 1;
    }
    if code.get(offset) != Some(&b')') {
        return Err("unsupported concatenated descriptor");
    }
    Ok(bytes)
}

const SCHEMA: &[(&str, &str, u64, u64)] = &[
    ("CortexStepGeneratorMetadata", "chat_model", 1, 11),
    ("CortexStepGeneratorMetadata", "error", 5, 9),
    ("ChatModelMetadata", "usage", 4, 11),
    ("ChatModelMetadata", "chat_start_metadata", 9, 11),
    ("ChatModelMetadata", "time_to_first_token", 11, 11),
    ("ChatModelMetadata", "streaming_duration", 12, 11),
    ("ChatModelMetadata", "response_model", 19, 9),
    ("ChatModelMetadata", "model_display_name", 21, 9),
    ("ChatStartMetadata", "created_at", 4, 11),
    ("ModelUsageStats", "input_tokens", 2, 4),
    ("ModelUsageStats", "output_tokens", 3, 4),
    ("ModelUsageStats", "cache_write_tokens", 4, 4),
    ("ModelUsageStats", "cache_read_tokens", 5, 4),
    ("ModelUsageStats", "message_id", 7, 9),
    ("ModelUsageStats", "thinking_output_tokens", 9, 4),
    ("ModelUsageStats", "response_output_tokens", 10, 4),
    ("ModelUsageStats", "response_id", 11, 9),
    ("ModelUsageStats", "provider_assigned_message_id", 12, 9),
];
fn verify_schema(path: &Path) -> SafeResult<()> {
    let mut archive = zip::ZipArchive::new(official_section(path)?)
        .map_err(|_| "invalid official ACP descriptor archive")?;
    let mut observed = HashMap::new();
    for path in [
        "google3/third_party/jetski/cortex_pb/cortex_pb2.py",
        "google3/third_party/jetski/codeium_common_pb/codeium_common_pb2.py",
    ] {
        let mut member = archive
            .by_name(path)
            .map_err(|_| "official ACP schema member missing")?;
        if member.size() > 8 * 1024 * 1024 {
            return Err("oversized official descriptor");
        }
        let mut code = Vec::new();
        member
            .read_to_end(&mut code)
            .map_err(|_| "official descriptor read failed")?;
        let descriptor = descriptor_literal(&code)?;
        for message in Proto::parse(&descriptor)?.repeated_messages(4)? {
            let name = message.text(1)?;
            if SCHEMA.iter().any(|(expected, _, _, _)| *expected == name) {
                for field in message.repeated_messages(2)? {
                    observed.insert(
                        (name.to_owned(), field.text(1)?.to_owned()),
                        (field.int(3)?, field.int(5)?),
                    );
                }
            }
        }
    }
    for (message, field, number, kind) in SCHEMA {
        if observed.get(&(message.to_string(), field.to_string())) != Some(&(*number, *kind)) {
            return Err("official ACP token schema changed; refusing import");
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct Record {
    request_id: String,
    semantic_id: String,
    model: String,
    session: String,
    created_at: i64,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    thinking: i64,
    response: i64,
    duration_ms: i64,
    first_token_ms: i64,
    /// Raw upstream error text when the generation failed (None otherwise).
    /// Storing the real reason keeps the ledger's status codes and the detail
    /// panel consistent with what actually happened in the IDE.
    error: Option<String>,
    /// ACP's friendly model name (e.g. `gemini-3.8-flash-high`). `response_model`
    /// may carry an internal serving-variant suffix (e.g. `-n`), so this is the
    /// better basis for matching a pricing row.
    model_display: String,
}
impl Record {
    fn usage(&self) -> [i64; 4] {
        [self.input, self.output, self.cache_read, self.cache_write]
    }
}
fn digest(value: serde_json::Value) -> String {
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}
fn duration_ms(message: &Proto<'_>, field: u32) -> SafeResult<i64> {
    let duration = message.sub(field)?;
    let seconds = duration.int(1)?;
    let nanos = duration.int(2)?;
    if seconds > 86400 || nanos >= 1_000_000_000 {
        return Err("invalid model duration");
    }
    Ok((seconds * 1000 + nanos / 1_000_000) as i64)
}
/// Field 5 of the outer metadata carries the raw upstream error text. Keep it
/// readable, single-line and bounded so the detail panel shows the real
/// failure reason instead of a generic label.
fn error_text(bytes: &[u8]) -> Option<String> {
    let text: String = String::from_utf8_lossy(bytes)
        .chars()
        .map(|c| {
            if (c as u32) < 32 || c == '\u{7f}' {
                ' '
            } else {
                c
            }
        })
        .collect();
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(text.chars().take(500).collect())
}
fn parse_record(blob: &[u8], trajectory: &str, index: i64) -> SafeResult<Option<Record>> {
    let metadata = Proto::parse(blob)?;
    let chat = metadata.sub(1)?;
    // Field 5 on the outer metadata is the generation's error text. A failed
    // generation aborts before usage or the stream window is recorded, so the
    // token guards below must not swallow it: the imported row is what makes
    // the failure — and its status code — visible in the usage ledger.
    let error = error_text(metadata.bytes(5)?);
    let failed = error.is_some();
    if !failed && (!chat.has(4) || !chat.has(12)) {
        return Ok(None);
    }
    let usage = chat.sub(4)?;
    let mut counts = [0i64; 6];
    for (index, field) in [2, 3, 5, 4, 9, 10].iter().enumerate() {
        let value = usage.int(*field)?;
        if value > u64::from(u32::MAX) {
            return Err("invalid native token count");
        }
        counts[index] = value as i64;
    }
    let [input, output, cache_read, cache_write, thinking, response] = counts;
    if counts[..4].iter().all(|n| *n == 0) && !failed {
        return Ok(None);
    }
    if (usage.has(9) || usage.has(10)) && output != thinking + response {
        return Err("native output/thinking decomposition changed");
    }
    let model_display = chat.text(21)?.to_owned();
    let mut model = chat.text(19)?.to_owned();
    if model.is_empty() {
        // A failure can abort before the response model is assigned; fall back
        // to the ACP display name (then "unknown") so the row stays attributable.
        if !failed {
            return Err("actual response model missing");
        }
        model = if model_display.is_empty() {
            "unknown".to_owned()
        } else {
            model_display.clone()
        };
    }
    let timestamp = chat.sub(9)?.sub(4)?;
    let seconds = timestamp.int(1)?;
    let nanos = timestamp.int(2)?;
    if !(946684800..=4102444800).contains(&seconds) || nanos >= 1_000_000_000 {
        // A failure without a usable time cannot be attributed; skip that blob
        // instead of failing the whole source read.
        if failed {
            return Ok(None);
        }
        return Err("native generation timestamp missing or invalid");
    }
    let mut identity = json!(["native", trajectory, index]);
    for field in [12, 11, 7] {
        let id = usage.text(field)?;
        if !id.is_empty() {
            identity = json!(["upstream", model, field, id]);
            break;
        }
    }
    // Timing of a failed generation is not a billing fact; tolerate junk there.
    let (stream_ms, ttft_ms) = if failed {
        (
            duration_ms(&chat, 12).unwrap_or(0),
            duration_ms(&chat, 11).unwrap_or(0),
        )
    } else {
        (duration_ms(&chat, 12)?, duration_ms(&chat, 11)?)
    };
    Ok(Some(Record {
        request_id: format!("{PREFIX}{}", digest(identity)),
        semantic_id: format!(
            "antigravity_native_semantic:{}",
            digest(json!([model, seconds, nanos]))
        ),
        model,
        session: trajectory.to_owned(),
        created_at: seconds as i64,
        input,
        output,
        cache_read,
        cache_write,
        thinking,
        response,
        duration_ms: stream_ms,
        first_token_ms: ttft_ms,
        error,
        model_display,
    }))
}

#[derive(PartialEq)]
struct Fingerprint {
    length: u64,
    modified: SystemTime,
    #[cfg(unix)]
    inode: u64,
}
fn fingerprint(path: &Path) -> SafeResult<Fingerprint> {
    let stat = fs::metadata(path).map_err(|_| "native database metadata unavailable")?;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Ok(Fingerprint {
        length: stat.len(),
        modified: stat
            .modified()
            .map_err(|_| "native database time unavailable")?,
        #[cfg(unix)]
        inode: stat.ino(),
    })
}
fn read_source(path: &Path) -> SafeResult<Vec<Record>> {
    let before = fingerprint(path)?;
    if before.length == 0 {
        return Ok(Vec::new());
    }
    let wal = PathBuf::from(format!("{}-wal", path.to_string_lossy()));
    let immutable = !wal.exists();
    let mut url = url::Url::from_file_path(path).map_err(|_| "invalid native database path")?;
    url.set_query(Some(if immutable {
        "mode=ro&immutable=1"
    } else {
        "mode=ro"
    }));
    let mut connection = Connection::open_with_flags(
        url.as_str(),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "native database could not be opened read-only")?;
    connection
        .busy_timeout(Duration::from_secs(3))
        .map_err(|_| "native read timeout setup failed")?;
    connection
        .pragma_update(None, "query_only", true)
        .map_err(|_| "native read-only setup failed")?;
    let transaction = connection
        .transaction()
        .map_err(|_| "native snapshot read failed")?;
    let mut statement = transaction
        .prepare("SELECT trajectory_id FROM trajectory_meta")
        .map_err(|_| "unsupported native trajectory schema")?;
    let trajectories = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|_| "native trajectory read failed")?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "native trajectory decode failed")?;
    if trajectories.len() != 1 || trajectories[0].is_empty() || trajectories[0].len() > 1024 {
        return Err("unsupported native trajectory layout");
    }
    let mut statement = transaction
        .prepare("SELECT idx,data FROM gen_metadata ORDER BY idx")
        .map_err(|_| "unsupported native generation schema")?;
    let mut rows = statement
        .query([])
        .map_err(|_| "native generation read failed")?;
    let mut records = Vec::new();
    while let Some(row) = rows.next().map_err(|_| "native generation row failed")? {
        let index: i64 = row.get(0).map_err(|_| "invalid native generation index")?;
        if index < 0 {
            return Err("invalid native generation index");
        }
        let blob: Vec<u8> = row.get(1).map_err(|_| "invalid native generation blob")?;
        if let Some(record) = parse_record(&blob, &trajectories[0], index)? {
            records.push(record);
        }
    }
    if immutable && (fingerprint(path)? != before || wal.exists()) {
        return Err("native closed database changed during read; retry later");
    }
    Ok(records)
}
fn scan(source: &Path) -> SafeResult<(Vec<Record>, u32)> {
    let mut paths = fs::read_dir(source)
        .map_err(|_| "native conversation directory unreadable")?
        .map(|entry| {
            entry
                .map(|e| e.path())
                .map_err(|_| "native conversation entry unreadable")
        })
        .collect::<SafeResult<Vec<_>>>()?;
    paths.retain(|p| p.extension().is_some_and(|e| e == "db"));
    paths.sort();
    let count = paths.len().min(u32::MAX as usize) as u32;
    let mut records: Vec<Record> = Vec::new();
    let mut seen = HashMap::new();
    for path in paths {
        for record in read_source(&path)? {
            if let Some(index) = seen.get(&record.semantic_id) {
                let prior: &Record = &records[*index];
                if prior.usage() != record.usage()
                    || prior.thinking != record.thinking
                    || prior.response != record.response
                {
                    return Err("conflicting duplicated native generation metadata");
                }
                continue;
            }
            seen.insert(record.semantic_id.clone(), records.len());
            records.push(record);
        }
    }
    Ok((records, count))
}
fn validate_destination(connection: &Connection) -> SafeResult<()> {
    let existing: i64 = connection.query_row(
        "SELECT COUNT(*) FROM proxy_request_logs WHERE provider_id=?1 AND app_type='pi' \
         AND (provider_type IS NULL OR provider_type<>?2) \
         AND (input_tokens<>0 OR output_tokens<>0 OR cache_read_tokens<>0 OR cache_creation_tokens<>0)",
        params![PROVIDER, MARKER], |row| row.get(0)).map_err(|_| "CC Switch usage schema unavailable")?;
    if existing > 0 {
        return Err("another importer already reports Antigravity tokens; reconcile to avoid double counting");
    }
    Ok(())
}
fn classify<'a>(connection: &Connection, records: &'a [Record]) -> SafeResult<Vec<&'a Record>> {
    let mut pending = Vec::new();
    for record in records {
        let mut stmt = connection.prepare(
            "SELECT l.provider_type,l.input_tokens,l.output_tokens,l.cache_read_tokens,l.cache_creation_tokens \
             FROM proxy_request_logs l WHERE l.request_id=?1 OR l.request_id IN \
             (SELECT request_id FROM session_usage_dedup WHERE data_source=?2 AND semantic_id=?3)")
            .map_err(|_| "native destination dedup schema unavailable")?;
        let existing = stmt
            .query_map(
                params![record.request_id, SOURCE, record.semantic_id],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        [
                            r.get::<_, i64>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, i64>(4)?,
                        ],
                    ))
                },
            )
            .map_err(|_| "native destination dedup query failed")?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "native destination row invalid")?;
        if !existing.is_empty() {
            if existing.len() != 1
                || existing[0].0.as_deref() != Some(MARKER)
                || existing[0].1 != record.usage()
            {
                return Err("previous native usage changed or conflicts; reconcile before syncing");
            }
            continue;
        }
        let ledger: i64 = connection.query_row(
            "SELECT COUNT(*) FROM session_usage_dedup WHERE data_source=?1 AND (request_id=?2 OR semantic_id=?3)",
            params![SOURCE, record.request_id, record.semantic_id], |row| row.get(0))
            .map_err(|_| "native historical dedup ledger unavailable")?;
        if ledger == 0 {
            pending.push(record);
        }
    }
    Ok(pending)
}

fn daily_backup(connection: &Connection, directory: &Path) -> SafeResult<()> {
    fs::create_dir_all(directory).map_err(|_| "native token backup directory unavailable")?;
    if fs::symlink_metadata(directory)
        .map_err(|_| "backup directory metadata unavailable")?
        .file_type()
        .is_symlink()
    {
        return Err("native token backup directory must not be a symlink");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .map_err(|_| "native token backup directory permissions failed")?;
    }
    // Once before the first new write each UTC day, not every 60-second poll.
    let path = directory.join(format!(
        "cc-switch-before-native-tokens-{}.db",
        chrono::Utc::now().format("%Y%m%d")
    ));
    if path.exists() {
        let stat = fs::symlink_metadata(&path).map_err(|_| "daily backup metadata unavailable")?;
        if stat.file_type().is_symlink() {
            return Err("daily backup must not be a symlink");
        }
        let backup = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| "existing daily backup unreadable")?;
        let check: String = backup
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .map_err(|_| "existing daily backup check failed")?;
        if check != "ok" {
            return Err("existing daily backup integrity failed");
        }
        return Ok(());
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    drop(
        options
            .open(&path)
            .map_err(|_| "daily backup creation failed")?,
    );
    let result = (|| {
        let mut target = Connection::open(&path).map_err(|_| "daily backup open failed")?;
        let backup = rusqlite::backup::Backup::new(connection, &mut target)
            .map_err(|_| "daily online backup setup failed")?;
        backup
            .run_to_completion(128, Duration::from_millis(20), None)
            .map_err(|_| "daily online backup failed")?;
        drop(backup);
        let check: String = target
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .map_err(|_| "daily backup check failed")?;
        if check != "ok" {
            return Err("daily backup integrity failed");
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}
/// Cost is derived exactly like every other session importer: look up the local
/// pricing row, subtract nothing (fresh input), then run the same CostCalculator.
/// No pricing row means unpriced (0), which the UI shows as 「未计价」 and can be
/// back-filled later when the user adds a price.
fn compute_costs(
    connection: &Connection,
    record: &Record,
) -> (Decimal, Decimal, Decimal, Decimal, Decimal) {
    let pricing_model = if !record.model_display.is_empty() {
        record.model_display.as_str()
    } else {
        record.model.as_str()
    };
    let usage = TokenUsage {
        // Already fresh: native `input_tokens` excludes cache read/write.
        input_tokens: record.input.clamp(0, i64::from(u32::MAX)) as u32,
        output_tokens: record.output.clamp(0, i64::from(u32::MAX)) as u32,
        cache_read_tokens: record.cache_read.clamp(0, i64::from(u32::MAX)) as u32,
        cache_creation_tokens: record.cache_write.clamp(0, i64::from(u32::MAX)) as u32,
        cache_creation_1h_tokens: 0,
        model: Some(pricing_model.to_owned()),
        message_id: None,
    };
    let zero = Decimal::ZERO;
    find_model_pricing(connection, pricing_model)
        .map(|pricing| {
            let calculated =
                CostCalculator::calculate_for_app(APP_TYPE, &usage, &pricing, Decimal::ONE);
            (
                calculated.input_cost,
                calculated.output_cost,
                calculated.cache_read_cost,
                calculated.cache_creation_cost,
                calculated.total_cost,
            )
        })
        .unwrap_or((zero, zero, zero, zero, zero))
}
fn insert_record(connection: &Connection, record: &Record) -> SafeResult<()> {
    // Pricing basis is stored so later back-fill/repricing stays on the same
    // model (mirrors the Pi importer storing `pricing_model = model`).
    let pricing_model = if record.model_display.is_empty() {
        record.model.clone()
    } else {
        record.model_display.clone()
    };
    let (input_cost, output_cost, cache_read_cost, cache_write_cost, total_cost) =
        compute_costs(connection, record);
    connection.execute(
        "INSERT INTO proxy_request_logs (request_id,provider_id,app_type,model,request_model,pricing_model,\
         input_tokens,output_tokens,cache_read_tokens,cache_creation_tokens,input_token_semantics,\
         input_cost_usd,output_cost_usd,cache_read_cost_usd,cache_creation_cost_usd,total_cost_usd,\
         latency_ms,first_token_ms,duration_ms,status_code,error_message,session_id,provider_type,\
         is_streaming,cost_multiplier,created_at,data_source) VALUES \
         (?1,?2,?3,?4,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?25,'1.0',?23,?24)",
        params![record.request_id, PROVIDER, APP_TYPE, record.model, pricing_model,
                record.input, record.output, record.cache_read, record.cache_write,
                INPUT_TOKEN_SEMANTICS_FRESH,
                input_cost.to_string(), output_cost.to_string(), cache_read_cost.to_string(),
                cache_write_cost.to_string(), total_cost.to_string(),
                record.first_token_ms.saturating_add(record.duration_ms),
                (record.first_token_ms > 0).then_some(record.first_token_ms), record.duration_ms,
                if record.error.is_some() { 500 } else { 200 },
                record.error.clone(),
                record.session, MARKER, record.created_at, ROW_SOURCE,
                i64::from(record.duration_ms > 0)])
        .map_err(|_| "native token request insert failed")?;
    connection.execute("INSERT INTO session_usage_dedup (data_source,request_id,semantic_id,has_entry_id) VALUES (?1,?2,?3,1)",
        params![SOURCE,record.request_id,record.semantic_id]).map_err(|_| "native token dedup insert failed")?;
    Ok(())
}
/// Legacy rows were imported before local pricing existed: they stored the
/// internal response model (e.g. `gemini-3.8-flash-n`) as the pricing basis,
/// which has no price row. Rebase them on the ACP display name and reprice with
/// the same calculator now that Antigravity is treated like every other source.
/// Only our own zero-cost rows are touched; anything already priced is left alone.
///
/// Returns the request ids that would actually gain a nonzero cost, so callers
/// can avoid a pointless write transaction when a model is simply unpriced.
fn repricable_ids(connection: &Connection, records: &[Record]) -> SafeResult<Vec<String>> {
    let by_request: HashMap<&str, &Record> = records
        .iter()
        .map(|record| (record.request_id.as_str(), record))
        .collect();
    let mut statement = connection
        .prepare(
            "SELECT request_id FROM proxy_request_logs WHERE provider_type=?1 \
             AND CAST(total_cost_usd AS REAL)<=0 \
             AND (input_tokens>0 OR output_tokens>0 OR cache_read_tokens>0 OR cache_creation_tokens>0)",
        )
        .map_err(|_| "native repricing schema unavailable")?;
    let ids = statement
        .query_map(params![MARKER], |row| row.get::<_, String>(0))
        .map_err(|_| "native repricing query failed")?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "native repricing row invalid")?;
    drop(statement);
    Ok(ids
        .into_iter()
        .filter(|request_id| {
            by_request
                .get(request_id.as_str())
                .is_some_and(|record| compute_costs(connection, record).4 > Decimal::ZERO)
        })
        .collect())
}
fn reprice_existing(connection: &Connection, records: &[Record]) -> SafeResult<u32> {
    let by_request: HashMap<&str, &Record> = records
        .iter()
        .map(|record| (record.request_id.as_str(), record))
        .collect();
    let mut updated = 0u32;
    for request_id in repricable_ids(connection, records)? {
        let Some(record) = by_request.get(request_id.as_str()) else {
            continue;
        };
        let pricing_model = if record.model_display.is_empty() {
            record.model.clone()
        } else {
            record.model_display.clone()
        };
        let (input_cost, output_cost, cache_read_cost, cache_write_cost, total_cost) =
            compute_costs(connection, record);
        connection
            .execute(
                "UPDATE proxy_request_logs SET pricing_model=?1, input_cost_usd=?2, output_cost_usd=?3, \
                 cache_read_cost_usd=?4, cache_creation_cost_usd=?5, total_cost_usd=?6 WHERE request_id=?7",
                params![
                    pricing_model,
                    input_cost.to_string(),
                    output_cost.to_string(),
                    cache_read_cost.to_string(),
                    cache_write_cost.to_string(),
                    total_cost.to_string(),
                    request_id
                ],
            )
            .map_err(|_| "native repricing update failed")?;
        updated += 1;
    }
    Ok(updated)
}
// Restore total timing for early rows and undo the old provider-specific
// one-second streaming cutoff. A native stream window is recorded as-is; the
// shared speed rules decide eligibility, just as for every measured channel.
// Only our marked rows are repaired; tokens, pricing and the ledger stay intact.
fn timing_repair_ids(connection: &Connection, records: &[Record]) -> SafeResult<Vec<String>> {
    let known: std::collections::HashSet<&str> =
        records.iter().map(|r| r.request_id.as_str()).collect();
    let mut statement = connection
        .prepare(
            "SELECT request_id FROM proxy_request_logs WHERE provider_type=?1 \
             AND (COALESCE(data_source,'')<>?2 \
                  OR COALESCE(is_streaming,-1)<>(duration_ms>0))",
        )
        .map_err(|_| "native timing repair schema unavailable")?;
    let ids = statement
        .query_map(params![MARKER, ROW_SOURCE], |r| r.get::<_, String>(0))
        .map_err(|_| "native timing repair query failed")?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "native timing repair row invalid")?;
    Ok(ids
        .into_iter()
        .filter(|id| known.contains(id.as_str()))
        .collect())
}

fn repair_existing_timings(connection: &Connection, records: &[Record]) -> SafeResult<u32> {
    let ids = timing_repair_ids(connection, records)?;
    let by_request: HashMap<&str, &Record> =
        records.iter().map(|r| (r.request_id.as_str(), r)).collect();
    for id in &ids {
        let record = by_request[id.as_str()];
        connection
            .execute(
                "UPDATE proxy_request_logs SET latency_ms=?1, first_token_ms=?2, duration_ms=?3, \
             data_source=?4, is_streaming=?5
             WHERE request_id=?6 AND provider_type=?7",
                params![
                    record.first_token_ms.saturating_add(record.duration_ms),
                    (record.first_token_ms > 0).then_some(record.first_token_ms),
                    record.duration_ms,
                    ROW_SOURCE,
                    i64::from(record.duration_ms > 0),
                    id,
                    MARKER
                ],
            )
            .map_err(|_| "native timing repair update failed")?;
    }
    Ok(ids.len().min(u32::MAX as usize) as u32)
}

fn import_records(
    db: &Database,
    records: &[Record],
    backups: &Path,
    files: u32,
) -> SafeResult<SessionSyncResult> {
    let mut connection = db
        .conn
        .lock()
        .map_err(|_| "CC Switch database lock failed")?;
    validate_destination(&connection)?;
    let pending = classify(&connection, records)?;
    let repricable = repricable_ids(&connection, records)?;
    let timing_repairs = timing_repair_ids(&connection, records)?;
    if pending.is_empty() && repricable.is_empty() && timing_repairs.is_empty() {
        log::info!(
            "Antigravity native sync: imported 0, skipped {}, files {files}",
            records.len()
        );
        return Ok(SessionSyncResult {
            skipped: records.len().min(u32::MAX as usize) as u32,
            files_scanned: files,
            ..Default::default()
        });
    }
    daily_backup(&connection, backups)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| "native token write transaction unavailable")?;
    validate_destination(&transaction)?;
    let pending = classify(&transaction, records)?;
    for record in &pending {
        insert_record(&transaction, record)?;
    }
    let repriced = reprice_existing(&transaction, records)?;
    let timings_repaired = repair_existing_timings(&transaction, records)?;
    let imported = pending.len().min(u32::MAX as usize) as u32;
    transaction
        .commit()
        .map_err(|_| "native token transaction commit failed")?;
    // Normal insertion hooks and the sync caller's log-recorded event refresh
    // CC Switch's stats; invalidate count cache too (same process, no sidecar).
    drop(connection);
    if let Ok(mut cache) = db.log_count_cache.lock() {
        *cache = None;
    }
    if imported == 0 && (repriced > 0 || timings_repaired > 0) {
        crate::usage_events::notify_log_recorded();
    }
    log::info!(
        "Antigravity native sync: imported {imported}, repriced {repriced}, timings repaired {timings_repaired}, skipped {}, files {files}",
        records.len().saturating_sub(imported as usize)
    );
    Ok(SessionSyncResult {
        imported,
        skipped: (records.len() as u32).saturating_sub(imported),
        files_scanned: files,
        ..Default::default()
    })
}

pub fn sync_antigravity_usage(db: &Database) -> Result<SessionSyncResult, AppError> {
    let Some(home) = dirs::home_dir() else {
        return Ok(SessionSyncResult::default());
    };
    let source = home.join(".gemini/antigravity-acp/conversations");
    if !source.exists() {
        return Ok(SessionSyncResult::default());
    }
    let result = (|| {
        verify_schema(&home.join(".local/opt/agy-acp/current/agy_acp_server.par"))?;
        let (records, files) = scan(&source)?;
        import_records(
            db,
            &records,
            &home.join(".config/antigravity-cc-switch-bridge/backups"),
            files,
        )
    })();
    result.map_err(|message| AppError::Message(format!("Antigravity token sync: {message}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn vint(mut n: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        while n > 127 {
            bytes.push((n as u8 & 127) | 128);
            n >>= 7;
        }
        bytes.push(n as u8);
        bytes
    }
    fn integer(n: u64, value: u64) -> Vec<u8> {
        [vint(n * 8), vint(value)].concat()
    }
    fn message(n: u64, value: &[u8]) -> Vec<u8> {
        [vint(n * 8 + 2), vint(value.len() as u64), value.to_vec()].concat()
    }
    fn blob(input: u64, complete: bool) -> Vec<u8> {
        let usage = [
            integer(2, input),
            integer(3, 7),
            integer(5, 30),
            integer(9, 5),
            integer(10, 2),
        ]
        .concat();
        let timestamp = [integer(1, 1791767907), integer(2, 123)].concat();
        let mut chat = [
            message(4, &usage),
            message(19, b"gemini-test"),
            message(21, b"gemini-test-high"),
            message(9, &message(4, &timestamp)),
            message(11, &integer(2, 100000000)),
        ]
        .concat();
        if complete {
            chat.extend(message(12, &integer(1, 2)));
        }
        message(1, &chat)
    }
    /// Shape observed in real failure blobs: the error is recorded but the
    /// generation aborted before any token was counted or the response model
    /// was assigned (usage exists but is empty, the stream window stays zero).
    fn failed_blob(error: &[u8], with_timestamp: bool) -> Vec<u8> {
        let mut chat = [
            message(4, &[]),
            message(21, b"gemini-test-high"),
            message(12, &integer(1, 0)),
        ]
        .concat();
        if with_timestamp {
            let timestamp = [integer(1, 1791767907), integer(2, 999)].concat();
            chat.extend(message(9, &message(4, &timestamp)));
        }
        [message(1, &chat), message(5, error)].concat()
    }
    fn record() -> Record {
        parse_record(&blob(10, true), "trajectory", 1)
            .unwrap()
            .unwrap()
    }
    fn database() -> Database {
        Database::memory().unwrap()
    }
    #[test]
    fn real_fresh_cache_thinking_semantics() {
        let record = record();
        assert_eq!(record.usage(), [10, 7, 30, 0]);
        assert_eq!(record.thinking + record.response, record.output);
        assert_eq!(record.duration_ms, 2000);
        assert_eq!(record.first_token_ms, 100);
    }
    #[test]
    fn native_timing_uses_shared_speed_gates_without_one_second_override() {
        let db = database();
        let directory = tempfile::tempdir().unwrap();
        for window in [0, 2, 99, 100, 428, 999, 1000, 8000] {
            let mut r = record();
            r.request_id = format!("{}:{window}", r.request_id);
            r.semantic_id = format!("{}:{window}", r.semantic_id);
            r.first_token_ms = 6000;
            r.duration_ms = window;
            r.output = 378;
            let id = r.request_id.clone();
            import_records(&db, &[r], directory.path(), 1).unwrap();
            let detail = db.get_request_detail(&id).unwrap().unwrap();
            assert_eq!(detail.latency_ms, 6000 + window as u64);
            assert_eq!(detail.first_token_ms, Some(6000));
            assert_eq!(detail.duration_ms, Some(window as u64));
            assert_eq!(detail.data_source.as_deref(), Some(ROW_SOURCE));
            assert_eq!(detail.is_streaming, window > 0);
        }
        let stats = db
            .get_model_stats(None, None, Some("pi"), None, None)
            .unwrap();
        // The shared 100 ms gate excludes 0/2/99 ms, but admits 100/428/999 ms
        // exactly like other channels; there is no native-only one-second gate.
        assert_eq!(stats[0].speed_output_tokens, 378 * 5);
        assert_eq!(stats[0].speed_generation_ms, 100 + 428 + 999 + 1000 + 8000);
        assert_eq!(stats[0].first_token_median_ms, Some(6000));
        assert_eq!(stats[0].est_speed_output_tokens, 0);
    }

    #[test]
    fn native_long_streams_contribute_measured_speed_not_historical_estimates() {
        let db = database();
        let directory = tempfile::tempdir().unwrap();
        let mut r = record();
        r.first_token_ms = 6000;
        r.duration_ms = 8000;
        r.output = 1200;
        import_records(&db, &[r], directory.path(), 1).unwrap();
        let stats = db
            .get_model_stats(None, None, Some("pi"), None, None)
            .unwrap();
        assert_eq!(stats[0].speed_output_tokens, 1200);
        assert_eq!(stats[0].speed_generation_ms, 8000);
        assert_eq!(stats[0].first_token_median_ms, Some(6000));
        assert_eq!(stats[0].est_first_token_median_ms, None);
    }

    #[test]
    fn legacy_native_timing_is_repaired_once_without_rebilling_or_repricing() {
        let db = database();
        let directory = tempfile::tempdir().unwrap();
        let r = record();
        import_records(&db, &[record()], directory.path(), 1).unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE proxy_request_logs SET data_source='pi_session', latency_ms=duration_ms, \
             is_streaming=0, total_cost_usd='1.234'",
                [],
            )
            .unwrap();
        assert_eq!(
            import_records(&db, &[record()], directory.path(), 1)
                .unwrap()
                .imported,
            0
        );
        let detail = db.get_request_detail(&r.request_id).unwrap().unwrap();
        assert_eq!(detail.latency_ms, 2100);
        // The streaming flag is re-derived too: a 2 s window is a sustained stream.
        assert!(detail.is_streaming);
        assert_eq!(detail.data_source.as_deref(), Some(ROW_SOURCE));
        assert_eq!(detail.total_cost_usd, "1.234");
        let conn = db.conn.lock().unwrap();
        let ledger_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_usage_dedup WHERE data_source='pi_session'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ledger_count, 1);
        assert!(timing_repair_ids(&conn, &[record()]).unwrap().is_empty());
    }

    #[test]
    fn provider_specific_streaming_cutoff_is_removed_from_existing_rows() {
        // Undo flags written by the native-only one-second cutoff without
        // changing usage, prices or dedup identities. The shared speed rule
        // must include eligible sub-second windows after the next sync.
        let db = database();
        let directory = tempfile::tempdir().unwrap();
        db.conn.lock().unwrap().execute("INSERT OR REPLACE INTO model_pricing (model_id,display_name,input_cost_per_million,output_cost_per_million,cache_read_cost_per_million,cache_creation_cost_per_million) VALUES ('gemini-test-high','test','10','20','5','0')",[]).unwrap();
        let mut burst = record();
        burst.duration_ms = 500;
        burst.output = 1200;
        let mut sustained = record();
        sustained.request_id = format!("{}sustained", record().request_id);
        sustained.semantic_id = format!("{}sustained", record().semantic_id);
        sustained.duration_ms = 8000;
        sustained.output = 2000;
        import_records(
            &db,
            &[burst.clone(), sustained.clone()],
            directory.path(),
            1,
        )
        .unwrap();
        let cost_before = db
            .get_request_detail(&burst.request_id)
            .unwrap()
            .unwrap()
            .total_cost_usd;
        {
            let connection = db.conn.lock().unwrap();
            // Simulate the old provider-specific cutoff on an existing row.
            connection
                .execute(
                    "UPDATE proxy_request_logs SET is_streaming=0 WHERE duration_ms<1000",
                    [],
                )
                .unwrap();
            // Before repair only the long window feeds the aggregate speed.
            let (speed_tokens, speed_window): (i64, i64) = connection
                .query_row(
                    "SELECT SUM(output_tokens),SUM(latency_ms-first_token_ms) FROM proxy_request_logs \
                     WHERE is_streaming=1 AND output_tokens>=100 AND latency_ms-first_token_ms>=100",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!((speed_tokens, speed_window), (2000, 8000));
        }
        assert_eq!(
            import_records(
                &db,
                &[burst.clone(), sustained.clone()],
                directory.path(),
                1
            )
            .unwrap()
            .imported,
            0
        );
        let burst_detail = db.get_request_detail(&burst.request_id).unwrap().unwrap();
        // The 500 ms window now follows the shared rule, with TTFT unchanged.
        assert!(burst_detail.is_streaming);
        assert_eq!(burst_detail.latency_ms, 600); // first token 100 ms + a 500 ms window
        assert_eq!(burst_detail.first_token_ms, Some(100));
        assert_eq!(burst_detail.duration_ms, Some(500));
        let sustained_detail = db
            .get_request_detail(&sustained.request_id)
            .unwrap()
            .unwrap();
        assert!(sustained_detail.is_streaming);
        let stats = db
            .get_model_stats(None, None, Some("pi"), None, None)
            .unwrap();
        // Both windows now feed the same aggregate speed as other channels.
        assert_eq!(stats[0].speed_output_tokens, 3200);
        assert_eq!(stats[0].speed_generation_ms, 8500);
        // Costs and the dedup ledger stay untouched by the timing repair.
        assert_eq!(burst_detail.total_cost_usd, cost_before);
        let connection = db.conn.lock().unwrap();
        let ledger_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM session_usage_dedup", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ledger_count, 2);
        assert!(timing_repair_ids(&connection, &[burst, sustained])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn request_and_semantic_ids_are_frozen() {
        // These ids are already persisted in session_usage_dedup + the request
        // log. If this assertion ever fails, the identity algorithm changed and
        // re-running sync would re-import every historical call as a new one.
        let r = record();
        assert_eq!(
            r.request_id,
            format!("{PREFIX}{}", digest(json!(["native", "trajectory", 1])))
        );
        assert_eq!(
            r.semantic_id,
            format!(
                "antigravity_native_semantic:{}",
                digest(json!(["gemini-test", 1791767907, 123]))
            )
        );
    }
    #[test]
    fn provisional_unknown_invalid_are_not_imported() {
        assert!(parse_record(&blob(10, false), "t", 1).unwrap().is_none());
        assert!(parse_record(&[], "t", 1).unwrap().is_none());
        assert!(parse_record(&blob(u64::MAX, true), "t", 1).is_err());
        for data in [&[0x80][..], &[0], &[10, 9, 1], &[11], &[9, 1]] {
            assert!(Proto::parse(data).is_err());
        }
    }

    #[test]
    fn failed_generations_import_with_their_real_status() {
        // Real failure blobs (403 quota errors, cancelled generations) carry an
        // error but no usage counts and no response model. They used to be
        // dropped, so the ledger only ever showed success status codes. They
        // must import as failed rows with the real error text and zero tokens.
        let db = database();
        let directory = tempfile::tempdir().unwrap();
        let error =
            b"Error 403, Message: Verify your account to continue., Status: PERMISSION_DENIED";
        let failed = parse_record(&failed_blob(error, true), "trajectory-failed", 0)
            .unwrap()
            .unwrap();
        assert_eq!(failed.usage(), [0, 0, 0, 0]);
        assert_eq!(failed.model, "gemini-test-high"); // display-name fallback
        assert_eq!(failed.duration_ms, 0);
        assert_eq!(
            failed.error.as_deref(),
            Some(std::str::from_utf8(error).unwrap())
        );
        import_records(&db, &[record(), failed], directory.path(), 1).unwrap();
        let failed_id = format!(
            "{PREFIX}{}",
            digest(json!(["native", "trajectory-failed", 0]))
        );
        let detail = db.get_request_detail(&failed_id).unwrap().unwrap();
        assert_eq!(detail.status_code, 500);
        assert_eq!(
            detail.error_message.as_deref(),
            Some("Error 403, Message: Verify your account to continue., Status: PERMISSION_DENIED")
        );
        assert!(!detail.is_streaming);
        assert_eq!(detail.first_token_ms, None);
        assert_eq!(detail.total_cost_usd, "0");
        let stats = db
            .get_model_stats(None, None, Some("pi"), None, None)
            .unwrap();
        // Both rows bill under the same pricing basis, so the failure drags
        // the shared group's success rate down to 50%.
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].success_rate, 50.0);
        assert_eq!(stats[0].request_count, 2);
    }

    #[test]
    fn failure_blobs_without_usable_metadata_are_skipped_safely() {
        // No usable timestamp: the row cannot be attributed, so it is skipped
        // instead of failing the whole source read.
        assert!(
            parse_record(&failed_blob(b"boom", false), "trajectory-failed", 1)
                .unwrap()
                .is_none()
        );
        // Without the error mark the old guards still hold: an empty-usage blob
        // is not a generation record.
        assert!(
            parse_record(&failed_blob(b"", true), "trajectory-failed", 2)
                .unwrap()
                .is_none()
        );
        // The error text is sanitized, single-line and bounded.
        let long = "x".repeat(600);
        assert_eq!(error_text(long.as_bytes()).unwrap().chars().count(), 500);
        assert_eq!(
            error_text(b"line1\nline2\ttab").as_deref(),
            Some("line1 line2 tab")
        );
        assert_eq!(error_text(b"   "), None);
    }
    #[test]
    fn descriptor_parser_never_executes_code() {
        assert_eq!(
            descriptor_literal(br#"x=p.AddSerializedFile(b'\x01\n\\\'')"#).unwrap(),
            [1, 10, 92, 39]
        );
        assert!(descriptor_literal(b"x=p.AddSerializedFile(run_code())").is_err());
        assert!(descriptor_literal(b"x=p.AddSerializedFile(b'bad' + b'code')").is_err());
    }
    #[test]
    fn idempotent_daily_backup_and_ledger() {
        let db = database();
        let directory = tempfile::tempdir().unwrap();
        let first = import_records(&db, &[record()], directory.path(), 1).unwrap();
        assert_eq!(first.imported, 1);
        assert_eq!(
            import_records(&db, &[record()], directory.path(), 1)
                .unwrap()
                .imported,
            0
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        let mut clone = record();
        clone.request_id.push_str("clone");
        assert_eq!(
            import_records(&db, &[clone], directory.path(), 1)
                .unwrap()
                .imported,
            0
        );
        db.conn
            .lock()
            .unwrap()
            .execute("DELETE FROM proxy_request_logs", [])
            .unwrap();
        assert_eq!(
            import_records(&db, &[record()], directory.path(), 1)
                .unwrap()
                .imported,
            0
        );
    }
    #[test]
    fn transaction_failure_rolls_back_both_tables() {
        let db = database();
        let directory = tempfile::tempdir().unwrap();
        db.conn.lock().unwrap().execute_batch("CREATE TRIGGER reject_ledger BEFORE INSERT ON session_usage_dedup BEGIN SELECT RAISE(ABORT,'test'); END").unwrap();
        assert!(import_records(&db, &[record()], directory.path(), 1).is_err());
        let connection = db.conn.lock().unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM proxy_request_logs", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM session_usage_dedup", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    #[test]
    fn existing_usage_conflict_aborts_without_overwriting() {
        let db = database();
        let directory = tempfile::tempdir().unwrap();
        import_records(&db, &[record()], directory.path(), 1).unwrap();
        let mut changed = record();
        changed.input = 999;
        assert!(import_records(&db, &[changed], directory.path(), 1).is_err());
    }
    #[test]
    fn price_backfill_and_detail_never_fabricate_native_usd() {
        // Unpriced model: same fallback as Pi/Claude/Codex (0 until a price is
        // added), and the cost back-fill picks the row up afterwards.
        let db = database();
        let directory = tempfile::tempdir().unwrap();
        import_records(&db, &[record()], directory.path(), 1).unwrap();
        assert_eq!(
            db.get_request_detail(&record().request_id)
                .unwrap()
                .unwrap()
                .total_cost_usd,
            "0"
        );
        // Adding a local price for the real model then back-fills like any other
        // importer, proving Antigravity is priced from CC Switch's own table.
        db.conn.lock().unwrap().execute("INSERT OR REPLACE INTO model_pricing (model_id,display_name,input_cost_per_million,output_cost_per_million,cache_read_cost_per_million,cache_creation_cost_per_million) VALUES ('gemini-test-high','test','10','20','5','10')",[]).unwrap();
        assert_eq!(db.backfill_missing_usage_costs().unwrap(), 1);
        let total = db
            .get_request_detail(&record().request_id)
            .unwrap()
            .unwrap()
            .total_cost_usd;
        assert_ne!(total, "0");
    }
    #[test]
    fn native_row_is_priced_at_insert_like_other_importers() {
        let db = database();
        let directory = tempfile::tempdir().unwrap();
        db.conn.lock().unwrap().execute("INSERT OR REPLACE INTO model_pricing (model_id,display_name,input_cost_per_million,output_cost_per_million,cache_read_cost_per_million,cache_creation_cost_per_million) VALUES ('gemini-test-high','test','10','20','5','0')",[]).unwrap();
        import_records(&db, &[record()], directory.path(), 1).unwrap();
        let connection = db.conn.lock().unwrap();
        let (model, pricing_model, total, semantics): (String, String, String, i64) = connection
            .query_row(
                "SELECT model,pricing_model,total_cost_usd,input_token_semantics FROM proxy_request_logs",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        // Real response model is kept, pricing basis is the ACP display name, the
        // stored semantics stay fresh (2) so aggregation never double-subtracts.
        assert_eq!(model, "gemini-test");
        assert_eq!(pricing_model, "gemini-test-high");
        assert_eq!(semantics, INPUT_TOKEN_SEMANTICS_FRESH);
        assert_ne!(total, "0");
    }
    #[test]
    fn legacy_unpriced_rows_are_repriced_once_a_local_price_exists() {
        let db = database();
        let directory = tempfile::tempdir().unwrap();
        // Simulate the legacy state: real tokens, internal-model pricing basis,
        // and zero cost copied from the old importer.
        {
            let connection = db.conn.lock().unwrap();
            connection
                .execute(
                    "INSERT INTO proxy_request_logs (request_id,provider_id,app_type,model,request_model,pricing_model,\
                     input_tokens,output_tokens,cache_read_tokens,cache_creation_tokens,input_token_semantics,\
                     input_cost_usd,output_cost_usd,cache_read_cost_usd,cache_creation_cost_usd,total_cost_usd,\
                     latency_ms,first_token_ms,duration_ms,status_code,session_id,provider_type,is_streaming,\
                     cost_multiplier,created_at,data_source) VALUES \
                     (?1,'antigravity-acp','pi','gemini-test','gemini-test','gemini-test',10,7,30,0,2,\
                      '0','0','0','0','0',2000,100,2000,200,'trajectory','antigravity_native_tokens_v1',1,\
                      '1.0',1791767907,'pi_session')",
                    params![record().request_id],
                )
                .unwrap();
        }
        // Without a price row the row is genuinely unpriced and must not churn.
        assert_eq!(
            import_records(&db, &[record()], directory.path(), 1)
                .unwrap()
                .imported,
            0
        );
        assert!(!directory.path().join("unused").exists());
        db.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT OR REPLACE INTO model_pricing (model_id,display_name,input_cost_per_million,output_cost_per_million,cache_read_cost_per_million,cache_creation_cost_per_million) VALUES ('gemini-test-high','test','10','20','5','0')",
                [],
            )
            .unwrap();
        // Now the periodic sync reprices the legacy row from the local table.
        import_records(&db, &[record()], directory.path(), 1).unwrap();
        let connection = db.conn.lock().unwrap();
        let (pricing_model, total): (String, String) = connection
            .query_row(
                "SELECT pricing_model,total_cost_usd FROM proxy_request_logs",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(pricing_model, "gemini-test-high");
        assert_ne!(total, "0");
    }
    #[test]
    fn closed_wal_database_read_is_read_only() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("native.db");
        {
            let connection = Connection::open(&path).unwrap();
            connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE trajectory_meta(trajectory_id TEXT); INSERT INTO trajectory_meta VALUES('trajectory'); CREATE TABLE gen_metadata(idx INTEGER,data BLOB)").unwrap();
            connection
                .execute(
                    "INSERT INTO gen_metadata VALUES(1,?1)",
                    params![blob(10, true)],
                )
                .unwrap();
        }
        let before = fs::read(&path).unwrap();
        assert_eq!(read_source(&path).unwrap().len(), 1);
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
    /// Explicit opt-in: reads the real local source but imports only into a
    /// memory DB, so it can never touch the user's CC Switch database.
    #[test]
    #[ignore = "requires local official Antigravity server and conversation databases"]
    fn actual_native_source_parses_and_imports_idempotently() {
        let home = dirs::home_dir().unwrap();
        verify_schema(&home.join(".local/opt/agy-acp/current/agy_acp_server.par")).unwrap();
        let (records, files) = scan(&home.join(".gemini/antigravity-acp/conversations")).unwrap();
        assert!(!records.is_empty());
        let db = database();
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            import_records(&db, &records, directory.path(), files)
                .unwrap()
                .imported,
            records.len() as u32
        );
        assert_eq!(
            import_records(&db, &records, directory.path(), files)
                .unwrap()
                .imported,
            0
        );
        eprintln!(
            "Native source: {} calls; temporary-only import passed",
            records.len()
        );
    }
}
