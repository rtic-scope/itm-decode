//! A decoder for the ITM and DWT packet protocol as specifed in the
//! [ARMv7-M architecture reference manual, Appendix
//! D4](https://developer.arm.com/documentation/ddi0403/ed/). Any
//! references in this code base refers to this document.
//!
//! Common abbreviations:
//!
//! - ITM: instrumentation trace macrocell;
//! - PC: program counter;
//! - DWT: data watchpoint and trace unit;
//! - MSB: most significant bit;
//! - BE: big-endian;

use std::convert::TryInto;
use std::io::Read;

use bitmatch::bitmatch;
use bitvec::prelude::*;
#[cfg(feature = "serde")]
use serde_crate::{Deserialize, Serialize};

/// Re-exports for exception types of the `cortex-m` crate for `serde`
/// purposes.
pub mod cortex_m {
    /// Denotes the exception type (interrupt event) of the processor.
    /// (Table B1-4)
    pub use cortex_m::peripheral::scb::{Exception, VectActive};

    /// Verbatim copy of used `cortex_m` enums for serde functionality.
    /// Should not be used directly. Public because serde requires it.
    /// See <https://serde.rs/remote-derive.html>
    #[cfg(feature = "serde")]
    pub mod serde {
        use super::{Exception, VectActive};
        use serde_crate::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize)]
        #[serde(crate = "serde_crate", remote = "Exception")]
        pub enum ExceptionDef {
            NonMaskableInt,
            HardFault,
            MemoryManagement,
            BusFault,
            UsageFault,
            SecureFault,
            SVCall,
            DebugMonitor,
            PendSV,
            SysTick,
        }

        #[derive(Serialize, Deserialize)]
        #[serde(crate = "serde_crate", remote = "VectActive")]
        pub enum VectActiveDef {
            ThreadMode,
            Exception(#[serde(with = "ExceptionDef")] Exception),
            Interrupt { irqn: u8 },
        }
    }
}

/// The set of valid packet types that can be decoded.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(crate = "serde_crate")
)]
pub enum TracePacket {
    // Synchronization packet category (Appendix D4, p. 782)
    /// A synchronization packet is a unique pattern in the bitstream.
    /// It is identified and used to provide the alignment of other
    /// packet bytes in the bitstream. (Appendix D4.2.1)
    Sync,

    // Protocol packet category (Appendix D4, p. 782)
    /// Found in the bitstream if
    ///
    /// - Software has written to an ITM stimulus port register when the
    /// stimulus port output buffer is full.
    /// - The DWT attempts to generate a hardware source packet when the
    /// DWT output buffer is full.
    /// - The local timestamp counter overflows.
    ///
    /// See (Appendix D4.2.3).
    Overflow,

    /// A delta timestamp that measures the interval since the
    /// generation of the last local timestamp and its relation to the
    /// corresponding ITM/DWT data packets. (Appendix D4.2.4)
    LocalTimestamp1 {
        /// Timestamp value.
        ts: u64,

        /// Indicates the relationship between the generation of `ts`
        /// and the corresponding ITM or DWT data packet.
        data_relation: TimestampDataRelation,
    },

    /// A derivative of `LocalTimestamp1` for timestamp values between
    /// 1-6. Always synchronous to te associated ITM/DWT data. (Appendix D4.2.4)
    LocalTimestamp2 {
        /// Timestamp value.
        ts: u8,
    },

    /// An absolute timestamp based on the global timestamp clock that
    /// contain the timestamp's lower-order bits. (Appendix D4.2.5)
    GlobalTimestamp1 {
        /// Lower-order bits of the timestamp; bits\[25:0\].
        ts: u64,

        /// Set if higher order bits output by the last GTS2 have
        /// changed.
        wrap: bool,

        /// Set if the system has asserted a clock change input to the
        /// processor since the last generated global timestamp.
        clkch: bool,
    },

    /// An absolute timestamp based on the global timestamp clock that
    /// contain the timestamp's higher-order bits. (Appendix D4.2.5)
    GlobalTimestamp2 {
        /// Higher-order bits of the timestamp value; bits\[63:26\] or
        /// bits\[47:26\] depending on implementation.
        ts: u64,
    },

    /// A packet that provides additional information about the
    /// identified source (one of two possible, theoretically). On
    /// ARMv7-M this packet is only used to denote on which ITM stimulus
    /// port a payload was written. (Appendix D4.2.6)
    Extension {
        /// Source port page number.
        page: u8,
    },

    // Source packet category
    /// Contains the payload written to the ITM stimulus ports.
    Instrumentation {
        /// Stimulus port number.
        port: u8,

        /// Instrumentation data written to the stimulus port. MSB, BE.
        payload: Vec<u8>,
    },

