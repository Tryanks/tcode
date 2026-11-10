//! The stored form of an event row (format 2), the only place rows are
//! encoded and decoded.
//!
//! ```text
//! 0x02 | flags:u8 | type:u8 | dict:varint | refs:varint
//!      | refs × { offset:varint, b64_len:varint, sha256:[32] }
//!      | payload
//! ```
//!
//! The payload is the row's canonical bytes with every external base64 run
//! cut out (its bare form), zstd-compressed when that is smaller. Each run is
//! kept as a blob file named by the SHA-256 of its decoded bytes; `offset` is
//! where it goes back in the bare form, relative to the previous one. The
//! header stays uncompressed so the type and references read without the
//! payload. Decoding puts every run back and yields the canonical bytes
//! exactly.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};

const FORMAT: u8 = 0x02;
const COMPRESSED: u8 = 1;
const NEWLINE: u8 = 2;
/// Shorter runs stay in the row and are compressed with it.
const MIN_RUN: usize = 4096;
const LEVEL: i32 = 3;
pub(super) const DICTIONARY_BYTES: usize = 112 << 10;

pub(super) type Hash = [u8; 32];

/// Reads a referenced blob, `None` when it is missing.
pub(super) type ReadBlob<'a> = dyn FnMut(&Hash) -> io::Result<Option<Arc<Vec<u8>>>> + 'a;

/// Event types as rows name them, for prefilters on the type byte. Append
/// only: a row's byte is its index here plus one, and 0 an unlisted type.
const TYPES: &[&str] = &[
    "turn_retry",
    "provider_relay",
    "plan_resolved",
    "provider_options",
    "session_started",
    "served_model",
    "model_fallback_detected",
    "turn_blocked",
    "turn_started",
    "turn_accepted",
    "mcp_servers_not_attached",
    "background_tasks_changed",
    "turn_changes_updated",
    "turn_checkpoint",
    "rewind_completed",
    "rewind_failed",
    "turn_completed",
    "item_started",
    "item_updated",
    "item_completed",
    "item_backgrounded",
    "steer_requested",
    "steer_accepted",
    "steer_rejected",
    "delta",
    "approval_requested",
    "approval_resolved",
    "user_input_requested",
    "user_input_resolved",
    "token_usage",
    "provider_commands",
    "context_compacted",
    "context_window_changed",
    "settings_kept",
    "control_action",
    "plan_updated",
    "goal_updated",
    "proposed_plan_delta",
    "proposed_plan",
    "warning",
    "provider_start_failed",
    "error",
    "usage_limit_reached",
    "session_closed",
    "steer_checkpoint",
    "checkpoints_cleared",
    "rewind_availability",
    "rewind_undone",
    "rewind_undo_failed",
];

/// The type byte rows of event type `name` carry.
pub(super) fn type_code(name: &str) -> u8 {
    TYPES
        .iter()
        .position(|known| *known == name)
        .map_or(0, |index| index as u8 + 1)
}

/// A run cut out of a row, with its decoded bytes.
#[derive(Debug, Clone)]
pub(super) struct Blob {
    pub hash: Hash,
    pub bytes: Arc<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Ref {
    /// Position in the bare form.
    pub at: usize,
    pub b64_len: usize,
    pub hash: Hash,
}

pub(super) struct Header<'a> {
    flags: u8,
    dict: u32,
    pub refs: Vec<Ref>,
    payload: &'a [u8],
}

impl Header<'_> {
    pub(super) fn ends_line(&self) -> bool {
        self.flags & NEWLINE != 0
    }
}

pub(super) fn header(stored: &[u8]) -> io::Result<Header<'_>> {
    let mut reader = Reader(stored);
    if reader.byte()? != FORMAT {
        return Err(invalid("not a format-2 row"));
    }
    let flags = reader.byte()?;
    // The type byte is for SQL prefilters.
    reader.byte()?;
    let dict = u32::try_from(reader.varint()?).map_err(|_| invalid("dictionary id"))?;
    let count = reader.varint()?;
    let mut refs = Vec::new();
    let mut at = 0usize;
    for _ in 0..count {
        at = at
            .checked_add(reader.varint()? as usize)
            .ok_or_else(|| invalid("reference offset"))?;
        let b64_len = reader.varint()? as usize;
        let hash = reader.take(32)?.try_into().expect("32 bytes");
        refs.push(Ref { at, b64_len, hash });
    }
    Ok(Header {
        flags,
        dict,
        refs,
        payload: reader.0,
    })
}

