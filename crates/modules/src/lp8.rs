//! Senseair LP8 CO2 sensor (the CO2 Module): the Modbus-like frames over its UART (9600 baud,
//! 8N2), per "LP8 sensor module specifications and integration guideline".
//!
//! One measurement is one power cycle of the sensor: power on, wait for RDY low, write the
//! calculation control, the sensor state kept from the previous measurement and the host's
//! pressure to RAM 0x80 ([`write_frame`]), read the acknowledgement ([`check_ack`]), wait for RDY
//! high, read 44 bytes from RAM 0x80 ([`READ_FRAME`], [`parse_reply`]), power off. Multi-byte
//! values are big endian; the CRC (Modbus CRC-16) goes low byte first.
//!
//! The sensor keeps nothing between measurements: its noise filters and its baseline live in the
//! 23-byte sensor state the host carries from one reply into the next write
//! ([`sensor_state`]).

use crate::Invalid;
use crate::crc::crc16;

/// "Any sensor".
const DEVICE: u8 = 0xFE;
const WRITE: u8 = 0x41;
const READ: u8 = 0x44;
/// The first RAM address written and read.
const RAM: u8 = 0x80;
/// Bytes written: the calculation control, the sensor state and the host pressure.
const WRITE_DATA: u8 = 1 + STATE_LEN as u8 + 2;
/// Bytes read: RAM 0x80 to 0xAB.
const READ_DATA: u8 = 0x2C;

/// The sensor state carried from each reply into the next write.
pub const STATE_LEN: usize = 23;
/// A write frame: address, function, RAM address (2), length, the data, CRC (2).
pub const WRITE_LEN: usize = 5 + WRITE_DATA as usize + 2;
/// A reply to [`READ_FRAME`]: address, function, length, the data, CRC (2).
pub const REPLY_LEN: usize = 3 + READ_DATA as usize + 2;
/// The only good answer to a write (an error answer is `FE C1 <code> <CRC>`).
pub const WRITE_ACK: [u8; 4] = [DEVICE, WRITE, 0x81, 0xE0];
/// The read sent once RDY is high: 44 bytes from RAM 0x80.
pub const READ_FRAME: [u8; 7] = [DEVICE, READ, 0x00, RAM, READ_DATA, 0x79, 0x39];
/// 1012.4 hPa in the sensor's unit (10 Pa): the pressure it assumes when the host has none.
pub const DEFAULT_PRESSURE: u16 = 10124;
/// The shortest time between two measurements, in seconds: below it the lamp's residual heat
/// spoils the accuracy.
pub const MIN_PERIOD_S: u32 = 16;
/// How often the host asks for an automatic baseline correction, in seconds (8 days).
pub const ABC_PERIOD_S: u32 = 8 * 24 * 3600;

/// What the sensor does in this measurement (the calculation control byte, RAM 0x80).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Calculation {
    /// No sensor state yet: the filters and the baseline history start over.
    Initial = 0x10,
    /// The normal case, with the previous measurement's sensor state.
    Sequential = 0x20,
    /// Automatic baseline correction: the lowest filtered reading since the last one is taken
    /// as fresh air (400 ppm). The host asks for it every [`ABC_PERIOD_S`].
    Abc = 0x70,
}

/// The write that starts a measurement. Without a sensor state the state bytes are zero (they
/// are ignored by an [`Calculation::Initial`] measurement). `pressure` is the ambient pressure in
/// units of 10 Pa ([`DEFAULT_PRESSURE`] when unknown).
pub fn write_frame(
    calculation: Calculation,
    state: Option<&[u8; STATE_LEN]>,
    pressure: u16,
) -> [u8; WRITE_LEN] {
    let mut frame = [0u8; WRITE_LEN];
    frame[..6].copy_from_slice(&[DEVICE, WRITE, 0x00, RAM, WRITE_DATA, calculation as u8]);
    if let Some(state) = state {
        frame[6..6 + STATE_LEN].copy_from_slice(state);
    }
    frame[6 + STATE_LEN..WRITE_LEN - 2].copy_from_slice(&pressure.to_be_bytes());
    let crc = crc16(&frame[..WRITE_LEN - 2]);
    frame[WRITE_LEN - 2..].copy_from_slice(&crc.to_le_bytes());
    frame
}