    /// One or more event counters have wrapped. (Appendix D4.3.1)
    EventCounterWrap {
        /// POSTCNT wrap (see Appendix C1, p. 732).
        cyc: bool,
        /// FOLDCNT wrap (see Appendix C1, p. 734).
        fold: bool,
        /// LSUCNT wrap (see Appendix C1, p. 734).
        lsu: bool,
        /// SLEEPCNT wrap (see Appendix C1, p. 734).
        sleep: bool,
        /// EXCCNT wrap (see Appendix C1, p. 734).
        exc: bool,
        /// CPICNT wrap (see Appendix C1, p. 734).
        cpi: bool,
    },

    /// The processor has entered, exit, or returned to an exception.
    /// (Appendix D4.3.2)
    ExceptionTrace {
        #[cfg_attr(feature = "serde", serde(with = "cortex_m::serde::VectActiveDef"))]
        exception: cortex_m::VectActive,
        action: ExceptionAction,
    },

    /// Periodic PC sample. (Appendix D4.3.3)
    PCSample {
        /// The value of the PC. `None` if periodic PC sleep packet.
        pc: Option<u32>,
    },

    /// A DWT comparator matched a PC value. (Appendix D4.3.4)
    DataTracePC {
        /// The comparator number that generated the data.
        comparator: u8,

        /// The PC value for the instruction that caused the successful
        /// address comparison.
        pc: u32,
    },

    /// A DWT comparator matched an address. (Appendix D4.3.4)
    DataTraceAddress {
        /// The comparator number that generated the data.
        comparator: u8,

        /// Data address content; bits\[15:0\]. MSB, BE.
        data: Vec<u8>,
    },

    /// A data trace packet with a value. (Appendix D4.3.4)
    DataTraceValue {
        /// The comparator number that generated the data.
        comparator: u8,

        /// Whether the data was read or written.
        access_type: MemoryAccessType,

        /// The data value. MSB, BE.
        value: Vec<u8>,
    },
}

/// Denotes the action taken by the processor by a given exception. (Table D4-6)
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(crate = "serde_crate")
)]
pub enum ExceptionAction {
    /// Exception was entered.
    Entered,

    /// Exception was exited.
    Exited,

    /// Exception was returned to.
    Returned,
}

/// Denotes the type of memory access.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(crate = "serde_crate")
)]
pub enum MemoryAccessType {
    /// Memory was read.
    Read,

    /// Memory was written.
    Write,
}

/// Indicates the relationship between the generation of the local
/// timestamp packet and the corresponding ITM or DWT data packet.
/// (Appendix D4.2.4)
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(crate = "serde_crate")
)]
pub enum TimestampDataRelation {
    /// The local timestamp value is synchronous to the corresponding
    /// ITM or DWT data. The value in the TS field is the timestamp
    /// counter value when the ITM or DWT packet is generated.
    Sync,

    /// The local timestamp value is delayed relative to the ITM or DWT
    /// data. The value in the TS field is the timestamp counter value
    /// when the Local timestamp packet is generated.
    ///
    /// Note: the local timestamp value corresponding to the previous
    /// ITM or DWT packet is unknown, but must be between the previous
    /// and the current local timestamp values.
    UnknownDelay,

    /// Output of the ITM or DWT packet corresponding to this Local
    /// timestamp packet is delayed relative to the associated event.
    /// The value in the TS field is the timestamp counter value when
    /// the ITM or DWT packets is generated.
    ///
    /// This encoding indicates that the ITM or DWT packet was delayed
    /// relative to other trace output packets.
    AssocEventDelay,

    /// Output of the ITM or DWT packet corresponding to this Local
    /// timestamp packet is delayed relative to the associated event,
    /// and this Local timestamp packet is delayed relative to the ITM
    /// or DWT data. This is a combined condition of `UnknownDelay` and
    /// `AssocEventDelay`.
    UnknownAssocEventDelay,
}

/// A header or payload byte failed to be decoded.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(crate = "serde_crate")
)]
pub enum MalformedPacket {
    /// Header is invalid and cannot be decoded.
    #[error("Header is invalid and cannot be decoded: {}", format!("{:#b}", .0))]
    InvalidHeader(u8),

    /// The type discriminator ID in the hardware source packet header
    /// is invalid or the associated payload is of wrong size.
    #[error("Hardware source packet type discriminator ID ({disc_id}) or payload length ({}) is invalid", .payload.len())]
    InvalidHardwarePacket {
        /// The discriminator ID. Potentially invalid.
        disc_id: u8,

        /// Associated payload. Potentially invalid length. MSB, BE.
        payload: Vec<u8>,
    },

