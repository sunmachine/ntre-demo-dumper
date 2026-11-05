//! Who held the ghost, and when.
//!
//! The game rules proxy's `m_iGhosterPlayer` names the ghost's owner. The
//! game sets it from the ghost weapon's owner on every think while a round
//! is live (upstream at `bea96408`,
//! `src/game/shared/neo/neo_gamerules.cpp:1414-1428`). A player owns the
//! ghost from pickup to drop, whichever weapon they have out, so the
//! property records possession, where `player_samples.weapon` records only
//! the ghost being equipped. The server sends the proxy to every client,
//! so both recording types carry it.
//!
//! Once a round is over, the think returns before that update
//! (`:1362-1404`), and the property keeps its last value until the next
//! round resets it (`:3516-3522`). A hold still open when its round ends is
//! therefore ended at the round's end tick, and possession after the end
//! is not recorded.

use std::collections::HashMap;

use super::entities::CarrierChange;
use super::net::{slot_holder, Player};
use super::rounds::{place, Round};

/// One stretch of one player holding the ghost.
pub struct GhostHold {
    /// When the player took the ghost.
    pub tick: u32,
    /// The first tick they no longer held it, or their round's end tick if
    /// that came first; None when they still held it in a round the
    /// recording cut off.
    pub end_tick: Option<u32>,
    /// The round the hold began in, as `rounds::place` finds it.
    pub round_number: Option<i32>,
    pub entity_id: u32,
    /// The connection holding the slot when the hold began.
    pub userid: Option<u32>,
}

/// Every hold, in tick order, from the carrier changes the entity pass
/// recorded.
pub fn holds(changes: &[CarrierChange], players: &HashMap<u32, Player>, rounds: &[Round]) -> Vec<GhostHold> {
    let mut out = raw_holds(changes, players, rounds);
    for hold in &mut out {
        // The end of the round the hold began in, past which the property
        // no longer follows the ghost.
        let tick = i64::from(hold.tick);
        let round_end = rounds
            .iter()
            .find(|r| {
                r.start_tick.is_none_or(|s| i64::from(s) <= tick)
                    && r.end_tick.is_some_and(|e| i64::from(e) >= tick)
            })
            .and_then(|r| r.end_tick)
            .and_then(|e| u32::try_from(e).ok());
        if let Some(end) = round_end {
            if hold.end_tick.is_none_or(|t| t > end) {
                hold.end_tick = Some(end);
            }
        }
    }
    out
}

/// The holds exactly as the property changes, before round ends cap them.
fn raw_holds(changes: &[CarrierChange], players: &HashMap<u32, Player>, rounds: &[Round]) -> Vec<GhostHold> {
    let mut out: Vec<GhostHold> = Vec::new();
    for change in changes {
        // A change ends the open hold, whether the ghost was dropped or
        // passed straight to another player on the same tick.
        if let Some(open) = out.last_mut().filter(|h| h.end_tick.is_none()) {
            open.end_tick = Some(change.tick);
        }
        if change.entity_id != 0 {
            let tick = i32::try_from(change.tick).unwrap_or(i32::MAX);
            out.push(GhostHold {
                tick: change.tick,
                end_tick: None,
                round_number: place(rounds, tick).and_then(|p| p.number),
                entity_id: change.entity_id,
                userid: slot_holder(players, change.entity_id, tick),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(tick: u32, entity_id: u32) -> CarrierChange {
        CarrierChange { tick, entity_id }
    }

    fn player(user_id: u32, entity_id: u32) -> (u32, Player) {
        let p = Player {
            entity_id,
            user_id,
            name: String::new(),
            steam_id: String::new(),
            is_bot: false,
            first_seen_tick: 0,
            neo_name: None,
        };
        (user_id, p)
    }

    fn rows(holds: &[GhostHold]) -> Vec<(u32, Option<u32>, Option<i32>, u32, Option<u32>)> {
        holds.iter().map(|h| (h.tick, h.end_tick, h.round_number, h.entity_id, h.userid)).collect()
    }

    #[test]
    fn holds_run_from_pickup_to_the_next_change() {
        let players = HashMap::from([player(11, 4), player(12, 7)]);
        // The recording ends during round 1.
        let rounds = [Round {
            number: Some(1),
            start_tick: Some(10),
            end_tick: None,
            winner: None,
            reason: None,
            capturer_userid: None,
        }];
        // Slot 4 picks it up, drops it, slot 7 picks it up and passes it
        // straight back to slot 4, who still holds it when the demo ends.
        let changes = [change(100, 4), change(150, 0), change(200, 7), change(260, 4)];
        assert_eq!(
            rows(&holds(&changes, &players, &rounds)),
            [
                (100, Some(150), Some(1), 4, Some(11)),
                (200, Some(260), Some(1), 7, Some(12)),
                (260, None, Some(1), 4, Some(11)),
            ]
        );
    }

    #[test]
    fn a_hold_open_at_the_round_end_ends_there() {
        let players = HashMap::from([player(11, 4)]);
        let rounds = [Round {
            number: Some(1),
            start_tick: Some(10),
            end_tick: Some(300),
            winner: None,
            reason: None,
            capturer_userid: None,
        }];
        // The property keeps naming slot 4 until the next round resets it.
        let changes = [change(100, 4), change(1300, 0)];
        assert_eq!(rows(&holds(&changes, &players, &rounds)), [(100, Some(300), Some(1), 4, Some(11))]);
    }

    #[test]
    fn a_slot_no_roster_player_held_has_no_userid() {
        let changes = [change(100, 9), change(120, 0)];
        assert_eq!(rows(&holds(&changes, &HashMap::new(), &[])), [(100, Some(120), None, 9, None)]);
    }
}
