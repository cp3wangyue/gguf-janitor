//! Streaming GGUF (v1/v2/v3) header parser.
//!
//! Reads magic, version, tensor infos and metadata key/value blocks without
//! loading tensor data, so multi-GB model files can be summarized cheaply.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use thiserror::Error;

pub const GGUF_MAGIC: u32 = 0x4655_4747; // "GGUF" little endian

#[derive(Debug, Error)]
pub enum GgufError {
    #[error("not a GGUF file (bad magic)")]
    BadMagic,
    #[error("unsupported GGUF version {0}")]
    UnsupportedVersion(u32),
    #[error("truncated GGUF header: expected {need} bytes at offset {offset}")]
    Truncated { need: usize, offset: u64 },
    #[error("implausible header value: {0}")]
    Implausible(&'static str),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq)]
pub enum GgufValue {
    U8(u8),
    I8(i8),
    U16(u16),
    I16(i16),
    U32(u32),
    I32(i32),
    F32(f32),
    Bool(bool),
    Str(String),
    U64(u64),
    I64(i64),
    F64(f64),
    Array(Vec<GgufValue>),
}

impl GgufValue {
    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            GgufValue::U8(v) => Some(v as u64),
            GgufValue::U16(v) => Some(v as u64),
            GgufValue::U32(v) => Some(v as u64),
            GgufValue::U64(v) => Some(v),
            GgufValue::I8(v) => u64::try_from(v).ok(),
            GgufValue::I16(v) => u64::try_from(v).ok(),
            GgufValue::I32(v) => u64::try_from(v).ok(),
            GgufValue::I64(v) => u64::try_from(v).ok(),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            GgufValue::Str(s) => Some(s),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GgufValueType {
    U8 = 0,
    I8 = 1,
    U16 = 2,
    I16 = 3,
    U32 = 4,
    I32 = 5,
    F32 = 6,
    Bool = 7,
    Str = 8,
    Array = 9,
    U64 = 10,
    I64 = 11,
    F64 = 12,
}

impl GgufValueType {
    fn from_u32(v: u32) -> Option<Self> {
        use GgufValueType::*;
        Some(match v {
            0 => U8,
            1 => I8,
            2 => U16,
            3 => I16,
            4 => U32,
            5 => I32,
            6 => F32,
            7 => Bool,
            8 => Str,
            9 => Array,
            10 => U64,
            11 => I64,
            12 => F64,
            _ => return None,
        })
    }

    /// Fixed byte size for non-string/non-array types.
    fn fixed_size(self) -> Option<u64> {
        use GgufValueType::*;
        Some(match self {
            U8 | I8 | Bool => 1,
            U16 | I16 => 2,
            U32 | I32 | F32 => 4,
            U64 | I64 | F64 => 8,
            Str | Array => return None,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct GgufInfo {
    pub version: u32,
    pub tensor_count: u64,
    pub kv_count: u64,
    pub metadata: BTreeMap<String, GgufValue>,
    /// Sum of element counts over all tensors (0 when tensor infos absent).
    pub tensor_elem_count: u64,
    /// Total bytes of tensor data declared by the tensor info table (approximate:
    /// derived from element counts and per-type sizes, excluding padding).
    pub tensor_bytes_estimate: u64,
}

const MAX_KEY_LEN: u64 = 8 * 1024;
const MAX_STR_LEN: u64 = 64 * 1024 * 1024;
const MAX_ARRAY_LEN: u64 = 64 * 1024 * 1024;
const MAX_KV_COUNT: u64 = 1_000_000;
const MAX_TENSOR_COUNT: u64 = 1_000_000;
/// Header region we are willing to walk before giving up (metadata only; data excluded).
const MAX_HEADER_BYTES: u64 = 256 * 1024 * 1024;

struct Cursor<R: BufRead + Seek> {
    inner: R,
    offset: u64,
}

impl<R: BufRead + Seek> Cursor<R> {
    fn need(&mut self, buf: &mut [u8]) -> Result<(), GgufError> {
        let n = buf.len();
        let mut filled = 0;
        while filled < n {
            let read = self.inner.read(&mut buf[filled..])?;
            if read == 0 {
                return Err(GgufError::Truncated { need: n, offset: self.offset });
            }
            filled += read;
            self.offset += read as u64;
        }
        Ok(())
    }

    fn u8(&mut self) -> Result<u8, GgufError> {
        let mut b = [0u8; 1];
        self.need(&mut b)?;
        Ok(b[0])
    }

    fn u32(&mut self) -> Result<u32, GgufError> {
        let mut b = [0u8; 4];
        self.need(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    fn u64(&mut self) -> Result<u64, GgufError> {
        let mut b = [0u8; 8];
        self.need(&mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    fn f32(&mut self) -> Result<f32, GgufError> {
        let mut b = [0u8; 4];
        self.need(&mut b)?;
        Ok(f32::from_le_bytes(b))
    }

    fn f64(&mut self) -> Result<f64, GgufError> {
        let mut b = [0u8; 8];
        self.need(&mut b)?;
        Ok(f64::from_le_bytes(b))
    }

    fn bytes(&mut self, len: u64) -> Result<Vec<u8>, GgufError> {
        let mut v = vec![0u8; len as usize];
        self.need(&mut v)?;
        Ok(v)
    }

    fn string(&mut self, max: u64) -> Result<String, GgufError> {
        let len = self.u64()?;
        if len > max {
            return Err(GgufError::Implausible("string length exceeds limit"));
        }
        let raw = self.bytes(len)?;
        Ok(String::from_utf8_lossy(&raw).into_owned())
    }
}

fn read_value<R: BufRead + Seek>(c: &mut Cursor<R>, ty: GgufValueType, depth: u32) -> Result<GgufValue, GgufError> {
    if depth > 4 {
        return Err(GgufError::Implausible("nested arrays too deep"));
    }
    Ok(match ty {
        GgufValueType::U8 => GgufValue::U8(c.u8()?),
        GgufValueType::I8 => GgufValue::I8(c.u8()? as i8),
        GgufValueType::U16 => {
            let mut b = [0u8; 2];
            c.need(&mut b)?;
            GgufValue::U16(u16::from_le_bytes(b))
        }
        GgufValueType::I16 => {
            let mut b = [0u8; 2];
            c.need(&mut b)?;
            GgufValue::I16(i16::from_le_bytes(b))
        }
        GgufValueType::U32 => GgufValue::U32(c.u32()?),
        GgufValueType::I32 => GgufValue::I32(c.u32()? as i32),
        GgufValueType::F32 => GgufValue::F32(c.f32()?),
        GgufValueType::Bool => GgufValue::Bool(c.u8()? != 0),
        GgufValueType::Str => GgufValue::Str(c.string(MAX_STR_LEN)?),
        GgufValueType::U64 => GgufValue::U64(c.u64()?),
        GgufValueType::I64 => GgufValue::I64(c.u64()? as i64),
        GgufValueType::F64 => GgufValue::F64(c.f64()?),
        GgufValueType::Array => {
            let elem_ty_u = c.u32()?;
            let elem_ty = GgufValueType::from_u32(elem_ty_u)
                .ok_or(GgufError::Implausible("unknown array element type"))?;
            let count = c.u64()?;
            if count > MAX_ARRAY_LEN {
                return Err(GgufError::Implausible("array length exceeds limit"));
            }
            // Fast-path: fixed-size element arrays are skipped without materializing.
            if let Some(step) = elem_ty.fixed_size() {
                let skip = step
                    .checked_mul(count)
                    .ok_or(GgufError::Implausible("array size overflow"))?;
                c.inner
                    .seek(SeekFrom::Current(skip as i64))
                    .map_err(|e| GgufError::Io(e))?;
                c.offset += skip;
                return Ok(GgufValue::Array(Vec::new()));
            }
            if depth + 1 > 2 && elem_ty == GgufValueType::Array {
                return Err(GgufError::Implausible("nested arrays disallowed"));
            }
            let mut items = Vec::new();
            // Only string arrays are materialized, and they are bounded in practice
            // by tokenizer lists; keep the first few for display, skip the rest.
            const KEEP: u64 = 8;
            for i in 0..count {
                if elem_ty == GgufValueType::Str {
                    if i < KEEP {
                        items.push(GgufValue::Str(c.string(MAX_STR_LEN)?));
                    } else {
                        let _ = c.string(MAX_STR_LEN)?;
                    }
                } else {
                    let v = read_value(c, elem_ty, depth + 1)?;
                    if i < KEEP {
                        items.push(v);
                    }
                }
            }
            GgufValue::Array(items)
        }
    })
}

/// Parse a GGUF header from any reader (streamed).
pub fn parse_header_from_reader<R: Read + Seek>(reader: R) -> Result<GgufInfo, GgufError> {
    let mut c = Cursor { inner: BufReader::with_capacity(256 * 1024, reader), offset: 0 };

    let magic = c.u32()?;
    if magic != GGUF_MAGIC {
        return Err(GgufError::BadMagic);
    }
    let version = c.u32()?;
    if !(1..=3).contains(&version) {
        return Err(GgufError::UnsupportedVersion(version));
    }
    let tensor_count = c.u64()?;
    let kv_count = c.u64()?;
    if tensor_count > MAX_TENSOR_COUNT || kv_count > MAX_KV_COUNT {
        return Err(GgufError::Implausible("tensor/kv count exceeds limit"));
    }

    let mut info = GgufInfo { version, tensor_count, kv_count, ..Default::default() };

    for _ in 0..kv_count {
        if c.offset > MAX_HEADER_BYTES {
            return Err(GgufError::Implausible("header exceeds size limit"));
        }
        let key = c.string(MAX_KEY_LEN)?;
        let ty_u = c.u32()?;
        let ty = GgufValueType::from_u32(ty_u)
            .ok_or(GgufError::Implausible("unknown metadata value type"))?;
        let val = read_value(&mut c, ty, 0)?;
        info.metadata.insert(key, val);
    }

    // Tensor info table: name, n_dims, dims, ggml type, offset.
    const GGML_TYPE_SIZES: [u64; 32] = [
        4, 2, 1, 1, 8, 8, 4, // F32..I64 (0..6)
        1, 1, 1, 1, 1, 1, 1, 1, // rest of the small enums, safe default 1
        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    ];
    let mut elem_total: u64 = 0;
    let mut bytes_est: u64 = 0;
    for _ in 0..tensor_count {
        if c.offset > MAX_HEADER_BYTES {
            return Err(GgufError::Implausible("header exceeds size limit"));
        }
        let _name = c.string(MAX_KEY_LEN)?;
        let n_dims = c.u32()?;
        if n_dims > 8 {
            return Err(GgufError::Implausible("tensor dims exceed 8"));
        }
        let mut elems: u64 = 1;
        for _ in 0..n_dims {
            let d = c.u64()?;
            elems = elems.saturating_mul(d);
        }
        let ggml_ty = c.u32()?;
        let _offset = c.u64()?;
        let item = GGML_TYPE_SIZES
            .get(ggml_ty as usize)
            .copied()
            .unwrap_or(1);
        elem_total = elem_total.saturating_add(elems);
        bytes_est = bytes_est.saturating_add(elems.saturating_mul(item));
    }
    info.tensor_elem_count = elem_total;
    info.tensor_bytes_estimate = bytes_est;
    Ok(info)
}

pub fn parse_header(path: &Path) -> Result<GgufInfo, GgufError> {
    let f = File::open(path)?;
    parse_header_from_reader(f)
}

/// Well-known GGML file_type values -> human quant label.
pub fn quant_label(file_type: Option<u64>) -> &'static str {
    match file_type {
        Some(0) => "F32",
        Some(1) => "F16",
        Some(2) => "Q4_0",
        Some(3) => "Q4_1",
        Some(7) => "Q8_0",
        Some(8) => "Q5_0",
        Some(9) => "Q5_1",
        Some(10) => "Q2_K",
        Some(11) => "Q3_K_S",
        Some(12) => "Q3_K_M",
        Some(13) => "Q3_K_L",
        Some(14) => "Q4_K_S",
        Some(15) => "Q4_K_M",
        Some(16) => "Q5_K_S",
        Some(17) => "Q5_K_M",
        Some(18) => "Q6_K",
        Some(19) => "IQ2_XXS",
        Some(20) => "IQ2_XS",
        Some(21) => "Q2_K_S",
        Some(22) => "IQ3_XS",
        Some(23) => "IQ3_XXS",
        Some(24) => "IQ1_S",
        Some(25) => "IQ4_NL",
        Some(26) => "IQ3_S",
        Some(27) => "IQ3_M",
        Some(28) => "IQ2_S",
        Some(29) => "IQ2_M",
        Some(30) => "IQ4_XS",
        Some(31) => "IQ1_M",
        Some(32) => "BF16",
        Some(36) => "TQ1_0",
        Some(37) => "TQ2_0",
        Some(38) => "MXFP4",
        _ => "unknown",
    }
}

/// Extracted, display-oriented summary of a GGUF file.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct GgufSummary {
    pub name: Option<String>,
    pub architecture: Option<String>,
    pub quant: Option<String>,
    pub parameter_count: Option<u64>,
    pub context_length: Option<u64>,
    pub n_layers: Option<u64>,
    pub n_embd: Option<u64>,
    pub n_head: Option<u64>,
    pub n_head_kv: Option<u64>,
    pub tensor_count: u64,
    pub version: u32,
}

impl GgufInfo {
    pub fn summarize(&self) -> GgufSummary {
        let arch = self
            .metadata
            .get("general.architecture")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let ctx_key = arch.as_ref().map(|a| format!("{a}.context_length"));
        let layers_key = arch.as_ref().map(|a| format!("{a}.block_count"));
        let embd_key = arch.as_ref().map(|a| format!("{a}.embedding_length"));
        let head_key = arch.as_ref().map(|a| format!("{a}.attention.head_count"));
        let head_kv_key = arch.as_ref().map(|a| format!("{a}.attention.head_count_kv"));
        let num = |k: &Option<String>| k.as_ref().and_then(|k| self.metadata.get(k)).and_then(|v| v.as_u64());

        // Parameter count: prefer explicit metadata, else estimate from tensors.
        let params = self
            .metadata
            .get("general.parameter_count")
            .and_then(|v| v.as_u64())
            .or(if self.tensor_elem_count > 0 { Some(self.tensor_elem_count) } else { None });

        GgufSummary {
            name: self
                .metadata
                .get("general.name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            architecture: arch,
            quant: self.metadata.get("general.file_type").and_then(|v| v.as_u64()).map(|t| quant_label(Some(t)).to_string()),
            parameter_count: params,
            context_length: num(&ctx_key),
            n_layers: num(&layers_key),
            n_embd: num(&embd_key),
            n_head: num(&head_key),
            n_head_kv: num(&head_kv_key),
            tensor_count: self.tensor_count,
            version: self.version,
        }
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;

    /// Serialize a GGUF header (no tensor data) into a byte vector.
    pub fn build_gguf(
        version: u32,
        kvs: Vec<(String, GgufValue)>,
        tensors: Vec<(String, Vec<u64>, u32)>,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&GGUF_MAGIC.to_le_bytes());
        out.extend_from_slice(&version.to_le_bytes());
        out.extend_from_slice(&(tensors.len() as u64).to_le_bytes());
        out.extend_from_slice(&(kvs.len() as u64).to_le_bytes());

        let ty_of = |v: &GgufValue| -> u32 {
            match v {
                GgufValue::U8(_) => 0,
                GgufValue::I8(_) => 1,
                GgufValue::U16(_) => 2,
                GgufValue::I16(_) => 3,
                GgufValue::U32(_) => 4,
                GgufValue::I32(_) => 5,
                GgufValue::F32(_) => 6,
                GgufValue::Bool(_) => 7,
                GgufValue::Str(_) => 8,
                GgufValue::Array(_) => 9,
                GgufValue::U64(_) => 10,
                GgufValue::I64(_) => 11,
                GgufValue::F64(_) => 12,
            }
        };
        for (k, v) in &kvs {
            out.extend_from_slice(&(k.len() as u64).to_le_bytes());
            out.extend_from_slice(k.as_bytes());
            out.extend_from_slice(&ty_of(v).to_le_bytes());
            match v {
                GgufValue::U8(x) => out.push(*x),
                GgufValue::I8(x) => out.push(*x as u8),
                GgufValue::U16(x) => out.extend_from_slice(&x.to_le_bytes()),
                GgufValue::I16(x) => out.extend_from_slice(&x.to_le_bytes()),
                GgufValue::U32(x) => out.extend_from_slice(&x.to_le_bytes()),
                GgufValue::I32(x) => out.extend_from_slice(&x.to_le_bytes()),
                GgufValue::F32(x) => out.extend_from_slice(&x.to_le_bytes()),
                GgufValue::Bool(x) => out.push(*x as u8),
                GgufValue::Str(s) => {
                    out.extend_from_slice(&(s.len() as u64).to_le_bytes());
                    out.extend_from_slice(s.as_bytes());
                }
                GgufValue::U64(x) => out.extend_from_slice(&x.to_le_bytes()),
                GgufValue::I64(x) => out.extend_from_slice(&x.to_le_bytes()),
                GgufValue::F64(x) => out.extend_from_slice(&x.to_le_bytes()),
                GgufValue::Array(items) => {
                    let elem_ty = items.first().map(ty_of).unwrap_or(0);
                    out.extend_from_slice(&elem_ty.to_le_bytes());
                    out.extend_from_slice(&(items.len() as u64).to_le_bytes());
                    for it in items {
                        match it {
                            GgufValue::Str(s) => {
                                out.extend_from_slice(&(s.len() as u64).to_le_bytes());
                                out.extend_from_slice(s.as_bytes());
                            }
                            GgufValue::U32(x) => out.extend_from_slice(&x.to_le_bytes()),
                            GgufValue::U64(x) => out.extend_from_slice(&x.to_le_bytes()),
                            other => panic!("testutil array elem unsupported: {other:?}"),
                        }
                    }
                }
            }
        }
        for (name, dims, ggml_ty) in tensors {
            out.extend_from_slice(&(name.len() as u64).to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&(dims.len() as u32).to_le_bytes());
            for d in &dims {
                out.extend_from_slice(&d.to_le_bytes());
            }
            out.extend_from_slice(&ggml_ty.to_le_bytes());
            out.extend_from_slice(&0u64.to_le_bytes()); // offset
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    #[test]
    fn parses_v3_with_all_scalar_types() {
        let kvs = vec![
            ("general.architecture".to_string(), GgufValue::Str("llama".to_string())),
            ("general.name".to_string(), GgufValue::Str("TestModel".to_string())),
            ("general.file_type".to_string(), GgufValue::U32(15)),
            ("llama.context_length".to_string(), GgufValue::U32(8192)),
            ("llama.block_count".to_string(), GgufValue::U32(32)),
            ("llama.embedding_length".to_string(), GgufValue::U32(4096)),
            ("llama.attention.head_count".to_string(), GgufValue::U32(32)),
            ("llama.attention.head_count_kv".to_string(), GgufValue::U32(8)),
            ("some.f64".to_string(), GgufValue::F64(1.5)),
            ("some.bool".to_string(), GgufValue::Bool(true)),
            ("some.u64".to_string(), GgufValue::U64(u64::MAX)),
        ];
        let tensors = vec![
            ("blk.0.attn_q.weight".to_string(), vec![4096, 4096], 1u32), // F16
            ("output.weight".to_string(), vec![4096, 32000], 0u32),     // F32
        ];
        let bytes = build_gguf(3, kvs.clone(), tensors);
        let info = parse_header_from_reader(std::io::Cursor::new(&bytes[..])).unwrap();
        assert_eq!(info.version, 3);
        assert_eq!(info.kv_count, kvs.len() as u64);
        assert_eq!(info.tensor_count, 2);
        assert_eq!(info.metadata.get("general.name"), Some(&GgufValue::Str("TestModel".into())));
        let s = info.summarize();
        assert_eq!(s.architecture.as_deref(), Some("llama"));
        assert_eq!(s.quant.as_deref(), Some("Q4_K_M"));
        assert_eq!(s.context_length, Some(8192));
        assert_eq!(s.n_layers, Some(32));
        assert_eq!(s.n_head_kv, Some(8));
        // 4096*4096 elems of F16 + 4096*32000 of F32
        assert_eq!(info.tensor_elem_count, 4096u64 * 4096 + 4096 * 32000);
        assert_eq!(info.tensor_bytes_estimate, 4096u64 * 4096 * 2 + 4096 * 32000 * 4);
    }

    #[test]
    fn skips_large_fixed_arrays_without_reading() {
        // 1M u32 entries: parser must seek past them, not read into memory.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&GGUF_MAGIC.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes()); // version
        bytes.extend_from_slice(&0u64.to_le_bytes()); // tensor_count
        bytes.extend_from_slice(&1u64.to_le_bytes()); // kv_count
        let key = "big.array";
        bytes.extend_from_slice(&(key.len() as u64).to_le_bytes());
        bytes.extend_from_slice(key.as_bytes());
        bytes.extend_from_slice(&9u32.to_le_bytes()); // array
        bytes.extend_from_slice(&4u32.to_le_bytes()); // u32 elems
        bytes.extend_from_slice(&1_000_000u64.to_le_bytes());
        bytes.extend_from_slice(&vec![0u8; 4_000_000]);
        let info = parse_header_from_reader(std::io::Cursor::new(&bytes[..])).unwrap();
        assert_eq!(info.kv_count, 1);
        assert!(info.metadata.contains_key("big.array"));
    }

    #[test]
    fn rejects_bad_magic_and_versions() {
        let bytes = build_gguf(3, vec![], vec![]);
        let mut bad = bytes.clone();
        bad[0] = b'X';
        assert!(matches!(parse_header_from_reader(std::io::Cursor::new(&bad[..])), Err(GgufError::BadMagic)));
        let bad_ver = build_gguf(4, vec![], vec![]);
        assert!(matches!(
            parse_header_from_reader(std::io::Cursor::new(&bad_ver[..])),
            Err(GgufError::UnsupportedVersion(4))
        ));
    }

    #[test]
    fn rejects_truncated() {
        let bytes = build_gguf(3, vec![("a".into(), GgufValue::U32(1))], vec![]);
        assert!(matches!(
            parse_header_from_reader(std::io::Cursor::new(&bytes[..bytes.len() - 2])),
            Err(GgufError::Truncated { .. })
        ));
    }

    #[test]
    fn v1_header_parses() {
        let bytes = build_gguf(1, vec![("general.name".into(), GgufValue::Str("old".into()))], vec![]);
        let info = parse_header_from_reader(std::io::Cursor::new(&bytes[..])).unwrap();
        assert_eq!(info.version, 1);
    }

    #[test]
    fn summarize_handles_missing_arch() {
        let bytes = build_gguf(3, vec![], vec![]);
        let info = parse_header_from_reader(std::io::Cursor::new(&bytes[..])).unwrap();
        let s = info.summarize();
        assert!(s.architecture.is_none());
        assert_eq!(s.parameter_count, None);
    }
}
