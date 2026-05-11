// SPDX-FileCopyrightText: 2026 Contributors to the Media eXchange Layer project.
// SPDX-License-Identifier: Apache-2.0

//! MXL `video/smpte291` data grains vs GStreamer `meta/x-st-2038` buffers.
//!
//! MXL `video/smpte291` data grains store bytes from the RFC 8331 *Length* field onward only
//! (see `lib/tests/test_flows.cpp`). That slice is also not the same as a bare ST 2038 packet:
//! RFC 8331 includes `S` (Data Stream Flag) and `StreamNum`, which ST 2038 omits. GStreamer
//! `rtpsmpte291pay` and `rtpsmpte291depay` perform a similar translation; see
//! <https://centricular.com/devlog/2025-11/st291-anc-rtp/>.
//!
//! [`AncillaryMeta`] represents ST 2038 / RFC 8331 ANC packets with the same logical fields as
//! `gstreamer_video::video_meta::AncillaryMeta`.

use bitstream_io::{BigEndian, BitRead, BitReader, BitWrite, BitWriter};
use mxl::MXL_DATA_FORMAT_GRAIN_SIZE;
use std::io::Cursor;

#[derive(Debug, thiserror::Error)]
pub enum AncillaryMapError {
    #[error("invalid ancillary data: {0}")]
    Invalid(&'static str),
    #[error("bitstream I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// One ancillary data packet: field names match
/// [`gstreamer_video::video_meta::AncillaryMeta`](https://docs.rs/gstreamer-video/) accessors
/// (`c_not_y_channel`, `line`, `offset`, `did`, `sdid_block_number`, `data_count`, `data`, `checksum`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AncillaryMeta {
    pub c_not_y_channel: bool,
    pub line: u16,
    pub offset: u16,
    pub did: u16,
    pub sdid_block_number: u16,
    /// Full 10-bit `data_count`; low 8 bits are `data.len()` for a well-formed packet.
    pub data_count: u16,
    pub data: Vec<u16>,
    pub checksum: u16,
}

/// RFC 8331 ANC from `start_pos` occupies a multiple of 4 bytes. `w` must already be byte-aligned
/// or `expect` will panic.
fn writer_word_aligned(w: &mut BitWriter<Cursor<&mut Vec<u8>>, BigEndian>, start_pos: u64) -> bool {
    (w.writer().expect("byte_aligned").position() - start_pos).is_multiple_of(4)
}

/// RFC 8331 ANC from `start_pos` occupies a multiple of 4 bytes. `r` must already be byte-aligned
/// or `expect` will panic.
fn reader_word_aligned(r: &mut BitReader<Cursor<&[u8]>, BigEndian>, start_pos: u64) -> bool {
    (r.reader().expect("byte_aligned").position() - start_pos).is_multiple_of(4)
}

/// Read one GStreamer ST 2038 ANC packet (including ST 2038 end padding) from
/// the front of `st2038` and advance `st2038` past consumed bytes.
///
/// Compare to `RtpSmpte291Pay::convert_next_packet` in gst-plugins-rs.
pub fn ancillary_meta_from_st2038_anc_packet(
    st2038: &mut &[u8],
) -> Result<AncillaryMeta, AncillaryMapError> {
    let r_cursor = Cursor::new(*st2038);
    let mut r = BitReader::endian(r_cursor, BigEndian);

    let zeroes = r
        .read::<6, u8>()
        .map_err(|_| AncillaryMapError::Invalid("read zero bits"))?;
    if zeroes != 0 {
        return Err(AncillaryMapError::Invalid(
            "ST 2038 leading zero bits must be zero",
        ));
    }
    let c_not_y_channel = r
        .read_bit()
        .map_err(|_| AncillaryMapError::Invalid("read C flag"))?;
    let line = r
        .read::<11, u16>()
        .map_err(|_| AncillaryMapError::Invalid("read line number"))?;
    let offset = r
        .read::<12, u16>()
        .map_err(|_| AncillaryMapError::Invalid("read horizontal offset"))?;
    let did = r
        .read::<10, u16>()
        .map_err(|_| AncillaryMapError::Invalid("read DID"))?;
    let sdid_block_number = r
        .read::<10, u16>()
        .map_err(|_| AncillaryMapError::Invalid("read SDID"))?;
    let data_count = r
        .read::<10, u16>()
        .map_err(|_| AncillaryMapError::Invalid("read data count"))?;
    let data_count8 = (data_count & 0xff) as usize;

    let mut data = Vec::new();
    for _ in 0..data_count8 {
        let udw = r
            .read::<10, u16>()
            .map_err(|_| AncillaryMapError::Invalid("read user data word"))?;
        data.push(udw);
    }
    let checksum = r
        .read::<10, u16>()
        .map_err(|_| AncillaryMapError::Invalid("read checksum"))?;

    // ST 2038 pads to a byte boundary with 1 bits.
    while !r.byte_aligned() {
        let one = r
            .read_bit()
            .map_err(|_| AncillaryMapError::Invalid("read padding bit"))?;
        if !one {
            return Err(AncillaryMapError::Invalid(
                "ST 2038 padding bits must be ones",
            ));
        }
    }

    let consumed = r.into_reader().position() as usize;
    *st2038 = &st2038[consumed..];

    Ok(AncillaryMeta {
        c_not_y_channel,
        line,
        offset,
        did,
        sdid_block_number,
        data_count,
        checksum,
        data,
    })
}

/// Append one RFC 8331 ANC packet to `smpte291` from [`AncillaryMeta`].
pub fn smpte291_anc_packet_from_ancillary_meta(
    smpte291: &mut Vec<u8>,
    meta: &AncillaryMeta,
) -> Result<(), AncillaryMapError> {
    let w_start_pos = smpte291.len() as u64;
    let mut w_cursor: Cursor<&mut Vec<u8>> = Cursor::new(smpte291);
    w_cursor.set_position(w_start_pos);
    let mut w = BitWriter::endian(w_cursor, BigEndian);

    w.write_bit(meta.c_not_y_channel)
        .map_err(|_| AncillaryMapError::Invalid("write C"))?;
    w.write::<11, u16>(meta.line)
        .map_err(|_| AncillaryMapError::Invalid("write line"))?;
    w.write::<12, u16>(meta.offset)
        .map_err(|_| AncillaryMapError::Invalid("write horizontal offset"))?;

    // ST 2038 has no S or StreamNum here; RFC 8331 inserts them
    // (cf. `RtpSmpte291Pay::convert_next_packet`).
    w.write_bit(false)
        .map_err(|_| AncillaryMapError::Invalid("write S"))?;
    w.write::<7, u8>(0u8)
        .map_err(|_| AncillaryMapError::Invalid("write StreamNum"))?;

    w.write::<10, u16>(meta.did)
        .map_err(|_| AncillaryMapError::Invalid("write DID"))?;
    w.write::<10, u16>(meta.sdid_block_number)
        .map_err(|_| AncillaryMapError::Invalid("write SDID"))?;
    w.write::<10, u16>(meta.data_count)
        .map_err(|_| AncillaryMapError::Invalid("write data_count"))?;

    for &udw in &meta.data {
        w.write::<10, u16>(udw)
            .map_err(|_| AncillaryMapError::Invalid("write user data word"))?;
    }
    w.write::<10, u16>(meta.checksum)
        .map_err(|_| AncillaryMapError::Invalid("write checksum"))?;

    // Byte-align and then pad with zero bits to a 4-byte boundary.
    w.byte_align()
        .map_err(|_| AncillaryMapError::Invalid("write word_align (bits)"))?;
    while !writer_word_aligned(&mut w, w_start_pos) {
        w.write::<8, u8>(0u8)
            .map_err(|_| AncillaryMapError::Invalid("write word_align (bytes)"))?;
    }

    w.flush()?;
    let _ = w.into_writer();

    Ok(())
}

/// Read one GStreamer ST 2038 ANC packet (including ST 2038 end padding) and
/// write one RFC 8331 ANC packet into `smpte291`. Advances `st2038` past the consumed bytes.
/// Compare to `RtpSmpte291Pay::convert_next_packet` in gst-plugins-rs.
pub fn smpte291_from_st2038_anc_packet(
    smpte291: &mut Vec<u8>,
    st2038: &mut &[u8],
) -> Result<(), AncillaryMapError> {
    let meta = ancillary_meta_from_st2038_anc_packet(st2038)?;
    smpte291_anc_packet_from_ancillary_meta(smpte291, &meta)?;
    Ok(())
}

/// Read one RFC 8331 ANC packet from the front of `smpte291` (including word-align padding)
/// and advance `smpte291` past consumed bytes.
pub fn ancillary_meta_from_smpte291_anc_packet(
    smpte291: &mut &[u8],
) -> Result<AncillaryMeta, AncillaryMapError> {
    let r_cursor = Cursor::new(*smpte291);
    let r_start_pos = r_cursor.position();
    let mut r = BitReader::endian(r_cursor, BigEndian);

    let c_not_y_channel = r.read_bit()?;
    let line = r.read::<11, u16>()?;
    let offset = r.read::<12, u16>()?;
    let _s = r.read::<1, u8>()?;
    let _stream_num = r.read::<7, u8>()?;
    let did = r.read::<10, u16>()?;
    let sdid_block_number = r.read::<10, u16>()?;
    let data_count = r.read::<10, u16>()?;
    let data_count8 = (data_count & 0xff) as usize;

    let mut data = Vec::new();
    for _ in 0..data_count8 {
        data.push(r.read::<10, u16>()?);
    }
    let checksum = r.read::<10, u16>()?;

    // RFC 8331 pads to 4-byte boundary with zero bits.
    while !r.byte_aligned() {
        let zero = r.read_bit()?;
        if zero {
            return Err(AncillaryMapError::Invalid(
                "RFC 8331 padding bits must be zero",
            ));
        }
    }
    while !reader_word_aligned(&mut r, r_start_pos) {
        let zero = r.read::<8, u8>()?;
        if zero != 0 {
            return Err(AncillaryMapError::Invalid(
                "RFC 8331 padding bytes must be zero",
            ));
        }
    }

    let consumed = r.into_reader().position() as usize;
    *smpte291 = &smpte291[consumed..];

    Ok(AncillaryMeta {
        c_not_y_channel,
        line,
        offset,
        did,
        sdid_block_number,
        data_count,
        checksum,
        data,
    })
}

/// Append one ST 2038 ANC packet to `st2038` from [`AncillaryMeta`].
pub fn st2038_anc_packet_from_ancillary_meta(
    st2038: &mut Vec<u8>,
    meta: &AncillaryMeta,
) -> Result<(), AncillaryMapError> {
    let w_start_pos = st2038.len() as u64;
    let mut w_cursor: Cursor<&mut Vec<u8>> = Cursor::new(st2038);
    w_cursor.set_position(w_start_pos);
    let mut w = BitWriter::endian(w_cursor, BigEndian);

    w.write::<6, u8>(0)?;
    w.write_bit(meta.c_not_y_channel)?;
    w.write::<11, u16>(meta.line)?;
    w.write::<12, u16>(meta.offset)?;
    w.write::<10, u16>(meta.did)?;
    w.write::<10, u16>(meta.sdid_block_number)?;
    w.write::<10, u16>(meta.data_count)?;
    for &udw in &meta.data {
        w.write::<10, u16>(udw)?;
    }
    w.write::<10, u16>(meta.checksum)?;

    while !w.byte_aligned() {
        w.write_bit(true)?;
    }
    w.flush()?;
    let _ = w.into_writer();

    Ok(())
}

/// Read one RFC 8331 ANC packet from `smpte291` (including word-align padding) and
/// write one ST 2038 ANC packet into `st2038`. Advances `smpte291` past the consumed bytes.
pub fn st2038_from_smpte291_anc_packet(
    st2038: &mut Vec<u8>,
    smpte291: &mut &[u8],
) -> Result<(), AncillaryMapError> {
    let meta = ancillary_meta_from_smpte291_anc_packet(smpte291)?;
    st2038_anc_packet_from_ancillary_meta(st2038, &meta)?;
    Ok(())
}

/// Incrementally builds an MXL `video/smpte291` grain from one or more GStreamer
/// `meta/x-st-2038` buffers (each buffer may contain multiple ST 2038 ANC packets).
pub struct MxlSmpte291GrainBuilder {
    smpte291: Vec<u8>,
    anc_count: u8,
}

impl MxlSmpte291GrainBuilder {
    /// Expected octets in the RFC 8331 fixed prefix before the ANC packets:
    /// `Length` (16 bits), `ANC_Count` (8), `F` (2), `reserved` (22)
    const HEADER_SIZE: usize = (16 + 8 + 2 + 22) / 8;

