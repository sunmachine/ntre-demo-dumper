//! ASCII skim of bit-packed packet payloads.
//!
//! Net messages inside a packet payload are bit-packed, so embedded text
//! lands at an arbitrary bit alignment. Rather than decoding the message
//! stream, we shift the payload by each of the 8 possible bit offsets and
//! collect printable ASCII runs. Callers filter the result against the
//! patterns they care about.

const MIN_LEN: usize = 8;

/// Extract printable ASCII runs (>= MIN_LEN chars) at every bit alignment.
pub fn skim(payload: &[u8], out: &mut Vec<String>) {
    for shift in 0..8u32 {
        let mut run = Vec::new();
        let mut emit = |run: &mut Vec<u8>| {
            if run.len() >= MIN_LEN {
                out.push(String::from_utf8_lossy(run).into_owned());
            }
            run.clear();
        };
        if shift == 0 {
            for &b in payload {
                if (0x20..0x7f).contains(&b) {
                    run.push(b);
                } else {
                    emit(&mut run);
                }
            }
        } else {
            // byte i of the shifted stream = low bits of payload[i+1] joined
            // with high bits of payload[i] (little-endian bit order)
            for w in payload.windows(2) {
                let b = (w[0] >> shift) | (w[1] << (8 - shift));
                if (0x20..0x7f).contains(&b) {
                    run.push(b);
                } else {
                    emit(&mut run);
                }
            }
        }
        emit(&mut run);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_string_at_any_bit_offset() {
        let msg = b"\0- CTG ROUND 2 STARTED -\0";
        for shift in 0..8u32 {
            // shift the whole message left by `shift` bits, little-endian
            let mut bytes = vec![0u8; msg.len() + 1];
            let mut carry = 0u8;
            for (i, &b) in msg.iter().enumerate() {
                bytes[i] = if shift == 0 { b } else { (b << shift) | carry };
                carry = if shift == 0 { 0 } else { b >> (8 - shift) };
            }
            bytes[msg.len()] = carry;
            let mut found = Vec::new();
            skim(&bytes, &mut found);
            assert!(
                found.iter().any(|s| s == "- CTG ROUND 2 STARTED -"),
                "missed string at bit offset {shift}: {found:?}"
            );
        }
    }
}
