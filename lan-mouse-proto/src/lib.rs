use hmac::{Hmac, Mac};
use input_event::{Event as InputEvent, KeyboardEvent, PointerEvent};
use num_enum::{IntoPrimitive, TryFromPrimitive, TryFromPrimitiveError};
use paste::paste;
use sha2::Sha256;
use std::{
    fmt::{Debug, Display},
    mem::size_of,
};
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

/// Size of the HMAC-SHA256 authentication tag in bytes.
pub const HMAC_TAG_SIZE: usize = 32;
/// Size of the monotonic counter prefix in bytes (u64 big-endian).
pub const COUNTER_SIZE: usize = size_of::<u64>();

/// defines the maximum size an encoded event can take up
/// this is currently the pointer motion event
/// type: u8, time: u32, dx: f64, dy: f64
pub const MAX_EVENT_SIZE: usize = size_of::<u8>() + size_of::<u32>() + 2 * size_of::<f64>();

/// error type for protocol violations
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// event type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidEventId(#[from] TryFromPrimitiveError<EventType>),
    /// datagram too short or too long for authenticated wire format
    #[error("bad datagram length: {0}")]
    BadLength(usize),
    /// HMAC-SHA256 verification failed (forged, corrupted, or wrong key)
    #[error("authentication failed")]
    AuthenticationFailed,
}

/// main lan-mouse protocol event type
#[derive(Clone, Copy, Debug)]
pub enum ProtoEvent {
    /// notify a client that the cursor entered its region
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Enter(u32),
    /// notify a client that the cursor left its region
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Leave(u32),
    /// acknowledge of an [`ProtoEvent::Enter`] or [`ProtoEvent::Leave`] event
    Ack(u32),
    /// Input event
    Input(InputEvent),
    /// Ping event for tracking unresponsive clients.
    /// A client has to respond with [`ProtoEvent::Pong`].
    Ping,
    /// Response to [`ProtoEvent::Ping`]
    Pong,
}

impl Display for ProtoEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoEvent::Enter(s) => write!(f, "Enter({s})"),
            ProtoEvent::Leave(s) => write!(f, "Leave({s})"),
            ProtoEvent::Ack(s) => write!(f, "Ack({s})"),
            ProtoEvent::Input(e) => write!(f, "{e}"),
            ProtoEvent::Ping => write!(f, "ping"),
            ProtoEvent::Pong => write!(f, "pong"),
        }
    }
}

#[derive(TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum EventType {
    PointerMotion,
    PointerButton,
    PointerAxis,
    PointerAxisValue120,
    KeyboardKey,
    KeyboardModifiers,
    Ping,
    Pong,
    Enter,
    Leave,
    Ack,
}

impl ProtoEvent {
    fn event_type(&self) -> EventType {
        match self {
            ProtoEvent::Input(e) => match e {
                InputEvent::Pointer(p) => match p {
                    PointerEvent::Motion { .. } => EventType::PointerMotion,
                    PointerEvent::Button { .. } => EventType::PointerButton,
                    PointerEvent::Axis { .. } => EventType::PointerAxis,
                    PointerEvent::AxisDiscrete120 { .. } => EventType::PointerAxisValue120,
                },
                InputEvent::Keyboard(k) => match k {
                    KeyboardEvent::Key { .. } => EventType::KeyboardKey,
                    KeyboardEvent::Modifiers { .. } => EventType::KeyboardModifiers,
                },
            },
            ProtoEvent::Ping => EventType::Ping,
            ProtoEvent::Pong => EventType::Pong,
            ProtoEvent::Enter(_) => EventType::Enter,
            ProtoEvent::Leave(_) => EventType::Leave,
            ProtoEvent::Ack(_) => EventType::Ack,
        }
    }
}

impl TryFrom<[u8; MAX_EVENT_SIZE]> for ProtoEvent {
    type Error = ProtocolError;

