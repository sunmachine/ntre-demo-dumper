//! `dem_usercmd` payload decoding: the recorder's raw input for one tick.
//!
//! Wire format is Source SDK 2013's `ReadUsercmd`, which NT;RE does not
//! modify: every field is prefixed by one presence bit; absent
//! fields keep their default. Demo usercmds are encoded against a null cmd,
//! so each frame is self-contained. NT;RE ships extra *buttons* (aim, lean,
//! thermoptic, vision) but they live inside the standard 32-bit buttons
//! field, so the wire format is untouched.

use anyhow::Result;

use super::bits::BitReader;

/// NEO-relevant button bits (src/game/shared/in_buttons.h in the NT;RE
/// repo). Reference for consumers of the raw `buttons` field; the SQLite
/// schema's generated columns encode the same bit numbers.
#[allow(dead_code)]
pub mod buttons {
    pub const ATTACK: u32 = 1 << 0;
    pub const JUMP: u32 = 1 << 1;
    pub const DUCK: u32 = 1 << 2;
    pub const USE: u32 = 1 << 5;
    pub const ATTACK2: u32 = 1 << 11;
    pub const RELOAD: u32 = 1 << 13;
    pub const SPEED: u32 = 1 << 17; // sprint
    pub const WALK: u32 = 1 << 18;
    pub const ZOOM: u32 = 1 << 19; // held aim-down-sights state
    // NEO-specific:
    pub const DROP: u32 = 1 << 26;
    pub const AIM: u32 = 1 << 27; // ADS-toggle keypress; ZOOM is the held state
    pub const LEAN_LEFT: u32 = 1 << 28;
    pub const LEAN_RIGHT: u32 = 1 << 29;
    pub const THERMOPTIC: u32 = 1 << 30;
    pub const VISION: u32 = 1 << 31;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct UserCmd {
    pub viewangles: [f32; 3], // pitch, yaw, roll
    pub forwardmove: f32,
    pub sidemove: f32,
    pub upmove: f32,
    pub buttons: u32,
    pub impulse: u8,
    /// Weapon the player switched to this tick (entity index), if any.
    pub weaponselect: Option<u32>,
    pub mousedx: i16,
    pub mousedy: i16,
}

const MAX_EDICT_BITS: u32 = 11;
const WEAPON_SUBTYPE_BITS: u32 = 6;

pub fn read_usercmd(payload: &[u8]) -> Result<UserCmd> {
    let mut r = BitReader::new(payload);
    let mut cmd = UserCmd::default();

    if r.read_bit()? {
        r.read_u32()?; // command_number: meaningless vs the null cmd
    }
    if r.read_bit()? {
        r.read_u32()?; // tick_count: the frame header's tick is authoritative
    }
    for angle in &mut cmd.viewangles {
        if r.read_bit()? {
            *angle = r.read_f32()?;
        }
    }
    if r.read_bit()? {
        cmd.forwardmove = r.read_f32()?;
    }
    if r.read_bit()? {
        cmd.sidemove = r.read_f32()?;
    }
    if r.read_bit()? {
        cmd.upmove = r.read_f32()?;
    }
    if r.read_bit()? {
        cmd.buttons = r.read_u32()?;
    }
    if r.read_bit()? {
        cmd.impulse = r.read_u8()?;
    }
    if r.read_bit()? {
        cmd.weaponselect = Some(r.read_bits(MAX_EDICT_BITS)?);
        if r.read_bit()? {
            r.read_bits(WEAPON_SUBTYPE_BITS)?; // weaponsubtype: TF2-only concept
        }
    }
    if r.read_bit()? {
        cmd.mousedx = r.read_i16()?;
    }
    if r.read_bit()? {
        cmd.mousedy = r.read_i16()?;
    }
    // Anything after this (e.g. HL2's entitygroundcontact) is trailing and
    // safely ignored.
    Ok(cmd)
}
