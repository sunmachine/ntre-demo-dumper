//! `dem_stringtables` frame decoding: a full dump of every string table,
//! written at recording start. We use it for the `userinfo` table: the
//! player roster (name, userid, steamid) as of the moment recording began.
//! Players who join later surface as svc_UpdateStringTable messages for the
//! same table, decoded by [`parse_userinfo_update`]. SourceTV recordings
//! carry no `player_connect` event, so the table is the only place a late
//! joiner's Steam ID appears.

use anyhow::Result;

use super::bits::{BitChunk, BitReader};

pub struct PlayerInfo {
    /// Client slot from the table index; entity id is slot + 1.
    pub entity_id: u32,
    pub name: String,
    pub user_id: u32,
    pub steam_id: String,
    pub is_fake_player: bool,
    pub is_hltv: bool,
}

/// Parse the userinfo entries out of a dem_stringtables frame payload.
pub fn parse_userinfo(payload: &[u8]) -> Result<Vec<PlayerInfo>> {
    let mut r = BitReader::new(payload);
    let mut players = Vec::new();
    let table_count = r.read_bits(8)?;
    for _ in 0..table_count {
        let table_name = r.read_string()?;
        let entry_count = r.read_bits(16)?;
        for _ in 0..entry_count {
            let text = r.read_string()?;
            let userdata = if r.read_bit()? {
                let byte_len = r.read_bits(16)? as usize;
                Some(r.read_chunk(byte_len * 8)?)
            } else {
                None
            };
            if table_name == "userinfo" {
                if let (Ok(slot), Some(data)) = (text.parse::<u32>(), &userdata) {
                    if let Some(info) = parse_player_info(&data.bytes, slot + 1) {
                        players.push(info);
                    }
                }
            }
        }
        if r.read_bit()? {
            // client-side entries: same format, nothing we need
            let client_count = r.read_bits(16)?;
            for _ in 0..client_count {
                r.read_string()?;
                if r.read_bit()? {
                    let byte_len = r.read_bits(16)? as usize;
                    r.skip_bits(byte_len * 8)?;
                }
            }
        }
    }
    Ok(players)
}

/// How many earlier entries of one update a later entry can borrow a string
/// prefix from.
const UPDATE_HISTORY: usize = 32;

/// Decode the players named by one svc_UpdateStringTable for the userinfo
/// table. `max_entries` comes from the table's svc_CreateStringTable and
/// sets the width of an entry index; `changed` is the update's entry count.
/// An entry's index is the client slot. A slot that was cleared, as on a
/// disconnect, carries no player and is left out.
///
/// Wire format ported from demostf/parser (MIT). It holds for tables without
/// a fixed userdata size, which userinfo is.
pub fn parse_userinfo_update(data: &BitChunk, max_entries: u16, changed: u16) -> Result<Vec<PlayerInfo>> {
    let mut r = data.reader();
    let index_bits = max_entries.checked_ilog2().unwrap_or(0);
    let mut history: Vec<Option<String>> = Vec::new();
    let mut players = Vec::new();
    let mut last_index: Option<u32> = None;
    for _ in 0..changed {
        let index = if r.read_bit()? {
            last_index.map_or(0, |i| i + 1)
        } else {
            r.read_bits(index_bits)?
        };
        last_index = Some(index);
        let text = if r.read_bit()? {
            if r.read_bit()? {
                // The string shares a prefix with an earlier entry of this
                // update.
                let from = r.read_bits(5)? as usize;
                let prefix_len = r.read_bits(5)? as usize;
                let rest = r.read_string()?;
                let prefix: String = history
                    .get(from)
                    .and_then(|t| t.as_deref())
                    .map(|t| t.chars().take(prefix_len).collect())
                    .unwrap_or_default();
                Some(prefix + &rest)
            } else {
                Some(r.read_string()?)
            }
        } else {
            None
        };
        if r.read_bit()? {
            let byte_len = r.read_bits(14)? as usize;
            let userdata = r.read_chunk(byte_len * 8)?;
            if let Some(info) = parse_player_info(&userdata.bytes, index + 1) {
                players.push(info);
            }
        }
        if history.len() == UPDATE_HISTORY {
            history.remove(0);
        }
        history.push(text);
    }
    Ok(players)
}