    fn try_from(buf: [u8; MAX_EVENT_SIZE]) -> Result<Self, Self::Error> {
        let mut buf = &buf[..];
        let event_type = decode_u8(&mut buf)?;
        match EventType::try_from(event_type)? {
            EventType::PointerMotion => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Motion {
                    time: decode_u32(&mut buf)?,
                    dx: decode_f64(&mut buf)?,
                    dy: decode_f64(&mut buf)?,
                })))
            }
            EventType::PointerButton => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Button {
                    time: decode_u32(&mut buf)?,
                    button: decode_u32(&mut buf)?,
                    state: decode_u32(&mut buf)?,
                })))
            }
            EventType::PointerAxis => Ok(Self::Input(InputEvent::Pointer(PointerEvent::Axis {
                time: decode_u32(&mut buf)?,
                axis: decode_u8(&mut buf)?,
                value: decode_f64(&mut buf)?,
            }))),
            EventType::PointerAxisValue120 => Ok(Self::Input(InputEvent::Pointer(
                PointerEvent::AxisDiscrete120 {
                    axis: decode_u8(&mut buf)?,
                    value: decode_i32(&mut buf)?,
                },
            ))),
            EventType::KeyboardKey => Ok(Self::Input(InputEvent::Keyboard(KeyboardEvent::Key {
                time: decode_u32(&mut buf)?,
                key: decode_u32(&mut buf)?,
                state: decode_u8(&mut buf)?,
            }))),
            EventType::KeyboardModifiers => Ok(Self::Input(InputEvent::Keyboard(
                KeyboardEvent::Modifiers {
                    depressed: decode_u32(&mut buf)?,
                    latched: decode_u32(&mut buf)?,
                    locked: decode_u32(&mut buf)?,
                    group: decode_u32(&mut buf)?,
                },
            ))),
            EventType::Ping => Ok(Self::Ping),
            EventType::Pong => Ok(Self::Pong),
            EventType::Enter => Ok(Self::Enter(decode_u32(&mut buf)?)),
            EventType::Leave => Ok(Self::Leave(decode_u32(&mut buf)?)),
            EventType::Ack => Ok(Self::Ack(decode_u32(&mut buf)?)),
        }
    }
}

impl From<ProtoEvent> for ([u8; MAX_EVENT_SIZE], usize) {
    fn from(event: ProtoEvent) -> Self {
        let mut buf = [0u8; MAX_EVENT_SIZE];
        let mut len = 0usize;
        {
            let mut buf = &mut buf[..];
            let buf = &mut buf;
            let len = &mut len;
            encode_u8(buf, len, event.event_type() as u8);
            match event {
                ProtoEvent::Input(event) => match event {
                    InputEvent::Pointer(p) => match p {
                        PointerEvent::Motion { time, dx, dy } => {
                            encode_u32(buf, len, time);
                            encode_f64(buf, len, dx);
                            encode_f64(buf, len, dy);
                        }
                        PointerEvent::Button {
                            time,
                            button,
                            state,
                        } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, button);
                            encode_u32(buf, len, state);
                        }
                        PointerEvent::Axis { time, axis, value } => {
                            encode_u32(buf, len, time);
                            encode_u8(buf, len, axis);
                            encode_f64(buf, len, value);
                        }
                        PointerEvent::AxisDiscrete120 { axis, value } => {
                            encode_u8(buf, len, axis);
                            encode_i32(buf, len, value);
                        }
                    },
                    InputEvent::Keyboard(k) => match k {
                        KeyboardEvent::Key { time, key, state } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, key);
                            encode_u8(buf, len, state);
                        }
                        KeyboardEvent::Modifiers {
                            depressed,
                            latched,
                            locked,
                            group,
                        } => {
                            encode_u32(buf, len, depressed);
                            encode_u32(buf, len, latched);
                            encode_u32(buf, len, locked);
                            encode_u32(buf, len, group);
                        }
                    },
                },
                ProtoEvent::Ping => {}
                ProtoEvent::Pong => {}
                ProtoEvent::Enter(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Leave(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Ack(serial) => encode_u32(buf, len, serial),
            }
        }
        (buf, len)
    }
}

