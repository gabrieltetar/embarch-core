//! Decoding a framed analog tap, one verified record at a time (EMB-475,
//! decision 125; the owner's EMB-469 decisions 7, 8 and 13).
//!
//! A tap whose study declares an `AnalogBlocks` record check carries binary
//! blocks, not lines: its int16 samples contain `0x0A` as often as any other
//! byte, so splitting at newlines corrupts every block. [`AnalogTapDecoder`]
//! takes the bytes of each read as they arrive, carries a partial record to
//! the next read, and hands back what the verified records mean. **Every
//! offset and every check comes from `embarch_study_designer::analog`**; this
//! module holds the carrying, the timing and the event shapes, nothing about
//! the wire layout.
//!
//! **The raw bytes are not this module's.** `write_stream_record` writes them
//! whole before the decoder sees them (decision 30(b)), so a refusal here
//! costs an event row, never the capture.

use embarch_study_designer::analog::{
    decode_block, decode_start_header, AnalogBlock, IndexStep, IndexTracker, StartHeader,
    BLOCK_MAGIC, START_HEADER_BYTES, START_MAGIC,
};
use embarch_study_designer::{Sample, Unit};

/// A marker edge placed on the sample axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkerEdge {
    /// Sample index (per channel) of the first edge in its block: the block's
    /// index plus the event word's frame offset.
    pub sample_index: u32,
    /// The marker level after that edge.
    pub level_high: bool,
}

/// One verified block, owned so it can outlive the read it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedBlock {
    /// Sample index of the block's first frame.
    pub index: u32,
    /// Offset in the tap's raw byte stream of the block's first byte.
    pub raw_offset: u64,
    pub channels: u8,
    /// Raw ADC codes, frame-interleaved.
    pub codes: Vec<i16>,
    /// The first edge in this block, when its event word names one inside it.
    pub edge: Option<MarkerEdge>,
    /// Edges in this block. Only the first has an offset on the wire.
    pub edge_count: u8,
    /// The event word names an offset at or past the block's end.
    pub edge_out_of_block: bool,
    /// Core's time for the block's first frame, from the declared rate and
    /// the index. `None` when no rate is known.
    pub first_frame_utc_ms: Option<u64>,
}

impl DecodedBlock {
    /// The block as live-view samples: raw codes (decision 8), one per
    /// channel per frame, stamped by index at the declared rate. With no rate
    /// every sample carries `arrival_utc_ms`, as `samples_in` does for an
    /// undeclared rate, rather than an interpolation nobody declared.
    pub fn samples(&self, rate_hz: Option<u32>, arrival_utc_ms: u64) -> Vec<Sample> {
        let channels = usize::from(self.channels.max(1));
        self.codes
            .iter()
            .enumerate()
            .map(|(i, code)| {
                let frame = (i / channels) as u64;
                let rx_utc_ms = match (rate_hz, self.first_frame_utc_ms) {
                    (Some(hz), Some(first)) if hz > 0 => first + frame * 1000 / u64::from(hz),
                    _ => arrival_utc_ms,
                };
                Sample {
                    rx_utc_ms,
                    value: f32::from(*code),
                    unit: Unit::Raw,
                    channel_id: (i % channels) as u8,
                }
            })
            .collect()
    }
}

/// Why bytes were not taken as a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// A block-shaped span whose CRC-32 did not match.
    BlockCrc,
    /// A start header that did not decode, or declares another channel count
    /// than the study's layout.
    Header,
    /// Bytes ahead of the next block magic that belong to no record.
    LeadingBytes,
    /// The tap closed with part of a record held.
    PartialAtEnd,
}

impl Refusal {
    /// The token an `events.csv` row carries.
    pub const fn token(self) -> &'static str {
        match self {
            Refusal::BlockCrc => "refused_crc",
            Refusal::Header => "refused_header",
            Refusal::LeadingBytes => "refused_leading_bytes",
            Refusal::PartialAtEnd => "refused_partial_at_end",
        }
    }
}