/// Whether `reply` is the acknowledgement of a write.
pub fn check_ack(reply: &[u8]) -> Result<(), Invalid> {
    match reply.get(..WRITE_ACK.len()) {
        None => Err(Invalid::Short),
        Some(ack) if ack == WRITE_ACK => Ok(()),
        Some(ack) if crc16(ack) != 0 => Err(Invalid::Crc),
        Some(_) => Err(Invalid::Unexpected),
    }
}

/// Bits of error status 0 (RAM 0xA7).
pub mod status0 {
    /// A configuration, memory or capacitor measurement fault: power cycle, initial measurement.
    pub const FATAL: u8 = 0x01;
    /// Algorithm parameters corrupted: power cycle, initial measurement.
    pub const ALGORITHM: u8 = 0x04;
    /// A calibration or baseline correction was out of range (the next one may succeed).
    pub const CALIBRATION: u8 = 0x08;
    /// The hardware self-diagnostics failed.
    pub const SELF_DIAG: u8 = 0x10;
    /// A value in the calculation was out of range; clears itself.
    pub const OUT_OF_RANGE: u8 = 0x20;
    /// A fault of the sensor's non-volatile memory.
    pub const MEMORY: u8 = 0x40;
}

/// Bits of error status 1 (RAM 0xA6).
pub mod status1 {
    /// The capacitor was below 2.8 V before the lamp pulse.
    pub const VCAP1_LOW: u8 = 0x01;
    /// The capacitor dropped below 2.7 V during the lamp pulse.
    pub const VCAP2_LOW: u8 = 0x02;
    /// The ADC was out of range: power cycle, initial measurement.
    pub const ADC: u8 = 0x04;
    /// The detector's signal was invalid (interference, perhaps).
    pub const DETECTOR: u8 = 0x08;
}

/// One measurement, decoded from the reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Measurement {
    /// Unfiltered, without pressure compensation (RAM 0x9A), ppm.
    pub co2: i16,
    /// Unfiltered, pressure compensated (0x9C), ppm.
    pub co2_pressure: i16,
    /// Noise filtered (0xA8), ppm.
    pub filtered: i16,
    /// Noise filtered and pressure compensated (0xAA), ppm: the value the datasheet specifies the
    /// accuracy for.
    pub filtered_pressure: i16,
    /// The sensor's temperature (0x9E), hundredths of a degree Celsius.
    pub temperature: i16,
    /// The capacitor before the lamp pulse (0xA0) and at its lowest during it (0xA2), millivolts.
    pub vcap1: u16,
    pub vcap2: u16,
    /// Error status 0 and 1 (0xA7, 0xA6); see [`status0`], [`status1`].
    pub status0: u8,
    pub status1: u8,
}

impl Measurement {
    /// A fault after which the sensor wants a power cycle and an initial measurement: the sensor
    /// state of this reply must not be written back.
    pub const fn needs_restart(&self) -> bool {
        self.status0 & (status0::FATAL | status0::ALGORITHM | status0::MEMORY | status0::SELF_DIAG) != 0
            || self.status1 & status1::ADC != 0
    }

    /// The capacitor was not charged enough for this measurement: charge longer next time.
    pub const fn vcap_low(&self) -> bool {
        self.status1 & (status1::VCAP1_LOW | status1::VCAP2_LOW) != 0
    }

    /// The CO2 concentration to report, in ppm: the filtered, pressure compensated value, unless
    /// this measurement says it is not to be trusted (any error but a failed calibration, which
    /// leaves the reading as good as before) or it is outside the sensor's 0 to 10000 ppm.
    pub const fn ppm(&self) -> Option<u16> {
        let bad0 = self.status0 & !status0::CALIBRATION != 0;
        let bad1 =
            self.status1 & (status1::VCAP1_LOW | status1::VCAP2_LOW | status1::ADC | status1::DETECTOR) != 0;
        if bad0 || bad1 || self.filtered_pressure < 0 || self.filtered_pressure > 10_000 {
            None
        } else {
            Some(self.filtered_pressure as u16)
        }
    }
}

/// Byte `ram` of the reply's data (RAM 0x80 is at offset 3).
const fn at(reply: &[u8; REPLY_LEN], ram: u8) -> u8 {
    reply[3 + (ram - RAM) as usize]
}