macro_rules! decode_impl {
    ($t:ty) => {
        paste! {
            fn [<decode_ $t>](data: &mut &[u8]) -> Result<$t, ProtocolError> {
                let (int_bytes, rest) = data.split_at(size_of::<$t>());
                *data = rest;
                Ok($t::from_be_bytes(int_bytes.try_into().unwrap()))
            }
        }
    };
}

decode_impl!(u8);
decode_impl!(u32);
decode_impl!(i32);
decode_impl!(f64);

macro_rules! encode_impl {
    ($t:ty) => {
        paste! {
            fn [<encode_ $t>](buf: &mut &mut [u8], amt: &mut usize, n: $t) {
                let src = n.to_be_bytes();
                let data = std::mem::take(buf);
                let (int_bytes, rest) = data.split_at_mut(size_of::<$t>());
                int_bytes.copy_from_slice(&src);
                *amt += size_of::<$t>();
                *buf = rest
            }
        }
    };
}

encode_impl!(u8);
encode_impl!(u32);
encode_impl!(i32);
encode_impl!(f64);

// ---------------------------------------------------------------------------
// Authenticated wire format
// ---------------------------------------------------------------------------
//
// Layout: [ counter: 8 bytes BE ] [ event_bytes: 1..MAX_EVENT_SIZE ] [ HMAC-SHA256 tag: 32 bytes ]
//
// The HMAC is computed over counter_bytes || event_bytes (everything except the
// tag itself).  `event_bytes` is the existing ProtoEvent encoding (variable
// length).

/// Minimum authenticated datagram size: counter + 1 byte event + tag.
pub const MIN_AUTH_DATAGRAM: usize = COUNTER_SIZE + 1 + HMAC_TAG_SIZE;
/// Maximum authenticated datagram size: counter + MAX_EVENT_SIZE + tag.
pub const MAX_AUTH_DATAGRAM: usize = COUNTER_SIZE + MAX_EVENT_SIZE + HMAC_TAG_SIZE;

/// Encode a `ProtoEvent` into the authenticated wire format.
///
/// Returns a buffer and its used length.  The caller should transmit
/// `&buf[..len]`.
pub fn encode_authenticated(
    event: &ProtoEvent,
    key: &[u8],
    counter: u64,
) -> ([u8; MAX_AUTH_DATAGRAM], usize) {
    // Serialize the event via the existing path.
    let (event_buf, event_len): ([u8; MAX_EVENT_SIZE], usize) = (*event).into();

    let mut buf = [0u8; MAX_AUTH_DATAGRAM];
    let counter_bytes = counter.to_be_bytes();

    // 1. Write counter.
    buf[..COUNTER_SIZE].copy_from_slice(&counter_bytes);
    // 2. Write event bytes.
    buf[COUNTER_SIZE..COUNTER_SIZE + event_len].copy_from_slice(&event_buf[..event_len]);

    let payload_end = COUNTER_SIZE + event_len;

    // 3. Compute HMAC-SHA256 over counter || event_bytes.
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(&buf[..payload_end]);
    let tag = mac.finalize().into_bytes();

    // 4. Append tag.
    buf[payload_end..payload_end + HMAC_TAG_SIZE].copy_from_slice(&tag);

    (buf, payload_end + HMAC_TAG_SIZE)
}