/// The dictionaries rows were compressed with, and the one new rows use.
#[derive(Default)]
pub(super) struct Codec {
    dictionaries: HashMap<u32, Arc<zstd::dict::DecoderDictionary<'static>>>,
    current: Option<(u32, Arc<zstd::dict::EncoderDictionary<'static>>)>,
}

impl Codec {
    /// Every stored dictionary; the highest id compresses new rows.
    pub(super) fn new(stored: impl IntoIterator<Item = (u32, Vec<u8>)>) -> Self {
        let mut codec = Self::default();
        for (id, bytes) in stored {
            codec
                .dictionaries
                .insert(id, Arc::new(zstd::dict::DecoderDictionary::copy(&bytes)));
            if codec
                .current
                .as_ref()
                .is_none_or(|(current, _)| *current < id)
            {
                codec.current = Some((
                    id,
                    Arc::new(zstd::dict::EncoderDictionary::copy(&bytes, LEVEL)),
                ));
            }
        }
        codec
    }

    pub(super) fn has_dictionary(&self) -> bool {
        self.current.is_some()
    }
}

/// A row ready to store, and the blobs it names, which must be on disk before
/// the row commits.
pub(super) struct Encoded {
    pub bytes: Vec<u8>,
    pub blobs: Vec<Blob>,
}

impl Encoded {
    pub(super) fn hashes(&self) -> Vec<u8> {
        self.blobs.iter().flat_map(|blob| blob.hash).collect()
    }
}

/// The bare form of `canonical` and the runs cut from it.
pub(super) struct Split {
    pub bare: Vec<u8>,
    pub refs: Vec<Ref>,
    pub blobs: Vec<Blob>,
}

/// Cut every external base64 run out of a canonical row: a run of at least
/// [`MIN_RUN`] standard base64 characters that decodes and re-encodes to
/// itself, opened by `"` (or `\"`) or `base64,` and closed by `"` or `\"`.
/// Such a run is wholly inside one JSON string, so the bare form is JSON too.
pub(super) fn split(canonical: &[u8]) -> Split {
    let mut bare = Vec::with_capacity(canonical.len());
    let mut refs = Vec::new();
    let mut blobs = Vec::new();
    let mut copied = 0;
    let mut index = 0;
    while index < canonical.len() {
        if !is_base64(canonical[index]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < canonical.len() && is_base64(canonical[index]) {
            index += 1;
        }
        let mut end = index;
        for _ in 0..2 {
            if end < canonical.len() && canonical[end] == b'=' {
                end += 1;
            }
        }
        index = end;
        let run = &canonical[start..end];
        let opened =
            canonical[..start].ends_with(b"\"") || canonical[..start].ends_with(b"base64,");
        let closed = canonical[end..].starts_with(b"\"") || canonical[end..].starts_with(b"\\\"");
        if run.len() < MIN_RUN || !run.len().is_multiple_of(4) || !opened || !closed {
            continue;
        }
        let Ok(bytes) = STANDARD.decode(run) else {
            continue;
        };
        if STANDARD.encode(&bytes).as_bytes() != run {
            continue;
        }
        bare.extend_from_slice(&canonical[copied..start]);
        copied = end;
        let hash: Hash = Sha256::digest(&bytes).into();
        refs.push(Ref {
            at: bare.len(),
            b64_len: run.len(),
            hash,
        });
        blobs.push(Blob {
            hash,
            bytes: Arc::new(bytes),
        });
    }
    bare.extend_from_slice(&canonical[copied..]);
    Split { bare, refs, blobs }
}

fn is_base64(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/'
}

/// The event type a bare row names, as its `type` field spells it.
fn kind_of(bare: &[u8]) -> u8 {
    #[derive(serde::Deserialize)]
    struct Address<'a> {
        #[serde(borrow, default)]
        event: Option<Kind<'a>>,
        #[serde(borrow, rename = "type", default)]
        kind: Option<std::borrow::Cow<'a, str>>,
    }
    #[derive(serde::Deserialize)]
    struct Kind<'a> {
        #[serde(borrow, rename = "type", default)]
        kind: Option<std::borrow::Cow<'a, str>>,
    }
    serde_json::from_slice::<Address<'_>>(bare)
        .ok()
        .and_then(|address| address.event.and_then(|event| event.kind).or(address.kind))
        .map_or(0, |kind| type_code(&kind))
}

impl Codec {
    pub(super) fn encoder(&self) -> io::Result<Encoder<'_>> {
        let (dict, compressor) = match &self.current {
            Some((id, dictionary)) => {
                let mut compressor = zstd::bulk::Compressor::with_prepared_dictionary(dictionary)?;
                // Every row names its dictionary in the header already.
                compressor.set_parameter(zstd::zstd_safe::CParameter::DictIdFlag(false))?;
                (*id, compressor)
            }
            None => (0, zstd::bulk::Compressor::new(LEVEL)?),
        };
        Ok(Encoder { compressor, dict })
    }

    pub(super) fn decoder(&self) -> Decoder<'_> {
        Decoder {
            codec: self,
            contexts: HashMap::new(),
        }
    }
}

/// Encodes rows with the newest dictionary, reusing one compression context.
pub(super) struct Encoder<'a> {
    compressor: zstd::bulk::Compressor<'a>,
    dict: u32,
}

