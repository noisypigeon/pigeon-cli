//! An LZO1X decompressor, for Procreate's tile chunks (ADR-0159).
//!
//! A port of the Linux kernel's `lzo1x_decompress_safe` (without its
//! bitstream-v1 RLE extension, which Procreate does not emit), checked
//! against all 276 chunks of a real document before being written: each
//! decompressed to exactly 256 x 256 x 4 bytes.
//!
//! "Safe" is the point. The input is an untrusted file, so every read and
//! every back-reference is bounds-checked and returns `Err` rather than
//! panicking, and the output may never exceed the caller's `max_len` --
//! a crafted chunk cannot make this allocate without limit. In-house
//! rather than a crate because the algorithm is short, decompression-only
//! is all this needs, and the tests below pin its behaviour directly.

/// The kernel's `M2_MAX_OFFSET`: the distance base of a short match that
/// follows a literal run.
const M2_MAX_OFFSET: usize = 0x0800;

struct Decoder<'a> {
    src: &'a [u8],
    ip: usize,
    out: Vec<u8>,
    max_len: usize,
}

impl Decoder<'_> {
    fn byte(&mut self) -> Result<usize, String> {
        let value = *self
            .src
            .get(self.ip)
            .ok_or("lzo: input ended mid-instruction")?;
        self.ip += 1;
        Ok(value as usize)
    }

    fn le16(&mut self) -> Result<usize, String> {
        Ok(self.byte()? | (self.byte()? << 8))
    }

    /// A run-length extension: each zero byte adds 255, and the first
    /// non-zero byte ends it.
    fn zero_run(&mut self, base: usize) -> Result<usize, String> {
        let mut zeros = 0usize;
        while self.byte()? == 0 {
            zeros += 1;
        }
        // `byte` just consumed the terminating non-zero byte; re-read it.
        let last = self.src[self.ip - 1] as usize;
        zeros
            .checked_mul(255)
            .and_then(|value| value.checked_add(base + last))
            .ok_or_else(|| "lzo: length overflow".to_string())
    }

    fn room(&self, n: usize) -> Result<(), String> {
        if self.out.len() + n > self.max_len {
            return Err(format!(
                "lzo: output would exceed the expected {} bytes",
                self.max_len
            ));
        }
        Ok(())
    }

    fn literals(&mut self, n: usize) -> Result<(), String> {
        let end = self.ip.checked_add(n).ok_or("lzo: length overflow")?;
        let run = self
            .src
            .get(self.ip..end)
            .ok_or("lzo: literal run past the end of the input")?;
        self.room(n)?;
        self.out.extend_from_slice(run);
        self.ip = end;
        Ok(())
    }

    /// Copies `n` bytes starting `distance` bytes back. Byte by byte on
    /// purpose: an overlapping match (distance < n) repeats its pattern.
    fn back_reference(&mut self, distance: usize, n: usize) -> Result<(), String> {
        let start = self
            .out
            .len()
            .checked_sub(distance)
            .ok_or("lzo: back-reference before the start of the output")?;
        self.room(n)?;
        for index in 0..n {
            let value = self.out[start + index];
            self.out.push(value);
        }
        Ok(())
    }
}