/// Decode and authenticate a datagram received from the network.
///
/// `datagram` must be exactly `&recv_buf[..actual_recv_len]` — the ACTUAL
/// number of bytes returned by `recv_from`, NOT a zero-padded fixed buffer.
///
/// On success returns `(counter, ProtoEvent)`.  On failure returns a
/// `ProtocolError` — the caller MUST drop the packet.
pub fn decode_authenticated(
    datagram: &[u8],
    key: &[u8],
) -> Result<(u64, ProtoEvent), ProtocolError> {
    let len = datagram.len();

    // --- length check (closes #9: no more fixed-size zero-padded buffer) ---
    if len < MIN_AUTH_DATAGRAM || len > MAX_AUTH_DATAGRAM {
        return Err(ProtocolError::BadLength(len));
    }

    let tag_start = len - HMAC_TAG_SIZE;
    let payload = &datagram[..tag_start]; // counter || event_bytes
    let tag = &datagram[tag_start..];

    // --- HMAC verification (constant-time, closes #1/#18: forged packets rejected) ---
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(payload);
    mac.verify_slice(tag).map_err(|_| ProtocolError::AuthenticationFailed)?;

    // --- parse counter ---
    let counter = u64::from_be_bytes(
        payload[..COUNTER_SIZE]
            .try_into()
            .expect("COUNTER_SIZE == 8"),
    );

    // --- parse event (only after authentication succeeds) ---
    let event_bytes = &payload[COUNTER_SIZE..];
    // The existing TryFrom<[u8; MAX_EVENT_SIZE]> expects a fixed-size array
    // padded with zeros.  Build one from the variable-length slice.
    let mut event_buf = [0u8; MAX_EVENT_SIZE];
    let copy_len = event_bytes.len().min(MAX_EVENT_SIZE);
    event_buf[..copy_len].copy_from_slice(&event_bytes[..copy_len]);
    let event = ProtoEvent::try_from(event_buf)?;

    Ok((counter, event))
}

// ---------------------------------------------------------------------------
// Anti-replay sliding window (WireGuard / IPsec style)
// ---------------------------------------------------------------------------

/// Sliding-window replay filter for authenticated counters.
///
/// Tracks the highest accepted counter and a 64-bit bitmap of recently seen
/// offsets below it.  This rejects duplicate and out-of-order-beyond-window
/// packets even if they carry a valid HMAC (recorded replay attack).
pub struct ReplayWindow {
    /// Highest counter value accepted so far.  `None` means no packet has been
    /// accepted yet.
    highest: Option<u64>,
    /// Bitmap: bit `i` is set if `(highest - 1 - i)` has been seen,
    /// for `i` in `0..63`.  This covers the 64 counters immediately below
    /// `highest`.
    bitmap: u64,
}

impl ReplayWindow {
    /// Window size (number of counters below `highest` that are tracked).
    const WINDOW_SIZE: u64 = 64;

    pub fn new() -> Self {
        Self {
            highest: None,
            bitmap: 0,
        }
    }

    /// Check whether `counter` is acceptable (not a replay) and, if so,
    /// record it.  Returns `true` if the packet should be accepted.
    pub fn check_and_record(&mut self, counter: u64) -> bool {
        let Some(highest) = self.highest else {
            // First packet ever — accept unconditionally.
            self.highest = Some(counter);
            return true;
        };

        if counter > highest {
            // New high-water mark.  Shift the bitmap to account for the gap.
            let shift = counter - highest;
            if shift < Self::WINDOW_SIZE {
                self.bitmap = (self.bitmap << shift) | (1u64 << (shift - 1));
            } else {
                // Gap exceeds window — everything old falls out.
                self.bitmap = 0;
            }
            self.highest = Some(counter);
            return true;
        }

        if counter == highest {
            // Exact duplicate of the highest — replay.
            return false;
        }

        // counter < highest
        let offset = highest - counter; // >= 1
        if offset > Self::WINDOW_SIZE {
            // Too old — outside the window.
            return false;
        }

        let bit = 1u64 << (offset - 1);
        if self.bitmap & bit != 0 {
            // Already seen — replay.
            return false;
        }

        // Accept and record.
        self.bitmap |= bit;
        true
    }
}
