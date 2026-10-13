//! DCP transport to a GoXLR Mini: packet framing, sequence numbers and the safety allowlist.
//!
//! Packet: u32 cmd = (category << 12) | opcode, u16 body length, u16 sequence,
//! 8 reserved bytes, then the body. Written with vendor bRequest 2; the reply is read with
//! bRequest 3 after the driver signals "data ready".

use anyhow::{Context, Result, bail, ensure};

use crate::tusb::{Api, Notification, NotificationEvent, PID_GOXLR_MINI, VID_TC_HELICON};

const HEADER_LEN: usize = 16;
const REPLY_MAX: u16 = 1040;
const REPLY_WAIT_MS: u32 = 1000;

/// SAFETY: the only DCP categories this program may ever send.
///
/// Rule: this app never flashes the GoXLR Mini. Firmware update (0x810) and every standard DICE
/// category (0 system, 1-3 caps, 4 NVM erase/program/crc) are deliberately absent, so they
/// cannot be sent by accident or by a bug elsewhere. Do not add them.
/// Each entry is (category, short label shown on the Device page).
pub const ALLOWED_CATEGORIES: &[(u16, &str)] = &[
    (0x800, "read state"), // buttons, encoders, faders
    (0x801, "effects"),    // effect parameters
    (0x802, "scribbles"),  // scribble strips
    (0x803, "colours"),    // colour map
    (0x804, "routing"),
    (0x805, "faders"),      // fader assignment
    (0x806, "volume"),      // channel volume
    (0x808, "buttons"),     // button LEDs
    (0x809, "mutes"),       // channel mute state
    (0x80a, "encoders"),    // encoder value
    (0x80b, "mic"),         // mic parameters
    (0x80c, "mic level"),   // read
    (0x80f, "info"),        // hardware info read (0 = versions, 1 = serial)
    (0x811, "enc. mode"),   // encoder mode
    (0x814, "fader style"), // fader display style
    (0x816, "animation"),   // lighting animation
    (0x817, "submix"),      // submix output assignment
    (0x818, "monitor"),     // headphone monitored mix
];

pub struct Reply {
    pub body: Vec<u8>,
}

/// Lower-case hex of a packet body, as the logs and tools print it.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Firmware and DICE versions from the 0x80f/0 reply (six little-endian words).
pub struct Versions {
    pub firmware: (u32, u32, u32, u32),
    pub dice: (u32, u32, u32, u32),
}

impl Versions {
    pub fn parse(body: &[u8]) -> Option<Self> {
        let word = |i: usize| Some(u32::from_le_bytes(body.get(i * 4..i * 4 + 4)?.try_into().ok()?));
        let (fw, build, dice_build, dice) = (word(0)?, word(1)?, word(4)?, word(5)?);
        Some(Self {
            firmware: (fw >> 12, (fw >> 8) & 0xf, fw & 0xff, build),
            dice: ((dice >> 20) & 0xf, (dice >> 12) & 0xff, dice & 0xfff, dice_build),
        })
    }
}

pub struct Device<'a> {
    api: &'a Api,
    handle: u32,
    events: NotificationEvent<'a>,
    seq: u16,
    /// Set when the driver reported a hardware input change, including while we waited for a reply.
    input_pending: bool,
}

impl<'a> Device<'a> {
    /// Opens the first attached GoXLR Mini and performs the transport handshake
    /// (read bRequest 0, reset sequence with bRequest 1, read the reset reply).
    pub fn open_mini(api: &'a Api) -> Result<Self> {
        let count = api.device_count()?;
        for index in 0..count {
            let Ok(handle) = api.open(index) else { continue };
            match api.properties(handle) {
                Ok(p) if p.vendor_id == VID_TC_HELICON && p.product_id == PID_GOXLR_MINI => {
                    let events = match NotificationEvent::register(api, handle) {
                        Ok(e) => e,
                        Err(e) => {
                            api.close(handle);
                            return Err(e);
                        }
                    };
                    let mut dev = Self { api, handle, events, seq: 0, input_pending: false };
                    dev.handshake().context("transport handshake")?;
                    return Ok(dev);
                }
                _ => api.close(handle),
            }
        }
        bail!("no GoXLR Mini found ({count} TUSBAUDIO device(s) present)")
    }