    /// The type discriminator ID in the hardware source packet header
    /// is invalid.
    #[error("Hardware source packet discriminator ID is invalid: {disc_id}")]
    InvalidHardwareDisc {
        /// The discriminator ID. Potentially invalid.
        disc_id: u8,

        /// Associated payload length.
        size: usize,
    },

    /// An exception trace packet refers to an invalid action or an
    /// invalid exception number.
    #[error("IRQ number {exception} and/or action {function} is invalid")]
    InvalidExceptionTrace {
        /// The exception number.
        exception: u16,

        /// Numerical representation of the function associated with the
        /// exception number.
        function: u8,
    },

    /// The payload length of a PCSample packet is invalid.
    #[error("Payload length of PC sample is invalid: {}", .payload.len())]
    InvalidPCSampleSize {
        /// The payload constituting the PC value, of invalid size. MSB, BE.
        payload: Vec<u8>,
    },

    /// The GlobalTimestamp2 packet does not contain a 48-bit or 64-bit
    /// timestamp.
    #[error("GlobalTimestamp2 packet does not contain a 48-bit or 64-bit timestamp")]
    InvalidGTS2Size {
        /// The payload constituting the timestamp, of invalid size. MSB, BE.
        payload: Vec<u8>,
    },

    /// The number of zeroes in the Synchronization packet is less than
    /// 47.
    #[error(
        "The number of zeroes in the Synchronization packet is less than expected: {0} < {}",
        SYNC_MIN_ZEROS
    )]
    InvalidSync(usize),

    /// A source packet (from software or hardware) contains an invalid
    /// expected payload size.
    #[error(
        "A source packet (from software or hardware) contains an invalid expected payload size"
    )]
    InvalidSourcePayload {
        /// The header which contains the invalid payload size.
        header: u8,

        /// The invalid payload size. See (Appendix D4.2.8, Table D4-4).
        size: u8,
    },
}

const SYNC_MIN_ZEROS: usize = 47;

/// The decoder's possible states. The default decoder state is `Header`
/// and will always return there after a maximum of two steps. (E.g. if
/// the current state is `Syncing` or `HardwareSource`, the next state
/// is `Header` again.)
enum PacketStub {
    /// Next zero bits will be assumed to be part of a a Synchronization
    /// packet until a set bit is encountered.
    Sync(usize),

    /// Next bytes will be assumed to be part of an Instrumentation
    /// packet, until `payload` contains `expected_size` bytes.
    Instrumentation { port: u8, expected_size: usize },

    /// Next bytes will be assumed to be part of a Hardware source
    /// packet, until `payload` contains `expected_size` bytes.
    HardwareSource { disc_id: u8, expected_size: usize },

    /// Next bytes will be assumed to be part of a LocalTimestamp{1,2}
    /// packet, until the MSB is set.
    LocalTimestamp {
        data_relation: TimestampDataRelation,
    },

    /// Next bytes will be assumed to be part of a GlobalTimestamp1
    /// packet, until the MSB is set.
    GlobalTimestamp1,

    /// Next bytes will be assumed to be part of a GlobalTimestamp2
    /// packet, until the MSB is set.
    GlobalTimestamp2,
}

/// Combined timestamp generated from local and global timestamp
/// packets. Field values relate to the target's global timestamp clock.
/// See (Appendix C1, page 713).
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(crate = "serde_crate")
)]
pub struct Timestamp {
    /// A base timestamp upon which to apply the delta. `Some(base)` if
    /// both a GTS1 and GTS2 packets where received.
    pub base: Option<usize>,

    /// A monotonically increasing local timestamp counter which apply
    /// on the base timestamp. The value is the sum of all local
    /// timestamps since the last global timestamp. `Some(delta)` if at
    /// least one LTS1/LTS2 where received; or, if global timestamps are
    /// enabled, if at least one LTS1/LTS2 where received since the last
    /// global timestamp.
    ///
    /// Will be `None` if [DecoderOptions::only_gts] is set.
    pub delta: Option<usize>,

    /// In what manner this timestamp relate to the associated data
    /// packets, if known.
    pub data_relation: Option<TimestampDataRelation>,

    /// An overflow packet was recieved, which may have been caused by a
    /// local timestamp counter overflow. See (Appendix D4.2.3). The
    /// timestamp in this structure is now potentially diverged from the
    /// true timestamp by the maximum value of the local timestamp
    /// counter (implementation defined), and will be considered such
    /// until the next global timestamp.
    pub diverged: bool,
}

