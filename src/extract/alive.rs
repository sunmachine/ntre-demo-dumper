//! How many players each team has alive, over time.
//!
//! The player resource's team, alive and connected arrays give every
//! player's state in both recording types, because the server sends the
//! resource to every client. A disconnect clears only the connected flag,
//! so a slot counts while it is connected and alive. The server refreshes
//! the resource every 0.1 seconds, so it shows a change a few ticks after
//! it happened. A death is moved back to the tick of its player_death event
//! and a respawn to its round's start tick, so a count changes on the tick
//! the game changed it.

use std::collections::HashMap;

use super::entities::ResourceSample;
use super::net::{Death, Player};
use super::rounds::{place, Round};

/// Jinrai and NSF, coded as `PlayerSample::team` codes them.
const TEAMS: [i64; 2] = [2, 3];

/// The most ticks the resource trails the event behind a change. The
/// refresh period is 7 ticks, and the update can arrive a packet later; the
/// longest lag measured is 8 ticks.
const RESOURCE_LAG: u32 = 10;

/// One change in the number of a team's living players.
pub struct AliveCount {
    pub tick: u32,
    /// The round the tick falls in, as `rounds::place` finds it.
    pub round_number: Option<i32>,
    /// That round's position in the demo, as `rounds.round_index`.
    pub round_index: Option<u32>,
    pub team: i64,
    pub alive: usize,
}

/// Every change in each team's living player count, in tick order. The
/// first rows give both teams' counts at the first resource update.
pub fn counts(
    resource: &[ResourceSample],
    deaths: &[Death],
    players: &HashMap<u32, Player>,
    rounds: &[Round],
) -> Vec<AliveCount> {
    let mut death_ticks: HashMap<u32, Vec<u32>> = HashMap::new();
    for d in deaths {
        if let (Some(p), Ok(tick)) = (players.get(&d.victim_userid), u32::try_from(d.tick)) {
            death_ticks.entry(p.entity_id).or_default().push(tick);
        }
    }
    let starts: Vec<u32> =
        rounds.iter().filter_map(|r| r.start_tick).filter_map(|t| u32::try_from(t).ok()).collect();
    let mut by_slot: HashMap<u32, Vec<&ResourceSample>> = HashMap::new();
    for s in resource {
        by_slot.entry(s.entity_id).or_default().push(s);
    }

    let mut changes: Vec<(u32, u32, i64, bool)> = Vec::new();
    let mut slots: Vec<u32> = by_slot.keys().copied().collect();
    slots.sort_unstable();
    for slot in slots {
        let mut deaths = death_ticks.remove(&slot).unwrap_or_default();
        deaths.sort_unstable();
        changes.extend(slot_changes(slot, &by_slot[&slot], &deaths, &starts));
    }
    // A stable sort, so a slot's changes on one tick apply in order.
    changes.sort_by_key(|&(tick, ..)| tick);

    let mut state: HashMap<u32, (i64, bool)> = HashMap::new();
    let mut written: HashMap<i64, usize> = HashMap::new();
    let mut out = Vec::new();
    for (i, &(tick, slot, team, alive)) in changes.iter().enumerate() {
        state.insert(slot, (team, alive));
        if changes.get(i + 1).is_some_and(|next| next.0 == tick) {
            continue;
        }
        for team in TEAMS {
            let alive = state.values().filter(|&&(t, a)| t == team && a).count();
            if written.insert(team, alive) != Some(alive) {
                let round = place(rounds, tick as i32);
                let round_number = round.as_ref().and_then(|p| p.number);
                let round_index = round.map(|p| p.index);
                out.push(AliveCount { tick, round_number, round_index, team, alive });
            }
        }
    }
    out
}

