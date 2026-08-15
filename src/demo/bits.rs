//! LSB-first bit reader matching the Source engine's `bf_read`: bits come
//! out of each byte least-significant first, bytes in memory order.

use anyhow::{bail, Result};

pub struct BitReader<'a> {
    data: &'a [u8],
    /// Cursor in bits from the start of `data`.
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn bits_left(&self) -> usize {
        self.data.len() * 8 - self.pos
    }

    /// Read `n` bits (n <= 32), LSB-first.
    pub fn read_bits(&mut self, n: u32) -> Result<u32> {
        debug_assert!(n <= 32);
        if self.bits_left() < n as usize {
            bail!("bit stream exhausted (wanted {n} bits, {} left)", self.bits_left());
        }
        let mut out: u32 = 0;
        for i in 0..n {
            let bit = (self.data[self.pos >> 3] >> (self.pos & 7)) & 1;
            out |= (bit as u32) << i;
            self.pos += 1;
        }
        Ok(out)
    }

    pub fn read_bit(&mut self) -> Result<bool> {
        Ok(self.read_bits(1)? == 1)
    }

    pub fn read_u8(&mut self) -> Result<u8> {
        Ok(self.read_bits(8)? as u8)
    }

    pub fn read_i16(&mut self) -> Result<i16> {
        Ok(self.read_bits(16)? as u16 as i16)
    }

    pub fn read_u32(&mut self) -> Result<u32> {
        self.read_bits(32)
    }

    /// The engine's ReadBitFloat: a raw 32-bit IEEE float.
    pub fn read_f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.read_bits(32)?))
    }

    pub fn skip_bits(&mut self, n: usize) -> Result<()> {
        if self.bits_left() < n {
            bail!("bit stream exhausted (skip {n} bits, {} left)", self.bits_left());
        }
        self.pos += n;
        Ok(())
    }

    /// Null-terminated string of (possibly bit-shifted) bytes.
    pub fn read_string(&mut self) -> Result<String> {
        let mut bytes = Vec::new();
        loop {
            let b = self.read_u8()?;
            if b == 0 {
                break;
            }
            if bytes.len() >= 4096 {
                bail!("unterminated string in bit stream");
            }
            bytes.push(b);
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Protobuf-style varint (7 bits per byte, up to 35 bits).
    pub fn read_var_int(&mut self) -> Result<u32> {
        let mut result: u32 = 0;
        for shift in (0..35u32).step_by(7) {
            let byte = self.read_u8()?;
            result |= ((byte & 0x7f) as u32) << shift;
            if byte & 0x80 == 0 {
                break;
            }
        }
        Ok(result)
    }

    /// The engine's ReadBitCoord (14-bit integer part, 5-bit fraction).
    pub fn read_bit_coord(&mut self) -> Result<f32> {
        let has_int = self.read_bit()?;
        let has_frac = self.read_bit()?;
        if !has_int && !has_frac {
            return Ok(0.0);
        }
        let negative = self.read_bit()?;
        let int_part = if has_int { self.read_bits(14)? + 1 } else { 0 };
        let frac_part = if has_frac { self.read_bits(5)? } else { 0 };
        let value = int_part as f32 + frac_part as f32 / 32.0;
        Ok(if negative { -value } else { value })
    }

    /// Copy the next `n` bits out into a byte-aligned buffer.
    pub fn read_chunk(&mut self, n: usize) -> Result<BitChunk> {
        let mut bytes = Vec::with_capacity(n.div_ceil(8));
        let mut left = n;
        while left >= 8 {
            bytes.push(self.read_bits(8)? as u8);
            left -= 8;
        }
        if left > 0 {
            bytes.push(self.read_bits(left as u32)? as u8);
        }
        Ok(BitChunk { bytes, bit_len: n })
    }
}

/// An extracted, bit-aligned copy of part of a bit stream. `bit_len` records
/// the exact extracted size; the final byte may carry padding bits past it.
#[derive(Debug, Clone)]
pub struct BitChunk {
    pub bytes: Vec<u8>,
    #[allow(dead_code)]
    pub bit_len: usize,
}

impl BitChunk {
    pub fn reader(&self) -> BitReader<'_> {
        BitReader::new(&self.bytes)
    }
}

/// Test-only LSB-first bit writer, the mirror of [`BitReader`]. Used to
/// construct synthetic wire-format fixtures.
#[cfg(test)]
pub mod testutil {
    #[derive(Default)]
    pub struct BitWriter {
        pub bytes: Vec<u8>,
        bit_pos: usize,
    }

    impl BitWriter {
        pub fn write_bits(&mut self, value: u32, n: u32) {
            for i in 0..n {
                if self.bit_pos % 8 == 0 {
                    self.bytes.push(0);
                }
                let bit = (value >> i) & 1;
                *self.bytes.last_mut().unwrap() |= (bit as u8) << (self.bit_pos % 8);
                self.bit_pos += 1;
            }
        }

        pub fn write_bit(&mut self, b: bool) {
            self.write_bits(b as u32, 1);
        }

        pub fn write_string(&mut self, s: &str) {
            for &b in s.as_bytes() {
                self.write_bits(b as u32, 8);
            }
            self.write_bits(0, 8);
        }

        pub fn write_f32(&mut self, f: f32) {
            self.write_bits(f.to_bits(), 32);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsb_first_within_bytes() {
        // 0b1101_0010, 0b0000_1011
        let mut r = BitReader::new(&[0xD2, 0x0B]);
        assert!(!r.read_bit().unwrap()); // bit 0 of 0xD2
        assert!(r.read_bit().unwrap()); // bit 1
        assert_eq!(r.read_bits(6).unwrap(), 0b110100); // bits 2..8
        assert_eq!(r.read_bits(8).unwrap(), 0x0B);
        assert_eq!(r.bits_left(), 0);
        assert!(r.read_bit().is_err());
    }

    #[test]
    fn floats_roundtrip() {
        let bytes = 123.456f32.to_le_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_f32().unwrap(), 123.456f32);
    }

    #[test]
    fn values_spanning_byte_boundaries() {
        // 12-bit value 0xABC split across two bytes, LSB-first
        let v: u16 = 0xABC;
        let bytes = [(v & 0xFF) as u8, (v >> 8) as u8];
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_bits(12).unwrap(), 0xABC);
    }
}