impl Default for Timestamp {
    fn default() -> Self {
        Timestamp {
            base: None,
            delta: None,
            data_relation: None,
            diverged: false,
        }
    }
}

/// A context in which to record the current timestamp between calls to [Decoder::pull_with_timestamp].
struct TimestampedContext {
    /// Data packets associated with [TimestampedContext::ts] in this structure.
    pub packets: Vec<TracePacket>,

    /// Malformed packets associated with [TimestampedContext::ts] in this structure.
    pub malformed_packets: Vec<MalformedPacket>,

    /// The potentially received [TracePacket::GlobalTimestamp1] packet.
    /// Used in combination with [TimestampedContext::gts2] to update
    /// [Timestamp::base].
    pub gts1: Option<usize>,

    /// The potentially received [TracePacket::GlobalTimestamp2] packet.
    /// Used in combination with [TimestampedContext::gts1] to update
    /// [Timestamp::base].
    pub gts2: Option<usize>,

    /// The current timestamp.
    pub ts: Timestamp,

    /// Number of ITM packets consumed thus far.
    pub packets_consumed: usize,
}

impl Default for TimestampedContext {
    fn default() -> Self {
        TimestampedContext {
            packets: vec![],
            malformed_packets: vec![],
            gts1: None,
            gts2: None,
            ts: Timestamp::default(),
            packets_consumed: 0,
        }
    }
}

/// Association between a set of [TracePacket]s and their Timestamp.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(crate = "serde_crate")
)]
pub struct TimestampedTracePackets {
    ///  Timestamp of [packets] and [malformed_packets].
    pub timestamp: Timestamp,
    pub packets: Vec<TracePacket>,
    pub malformed_packets: Vec<MalformedPacket>,

    /// Number of ITM packets consumed to create this structure.
    pub packets_consumed: usize,
}

enum HeaderVariant {
    Packet(TracePacket),
    Stub(PacketStub),
}

pub struct DecoderOptions {
    /// Whether to only process global timestamps in the bitstream on
    /// [Decoder::pull_with_timestamps].
    pub only_gts: bool,

    /// Whether to keep reading after a (temporary) EOF condition.
    pub keep_reading: bool,
}

impl Default for DecoderOptions {
    fn default() -> Self {
        Self {
            only_gts: false,
            keep_reading: true,
        }
    }
}

/// ITM/DWT packet protocol decoder.
pub struct Decoder<R>
where
    R: Read,
{
    /// Decoder options.
    options: DecoderOptions,

    /// Intermediate buffer to store the trace byte stream read from [reader].
    buffer: BitVec,

    /// Source from which to read the trace byte stream.
    reader: R,

    /// Whether the decoder is in a state of synchronization.
    sync: Option<usize>,

    /// Timestamp context. Used exclusively in
    /// [Decoder::pull_with_timestamp] for bookkeeping purposes.
    ts_ctx: TimestampedContext,
}