    pub fn new() -> Result<Self, AncillaryMapError> {
        let mut smpte291 = Vec::new();
        {
            let mut w = BitWriter::endian(&mut smpte291, BigEndian);
            w.write_from::<u16>(0u16)?; // Length (updated in `into_padded_grain`)
            w.write_from::<u8>(0u8)?; // ANC_Count (updated in `into_padded_grain`)
            w.write::<2, u8>(0u8)?; // F
            w.write::<22, u32>(0u32)?; // reserved
            w.flush()?;
        }
        debug_assert_eq!(smpte291.len(), Self::HEADER_SIZE);
        Ok(Self {
            smpte291,
            anc_count: 0,
        })
    }

    pub fn append_st2038_packets(&mut self, st2038: &[u8]) -> Result<(), AncillaryMapError> {
        let mut remaining = st2038;
        while !remaining.is_empty() {
            if self.anc_count == u8::MAX {
                return Err(AncillaryMapError::Invalid(
                    "more than 255 ST 2038 ANC packets in one grain",
                ));
            }
            smpte291_from_st2038_anc_packet(&mut self.smpte291, &mut remaining)?;
            self.anc_count += 1;
        }
        Ok(())
    }

    pub fn into_padded_grain(mut self) -> Result<Vec<u8>, AncillaryMapError> {
        let length = u16::try_from(self.smpte291.len() - Self::HEADER_SIZE)
            .map_err(|_| AncillaryMapError::Invalid("RFC 8331 Length overflow"))?;
        self.smpte291[0..2].copy_from_slice(&length.to_be_bytes());
        self.smpte291[2] = self.anc_count;
        pad_grain(&mut self.smpte291)?;
        Ok(self.smpte291)
    }
}

/// Make an MXL `video/smpte291` data grain from a GStreamer `meta/x-st-2038` buffer.
pub fn mxl_smpte291_grain_from_gst_st2038(st2038: &[u8]) -> Result<Vec<u8>, AncillaryMapError> {
    let mut builder = MxlSmpte291GrainBuilder::new()?;
    builder.append_st2038_packets(st2038)?;
    builder.into_padded_grain()
}

/// Make a GStreamer `meta/x-st-2038` buffer from an MXL `video/smpte291` data grain.
pub fn gst_st2038_from_mxl_smpte291_grain(smpte291: &[u8]) -> Result<Vec<u8>, AncillaryMapError> {
    let mut r = BitReader::endian(Cursor::new(smpte291), BigEndian);
    let length = r.read_to::<u16>()? as usize;
    let anc_count = r.read_to::<u8>()? as usize;
    let _f = r.read::<2, u8>()?;
    let _reserved = r.read::<22, u32>()?;
    assert!(r.byte_aligned(), "RFC 8331 header is byte-aligned");
    let header_len = r.into_reader().position() as usize;
    debug_assert_eq!(header_len, MxlSmpte291GrainBuilder::HEADER_SIZE);
    let mut remaining =
        smpte291
            .get(header_len..header_len + length)
            .ok_or(AncillaryMapError::Invalid(
                "RFC 8331 Length exceeds MXL data grain size",
            ))?;

    let mut st2038 = Vec::new();
    for _ in 0..anc_count {
        st2038_from_smpte291_anc_packet(&mut st2038, &mut remaining)?;
    }
    if !remaining.is_empty() {
        return Err(AncillaryMapError::Invalid(
            "RFC 8331 Length does not match ANC_Count",
        ));
    }
    Ok(st2038)
}

fn pad_grain(buf: &mut Vec<u8>) -> Result<(), AncillaryMapError> {
    if buf.len() > MXL_DATA_FORMAT_GRAIN_SIZE {
        return Err(AncillaryMapError::Invalid(
            "RFC 8331 payload exceeds MXL data grain size",
        ));
    }
    buf.resize(MXL_DATA_FORMAT_GRAIN_SIZE, 0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known-good ST 2038 ANC packets used by tests. Same bytes as
    /// `gst-plugin-rtp` `smpte291::tests::packets`. Each is 100 octets and
    /// parses cleanly, so single-packet tests use index 0 by convention.
    /// The two packets differ in one user-data word and the resulting
    /// checksum, so multi-ANC round-trip tests can verify per-packet
    /// independence rather than coincidentally returning the same packet
    /// twice.
    const ST2038_TEST_PACKETS: &[&[u8]] = &[
        &[
            0x00, 0x02, 0x40, 0x02, 0x61, 0x80, 0x64, 0x96, 0x59, 0x69, 0x92, 0x64, 0xf9, 0x0e,
            0x02, 0x8f, 0x57, 0x2b, 0xd1, 0xfc, 0xa0, 0x28, 0x0b, 0xf6, 0x80, 0xa0, 0x1f, 0xa4,
            0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40, 0x1f,
            0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40,
            0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00,
            0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9,
            0x00, 0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0x74, 0x80, 0xa3, 0xd5,
            0x06, 0xab,
        ],
        &[
            0x00, 0x02, 0x40, 0x02, 0x61, 0x80, 0x64, 0x96, 0x59, 0x69, 0x92, 0x64, 0xf9, 0x0e,
            0x02, 0x8f, 0x97, 0x2b, 0xd1, 0xfc, 0xa0, 0x28, 0x0b, 0xf6, 0x80, 0xa0, 0x1f, 0xa4,
            0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40, 0x1f,
            0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40,
            0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00,
            0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9,
            0x00, 0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0x74, 0x80, 0xa3, 0xe4,
            0xfe, 0xab,
        ],
    ];

    #[test]
    fn smpte291_from_st2038_anc_packet_round_trip() {
        let expected = ST2038_TEST_PACKETS[0];
        let mut smpte291 = Vec::new();
        let mut remaining_st2038 = expected;
        smpte291_from_st2038_anc_packet(&mut smpte291, &mut remaining_st2038).unwrap();
        assert!(remaining_st2038.is_empty());
        let mut remaining_smpte291 = smpte291.as_slice();
        let mut actual = Vec::new();
        st2038_from_smpte291_anc_packet(&mut actual, &mut remaining_smpte291)
            .expect("st2038_from_smpte291");
        assert!(remaining_smpte291.is_empty());
        assert_eq!(actual, expected);
    }

    #[test]
    fn smpte291_from_st2038_anc_packet_rejects_invalid_leading_zero_bits() {
        let bad = [0xffu8, 0xff, 0xff];
        let mut smpte291 = Vec::new();
        let mut remaining = bad.as_slice();
        match smpte291_from_st2038_anc_packet(&mut smpte291, &mut remaining) {
            Err(AncillaryMapError::Invalid(msg)) => {
                assert_eq!(msg, "ST 2038 leading zero bits must be zero");
            }
            Ok(_) => panic!("expected error"),
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }

    #[test]
    fn mxl_smpte291_grain_from_gst_st2038_single_anc_round_trip() {
        let expected = ST2038_TEST_PACKETS[0];
        let grain = mxl_smpte291_grain_from_gst_st2038(expected).unwrap();
        let actual = gst_st2038_from_mxl_smpte291_grain(&grain).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn mxl_smpte291_grain_from_gst_st2038_empty_round_trip() {
        let grain = mxl_smpte291_grain_from_gst_st2038(&[]).unwrap();
        let actual = gst_st2038_from_mxl_smpte291_grain(&grain).unwrap();
        assert!(actual.is_empty());
    }

    #[test]
    fn gst_st2038_from_mxl_smpte291_grain_rejects_length_past_buffer() {
        let mut grain = mxl_smpte291_grain_from_gst_st2038(ST2038_TEST_PACKETS[0]).unwrap();
        grain[0] = 0xff;
        grain[1] = 0xff;
        match gst_st2038_from_mxl_smpte291_grain(&grain) {
            Err(AncillaryMapError::Invalid(msg)) => {
                assert_eq!(msg, "RFC 8331 Length exceeds MXL data grain size");
            }
            Ok(_) => panic!("expected error"),
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }

    #[test]
    fn gst_st2038_from_mxl_smpte291_grain_rejects_anc_count_zero_with_payload() {
        let mut grain = mxl_smpte291_grain_from_gst_st2038(ST2038_TEST_PACKETS[0]).unwrap();
        grain[2] = 0;
        match gst_st2038_from_mxl_smpte291_grain(&grain) {
            Err(AncillaryMapError::Invalid(msg)) => {
                assert_eq!(msg, "RFC 8331 Length does not match ANC_Count");
            }
            Ok(_) => panic!("expected error"),
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }

    #[test]
    fn gst_st2038_from_mxl_smpte291_grain_rejects_anc_count_above_rfc_payload() {
        let expected = ST2038_TEST_PACKETS.concat();
        let mut grain = mxl_smpte291_grain_from_gst_st2038(&expected).unwrap();
        grain[2] = 3;
        match gst_st2038_from_mxl_smpte291_grain(&grain) {
            Err(AncillaryMapError::Io(e)) => {
                assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof);
            }
            Ok(_) => panic!("expected error"),
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }

    #[test]
    fn mxl_smpte291_grain_from_gst_st2038_two_anc_round_trip() {
        let expected = ST2038_TEST_PACKETS.concat();
        let grain = mxl_smpte291_grain_from_gst_st2038(&expected).unwrap();
        let actual = gst_st2038_from_mxl_smpte291_grain(&grain).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn mxl_smpte291_grain_from_gst_st2038_empty_st2038_zero_fills_grain() {
        let actual = mxl_smpte291_grain_from_gst_st2038(&[]).unwrap();
        assert_eq!(actual.len(), MXL_DATA_FORMAT_GRAIN_SIZE);
        assert!(actual.iter().all(|&b| b == 0));
    }

    #[test]
    fn mxl_smpte291_grain_builder_empty_matches_from_gst_st2038() {
        let from_fn = mxl_smpte291_grain_from_gst_st2038(&[]).unwrap();
        let from_builder = MxlSmpte291GrainBuilder::new()
            .unwrap()
            .into_padded_grain()
            .unwrap();
        assert_eq!(from_builder, from_fn);
    }

    #[test]
    fn mxl_smpte291_grain_builder_two_appends_matches_single_buffer() {
        let expected = ST2038_TEST_PACKETS.concat();
        let one_shot = mxl_smpte291_grain_from_gst_st2038(&expected).unwrap();

        let mut b = MxlSmpte291GrainBuilder::new().unwrap();
        b.append_st2038_packets(ST2038_TEST_PACKETS[0]).unwrap();
        b.append_st2038_packets(ST2038_TEST_PACKETS[1]).unwrap();
        let incremental = b.into_padded_grain().unwrap();

        assert_eq!(incremental, one_shot);
        let round_trip = gst_st2038_from_mxl_smpte291_grain(&incremental).unwrap();
        assert_eq!(round_trip, expected);
    }

    #[test]
    fn mxl_smpte291_grain_builder_append_two_anc_in_one_slice_then_empty() {
        let both = ST2038_TEST_PACKETS.concat();
        let one_shot = mxl_smpte291_grain_from_gst_st2038(&both).unwrap();

        let mut b = MxlSmpte291GrainBuilder::new().unwrap();
        b.append_st2038_packets(&both).unwrap();
        b.append_st2038_packets(&[]).unwrap();
        assert_eq!(b.into_padded_grain().unwrap(), one_shot);
    }

    #[test]
    fn mxl_smpte291_grain_builder_rejects_too_many_anc_packets() {
        let mut b = MxlSmpte291GrainBuilder::new().unwrap();
        for _ in 0..255 {
            b.append_st2038_packets(ST2038_TEST_PACKETS[0])
                .expect("255 packets fit");
        }
        match b.append_st2038_packets(ST2038_TEST_PACKETS[0]) {
            Err(AncillaryMapError::Invalid(msg)) => {
                assert_eq!(msg, "more than 255 ST 2038 ANC packets in one grain");
            }
            Ok(_) => panic!("expected error"),
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }
}
