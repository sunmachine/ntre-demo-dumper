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