impl Encoder<'_> {
    pub(super) fn encode(&mut self, canonical: &[u8]) -> io::Result<Encoded> {
        let Split { bare, refs, blobs } = split(canonical);
        let compressed = self.compressor.compress(&bare)?;
        let compress = compressed.len() < bare.len();
        let mut bytes = vec![
            FORMAT,
            u8::from(compress) * COMPRESSED + u8::from(canonical.ends_with(b"\n")) * NEWLINE,
            kind_of(&bare),
        ];
        varint(&mut bytes, u64::from(if compress { self.dict } else { 0 }));
        varint(&mut bytes, refs.len() as u64);
        let mut previous = 0;
        for reference in &refs {
            varint(&mut bytes, (reference.at - previous) as u64);
            varint(&mut bytes, reference.b64_len as u64);
            bytes.extend_from_slice(&reference.hash);
            previous = reference.at;
        }
        bytes.extend_from_slice(if compress { &compressed } else { &bare });
        Ok(Encoded { bytes, blobs })
    }
}

/// Decodes rows, keeping one decompression context per dictionary.
pub(super) struct Decoder<'a> {
    codec: &'a Codec,
    contexts: HashMap<u32, zstd::bulk::Decompressor<'a>>,
}

impl Decoder<'_> {
    /// The bare form of a stored row.
    pub(super) fn bare(&mut self, stored: &[u8]) -> io::Result<Vec<u8>> {
        let header = header(stored)?;
        self.payload(&header)
    }

    fn payload(&mut self, header: &Header<'_>) -> io::Result<Vec<u8>> {
        if header.flags & COMPRESSED == 0 {
            return Ok(header.payload.to_vec());
        }
        let capacity = zstd::zstd_safe::get_frame_content_size(header.payload)
            .ok()
            .flatten()
            .ok_or_else(|| invalid("a compressed row without its size"))?;
        let capacity = usize::try_from(capacity).map_err(|_| invalid("row size"))?;
        let context = match self.contexts.entry(header.dict) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let context = if header.dict == 0 {
                    zstd::bulk::Decompressor::new()?
                } else {
                    let dictionary =
                        self.codec.dictionaries.get(&header.dict).ok_or_else(|| {
                            invalid(format!("dictionary {} is missing", header.dict))
                        })?;
                    zstd::bulk::Decompressor::with_prepared_dictionary(dictionary)?
                };
                entry.insert(context)
            }
        };
        context.decompress(header.payload, capacity)
    }

    /// Decode a stored row. `blob` reads a referenced blob; one that is
    /// missing or the wrong size leaves its run empty and marks the result.
    pub(super) fn decode(
        &mut self,
        stored: &[u8],
        mode: Mode,
        blob: &mut ReadBlob<'_>,
    ) -> io::Result<Decoded> {
        let header = header(stored)?;
        let bare = self.payload(&header)?;
        if mode == Mode::Bare || header.refs.is_empty() {
            return Ok(Decoded {
                bytes: bare,
                missing: false,
            });
        }
        let mut bytes =
            Vec::with_capacity(bare.len() + header.refs.iter().map(|r| r.b64_len).sum::<usize>());
        let mut copied = 0;
        let mut missing = false;
        for reference in &header.refs {
            let at = reference.at.min(bare.len());
            bytes.extend_from_slice(&bare[copied..at]);
            copied = at;
            match blob(&reference.hash)? {
                Some(data) if base64_len(data.len()) == reference.b64_len => {
                    let start = bytes.len();
                    bytes.resize(start + reference.b64_len, 0);
                    STANDARD
                        .encode_slice(data.as_slice(), &mut bytes[start..])
                        .map_err(invalid)?;
                }
                _ => missing = true,
            }
        }
        bytes.extend_from_slice(&bare[copied..]);
        Ok(Decoded { bytes, missing })
    }
}

fn base64_len(bytes: usize) -> usize {
    bytes.div_ceil(3) * 4
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// Every run put back: the canonical bytes.
    Full,
    /// Every run left out, for folds that never read images or tool output.
    Bare,
}

pub(super) struct Decoded {
    pub bytes: Vec<u8>,
    /// A referenced blob was missing or damaged and its run left empty.
    pub missing: bool,
}

/// Train the dictionary new rows are compressed with from bare rows.
pub(super) fn train(samples: &[Vec<u8>]) -> io::Result<Vec<u8>> {
    zstd::dict::from_samples(samples, DICTIONARY_BYTES)
}

fn varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn byte(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn take(&mut self, count: usize) -> io::Result<&'a [u8]> {
        if self.0.len() < count {
            return Err(invalid("a truncated row header"));
        }
        let (head, rest) = self.0.split_at(count);
        self.0 = rest;
        Ok(head)
    }

    fn varint(&mut self) -> io::Result<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = self.byte()?;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(invalid("a varint longer than 64 bits"))
    }
}

fn invalid(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
