use std::io::{self, Write};

use bytes::Bytes;
use loro_common::LoroError;
use lz4_flex::frame::{BlockSize, FrameEncoder, FrameInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionType {
    None,
    LZ4,
}

impl CompressionType {
    pub fn is_none(&self) -> bool {
        matches!(self, CompressionType::None)
    }
}

impl TryFrom<u8> for CompressionType {
    type Error = LoroError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(CompressionType::None),
            1 => Ok(CompressionType::LZ4),
            _ => Err(LoroError::DecodeError(
                format!("Invalid compression type: {value}").into(),
            )),
        }
    }
}

impl From<CompressionType> for u8 {
    fn from(value: CompressionType) -> Self {
        match value {
            CompressionType::None => 0,
            CompressionType::LZ4 => 1,
        }
    }
}

pub fn compress(w: &mut Vec<u8>, data: &[u8], compression_type: CompressionType) {
    match compression_type {
        CompressionType::None => {
            w.write_all(data).unwrap();
        }
        CompressionType::LZ4 => {
            // Auto skips the 1 MiB frame size, allocating 4 MiB buffers for >256 KiB.
            let block_size = match data.len() {
                0..=65_536 => BlockSize::Max64KB,
                65_537..=262_144 => BlockSize::Max256KB,
                262_145..=1_048_576 => BlockSize::Max1MB,
                _ => BlockSize::Max4MB,
            };
            let mut encoder = FrameEncoder::with_frame_info(FrameInfo::new().block_size(block_size), w);
            encoder.write_all(data).unwrap();
            let _w = encoder.finish().unwrap();
        }
    }
}

pub fn decompress(
    out: &mut Vec<u8>,
    data: Bytes,
    compression_type: CompressionType,
) -> Result<(), LoroError> {
    match compression_type {
        CompressionType::None => {
            out.write_all(&data).unwrap();
            Ok(())
        }
        CompressionType::LZ4 => {
            let mut decoder = lz4_flex::frame::FrameDecoder::new(data.as_ref());
            io::copy(&mut decoder, out)
                .map_err(|e| LoroError::DecodeError(e.to_string().into()))?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lz4_roundtrips_frame_boundaries() {
        for length in [0, 65_536, 65_537, 262_144, 262_145, 1_048_576, 1_048_577, 4_194_305] {
            let mut state = 0x1234_5678_u32;
            let data: Vec<u8> = (0..length).map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            }).collect();
            let mut encoded = Vec::new();
            compress(&mut encoded, &data, CompressionType::LZ4);
            let mut decoded = Vec::new();
            decompress(&mut decoded, Bytes::from(encoded), CompressionType::LZ4).unwrap();
            assert_eq!(decoded, data, "frame boundary {length}");
        }
    }
}