impl<R> Decoder<R>
where
    R: Read,
{
    pub fn new(reader: R, options: DecoderOptions) -> Decoder<R> {
        Decoder {
            options,
            buffer: BitVec::new(),
            reader,
            sync: None,
            ts_ctx: TimestampedContext::default(),
        }
    }

    /// Gets a reference to the underlying reader.
    pub fn get_ref(&self) -> &R {
        &self.reader
    }

    /// Gets a mutable reference to the underlying reader.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.reader
    }

    /// Push trace data into the decoder.
    pub fn push(&mut self, data: &[u8]) {
        // To optimize the performance in pull, we must reverse the
        // input bitstream and prepend it. This is a costly operation,
        // but is better done here than elsewhere.
        let mut bv = BitVec::<LocalBits, _>::from_vec(data.to_vec());
        bv.reverse();
        bv.append(&mut self.buffer);
        self.buffer.append(&mut bv);
    }

    /// Reads a byte from [Self::reader] into the buffer
    fn buffer_byte(&mut self) -> std::io::Result<usize> {
        todo!();
    }

    /// Decode the next [TracePacket].
    pub fn next(&mut self) -> Result<Option<TracePacket>, MalformedPacket> {
        if self.sync.is_some() {
            return self.handle_sync();
        }
        assert!(self.sync.is_none());

        if self.buffer.len() < 8 {
            // TODO read from reader until we have at least one byte?

            // No header to decode, nothing to do
            // TODO return any transient bytes as an error (if keep_reading == false)
            return Ok(None);
        }

        self.ts_ctx.packets_consumed += 1;
        match decode_header(self.pull_byte())? {
            HeaderVariant::Packet(p) => Ok(Some(p)),
            HeaderVariant::Stub(s) => self.process_stub(&s),
        }
    }

    /// Read zeros from the bitstream until the first bit is set. This
    /// realigns the incoming bitstream for further processing, which
    /// may not be 8-bit aligned.
    fn handle_sync(&mut self) -> Result<Option<TracePacket>, MalformedPacket> {
        if let Some(mut count) = self.sync {
            while let Some(bit) = self.buffer.pop() {
                if !bit && count < SYNC_MIN_ZEROS {
                    count += 1;
                    continue;
                } else if bit && count >= SYNC_MIN_ZEROS {
                    self.sync = None;
                    return Ok(Some(TracePacket::Sync));
                } else {
                    self.sync = None;
                    return Err(MalformedPacket::InvalidSync(count));
                }
            }
        }

        // Ok(None)
        unreachable!();
    }

    /// Pull the next set of ITM data packets (not timestamps) from the
    /// decoder and associates a [Timestamp]. **Assumes that local
    /// timestamps will be found in the bitstream.**
    ///
    /// According to (Appendix C1.7.1, page 710-711), a local timestamp
    /// relating to a single, or to a stream of back-to-back packets, is
    /// generated and sent after the data packets in question in the
    /// bitstream.
    ///
    /// This function thus [Decoder::pull]s packets until a local
    /// timestamp is read (by default), and opportunely calculates an
    /// associated [Timestamp]: local timestamps monotonically increase
    /// an internal delta counter; upon a global timestamps the base is
    /// updated, and the delta is reset.
    pub fn pull_with_timestamp(&mut self) -> Option<TimestampedTracePackets> {
        // Common functionality for LTS{1,2}
        fn assoc_packets_with_lts(
            packets: Vec<TracePacket>,
            malformed_packets: Vec<MalformedPacket>,
            ts: &mut Timestamp,
            lts: usize,
            data_relation: TimestampDataRelation,
            packets_consumed: &mut usize,
        ) -> TimestampedTracePackets {
            if let Some(ref mut delta) = ts.delta {
                *delta += lts as usize;
            } else {
                ts.delta = Some(lts);
            }
            ts.data_relation = Some(data_relation);
            let ttp = TimestampedTracePackets {
                timestamp: ts.clone(),
                packets,
                malformed_packets,
                packets_consumed: *packets_consumed,
            };
            *packets_consumed = 0;
            ttp
        }

        loop {
            match self.next() {
                // No packets remaining
                Ok(None) => return None,

                // A local timestamp: packets received after the last
                // local timestamp (all self.ts_ctx.packets) relate to
                // this local timestamp. Return the packets and
                // timestamp.
                Ok(Some(TracePacket::LocalTimestamp1 { ts, data_relation }))
                    if !self.options.only_gts =>
                {
                    return Some(assoc_packets_with_lts(
                        self.ts_ctx.packets.drain(..).collect(),
                        self.ts_ctx.malformed_packets.drain(..).collect(),
                        &mut self.ts_ctx.ts,
                        ts as usize,
                        data_relation,
                        &mut self.ts_ctx.packets_consumed,
                    ));
                }
                Ok(Some(TracePacket::LocalTimestamp2 { ts })) if !self.options.only_gts => {
                    return Some(assoc_packets_with_lts(
                        self.ts_ctx.packets.drain(..).collect(),
                        self.ts_ctx.malformed_packets.drain(..).collect(),
                        &mut self.ts_ctx.ts,
                        ts as usize,
                        TimestampDataRelation::Sync,
                        &mut self.ts_ctx.packets_consumed,
                    ));
                }

                // A global timestamp: store until we have both the
                // upper (GTS2) and lower bits (GTS1).
                Ok(Some(TracePacket::GlobalTimestamp1 { ts, wrap, clkch })) => {
                    self.ts_ctx.gts1 = Some(ts as usize);
                    if wrap {
                        // upper bits have changed; GTS2 incoming
                        self.ts_ctx.gts2 = None;
                    }
                    if clkch {
                        // changed input clock to ITM; full GTS incoming
                        self.ts_ctx.gts1 = None;
                        self.ts_ctx.gts2 = None;
                    }
                }
                Ok(Some(TracePacket::GlobalTimestamp2 { ts })) => {
                    self.ts_ctx.gts2 = Some(ts as usize)
                }

                // An overflow: the local timestamp may potentially have
                // wrapped around, but this is not necessarily the case.
                // We can in any case no longer generate an accurate
                // Timestamp.
                Ok(Some(TracePacket::Overflow)) => {
                    self.ts_ctx.ts.diverged = true;
                    self.ts_ctx.packets.push(TracePacket::Overflow);
                }

                // A packet that doesn't relate to the timestamp: stash
                // it until the next local timestamp.
                Ok(Some(packet)) if !self.options.only_gts => self.ts_ctx.packets.push(packet),

                Err(malformed) => self.ts_ctx.malformed_packets.push(malformed),

                // As above, but with local timestamps considered data: return the packet directly.
                Ok(Some(packet)) if self.options.only_gts => {
                    return Some(TimestampedTracePackets {
                        timestamp: self.ts_ctx.ts.clone(),
                        packets: vec![packet],
                        malformed_packets: vec![],
                        packets_consumed: 1,
                    });
                }
                _ => unreachable!(),
            }

            // Do we have enough info two calculate a new base for the timestamp?
            if let (Some(lower), Some(upper)) = (self.ts_ctx.gts1, self.ts_ctx.gts2) {
                // XXX Should we move this calc into some Timestamp::from()?
                const GTS2_TS_SHIFT: usize = 26; // see (Appendix D4.2.5).
                self.ts_ctx.ts = Timestamp::default();
                self.ts_ctx.ts.base = Some((upper << GTS2_TS_SHIFT) | lower);
                self.ts_ctx.gts1 = None;
                self.ts_ctx.gts2 = None;
            }
        }
    }

    /// Pulls a single byte from the incoming buffer.
    fn pull_byte(&mut self) -> u8 {
        let mut b: u8 = 0;
        for i in 0..8 {
            b |= (self.buffer.pop().unwrap() as u8) << i;
        }

        b
    }

    /// Pulls `cnt` bytes from the incoming buffer, if `cnt` bytes are
    /// available.
    fn pull_bytes(&mut self, cnt: usize) -> Option<Vec<u8>> {
        if self.buffer.len() < cnt * 8 {
            return None;
        }

        let mut payload = vec![];
        for _ in 0..cnt {
            payload.push(self.pull_byte());
        }
        Some(payload)
    }

    /// Pulls bytes from the incoming buffer until the continuation-bit
    /// is not set. All [PacketStub]s follow follow this payload schema.
    /// (e.g. Appendix D4, Fig. D4-4)
    fn pull_payload(&mut self) -> Option<Vec<u8>> {
        let mut iter = self.buffer.rchunks(8);
        let mut cnt = 0;
        loop {
            cnt += 1;
            match iter.next() {
                None => return None,
                Some(b) if b.len() < 8 => return None,
                Some(b) => match b.first_zero() {
                    // bit 7 is not set: we have reached the end of the
                    // payload
                    //
                    // TODO replace with Option::contains when stable
                    Some(0) => break,
                    _ => continue,
                },
            }
        }

        Some(self.pull_bytes(cnt).unwrap())
    }

    fn process_stub(&mut self, stub: &PacketStub) -> Result<Option<TracePacket>, MalformedPacket> {
        match stub {
            PacketStub::Sync(count) => {
                self.sync = Some(*count);
                self.handle_sync()
            }

            PacketStub::HardwareSource {
                disc_id,
                expected_size,
            } => {
                if let Some(payload) = self.pull_bytes(*expected_size) {
                    handle_hardware_source(*disc_id, payload).map(Some)
                } else {
                    Ok(None)
                }
            }
            PacketStub::LocalTimestamp { data_relation } => {
                if let Some(payload) = self.pull_payload() {
                    Ok(Some(TracePacket::LocalTimestamp1 {
                        data_relation: data_relation.clone(),
                        ts: extract_timestamp(payload, 27),
                    }))
                } else {
                    Ok(None)
                }
            }
            PacketStub::GlobalTimestamp1 => {
                if let Some(payload) = self.pull_payload() {
                    Ok(Some(TracePacket::GlobalTimestamp1 {
                        ts: extract_timestamp(payload.clone(), 25),
                        clkch: (payload.last().unwrap() & (1 << 5)) >> 5 == 1,
                        wrap: (payload.last().unwrap() & (1 << 6)) >> 6 == 1,
                    }))
                } else {
                    Ok(None)
                }
            }
            PacketStub::GlobalTimestamp2 => {
                if let Some(payload) = self.pull_payload() {
                    Ok(Some(TracePacket::GlobalTimestamp2 {
                        ts: extract_timestamp(
                            payload.to_vec(),
                            match payload.len() {
                                4 => 47 - 26, // 48 bit timestamp
                                6 => 63 - 26, // 64 bit timestamp
                                _ => {
                                    return Err(MalformedPacket::InvalidGTS2Size {
                                        payload: payload.to_vec(),
                                    })
                                }
                            },
                        ),
                    }))
                } else {
                    Ok(None)
                }
            }
            PacketStub::Instrumentation {
                port,
                expected_size,
            } => {
                if let Some(payload) = self.pull_bytes(*expected_size) {
                    Ok(Some(TracePacket::Instrumentation {
                        port: *port,
                        payload: payload.to_vec(),
                    }))
                } else {
                    Ok(None)
                }
            }
        }
    }
}