fn fixed_str(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::bits::testutil::BitWriter;

    /// A 132-byte player_info_s for a human player.
    fn player_info(name: &str, user_id: u32, steam_id: &str) -> Vec<u8> {
        let mut info = vec![0u8; 132];
        info[..name.len()].copy_from_slice(name.as_bytes());
        info[32..36].copy_from_slice(&user_id.to_le_bytes());
        info[36..36 + steam_id.len()].copy_from_slice(steam_id.as_bytes());
        info
    }

    fn write_userdata(w: &mut BitWriter, info: &[u8]) {
        w.write_bit(true); // has userdata
        w.write_bits(info.len() as u32, 14);
        for b in info {
            w.write_bits(*b as u32, 8);
        }
    }

    /// One update can carry an explicit slot with no string, the slot after
    /// it with a string, and a cleared slot. The cleared slot names nobody.
    #[test]
    fn parses_userinfo_update() {
        let mut w = BitWriter::default();
        w.write_bit(false); // explicit index
        w.write_bits(2, 8); // slot 2, log2(256) bits
        w.write_bit(false); // string unchanged
        write_userdata(&mut w, &player_info("sun", 28, "[U:1:12345]"));
        w.write_bit(true); // next index: slot 3
        w.write_bit(true); // string present
        w.write_bit(false); // not a prefix of an earlier entry
        w.write_string("3");
        write_userdata(&mut w, &player_info("moon", 29, "[U:1:67890]"));
        w.write_bit(true); // slot 4
        w.write_bit(false);
        w.write_bit(true); // userdata present but empty: the slot was cleared
        w.write_bits(0, 14);

        let chunk = BitChunk { bit_len: w.bytes.len() * 8, bytes: w.bytes };
        let players = parse_userinfo_update(&chunk, 256, 3).unwrap();
        let got: Vec<_> =
            players.iter().map(|p| (p.entity_id, p.user_id, p.name.as_str(), p.steam_id.as_str())).collect();
        assert_eq!(got, [(3, 28, "sun", "[U:1:12345]"), (4, 29, "moon", "[U:1:67890]")]);
    }

    #[test]
    fn parses_userinfo_from_table_dump() {
        let info = player_info("sun", 7, "[U:1:12345]");

        let mut w = BitWriter::default();
        w.write_bits(1, 8); // one table
        w.write_string("userinfo");
        w.write_bits(1, 16); // one entry
        w.write_string("2"); // slot 2 -> entity 3
        w.write_bit(true); // has userdata
        w.write_bits(info.len() as u32, 16);
        for b in &info {
            w.write_bits(*b as u32, 8);
        }
        w.write_bit(false); // no client entries

        let players = parse_userinfo(&w.bytes).unwrap();
        assert_eq!(players.len(), 1);
        assert_eq!(players[0].name, "sun");
        assert_eq!(players[0].user_id, 7);
        assert_eq!(players[0].steam_id, "[U:1:12345]");
        assert_eq!(players[0].entity_id, 3);
        assert!(!players[0].is_fake_player);
    }
}

/// player_info_s for this engine branch (132 bytes):
/// name[32], user_id u32, steam_id[32], extra u32, friends_id u32,
/// friends_name[32], fake u8, hltv u8, replay u8, custom_files u32[4],
/// files_downloaded u32, padding u8.
pub fn parse_player_info(data: &[u8], entity_id: u32) -> Option<PlayerInfo> {
    if data.len() < 108 {
        return None;
    }
    let name = fixed_str(&data[0..32]);
    let user_id = u32::from_le_bytes(data[32..36].try_into().ok()?);
    let steam_id = fixed_str(&data[36..68]);
    // Two player_info_s layouts exist, with and without a 4-byte `extra`
    // field; pick the flag offsets by total length.
    let (fake_off, hltv_off) = if data.len() >= 132 { (108, 109) } else { (104, 105) };
    Some(PlayerInfo {
        entity_id,
        name,
        user_id,
        steam_id,
        is_fake_player: data.get(fake_off).is_some_and(|&b| b != 0),
        is_hltv: data.get(hltv_off).is_some_and(|&b| b != 0),
    })
}
