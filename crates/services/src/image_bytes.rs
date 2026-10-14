use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::io;
use tcode_protocol::QueryResponse;

/// An owned encoded selection that can be decoded after releasing the session log.
pub struct ImageRead {
    encoded: Vec<u8>,
    part: Option<ImagePart>,
}

struct ImagePart {
    offset: u64,
    total: u64,
    skip: usize,
    length: usize,
    revision: Option<String>,
}

fn decoded_len(encoded: &str) -> io::Result<u64> {
    let padding = encoded
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'=')
        .count();
    if !encoded.len().is_multiple_of(4) || padding > 2 {
        return Err(io::Error::other("Invalid image encoding"));
    }
    Ok((encoded.len() / 4 * 3 - padding) as u64)
}

impl ImageRead {
    pub fn complete(encoded: &str, max_bytes: Option<u64>) -> io::Result<Self> {
        let total = decoded_len(encoded)?;
        if max_bytes.is_some_and(|limit| total > limit) {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "Image exceeds the reply budget",
            ));
        }
        Ok(Self {
            encoded: encoded.as_bytes().to_vec(),
            part: None,
        })
    }

    pub fn chunk(encoded: &str, offset: u64, revision: Option<String>) -> io::Result<Self> {
        const CHUNK_BYTES: u64 = 4 * 1024 * 1024;
        let total = decoded_len(encoded)?;
        let current = total.to_string();
        if offset > total
            || (offset != 0 && revision.is_none())
            || revision.as_ref().is_some_and(|revision| {
                revision
                    .rsplit_once(':')
                    .is_none_or(|(stamp, _)| stamp != current)
            })
        {
            return Err(io::Error::other("Image changed during transfer"));
        }
        let length = (total - offset).min(CHUNK_BYTES) as usize;
        let skip = (offset % 3) as usize;
        let selected = if revision.is_none() {
            encoded.as_bytes()
        } else {
            let start = (offset / 3 * 4) as usize;
            let end = ((offset + length as u64).div_ceil(3) * 4) as usize;
            &encoded.as_bytes()[start..end]
        };
        Ok(Self {
            encoded: selected.to_vec(),
            part: Some(ImagePart {
                offset,
                total,
                skip,
                length,
                revision,
            }),
        })
    }

    pub fn read(self) -> io::Result<QueryResponse> {
        let Some(part) = self.part else {
            return STANDARD
                .decode(&self.encoded)
                .map(QueryResponse::FileBytes)
                .map_err(io::Error::other);
        };
        let revision = match part.revision {
            Some(revision) => revision,
            None => {
                let reader = base64::read::DecoderReader::new(&self.encoded[..], &STANDARD);
                format!(
                    "{}:{}",
                    part.total,
                    crate::user_files::content_digest(reader)?
                )
            }
        };
        let end = (part.skip + part.length).div_ceil(3) * 4;
        let mut bytes = STANDARD
            .decode(&self.encoded[..end])
            .map_err(io::Error::other)?;
        bytes.drain(..part.skip);
        bytes.truncate(part.length);
        Ok(QueryResponse::FileChunk {
            bytes,
            offset: part.offset,
            total: part.total,
            revision,
        })
    }
}