/// Decompresses `src`, refusing to produce more than `max_len` bytes.
pub(crate) fn decompress(src: &[u8], max_len: usize) -> Result<Vec<u8>, String> {
    let mut d = Decoder {
        src,
        ip: 0,
        out: Vec::with_capacity(max_len.min(1 << 20)),
        max_len,
    };

    // `state` is how many literals the previous instruction copied: 0, 1-3,
    // or 4 for "a long run". It changes how a following byte < 16 decodes.
    let mut state = 0usize;
    let first = *src.first().ok_or("lzo: empty input")? as usize;
    if first > 17 {
        d.ip = 1;
        let run = first - 17;
        d.literals(run)?;
        state = if run < 4 { run } else { 4 };
    }

    loop {
        let t = d.byte()?;
        let (distance, length, next);
        if t < 16 {
            if state == 0 {
                let run = if t == 0 { d.zero_run(15)? } else { t } + 3;
                d.literals(run)?;
                state = 4;
                continue;
            } else if state != 4 {
                next = t & 3;
                distance = 1 + (t >> 2) + (d.byte()? << 2);
                length = 2;
            } else {
                next = t & 3;
                distance = 1 + M2_MAX_OFFSET + (t >> 2) + (d.byte()? << 2);
                length = 3;
            }
        } else if t >= 64 {
            next = t & 3;
            distance = 1 + ((t >> 2) & 7) + (d.byte()? << 3);
            length = (t >> 5) + 1;
        } else if t >= 32 {
            let mut n = (t & 31) + 2;
            if n == 2 {
                n = d.zero_run(31)? + 2;
            }
            let v = d.le16()?;
            distance = 1 + (v >> 2);
            next = v & 3;
            length = n;
        } else {
            let mut n = (t & 7) + 2;
            if n == 2 {
                n = d.zero_run(7)? + 2;
            }
            let v = d.le16()?;
            let far = ((t & 8) << 11) + (v >> 2);
            if far == 0 {
                // The end-of-stream marker: `0x11 0x00 0x00`.
                return Ok(d.out);
            }
            distance = far + 0x4000;
            next = v & 3;
            length = n;
        }
        d.back_reference(distance, length)?;
        d.literals(next)?;
        state = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EOF: [u8; 3] = [0x11, 0x00, 0x00];

    #[test]
    fn decodes_an_initial_literal_run() {
        let mut stream = vec![17 + 5];
        stream.extend_from_slice(b"hello");
        stream.extend_from_slice(&EOF);
        assert_eq!(decompress(&stream, 64).unwrap(), b"hello");
    }

    #[test]
    fn decodes_an_overlapping_short_match() {
        // "abcd", then an M2 match of length 8 at distance 4.
        let mut stream = vec![17 + 4];
        stream.extend_from_slice(b"abcd");
        stream.extend_from_slice(&[0xEC, 0x00]);
        stream.extend_from_slice(&EOF);
        assert_eq!(decompress(&stream, 64).unwrap(), b"abcdabcdabcd");
    }

    #[test]
    fn decodes_a_long_zero_run_like_an_empty_tile() {
        // One zero literal, then an M3 match at distance 1 whose length
        // needs the zero-run extension: 1 + (255 + 31 + 10 + 2) = 299.
        let stream = [17 + 1, 0x00, 0x20, 0x00, 10, 0x00, 0x00, 0x11, 0x00, 0x00];
        let out = decompress(&stream, 1024).unwrap();
        assert_eq!(out.len(), 1 + 255 + 31 + 10 + 2);
        assert!(out.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn rejects_truncated_input_without_panicking() {
        let mut stream = vec![17 + 5];
        stream.extend_from_slice(b"hel");
        assert!(decompress(&stream, 64).is_err());
        assert!(decompress(&[], 64).is_err());
        assert!(decompress(&[17 + 4, b'a', b'b', b'c', b'd', 0xEC], 64).is_err());
    }

    #[test]
    fn rejects_a_back_reference_before_the_output_start() {
        // A match at distance 8 with only 4 bytes written.
        let mut stream = vec![17 + 4];
        stream.extend_from_slice(b"abcd");
        stream.extend_from_slice(&[0xFC, 0x00]);
        stream.extend_from_slice(&EOF);
        let err = decompress(&stream, 64).unwrap_err();
        assert!(err.contains("before the start"), "{err}");
    }

    #[test]
    fn refuses_to_exceed_the_expected_size() {
        let mut stream = vec![17 + 4];
        stream.extend_from_slice(b"abcd");
        stream.extend_from_slice(&[0xEC, 0x00]);
        stream.extend_from_slice(&EOF);
        let err = decompress(&stream, 6).unwrap_err();
        assert!(err.contains("exceed"), "{err}");
    }
}