// TODO template this for u32, u64?
fn extract_timestamp(payload: Vec<u8>, max_len: u64) -> u64 {
    // Decode the first N - 1 payload bytes
    let (rtail, head) = payload.split_at(payload.len() - 1);
    let mut ts: u64 = 0;
    for (i, b) in rtail.iter().enumerate() {
        ts |= ((b & !(1 << 7)) as u64) // mask out continuation bit
            << (7 * i);
    }

    // Mask out the timestamp's MSBs and shift them into the final
    // value.
    let shift = 7 - (max_len % 7);
    let mask: u8 = 0xFFu8.wrapping_shl(shift.try_into().unwrap()) >> shift;
    ts | (((head[0] & mask) as u64) << (7 * rtail.len()))
}

/// Decodes the first byte of a packet, the header, into a complete packet or a packet stub.
#[allow(clippy::bad_bit_mask)]
#[bitmatch]
fn decode_header(header: u8) -> Result<HeaderVariant, MalformedPacket> {
    fn translate_ss(ss: u8) -> Option<usize> {
        // See (Appendix D4.2.8, Table D4-4)
        Some(
            match ss {
                0b01 => 2,
                0b10 => 3,
                0b11 => 5,
                _ => return None,
            } - 1, // ss would include the header byte, but it has already been processed
        )
    }

    let stub = |s| Ok(HeaderVariant::Stub(s));
    let packet = |p| Ok(HeaderVariant::Packet(p));

    #[bitmatch]
    match header {
        // Synchronization packet category
        "0000_0000" => stub(PacketStub::Sync(8)),

        // Protocol packet category
        "0111_0000" => packet(TracePacket::Overflow),
        "11rr_0000" => {
            // Local timestamp, format 1 (LTS1)
            let tc = r; // relationship with corresponding data

            stub(PacketStub::LocalTimestamp {
                data_relation: match tc {
                    0b00 => TimestampDataRelation::Sync,
                    0b01 => TimestampDataRelation::UnknownDelay,
                    0b10 => TimestampDataRelation::AssocEventDelay,
                    0b11 => TimestampDataRelation::UnknownAssocEventDelay,
                    _ => unreachable!(),
                },
            })
        }
        "0ttt_0000" => {
            // Local timestamp, format 2 (LTS2)
            packet(TracePacket::LocalTimestamp2 { ts: t })
        }
        "1001_0100" => {
            // Global timestamp, format 1 (GTS1)
            stub(PacketStub::GlobalTimestamp1)
        }
        "1011_0100" => {
            // Global timestamp, format 2(GTS2)
            stub(PacketStub::GlobalTimestamp2)
        }
        "0ppp_1000" => {
            // Extension packet
            packet(TracePacket::Extension { page: p })
        }

        // Source packet category
        "aaaa_a0ss" => {
            // Instrumentation packet
            stub(PacketStub::Instrumentation {
                port: a,
                expected_size: if let Some(s) = translate_ss(s) {
                    s
                } else {
                    return Err(MalformedPacket::InvalidSourcePayload { header, size: s });
                },
            })
        }
        "aaaa_a1ss" => {
            // Hardware source packet
            let disc_id = a;

            if !(0..=2).contains(&disc_id) && !(8..=23).contains(&disc_id) {
                return Err(MalformedPacket::InvalidHardwareDisc {
                    disc_id,
                    size: s.into(),
                });
            }

            stub(PacketStub::HardwareSource {
                disc_id,
                expected_size: if let Some(s) = translate_ss(s) {
                    s
                } else {
                    return Err(MalformedPacket::InvalidSourcePayload { header, size: s });
                },
            })
        }
        "hhhh_hhhh" => Err(MalformedPacket::InvalidHeader(h)),
    }
}