/// What a read's bytes meant.
#[derive(Debug, Clone, PartialEq)]
pub enum AnalogEvent {
    /// The session's start header, verified.
    Header(StartHeader),
    /// A verified block.
    Block(DecodedBlock),
    /// Blocks were skipped: `missing_samples` frames per channel are absent
    /// between `expected` and `got`. `raw_offset` is where the block that
    /// revealed it starts.
    Gap { expected: u32, got: u32, missing_samples: u32, raw_offset: u64 },
    /// A block's index went backwards or repeated.
    Backwards { expected: u32, got: u32, raw_offset: u64 },
    /// `skipped_bytes` at `raw_offset` were not taken as a record.
    Refused { raw_offset: u64, skipped_bytes: usize, why: Refusal },
    /// The start header's rate differs from the study's declared one. The
    /// declared one is applied.
    RateMismatch { declared_hz: u32, header_hz: u32 },
}

/// Why a decoder could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadLayout;

/// One framed tap's decoder. Lives for the capture, never per read: a record
/// is 528 bytes at the default layout and a read is at most 512, so a record
/// straddling two reads is the normal case.
pub struct AnalogTapDecoder {
    channels: u8,
    frames: u16,
    block_len: usize,
    declared_hz: Option<u32>,
    /// Bytes held: the start of a record that has not finished arriving.
    pending: Vec<u8>,
    /// Raw-stream offset of `pending[0]`.
    pending_offset: u64,
    tracker: IndexTracker,
    header: Option<StartHeader>,
    /// Whether the first bytes have been checked for a start header.
    started: bool,
    /// Core's time for sample index `anchor.0`, set by the first block.
    anchor: Option<(u32, u64)>,
}

impl AnalogTapDecoder {
    /// A decoder for `channels` x `frames` blocks. `declared_hz` is the
    /// study's rate for the tap, when its source declares one.
    pub fn new(channels: u8, frames: u16, declared_hz: Option<u32>) -> Result<Self, BadLayout> {
        let block_len =
            embarch_study_designer::analog::block_len(channels, frames).ok_or(BadLayout)?;
        Ok(Self {
            channels,
            frames,
            block_len,
            declared_hz,
            pending: Vec::new(),
            pending_offset: 0,
            tracker: IndexTracker::new(),
            header: None,
            started: false,
            anchor: None,
        })
    }

    /// The rate in force: the declared one, else the start header's.
    pub fn rate_hz(&self) -> Option<u32> {
        self.declared_hz.or_else(|| self.header.as_ref().map(|h| h.sample_rate_hz))
    }

    /// The verified start header, once one has arrived.
    #[cfg(test)]
    pub fn header(&self) -> Option<&StartHeader> {
        self.header.as_ref()
    }