    pub fn properties(&self) -> Result<Box<crate::tusb::Properties>> {
        self.api.properties(self.handle)
    }

    fn handshake(&mut self) -> Result<()> {
        self.api.vendor_in(self.handle, 0, 24)?;
        self.api.vendor_out(self.handle, 1, &[])?;
        self.wait_data_ready()?;
        self.api.vendor_in(self.handle, 3, REPLY_MAX)?;
        self.seq = 0;
        Ok(())
    }

    fn wait_data_ready(&mut self) -> Result<()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(REPLY_WAIT_MS.into());
        while std::time::Instant::now() < deadline {
            let events = self.events.wait(50)?;
            self.input_pending |= events.contains(&Notification::InputChanged);
            if events.contains(&Notification::DataReady) {
                return Ok(());
            }
        }
        bail!("timed out waiting for the device reply")
    }

    /// Waits up to `timeout_ms` for a button, fader or encoder change. Returns true if one happened.
    pub fn wait_input(&mut self, timeout_ms: u32) -> Result<bool> {
        if !self.input_pending {
            let events = self.events.wait(timeout_ms)?;
            self.input_pending = events.contains(&Notification::InputChanged);
        }
        Ok(std::mem::take(&mut self.input_pending))
    }

    /// Sends one DCP command and returns its reply. Refuses anything outside ALLOWED_CATEGORIES.
    pub fn request(&mut self, category: u16, opcode: u16, body: &[u8]) -> Result<Reply> {
        ensure!(
            ALLOWED_CATEGORIES.iter().any(|&(c, _)| c == category),
            "refusing DCP category {category:#x}: not on the safety allowlist (firmware and NVM access are forbidden)"
        );
        ensure!(opcode < 0x1000, "opcode {opcode:#x} out of range");
        let len: u16 = body.len().try_into().context("body too long")?;

        self.seq = self.seq.wrapping_add(1);
        let cmd = (u32::from(category) << 12) | u32::from(opcode);
        let mut packet = Vec::with_capacity(HEADER_LEN + body.len());
        packet.extend_from_slice(&cmd.to_le_bytes());
        packet.extend_from_slice(&len.to_le_bytes());
        packet.extend_from_slice(&self.seq.to_le_bytes());
        packet.extend_from_slice(&[0; 8]);
        packet.extend_from_slice(body);

        self.api.vendor_out(self.handle, 2, &packet)?;
        self.wait_data_ready()?;
        let reply = self.api.vendor_in(self.handle, 3, REPLY_MAX)?;
        ensure!(reply.len() >= HEADER_LEN, "short reply ({} bytes)", reply.len());

        let rcmd = u32::from_le_bytes(reply[0..4].try_into()?);
        let rlen = u16::from_le_bytes(reply[4..6].try_into()?) as usize;
        let rseq = u16::from_le_bytes(reply[6..8].try_into()?);
        ensure!(rseq == self.seq, "reply sequence {rseq} does not match request {}", self.seq);
        ensure!(rcmd == cmd, "reply is for command {rcmd:#x}, expected {cmd:#x}");
        ensure!(reply.len() >= HEADER_LEN + rlen, "reply body truncated");
        Ok(Reply { body: reply[HEADER_LEN..HEADER_LEN + rlen].to_vec() })
    }
}

impl Drop for Device<'_> {
    fn drop(&mut self) {
        self.api.close(self.handle);
    }
}

#[cfg(test)]
mod tests {
    use super::{ALLOWED_CATEGORIES, Versions};

    #[test]
    fn versions_need_the_whole_reply() {
        assert!(Versions::parse(&[0; 23]).is_none());
        let mut body = [0u8; 24];
        body[0..4].copy_from_slice(&0x1201u32.to_le_bytes());
        body[4..8].copy_from_slice(&49u32.to_le_bytes());
        assert_eq!(Versions::parse(&body).unwrap().firmware, (1, 2, 1, 49));
    }

    #[test]
    fn firmware_and_nvm_categories_are_never_allowed() {
        for forbidden in [0x0, 0x1, 0x2, 0x3, 0x4, 0x810] {
            assert!(ALLOWED_CATEGORIES.iter().all(|&(c, _)| c != forbidden), "category {forbidden:#x} must stay blocked");
        }
    }
}
