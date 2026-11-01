//! Just enough Matroska to hand ffmpeg uncompressed frames *with their
//! timestamps*: an EBML header, a live (unknown-size) Segment, one video
//! track (`V_UNCOMPRESSED`, pixel layout as a FourCC) and one Cluster per
//! frame holding a single SimpleBlock.
//!
//! Raw video on a pipe has no timestamps, and letting ffmpeg stamp frames on
//! arrival depends on when it gets round to reading them, which drifted by
//! up to a second around sparse frames. With timestamps in the stream the
//! video's timing is exactly the capture timing, and leaving paused time out
//! is a subtraction.

use crate::capture::PixelOrder;

/// Timestamps are in milliseconds (TimestampScale 1 000 000 ns).
const TIMESTAMP_SCALE: u64 = 1_000_000;

/// EBML variable-length size.
fn size(n: u64) -> Vec<u8> {
    for len in 1..=8u32 {
        // All ones is reserved (it means "unknown size").
        if n < (1u64 << (7 * len)) - 1 {
            let mut v = n.to_be_bytes()[8 - len as usize..].to_vec();
            v[0] |= 0x80 >> (len - 1);
            return v;
        }
    }
    panic!("EBML size too large: {n}");
}

fn element(id: &[u8], data: &[u8]) -> Vec<u8> {
    let mut v = id.to_vec();
    v.extend(size(data.len() as u64));
    v.extend_from_slice(data);
    v
}

fn uint(id: &[u8], n: u64) -> Vec<u8> {
    let bytes = n.to_be_bytes();
    let skip = bytes.iter().take(7).take_while(|&&b| b == 0).count();
    element(id, &bytes[skip..])
}

/// FourCC ffmpeg maps to the matching raw pixel format.
fn fourcc(order: PixelOrder) -> &'static [u8; 4] {
    match order {
        PixelOrder::Bgrx => b"BGR\0",
        PixelOrder::Rgbx => b"RGB\0",
    }
}

/// Everything before the first frame. `frame_ns` is the nominal frame
/// interval: ffmpeg uses it for the last frame's duration and its frame-rate
/// guess (otherwise taken from the first, often sparse, timestamps).
pub fn header(width: u32, height: u32, order: PixelOrder, frame_ns: u64) -> Vec<u8> {
    let ebml = [
        uint(&[0x42, 0x86], 1),              // EBMLVersion
        uint(&[0x42, 0xf7], 1),              // EBMLReadVersion
        uint(&[0x42, 0xf2], 4),              // EBMLMaxIDLength
        uint(&[0x42, 0xf3], 8),              // EBMLMaxSizeLength
        element(&[0x42, 0x82], b"matroska"), // DocType
        uint(&[0x42, 0x87], 4),              // DocTypeVersion
        uint(&[0x42, 0x85], 2),              // DocTypeReadVersion
    ]
    .concat();
    let info = [
        uint(&[0x2a, 0xd7, 0xb1], TIMESTAMP_SCALE),
        element(&[0x4d, 0x80], b"clipt"), // MuxingApp
        element(&[0x57, 0x41], b"clipt"), // WritingApp
    ]
    .concat();
    let video = [
        uint(&[0xb0], width as u64),                 // PixelWidth
        uint(&[0xba], height as u64),                // PixelHeight
        element(&[0x2e, 0xb5, 0x24], fourcc(order)), // ColourSpace
    ]
    .concat();
    let track = [
        uint(&[0xd7], 1),                    // TrackNumber
        uint(&[0x73, 0xc5], 1),              // TrackUID
        uint(&[0x83], 1),                    // TrackType: video
        uint(&[0x23, 0xe3, 0x83], frame_ns), // DefaultDuration
        element(&[0x86], b"V_UNCOMPRESSED"), // CodecID
        element(&[0xe0], &video),
    ]
    .concat();
    let mut v = element(&[0x1a, 0x45, 0xdf, 0xa3], &ebml);
    // Segment of unknown size: it's a live stream.
    v.extend([0x18, 0x53, 0x80, 0x67, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
    v.extend(element(&[0x15, 0x49, 0xa9, 0x66], &info));
    v.extend(element(&[0x16, 0x54, 0xae, 0x6b], &element(&[0xae], &track)));
    v
}

/// The bytes before a frame of `len` bytes shown at `ts_ms`; the frame
/// follows directly, so it never needs copying.
pub fn frame_prefix(ts_ms: u64, len: usize) -> Vec<u8> {
    // SimpleBlock: track 1, relative timestamp 0, keyframe.
    let block_head = [0x81, 0x00, 0x00, 0x80];
    let block_len = (block_head.len() + len) as u64;
    let ts = uint(&[0xe7], ts_ms);
    let mut block = vec![0xa3];
    block.extend(size(block_len));
    let cluster_len = ts.len() as u64 + block.len() as u64 + block_len;
    let mut v = vec![0x1f, 0x43, 0xb6, 0x75];
    v.extend(size(cluster_len));
    v.extend(ts);
    v.extend(block);
    v.extend(block_head);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(size(0), [0x80]);
        assert_eq!(size(126), [0xfe]);
        assert_eq!(size(127), [0x40, 0x7f], "0x7f would read as unknown size");
        assert_eq!(size(16382), [0x7f, 0xfe]);
        assert_eq!(size(33_177_600)[0] >> 4, 0x1, "a 4K frame needs 4 bytes");
    }

    #[test]
    fn uints_are_minimal() {
        assert_eq!(uint(&[0xe7], 0), [0xe7, 0x81, 0x00]);
        assert_eq!(uint(&[0xe7], 1500), [0xe7, 0x82, 0x05, 0xdc]);
    }

    #[test]
    fn frame_prefix_lengths_add_up() {
        let len = 1920 * 1080 * 4;
        let p = frame_prefix(1500, len);
        assert_eq!(&p[..4], &[0x1f, 0x43, 0xb6, 0x75]);
        // cluster size (4 bytes here) covers everything after it, frame included
        let cluster = u64::from_be_bytes([0, 0, 0, 0, p[4] & 0x0f, p[5], p[6], p[7]]);
        assert_eq!(cluster as usize, p.len() - 8 + len);
    }
}