    /// Takes one read's bytes and returns what its completed records meant.
    /// Whatever is left over waits for the next read.
    pub fn push(&mut self, bytes: &[u8], arrival_utc_ms: u64) -> Vec<AnalogEvent> {
        let mut held = std::mem::take(&mut self.pending);
        held.extend_from_slice(bytes);
        let mut events = Vec::new();
        let mut pos = 0usize;

        if !self.started {
            // Four bytes decide whether a start header leads the stream.
            if held.len() < START_MAGIC.len() {
                self.pending = held;
                return events;
            }
            self.started = true;
            if held[..START_MAGIC.len()] == START_MAGIC {
                if held.len() < START_HEADER_BYTES {
                    // Wait for the rest of the header, and decide again then.
                    self.started = false;
                    self.pending = held;
                    return events;
                }
                match decode_start_header(&held[..START_HEADER_BYTES]) {
                    Ok(header) if header.channel_count == self.channels => {
                        if let Some(declared) = self.declared_hz {
                            if declared != header.sample_rate_hz {
                                events.push(AnalogEvent::RateMismatch {
                                    declared_hz: declared,
                                    header_hz: header.sample_rate_hz,
                                });
                            }
                        }
                        self.header = Some(header.clone());
                        events.push(AnalogEvent::Header(header));
                        pos = START_HEADER_BYTES;
                    }
                    _ => {
                        // Skip to the next block magic, as `verify_blocks` does.
                        let next = find_block_magic(&held, 1);
                        let skip = next.unwrap_or(held.len());
                        events.push(AnalogEvent::Refused {
                            raw_offset: self.pending_offset,
                            skipped_bytes: skip,
                            why: Refusal::Header,
                        });
                        pos = skip;
                    }
                }
            }
        }

        while pos < held.len() {
            let rest = &held[pos..];
            if !rest.starts_with(&BLOCK_MAGIC) {
                // Not a block: skip to the next magic. Keep a tail that could
                // be the start of one, so a magic split across reads is found.
                let skip = match find_block_magic(&held, pos) {
                    Some(at) => at - pos,
                    None => {
                        let keep = rest.len().min(BLOCK_MAGIC.len() - 1);
                        let tail = &rest[rest.len() - keep..];
                        let keep = if BLOCK_MAGIC.starts_with(tail) { keep } else { 0 };
                        rest.len() - keep
                    }
                };
                if skip == 0 {
                    break;
                }
                events.push(AnalogEvent::Refused {
                    raw_offset: self.pending_offset + pos as u64,
                    skipped_bytes: skip,
                    why: Refusal::LeadingBytes,
                });
                pos += skip;
                continue;
            }
            if rest.len() < self.block_len {
                break;
            }
            let raw_offset = self.pending_offset + pos as u64;
            let span = &rest[..self.block_len];
            match decode_block(span, self.channels, self.frames) {
                Ok(block) => {
                    let decoded = self.place(&block, raw_offset, arrival_utc_ms, &mut events);
                    events.push(AnalogEvent::Block(decoded));
                    pos += self.block_len;
                }
                Err(_) => {
                    // One damaged block costs one block: resume at the next
                    // magic inside the span, or after it.
                    let next = find_block_magic(&held, pos + 1);
                    let end = next.unwrap_or(pos + self.block_len).min(pos + self.block_len);
                    events.push(AnalogEvent::Refused {
                        raw_offset,
                        skipped_bytes: end - pos,
                        why: Refusal::BlockCrc,
                    });
                    pos = end;
                }
            }
        }

        held.drain(..pos);
        self.pending = held;
        self.pending_offset += pos as u64;
        events
    }

    /// Closes the stream: bytes still held are a record that never finished.
    pub fn finish(&mut self) -> Vec<AnalogEvent> {
        let mut events = Vec::new();
        if !self.pending.is_empty() {
            events.push(AnalogEvent::Refused {
                raw_offset: self.pending_offset,
                skipped_bytes: self.pending.len(),
                why: Refusal::PartialAtEnd,
            });
            self.pending_offset += self.pending.len() as u64;
            self.pending.clear();
        }
        events
    }

    /// Places a verified block on the index axis: the gap before it, and its
    /// own time and marker.
    fn place(
        &mut self,
        block: &AnalogBlock<'_>,
        raw_offset: u64,
        arrival_utc_ms: u64,
        events: &mut Vec<AnalogEvent>,
    ) -> DecodedBlock {
        match self.tracker.observe(block.index, self.frames) {
            IndexStep::InOrder => {}
            IndexStep::Gap { expected, got, missing_samples } => {
                events.push(AnalogEvent::Gap { expected, got, missing_samples, raw_offset })
            }
            IndexStep::Backwards { expected, got } => {
                events.push(AnalogEvent::Backwards { expected, got, raw_offset })
            }
        }
        // The first block anchors the axis: sample `index` happened at
        // `arrival`. Every later block is placed from it by index, so a gap
        // keeps its length instead of being squeezed out by arrival times.
        let (anchor_index, anchor_ms) = *self.anchor.get_or_insert((block.index, arrival_utc_ms));
        let first_frame_utc_ms = self.rate_hz().filter(|hz| *hz > 0).map(|hz| {
            let ahead = i64::from(block.index) - i64::from(anchor_index);
            let ms = i64::try_from(anchor_ms).unwrap_or(i64::MAX).saturating_add(ahead * 1000 / i64::from(hz));
            u64::try_from(ms).unwrap_or(0)
        });
        let edge_count = block.events.edge_count();
        let (edge, edge_out_of_block) = match (block.events.edge_offset(), block.events.edge_level()) {
            (Some(offset), Some(level_high)) if u16::from(offset) < self.frames => (
                Some(MarkerEdge { sample_index: block.index.wrapping_add(u32::from(offset)), level_high }),
                false,
            ),
            (Some(_), _) => (None, true),
            _ => (None, false),
        };
        DecodedBlock {
            index: block.index,
            raw_offset,
            channels: self.channels,
            codes: block.codes().collect(),
            edge,
            edge_count,
            edge_out_of_block,
            first_frame_utc_ms,
        }
    }
}

