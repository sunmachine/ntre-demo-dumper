//! `dem_stringtables` frame decoding: a full dump of every string table,
//! written at recording start. We use it for the `userinfo` table: the
//! player roster (name, userid, steamid) as of the moment recording began.
//! Players who join later surface via `player_connect`/`player_info` game
//! events instead.

use anyhow::Result;

use super::bits::BitReader;

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

fn fixed_str(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::bits::testutil::BitWriter;

    #[test]
    fn parses_userinfo_from_table_dump() {
        // 132-byte player_info_s
        let mut info = vec![0u8; 132];
        info[..3].copy_from_slice(b"sun");
        info[32..36].copy_from_slice(&7u32.to_le_bytes()); // user id
        info[36..47].copy_from_slice(b"[U:1:12345]");
        info[108] = 0; // not fake
        info[109] = 0; // not hltv

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