/// Decode the reply to [`READ_FRAME`]: checked for its CRC, its header and its length.
pub fn parse_reply(reply: &[u8; REPLY_LEN]) -> Result<Measurement, Invalid> {
    if crc16(reply) != 0 {
        return Err(Invalid::Crc);
    }
    if reply[..3] != [DEVICE, READ, READ_DATA] {
        return Err(Invalid::Unexpected);
    }
    let s16 = |ram: u8| i16::from_be_bytes([at(reply, ram), at(reply, ram + 1)]);
    let u16v = |ram: u8| u16::from_be_bytes([at(reply, ram), at(reply, ram + 1)]);
    Ok(Measurement {
        co2: s16(0x9A),
        co2_pressure: s16(0x9C),
        filtered: s16(0xA8),
        filtered_pressure: s16(0xAA),
        temperature: s16(0x9E),
        vcap1: u16v(0xA0),
        vcap2: u16v(0xA2),
        status0: at(reply, 0xA7),
        status1: at(reply, 0xA6),
    })
}

/// The sensor state in a reply (RAM 0x81 to 0x97), to write back with the next measurement. Take
/// it only from a reply that [`parse_reply`] accepted and that does not
/// [need a restart](Measurement::needs_restart).
pub fn sensor_state(reply: &[u8; REPLY_LEN]) -> [u8; STATE_LEN] {
    let mut state = [0u8; STATE_LEN];
    state.copy_from_slice(&reply[4..4 + STATE_LEN]);
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_match_the_guideline() {
        // The guideline's two periods: 16 s between measurements, 8 days between corrections.
        assert_eq!((MIN_PERIOD_S, ABC_PERIOD_S), (16, 691_200));
        assert_eq!(WRITE_LEN, 33);
        assert_eq!(REPLY_LEN, 49);
        assert_eq!(crc16(&READ_FRAME), 0, "the constant read frame carries its CRC");
        assert_eq!(crc16(&WRITE_ACK), 0);
        assert_eq!(READ_FRAME[..5], [0xFE, 0x44, 0x00, 0x80, 0x2C]);
    }

    #[test]
    fn builds_the_write() {
        let state = [7u8; STATE_LEN];
        let frame = write_frame(Calculation::Sequential, Some(&state), 9850);
        assert_eq!(frame[..6], [0xFE, 0x41, 0x00, 0x80, 0x1A, 0x20]);
        assert_eq!(frame[6..29], state);
        assert_eq!(frame[29..31], 9850u16.to_be_bytes());
        assert_eq!(crc16(&frame), 0, "a frame with its CRC checks to zero");

        let initial = write_frame(Calculation::Initial, None, DEFAULT_PRESSURE);
        assert_eq!(initial[5], 0x10);
        assert_eq!(initial[6..29], [0u8; STATE_LEN]);
        assert_eq!(initial[29..31], [0x27, 0x8C]);
        assert_eq!(write_frame(Calculation::Abc, Some(&state), 9850)[5], 0x70);
    }

    /// A reply as the sensor would send it, with these RAM values.
    fn reply(values: &[(u8, &[u8])]) -> [u8; REPLY_LEN] {
        let mut frame = [0u8; REPLY_LEN];
        frame[..3].copy_from_slice(&[0xFE, 0x44, 0x2C]);
        for (ram, bytes) in values {
            let at = 3 + usize::from(ram - 0x80);
            frame[at..at + bytes.len()].copy_from_slice(bytes);
        }
        let crc = crc16(&frame[..REPLY_LEN - 2]);
        frame[REPLY_LEN - 2..].copy_from_slice(&crc.to_le_bytes());
        frame
    }

    #[test]
    fn decodes_a_reply() {
        let r = reply(&[
            (0x81, &[1, 2, 3]),
            (0x97, &[9]),
            (0x9A, &612i16.to_be_bytes()),
            (0x9C, &605i16.to_be_bytes()),
            (0x9E, &2350i16.to_be_bytes()),
            (0xA0, &3300u16.to_be_bytes()),
            (0xA2, &3100u16.to_be_bytes()),
            (0xA8, &598i16.to_be_bytes()),
            (0xAA, &590i16.to_be_bytes()),
        ]);
        let m = parse_reply(&r).unwrap();
        assert_eq!(
            (m.co2, m.co2_pressure, m.filtered, m.filtered_pressure),
            (612, 605, 598, 590)
        );
        assert_eq!((m.temperature, m.vcap1, m.vcap2), (2350, 3300, 3100));
        assert_eq!((m.status0, m.status1), (0, 0));
        assert_eq!(m.ppm(), Some(590));
        assert!(!m.needs_restart() && !m.vcap_low());
        let state = sensor_state(&r);
        assert_eq!(state[..3], [1, 2, 3]);
        assert_eq!(state[STATE_LEN - 1], 9, "the state ends at RAM 0x97");
    }

    #[test]
    fn errors_withhold_the_value() {
        let ppm = |s0: u8, s1: u8, value: i16| {
            parse_reply(&reply(&[
                (0xAA, &value.to_be_bytes()),
                (0xA7, &[s0]),
                (0xA6, &[s1]),
            ]))
            .unwrap()
            .ppm()
        };
        assert_eq!(ppm(0, 0, 500), Some(500));
        assert_eq!(
            ppm(status0::CALIBRATION, 0, 500),
            Some(500),
            "a failed calibration keeps the value"
        );
        assert_eq!(ppm(status0::OUT_OF_RANGE, 0, 500), None);
        assert_eq!(ppm(status0::FATAL, 0, 500), None);
        assert_eq!(ppm(0, status1::VCAP1_LOW, 500), None);
        assert_eq!(ppm(0, status1::VCAP2_LOW, 500), None);
        assert_eq!(ppm(0, status1::DETECTOR, 500), None);
        assert_eq!(ppm(0, status1::ADC, 500), None);
        assert_eq!(ppm(0, 0, -1), None);
        assert_eq!(ppm(0, 0, 10_001), None);
        assert_eq!(ppm(0, 0, 10_000), Some(10_000));
        assert_eq!(ppm(0, 0, 0), Some(0));
    }

    #[test]
    fn faults_that_need_a_restart() {
        let m = |s0: u8, s1: u8| parse_reply(&reply(&[(0xA7, &[s0]), (0xA6, &[s1])])).unwrap();
        for s0 in [
            status0::FATAL,
            status0::ALGORITHM,
            status0::MEMORY,
            status0::SELF_DIAG,
        ] {
            assert!(m(s0, 0).needs_restart(), "status 0 bit {s0:#04x}");
        }
        assert!(m(0, status1::ADC).needs_restart());
        assert!(!m(status0::CALIBRATION | status0::OUT_OF_RANGE, status1::DETECTOR).needs_restart());
        assert!(m(0, status1::VCAP1_LOW).vcap_low() && !m(0, status1::VCAP1_LOW).needs_restart());
        assert!(m(0, status1::VCAP2_LOW).vcap_low());
        assert!(!m(0, status1::DETECTOR).vcap_low());
    }

    #[test]
    fn rejects_bad_replies() {
        let mut r = reply(&[]);
        r[10] ^= 1;
        assert_eq!(parse_reply(&r), Err(Invalid::Crc));
        // A well-formed frame of another function.
        let mut other = reply(&[]);
        other[1] = 0x41;
        let crc = crc16(&other[..REPLY_LEN - 2]);
        other[REPLY_LEN - 2..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(parse_reply(&other), Err(Invalid::Unexpected));
        // And one with another length byte.
        let mut other = reply(&[]);
        other[2] = 0x2B;
        let crc = crc16(&other[..REPLY_LEN - 2]);
        other[REPLY_LEN - 2..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(parse_reply(&other), Err(Invalid::Unexpected));
    }

    #[test]
    fn checks_the_acknowledgement() {
        assert_eq!(check_ack(&WRITE_ACK), Ok(()));
        assert_eq!(check_ack(&WRITE_ACK[..3]), Err(Invalid::Short));
        assert_eq!(check_ack(&[0xFE, 0x41, 0x81, 0xE1]), Err(Invalid::Crc));
        // An error answer: FE C1 <code> with a good CRC over its first two bytes is not an ACK.
        let crc = crc16(&[0xFE, 0xC1]).to_le_bytes();
        assert_eq!(check_ack(&[0xFE, 0xC1, crc[0], crc[1]]), Err(Invalid::Unexpected));
        // Extra bytes after a good acknowledgement are not looked at.
        assert_eq!(check_ack(&[0xFE, 0x41, 0x81, 0xE0, 0x55]), Ok(()));
    }
}