/// One slot's team and alive changes as `(tick, slot, team, alive)`, in
/// tick order. A death takes the slot's player out on the death's own tick,
/// and the resource row that later shows it changes nothing. A row can
/// still show the player alive because the server read it before the
/// death. Such a row comes within `RESOURCE_LAG` ticks and still carries
/// the death count from before the death, so it is skipped. A respawn
/// moves back to the round start behind it. A player who died just before
/// a round began and was alive again by the next refresh therefore counts
/// out from the death to the start.
fn slot_changes(
    slot: u32,
    rows: &[&ResourceSample],
    deaths: &[u32],
    starts: &[u32],
) -> Vec<(u32, u32, i64, bool)> {
    // The latest round start that the resource tick `tick` trails.
    let start_behind =
        |tick: u32| starts.iter().copied().filter(|&t| t <= tick && tick - t <= RESOURCE_LAG).max();
    let mut changes = Vec::new();
    let mut latest: Option<(u32, i64, bool)> = None;
    // The last death so far, with the slot's death count before it.
    let mut last_death: Option<(u32, i64)> = None;
    let mut death_count = 0;
    let mut next_death = deaths.iter().copied().peekable();
    for s in rows {
        // Deaths up to this row's tick come first, as the row may only now
        // show one of them.
        while let Some(death) = next_death.next_if(|&d| d <= s.tick) {
            last_death = Some((death, death_count));
            if let Some((tick, team, true)) = latest {
                let tick = death.max(tick);
                latest = Some((tick, team, false));
                changes.push((tick, slot, team, false));
            }
        }
        let alive = s.alive && s.connected;
        let stale = alive
            && last_death.is_some_and(|(d, count)| s.tick - d <= RESOURCE_LAG && s.deaths <= count);
        death_count = s.deaths;
        if stale || latest.is_some_and(|(_, team, was)| team == s.team && was == alive) {
            continue;
        }
        let respawn = alive && !latest.is_some_and(|(_, _, was)| was);
        let event = if respawn { start_behind(s.tick) } else { None };
        // Never before the slot's previous change, so its changes keep their order.
        let tick = event.unwrap_or(s.tick).max(latest.map_or(0, |(tick, _, _)| tick));
        latest = Some((tick, s.team, alive));
        changes.push((tick, slot, s.team, alive));
    }
    for death in next_death {
        if let Some((tick, team, true)) = latest {
            let tick = death.max(tick);
            latest = Some((tick, team, false));
            changes.push((tick, slot, team, false));
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(tick: u32, entity_id: u32, team: i64, alive: bool) -> ResourceSample {
        ResourceSample {
            tick,
            entity_id,
            team,
            alive,
            connected: true,
            xp: 0,
            score: 0,
            deaths: 0,
            ping: 0,
        }
    }

    fn player(user_id: u32) -> (u32, Player) {
        let p = Player {
            entity_id: user_id,
            user_id,
            name: String::new(),
            steam_id: String::new(),
            is_bot: false,
            first_seen_tick: 0,
            neo_name: None,
        };
        (user_id, p)
    }

    fn death(tick: i32, victim_userid: u32) -> Death {
        Death {
            tick,
            victim_userid,
            attacker_userid: 0,
            assists: 0,
            weapon: String::new(),
            headshot: false,
            suicide: false,
            explosive: false,
            ghoster: false,
            round_number: None,
            round_index: None,
            post_round: false,
            victim_team: None,
            attacker_team: None,
            assister_team: None,
            teamkill: None,
        }
    }

    fn round(number: i32, start_tick: i32, end_tick: i32) -> Round {
        Round {
            number: Some(number),
            start_tick: Some(start_tick),
            end_tick: Some(end_tick),
            winner: None,
            reason: None,
            capturer_userid: None,
            ghost_spawn: None,
        }
    }

    fn rows(counts: &[AliveCount]) -> Vec<(u32, Option<i32>, i64, usize)> {
        counts.iter().map(|c| (c.tick, c.round_number, c.team, c.alive)).collect()
    }

    #[test]
    fn counts_change_on_the_death_and_round_start_ticks() {
        let players = HashMap::from([player(1), player(2), player(3)]);
        let rounds = [round(1, 10, 150), round(2, 200, 300)];
        let resource = [
            // Round 1 starts at 10 and the resource shows the spawns at 14.
            sample(14, 1, 2, true),
            sample(14, 2, 2, true),
            sample(14, 3, 3, true),
            // Player 1 dies at 100 and the resource shows it at 106.
            sample(106, 1, 2, false),
            // Everyone respawns at 200 and the resource shows it at 205.
            sample(205, 1, 2, true),
        ];
        let deaths = [death(100, 1)];
        assert_eq!(
            rows(&counts(&resource, &deaths, &players, &rounds)),
            [(10, Some(1), 2, 2), (10, Some(1), 3, 1), (100, Some(1), 2, 1), (200, Some(2), 2, 2)]
        );
    }

    #[test]
    fn a_change_with_no_event_behind_it_stays_on_the_resource_tick() {
        let players = HashMap::from([player(1), player(2)]);
        let resource = [
            sample(14, 1, 2, true),
            sample(14, 2, 3, true),
            // Player 1 leaves without dying. The resource clears only the
            // connected flag and keeps the slot's team and alive flag.
            ResourceSample { connected: false, ..sample(120, 1, 2, true) },
            // Player 2 switches team while alive.
            sample(130, 2, 2, true),
        ];
        assert_eq!(
            rows(&counts(&resource, &[], &players, &[])),
            [(14, None, 2, 1), (14, None, 3, 1), (120, None, 2, 0), (130, None, 2, 1), (130, None, 3, 0)]
        );
    }

    #[test]
    fn a_reused_slot_counts_for_its_new_player() {
        let players = HashMap::from([player(1), player(2)]);
        let resource = [
            sample(14, 1, 2, true),
            sample(14, 2, 3, true),
            // Player 1 leaves; a joiner takes slot 1 on NSF, dead until spawned.
            ResourceSample { connected: false, ..sample(120, 1, 2, true) },
            sample(160, 1, 3, false),
            sample(400, 1, 3, true),
        ];
        assert_eq!(
            rows(&counts(&resource, &[], &players, &[])),
            [(14, None, 2, 1), (14, None, 3, 1), (120, None, 2, 0), (400, None, 3, 2)]
        );
    }

    #[test]
    fn a_row_read_before_the_death_does_not_bring_the_player_back() {
        let players = HashMap::from([player(1)]);
        let resource = [
            sample(14, 1, 2, true),
            // On the death tick the resource still shows the count before it.
            ResourceSample { xp: 5, ..sample(100, 1, 2, true) },
            ResourceSample { deaths: 1, ..sample(106, 1, 2, false) },
        ];
        let deaths = [death(100, 1)];
        assert_eq!(
            rows(&counts(&resource, &deaths, &players, &[])),
            [(14, None, 2, 1), (14, None, 3, 0), (100, None, 2, 0)]
        );
    }

    #[test]
    fn a_death_just_before_a_round_start_still_counts_out() {
        let players = HashMap::from([player(1)]);
        let rounds = [round(1, 104, 500)];
        // The next refresh after the death already shows the respawn.
        let resource = [sample(14, 1, 2, true), ResourceSample { deaths: 1, ..sample(107, 1, 2, true) }];
        let deaths = [death(100, 1)];
        assert_eq!(
            rows(&counts(&resource, &deaths, &players, &rounds)),
            [(14, None, 2, 1), (14, None, 3, 0), (100, None, 2, 0), (104, Some(1), 2, 1)]
        );
    }
}