fn find_block_magic(bytes: &[u8], from: usize) -> Option<usize> {
    if from >= bytes.len() {
        return None;
    }
    bytes[from..].windows(BLOCK_MAGIC.len()).position(|w| w == BLOCK_MAGIC).map(|p| from + p)
}

/// Header of a tap's `events.csv`: one row per gap, refusal, marker edge and
/// rate mismatch, placed by sample index (`raw_offset` is the byte offset in
/// the raw file).
pub const EVENTS_HEADER: &str = "kind,sample_index,value,raw_offset,core_rx_utc_ms";

/// The `events.csv` rows an event produces, `\n`-terminated. A verified block
/// with no marker edge produces none: its samples are in the raw file.
pub fn event_rows(event: &AnalogEvent, core_rx_utc_ms: u64) -> Vec<String> {
    let row = |kind: &str, sample_index: Option<u32>, value: String, raw_offset: Option<u64>| {
        format!(
            "{kind},{},{value},{},{core_rx_utc_ms}\n",
            sample_index.map(|i| i.to_string()).unwrap_or_default(),
            raw_offset.map(|o| o.to_string()).unwrap_or_default(),
        )
    };
    match event {
        AnalogEvent::Header(h) => vec![row("start", None, h.sample_rate_hz.to_string(), Some(0))],
        AnalogEvent::Block(b) => {
            let mut rows = Vec::new();
            if let Some(edge) = b.edge {
                rows.push(row(
                    "marker",
                    Some(edge.sample_index),
                    u8::from(edge.level_high).to_string(),
                    Some(b.raw_offset),
                ));
            }
            if b.edge_out_of_block {
                rows.push(row("marker_out_of_block", Some(b.index), b.edge_count.to_string(), Some(b.raw_offset)));
            }
            // Only the first edge has an offset on the wire; say how many more
            // this block holds instead of inventing places for them.
            let unplaced = b.edge_count.saturating_sub(u8::from(b.edge.is_some()));
            if unplaced > 0 && !b.edge_out_of_block {
                rows.push(row("marker_unplaced", Some(b.index), unplaced.to_string(), Some(b.raw_offset)));
            }
            rows
        }
        AnalogEvent::Gap { expected, got: _, missing_samples, raw_offset } => {
            vec![row("gap", Some(*expected), missing_samples.to_string(), Some(*raw_offset))]
        }
        AnalogEvent::Backwards { expected, got, raw_offset } => {
            vec![row("backwards", Some(*got), expected.to_string(), Some(*raw_offset))]
        }
        AnalogEvent::Refused { raw_offset, skipped_bytes, why } => {
            vec![row(why.token(), None, skipped_bytes.to_string(), Some(*raw_offset))]
        }
        AnalogEvent::RateMismatch { declared_hz, header_hz } => {
            vec![row("rate_mismatch", None, format!("{declared_hz}/{header_hz}"), None)]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embarch_study_designer::analog::{encode_block, encode_start_header, AnalogReference, EventWord};

    pub const FRAMES: u16 = 256;

    pub fn block_bytes(index: u32, events: EventWord, seed: i16) -> Vec<u8> {
        let codes: Vec<i16> = (0..FRAMES).map(|i| seed.wrapping_add(i as i16)).collect();
        let mut out = vec![0u8; 16 + 2 * usize::from(FRAMES)];
        let n = encode_block(&mut out, index, events, &codes, 1, FRAMES).unwrap();
        out.truncate(n);
        out
    }

    pub fn header_bytes(rate: u32) -> Vec<u8> {
        let header = StartHeader {
            sample_rate_hz: rate,
            resolution_bits: 12,
            gain_numerator: 1,
            gain_denominator: 4,
            reference: AnalogReference::VddDiv4,
            channel_count: 1,
            vdd_mv: 3300,
            input_name: heapless::String::try_from("A0").unwrap(),
            board: heapless::String::try_from("dk").unwrap(),
            image_version: heapless::String::try_from("1").unwrap(),
        };
        let mut out = vec![0u8; START_HEADER_BYTES];
        encode_start_header(&mut out, &header).unwrap();
        out
    }

    fn blocks_of(events: &[AnalogEvent]) -> Vec<&DecodedBlock> {
        events.iter().filter_map(|e| if let AnalogEvent::Block(b) = e { Some(b) } else { None }).collect()
    }

    #[test]
    fn a_five_minute_32_ksps_stream_has_no_lost_block() {
        // 37 500 blocks of 256 frames is 300 s at 32 ksps; fed in reads of
        // 512 bytes, so no block lands whole in one read.
        let mut stream = header_bytes(32_000);
        let total_blocks = 300 * 32_000 / u32::from(FRAMES);
        assert_eq!(total_blocks, 37_500);
        for n in 0..total_blocks {
            // 0x0A bytes everywhere: the failure line splitting had.
            stream.extend(block_bytes(n * u32::from(FRAMES), EventWord::from_raw(0), 0x0A0A));
        }
        let mut dec = AnalogTapDecoder::new(1, FRAMES, Some(32_000)).unwrap();
        let (mut blocks, mut other, mut samples) = (0u32, 0u32, 0u64);
        for read in stream.chunks(512) {
            for e in dec.push(read, 1_000) {
                match e {
                    AnalogEvent::Block(b) => {
                        blocks += 1;
                        samples += b.codes.len() as u64;
                    }
                    AnalogEvent::Header(_) => {}
                    _ => other += 1,
                }
            }
        }
        assert!(dec.finish().is_empty());
        assert_eq!((blocks, other), (37_500, 0));
        assert_eq!(samples, 300 * 32_000);
        assert_eq!(dec.header().unwrap().sample_rate_hz, 32_000);
    }

    #[test]
    fn a_dropped_block_is_a_gap_placed_by_index() {
        let mut stream = Vec::new();
        for n in [0u32, 1, 3, 4] {
            stream.extend(block_bytes(n * u32::from(FRAMES), EventWord::from_raw(0), 0));
        }
        let mut dec = AnalogTapDecoder::new(1, FRAMES, Some(32_000)).unwrap();
        let mut events = Vec::new();
        for read in stream.chunks(300) {
            events.extend(dec.push(read, 5_000));
        }
        let gaps: Vec<_> = events.iter().filter(|e| matches!(e, AnalogEvent::Gap { .. })).collect();
        assert_eq!(gaps.len(), 1);
        assert_eq!(
            *gaps[0],
            AnalogEvent::Gap { expected: 512, got: 768, missing_samples: 256, raw_offset: 2 * 528 }
        );
        assert_eq!(blocks_of(&events).len(), 4);
        // Block 3 sits 768 samples (24 ms) after block 0 whatever its arrival.
        let times: Vec<_> = blocks_of(&events).iter().map(|b| b.first_frame_utc_ms.unwrap()).collect();
        assert_eq!(times, vec![5_000, 5_008, 5_024, 5_032]);
    }

    #[test]
    fn a_corrupted_crc_is_a_refusal_and_the_gap_it_leaves() {
        let mut stream = Vec::new();
        for n in 0u32..4 {
            let mut b = block_bytes(n * u32::from(FRAMES), EventWord::from_raw(0), 7);
            if n == 1 {
                b[100] ^= 0xFF;
            }
            stream.extend(b);
        }
        let mut dec = AnalogTapDecoder::new(1, FRAMES, None).unwrap();
        let mut events = Vec::new();
        for read in stream.chunks(512) {
            events.extend(dec.push(read, 0));
        }
        events.extend(dec.finish());
        assert_eq!(blocks_of(&events).len(), 3);
        assert!(events.contains(&AnalogEvent::Refused {
            raw_offset: 528,
            skipped_bytes: 528,
            why: Refusal::BlockCrc
        }));
        assert!(events.contains(&AnalogEvent::Gap {
            expected: 256,
            got: 512,
            missing_samples: 256,
            raw_offset: 2 * 528
        }));
    }

    #[test]
    fn a_record_split_across_reads_decodes_whole() {
        let b = block_bytes(0, EventWord::new(1, 9, true), 1);
        let mut dec = AnalogTapDecoder::new(1, FRAMES, None).unwrap();
        // Every split point, including inside the magic and the CRC.
        for cut in 1..b.len() {
            let mut dec = AnalogTapDecoder::new(1, FRAMES, None).unwrap();
            let mut events = dec.push(&b[..cut], 0);
            assert!(blocks_of(&events).is_empty(), "cut {cut}: nothing is complete yet");
            events.extend(dec.push(&b[cut..], 0));
            let got = blocks_of(&events);
            assert_eq!(got.len(), 1, "cut {cut}");
            assert_eq!(got[0].codes.len(), usize::from(FRAMES));
            assert!(dec.finish().is_empty());
        }
        assert_eq!(dec.push(&b, 0).len(), 1);
    }

    #[test]
    fn a_marker_edge_is_recorded_with_its_sample_offset() {
        let mut stream = block_bytes(1_000, EventWord::new(2, 9, true), 0);
        stream.extend(block_bytes(1_256, EventWord::from_raw(0), 0));
        let mut dec = AnalogTapDecoder::new(1, FRAMES, None).unwrap();
        let events = dec.push(&stream, 0);
        let rows: Vec<String> = events.iter().flat_map(|e| event_rows(e, 42)).collect();
        assert_eq!(rows, vec!["marker,1009,1,0,42\n", "marker_unplaced,1000,1,0,42\n"]);
    }

    #[test]
    fn bytes_before_the_first_block_and_a_torn_tail_are_refused() {
        let mut stream = vec![0x0A, 0x0A, 0x01];
        stream.extend(block_bytes(0, EventWord::from_raw(0), 0));
        stream.extend_from_slice(&block_bytes(256, EventWord::from_raw(0), 0)[..100]);
        let mut dec = AnalogTapDecoder::new(1, FRAMES, None).unwrap();
        let mut events = dec.push(&stream, 0);
        events.extend(dec.finish());
        assert_eq!(blocks_of(&events).len(), 1);
        assert!(events.contains(&AnalogEvent::Refused {
            raw_offset: 0,
            skipped_bytes: 3,
            why: Refusal::LeadingBytes
        }));
        assert!(events.contains(&AnalogEvent::Refused {
            raw_offset: 3 + 528,
            skipped_bytes: 100,
            why: Refusal::PartialAtEnd
        }));
    }

    #[test]
    fn the_declared_rate_wins_and_a_mismatch_is_recorded() {
        let mut stream = header_bytes(16_000);
        stream.extend(block_bytes(0, EventWord::from_raw(0), 0));
        let mut dec = AnalogTapDecoder::new(1, FRAMES, Some(32_000)).unwrap();
        let events = dec.push(&stream, 0);
        assert!(events.contains(&AnalogEvent::RateMismatch { declared_hz: 32_000, header_hz: 16_000 }));
        assert_eq!(dec.rate_hz(), Some(32_000));
        // No declared rate: the header's.
        let mut dec = AnalogTapDecoder::new(1, FRAMES, None).unwrap();
        dec.push(&stream, 0);
        assert_eq!(dec.rate_hz(), Some(16_000));
    }

    #[test]
    fn samples_are_stamped_by_index_at_the_declared_rate() {
        let b = block_bytes(0, EventWord::from_raw(0), 0);
        let mut dec = AnalogTapDecoder::new(1, FRAMES, Some(32_000)).unwrap();
        let events = dec.push(&b, 10_000);
        let block = blocks_of(&events)[0].clone();
        let s = block.samples(Some(32_000), 10_000);
        assert_eq!(s.len(), 256);
        assert_eq!(s[0].rx_utc_ms, 10_000);
        assert_eq!(s[255].rx_utc_ms, 10_007);
        assert_eq!(s[3].value, 3.0);
        // No rate: every sample carries the arrival stamp.
        assert!(block.samples(None, 77).iter().all(|x| x.rx_utc_ms == 77));
    }
}