/// Decodes the payload of a hardware source packet.
#[bitmatch]
fn handle_hardware_source(disc_id: u8, payload: Vec<u8>) -> Result<TracePacket, MalformedPacket> {
    match disc_id {
        0 => {
            // event counter wrap

            if payload.len() != 1 {
                return Err(MalformedPacket::InvalidHardwarePacket { disc_id, payload });
            }

            let b = payload[0];
            Ok(TracePacket::EventCounterWrap {
                cyc: b & (1 << 5) != 0,
                fold: b & (1 << 4) != 0,
                lsu: b & (1 << 3) != 0,
                sleep: b & (1 << 2) != 0,
                exc: b & (1 << 1) != 0,
                cpi: b & (1 << 0) != 0,
            })
        }
        1 => {
            // exception trace

            if payload.len() != 2 {
                return Err(MalformedPacket::InvalidHardwarePacket { disc_id, payload });
            }

            let function = (payload[1] >> 4) & 0b11;
            let exception_number = ((payload[1] as u16 & 1) << 8) | payload[0] as u16;
            let exception_number: u8 = if let Ok(nr) = exception_number.try_into() {
                nr
            } else {
                return Err(MalformedPacket::InvalidExceptionTrace {
                    exception: exception_number,
                    function,
                });
            };

            Ok(TracePacket::ExceptionTrace {
                exception: if let Some(exception) = cortex_m::VectActive::from(exception_number) {
                    exception
                } else {
                    return Err(MalformedPacket::InvalidExceptionTrace {
                        exception: exception_number.into(),
                        function,
                    });
                },
                action: match function {
                    0b01 => ExceptionAction::Entered,
                    0b10 => ExceptionAction::Exited,
                    0b11 => ExceptionAction::Returned,
                    _ => {
                        return Err(MalformedPacket::InvalidExceptionTrace {
                            exception: exception_number.into(),
                            function,
                        })
                    }
                },
            })
        }
        2 => {
            // PC sample
            match payload.len() {
                1 if payload[0] == 0 => Ok(TracePacket::PCSample { pc: None }),
                4 => Ok(TracePacket::PCSample {
                    pc: Some(u32::from_le_bytes(payload.try_into().unwrap())),
                }),
                _ => Err(MalformedPacket::InvalidPCSampleSize { payload }),
            }
        }
        8..=23 => {
            // data trace
            #[bitmatch]
            let "???t_tccd" = disc_id; // we have already masked out bit[2:0]
            let comparator = c;

            match (t, d, payload.len()) {
                (0b01, 0, 4) => {
                    // PC value packet
                    Ok(TracePacket::DataTracePC {
                        comparator,
                        pc: u32::from_le_bytes(payload.try_into().unwrap()),
                    })
                }
                (0b01, 1, 2) => {
                    // address packet
                    Ok(TracePacket::DataTraceAddress {
                        comparator,
                        data: payload,
                    })
                }
                (0b10, d, _) => {
                    // data value packet
                    Ok(TracePacket::DataTraceValue {
                        comparator,
                        access_type: if d == 0 {
                            MemoryAccessType::Read
                        } else {
                            MemoryAccessType::Write
                        },
                        value: payload,
                    })
                }
                _ => Err(MalformedPacket::InvalidHardwarePacket { disc_id, payload }),
            }
        }
        _ => unreachable!(), // we already verify the discriminator when we decode the header
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_bytes() {
        let mut decoder = Decoder::new(DecoderOptions::default());
        let payload = vec![0b1000_0000, 0b1010_0000, 0b1000_0100, 0b0110_0000];
        decoder.push(&payload);
        assert_eq!(decoder.pull_bytes(3).unwrap().len(), 3);
    }

    #[test]
    fn pull_payload() {
        let mut decoder = Decoder::new(DecoderOptions::default());
        let payload = vec![0b1000_0000, 0b1010_0000, 0b1000_0100, 0b0110_0000];
        #[rustfmt::skip]
        decoder.push(&payload);
        assert_eq!(decoder.pull_payload(), Some(payload));
    }

    #[test]
    fn extract_timestamp() {
        #[rustfmt::skip]
        let ts: Vec<u8> = [
            0b1000_0000,
            0b1000_0000,
            0b1000_0000,
            0b0000_0000,
        ].to_vec();

        assert_eq!(extract_timestamp(ts, 25), 0);

        #[rustfmt::skip]
        let ts: Vec<u8> = [
            0b1000_0001,
            0b1000_0111,
            0b1001_1111,
            0b0111_1111
        ].to_vec();

        assert_eq!(extract_timestamp(ts, 27), 0b1111111_0011111_0000111_0000001,);

        #[rustfmt::skip]
        let ts: Vec<u8> = [
            0b1000_0001,
            0b1000_0111,
            0b1001_1111,
            0b1111_1111
        ].to_vec();

        assert_eq!(extract_timestamp(ts, 25), 0b11111_0011111_0000111_0000001,);
    }
}
