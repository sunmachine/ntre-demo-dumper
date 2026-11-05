//! Round reconstruction from start markers, round-end messages and entity
//! state.
//!
//! Pairs "ROUND N STARTED" announcements with round ends. Ends come from the
//! server's RoundResult user message when the demo carries it (it names the
//! winner, and "tie" for stalemates, which no on-screen text announces);
//! otherwise from "Team X wins ..." announcements. Handles demos that begin
//! mid-round (first round has no start marker) or end mid-round (last round
//! has no end). A recording that begins in a round's freeze time can miss
//! that round's start text, so its first round takes the start from the
//! game rules entering the freeze instead. Facts the round-end message does not carry, such as who
//! captured the ghost, come from the entity state around each end.

use anyhow::{bail, Result};
use regex::Regex;
use std::collections::HashMap;

use super::announcements::Announcement;
use super::entities::{CarrierChange, EntityOutput, FreezeStart, GhostSample, PlayerSample, ZoneChange};
use super::net::{slot_holder, Death, Player, RoundResult};

pub struct Round {
    pub number: Option<i32>,
    pub start_tick: Option<i32>,
    pub end_tick: Option<i32>,
    pub winner: Option<String>,
    pub reason: Option<String>,
    /// The userid of the player who carried the ghost into the capture zone.
    pub capturer_userid: Option<u32>,
    /// Where the ghost spawned, set by `place_ghost_spawns`.
    pub ghost_spawn: Option<[f32; 3]>,
}

impl Round {
    fn unstarted() -> Self {
        Round {
            number: None,
            start_tick: None,
            end_tick: None,
            winner: None,
            reason: None,
            capturer_userid: None,
            ghost_spawn: None,
        }
    }
}

/// Game state that the rules in this module read around each round end.
/// Each field borrows what another pass produced, so a rule that needs more
/// end-of-round state, such as alive players or deaths, adds a field here and
/// leaves the `derive` call alone. An empty list means the demo did not
/// carry that state, and the columns that need it stay NULL.
pub struct Evidence<'a> {
    /// Ghost carrier changes from the game rules proxy, in tick order.
    pub carrier_changes: &'a [CarrierChange],
    /// Capture zone activity changes, in tick order.
    pub zone_changes: &'a [ZoneChange],
    /// The roster by userid, for turning an entity slot into a userid.
    pub players: &'a HashMap<u32, Player>,
    /// All-player state samples, for a losing team's alive state and a
    /// killed player's class at a round's end.
    pub player_samples: &'a [PlayerSample],
    /// The deaths, for a death landing near a round's end.
    pub deaths: &'a [Death],
    /// The game rules entering each freeze time, for a first round whose
    /// start text the recording missed.
    pub freeze_starts: &'a [FreezeStart],
}

impl<'a> Evidence<'a> {
    /// Collect the evidence from the entity pass, which is `None` when that
    /// pass failed, and the net pass's roster and deaths.
    pub fn new(
        entities: Option<&'a EntityOutput>,
        players: &'a HashMap<u32, Player>,
        deaths: &'a [Death],
    ) -> Self {
        Evidence {
            carrier_changes: entities.map(|e| &e.carrier_changes[..]).unwrap_or_default(),
            zone_changes: entities.map(|e| &e.zone_changes[..]).unwrap_or_default(),
            players,
            player_samples: entities.map(|e| &e.samples[..]).unwrap_or_default(),
            deaths,
            freeze_starts: entities.map(|e| &e.freeze_starts[..]).unwrap_or_default(),
        }
    }
}

/// Ticks either side of a round end within which the capture zones must
/// switch off, a VIP's death must fall, or any death must fall, for that fact
/// to count toward the round's outcome. The game applies all three in the
/// think that ends the round, so the window only absorbs a change landing
/// in a neighbouring packet.
const END_WINDOW: i64 = 2;

/// Whether a tick falls within `END_WINDOW` of `end_tick`.
fn near_end(tick: i32, end_tick: i32) -> bool {
    (i64::from(tick) - i64::from(end_tick)).abs() <= END_WINDOW
}

/// Whether every capture zone active before the window around `end_tick`
/// switched off within it. A ghost capture and a VIP escort both clear
/// every zone this way when they end a round (upstream at `bea96408`,
/// `neo_gamerules.cpp:1458-1468`, `:1575-1585`).
fn zones_switched_off(end_tick: i32, evidence: &Evidence) -> bool {
    let end = i64::from(end_tick);
    // Each zone's state before the window opens and after it closes.
    let mut zones: HashMap<u32, (bool, bool)> = HashMap::new();
    for z in evidence.zone_changes {
        let tick = i64::from(z.tick);
        let (before, after) = zones.entry(z.entity_id).or_default();
        if tick < end - END_WINDOW {
            *before = z.active;
        }
        if tick <= end + END_WINDOW {
            *after = z.active;
        }
    }
    let was_active: Vec<bool> =
        zones.values().filter(|(before, _)| *before).map(|(_, after)| *after).collect();
    !was_active.is_empty() && !was_active.contains(&true)
}

/// The userid of the ghost capturer for a round ending at `end_tick`, or
/// None when the round was not won by a capture.
///
/// A capture is a round end where the zone test above passes. A VIP escort
/// passes it too, but that rule runs only while no ghost exists, so the
/// carrier is 0 and this function gives None. The carrier is the one at the
/// end tick, or the one on the tick before when it was cleared at the end
/// tick.
fn capturer(end_tick: i32, evidence: &Evidence) -> Option<u32> {
    if !zones_switched_off(end_tick, evidence) {
        return None;
    }
    let end = i64::from(end_tick);

    // The carrier as of the last change before `limit`.
    let carrier_before = |limit: i64| {
        evidence
            .carrier_changes
            .iter()
            .take_while(|c| i64::from(c.tick) < limit)
            .last()
            .map_or(0, |c| c.entity_id)
    };
    let carrier = [carrier_before(end + 1), carrier_before(end)].into_iter().find(|&e| e != 0)?;

    slot_holder(evidence.players, carrier, end_tick)
}

/// A round end from either source, normalized. `winner` is None only for a
/// fallback announcement that names the match's winner rather than this
/// round's.
struct End {
    tick: i32,
    winner: Option<String>,
    reason: Option<String>,
}

/// Whether a RoundResult message is one of the three texts
/// `CNEORules::SetWinningTeam` substitutes when a round ends the match or
/// leads into sudden death, instead of the round's own reason (upstream at
/// `bea96408`, `neo_gamerules.cpp:3990-4036`).
fn is_match_end_text(message: &str) -> bool {
    ["wins the match", "The match is tied!", "Next round: Sudden death!"]
        .iter()
        .any(|text| message.contains(text))
}

/// Maps a round-end message to a short win-reason code.
///
/// The phrases are the victory strings NEO_VICTORY_* builds in upstream
/// NeotokyoRebuild/neo's `src/game/shared/neo/neo_gamerules.cpp` at
/// `bea96408` (lines 4044-4069), plus the deathmatch message (line 1665). NULL
/// means the reason could not be identified from the text alone: an empty
/// message, or a match-end text such as "wins the match", which replaces
/// the round's own reason. `derive` recovers that case separately, in
/// `recovered_reason`.
fn win_reason_code(message: &str) -> Option<String> {
    if message.trim() == "TIE" {
        return Some("tie".to_string());
    }
    const CODES: &[(&str, &str)] = &[
        ("wins by capturing the ghost", "objective"),
        ("wins by escorting the vip", "objective"),
        ("wins by eliminating the vip", "objective"),
        ("wins by eliminating the other team", "elimination"),
        ("wins by highest score", "score"),
        ("wins by numbers", "score"),
        ("is the winner of the deathmatch", "score"),
        ("wins by forfeit", "forfeit"),
    ];
    CODES.iter().find(|(phrase, _)| message.contains(phrase)).map(|(_, code)| code.to_string())
}

/// Whether a death in `deaths` fell within `END_WINDOW` of `end_tick`.
fn death_near_end(end_tick: i32, deaths: &[Death]) -> bool {
    deaths.iter().any(|k| near_end(k.tick, end_tick))
}

/// The class of the player who owns `entity_id`, by their latest sample at
/// or before `tick`, or None when no such sample exists.
fn class_at(entity_id: u32, tick: i32, samples: &[PlayerSample]) -> Option<i64> {
    samples
        .iter()
        .filter(|s| s.entity_id == entity_id && i64::from(s.tick) <= i64::from(tick))
        .max_by_key(|s| s.tick)
        .map(|s| s.class_num)
}

/// Whether a death within the window around `end_tick` was the VIP's, NT;RE
/// class 3 (upstream at `bea96408`, `neo_enums.h:21`), ending the round for
/// the opposing team (`neo_gamerules.cpp:1551`).
fn vip_killed(end_tick: i32, evidence: &Evidence) -> bool {
    const VIP_CLASS: i64 = 3;
    evidence.deaths.iter().filter(|k| near_end(k.tick, end_tick)).any(|k| {
        evidence
            .players
            .get(&k.victim_userid)
            .and_then(|p| class_at(p.entity_id, k.tick, evidence.player_samples))
            == Some(VIP_CLASS)
    })
}

/// Whether every player on `losing_team`, by their latest sample at or
/// before `end_tick`, was dead, and at least one such player exists.
fn losers_eliminated(losing_team: i64, end_tick: i32, evidence: &Evidence) -> bool {
    let mut latest: HashMap<u32, &PlayerSample> = HashMap::new();
    for s in evidence.player_samples.iter().filter(|s| i64::from(s.tick) <= i64::from(end_tick)) {
        latest.entry(s.entity_id).and_modify(|cur| if s.tick > cur.tick { *cur = s }).or_insert(s);
    }
    let losers: Vec<_> = latest.values().filter(|s| s.team == losing_team).collect();
    !losers.is_empty() && losers.iter().all(|s| !s.alive)
}

/// The team opposite `winner` (`jinrai` team 2, `nsf` team 3, per
/// `player_samples.team`), or None when `winner` names neither.
fn losing_team(winner: &str) -> Option<i64> {
    match winner {
        "jinrai" => Some(3),
        "nsf" => Some(2),
        _ => None,
    }
}

/// Recovers a round's real win reason when a match-end text has replaced
/// it, from the RoundResult's team field and the game state at the round's
/// end tick. Checked in the order the server itself checks a round
/// (upstream at `bea96408`, `neo_gamerules.cpp:1479`, `:1551`, `:1588`,
/// `:1630`); the first that matches wins, and forfeits and points wins
/// hidden behind the match text stay unrecoverable (None).
fn recovered_reason(winner: &str, end_tick: i32, evidence: &Evidence) -> Option<String> {
    if winner == "tie" {
        return Some("tie".to_string());
    }
    if zones_switched_off(end_tick, evidence) || vip_killed(end_tick, evidence) {
        return Some("objective".to_string());
    }
    let losing_team = losing_team(winner)?;
    if losers_eliminated(losing_team, end_tick, evidence) && death_near_end(end_tick, evidence.deaths) {
        return Some("elimination".to_string());
    }
    None
}

/// Build the rounds of one demo. `announcements` supplies the start markers,
/// and the win texts when `results` is empty; `evidence` supplies the facts
/// the round-end message does not carry.
pub fn derive(
    announcements: &[Announcement],
    results: &[RoundResult],
    evidence: &Evidence,
) -> Vec<Round> {
    let start_re = Regex::new(r"ROUND (\d+) STARTED").unwrap();
    let win_re = Regex::new(r"Team (\w+) wins( [a-z ]*)?!").unwrap();

    let mut ends: Vec<End> = if results.is_empty() {
        announcements
            .iter()
            .filter_map(|a| {
                let c = win_re.captures(&a.text)?;
                if is_match_end_text(&a.text) {
                    // Without a RoundResult's team field, a match-end
                    // announcement gives no way to recover this round's own
                    // winner or reason: the name it carries is the match's.
                    Some(End { tick: a.tick, winner: None, reason: None })
                } else {
                    Some(End {
                        tick: a.tick,
                        // Lowercased to match round_results.team, which this
                        // fallback path stands in for when a demo carries
                        // no RoundResult messages at all.
                        winner: Some(c[1].to_lowercase()),
                        reason: win_reason_code(&a.text),
                    })
                }
            })
            .collect()
    } else {
        results
            .iter()
            .map(|r| {
                let winner = r.team.clone();
                let reason = if is_match_end_text(&r.message) {
                    recovered_reason(&winner, r.tick, evidence)
                } else {
                    win_reason_code(&r.message)
                };
                End { tick: r.tick, winner: Some(winner), reason }
            })
            .collect()
    };
    ends.sort_by_key(|e| e.tick);

    let starts = announcements.iter().filter_map(|a| {
        let c = start_re.captures(&a.text)?;
        Some((a.tick, c[1].parse::<i32>().ok()))
    });

    let mut rounds: Vec<Round> = Vec::new();
    let mut open: Option<Round> = None;
    let mut ends = ends.into_iter().peekable();
    let close = |open: &mut Option<Round>, end: End, rounds: &mut Vec<Round>| {
        let mut r = open.take().unwrap_or_else(Round::unstarted);
        r.end_tick = Some(end.tick);
        r.winner = end.winner;
        r.reason = end.reason;
        rounds.push(r);
    };
    for (start_tick, number) in starts {
        while let Some(end) = ends.next_if(|e| e.tick < start_tick) {
            close(&mut open, end, &mut rounds);
        }
        if let Some(r) = open.take() {
            rounds.push(r); // previous round never ended (aborted or cut off)
        }
        open = Some(Round { number, start_tick: Some(start_tick), ..Round::unstarted() });
    }
    for end in ends {
        close(&mut open, end, &mut rounds);
    }
    if let Some(r) = open {
        rounds.push(r);
    }

    // A round that ended before the first start marker is the one prior to it.
    let numbers: Vec<Option<i32>> = rounds.iter().map(|r| r.number).collect();
    for (i, r) in rounds.iter_mut().enumerate() {
        if r.number.is_none() {
            r.number = numbers
                .get(i + 1)
                .copied()
                .flatten()
                .map(|next| next - 1)
                .or(Some(i as i32 + 1));
        }
        if let Some(end_tick) = r.end_tick {
            r.capturer_userid = capturer(end_tick, evidence);
        }
    }
    if let Some(first) = rounds.first_mut() {
        first.start_tick = first.start_tick.or_else(|| first_freeze(first, evidence));
    }
    rounds
}

/// The start tick of a first round whose start text the recording missed,
/// or None when the recording began after the round's freeze time.
///
/// Auto-recording starts on the `round_start` event, after the start text
/// has gone out (upstream at `bea96408`, `neo_gamerules.cpp:1935` for a
/// player's recording, `:1946` and `neo_misc.cpp:130` for SourceTV).
/// Whether the text reaches the recording varies from match to match. The
/// first snapshot shows the game rules in the round's freeze time, on the
/// tick the text lands on when it does arrive.
fn first_freeze(round: &Round, evidence: &Evidence) -> Option<i32> {
    let freeze = evidence.freeze_starts.first()?;
    let tick = i32::try_from(freeze.tick).ok()?;
    let same_round = round.number == Some(freeze.round_number);
    let before_end = round.end_tick.is_none_or(|end| tick <= end);
    (same_round && before_end).then_some(tick)
}

/// Drop the rounds after round `last` and return the tick to cut the
/// demo's other rows at, or None when no later round began, so nothing
/// needs cutting.
///
/// The cut is the start tick of the first round numbered after `last`. The
/// server fires that round's `round_start` event, respawns the players and
/// prints its start text in one call (upstream at `bea96408`,
/// `neo_gamerules.cpp:2879`, `:3004`, `:3032`), so all of the next round lies
/// at or after that tick, while round `last` keeps its end and the time after
/// it. A repeated number, as after an admin pause, is not later, so a
/// restarted round `last` stays.
pub fn cut_after(rounds: &mut Vec<Round>, last: i32) -> Result<Option<i32>> {
    let Some(i) = rounds.iter().position(|r| r.number.is_some_and(|n| n > last)) else {
        return Ok(None);
    };
    let next = rounds[i].number.unwrap_or_default();
    if i == 0 {
        bail!("the recording has no round up to round {last}; it begins in round {next}");
    }
    let Some(cut) = rounds[i].start_tick else {
        bail!("the start of round {next} was not recorded, so there is no tick to cut at");
    };
    rounds.truncate(i);
    Ok(Some(cut))
}

/// The round a tick belongs to, as `place` finds it.
#[derive(Debug, PartialEq)]
pub struct RoundPlace {
    /// The round's number, as in `Round::number`.
    pub number: Option<i32>,
    /// The round's position in the demo's rounds, counting from 1. The
    /// number repeats when a round restarts after an admin pause or a match
    /// restart, so this is the key that names one round.
    pub index: u32,
    /// The tick falls after the round's end and before the next round's
    /// start, while the game shows the result.
    pub post_round: bool,
}

/// Where `tick` falls among `rounds`, which `derive` returns in tick order.
/// None means the tick comes before the first round began.
///
/// A round covers its start tick through its end tick. The death that ends
/// an elimination round lands on the end tick or just before it, so it
/// counts as part of the round. A round with no recorded start covers everything after the
/// round before it, and a round that never ended runs until the next start.
/// The time between a round's end and the next start is that round's post
/// round.
pub fn place(rounds: &[Round], tick: i32) -> Option<RoundPlace> {
    // The last tick round `i` covers before its post round, if bounded.
    let last_tick = |i: usize| {
        rounds[i].end_tick.or_else(|| rounds.get(i + 1).and_then(|r| r.start_tick).map(|s| s - 1))
    };
    let current = (0..rounds.len()).find(|&i| last_tick(i).is_none_or(|last| tick <= last));
    let previous = match current {
        Some(i) if rounds[i].start_tick.is_none_or(|start| start <= tick) => {
            return Some(RoundPlace { number: rounds[i].number, index: ordinal(i), post_round: false });
        }
        Some(i) => i.checked_sub(1)?,
        None => rounds.len().checked_sub(1)?,
    };
    Some(RoundPlace { number: rounds[previous].number, index: ordinal(previous), post_round: true })
}

/// Give each round the ghost's position on its start tick. The game prints
/// the start banner and moves the ghost to a spawn point in the same call
/// (upstream at `bea96408`, `neo_gamerules.cpp:3031-3034`, reaching
/// `SpawnTheGhost` through `:3451-3459`), so the ghost's first sample of the
/// round lands on the start tick. A round with no start tick, or in a mode
/// without a ghost, keeps None.
pub fn place_ghost_spawns(rounds: &mut [Round], ghost_samples: &[GhostSample]) {
    for round in rounds {
        let Some(start) = round.start_tick else { continue };
        round.ghost_spawn = ghost_samples
            .iter()
            .find(|g| i64::from(g.tick) == i64::from(start))
            .map(|g| [g.x, g.y, g.z]);
    }
}

/// The 1-based position stored as `rounds.round_index` for `rounds[i]`.
pub fn ordinal(i: usize) -> u32 {
    u32::try_from(i + 1).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ann(tick: i32, text: &str) -> Announcement {
        Announcement { tick, text: text.to_string() }
    }

    fn res(tick: i32, team: &str, message: &str) -> RoundResult {
        RoundResult { tick, team: team.to_string(), message: message.to_string() }
    }

    /// Rounds from text and round results alone, as when the entity pass fails.
    fn derive_without_entities(anns: &[Announcement], results: &[RoundResult]) -> Vec<Round> {
        derive(anns, results, &Evidence::new(None, &HashMap::new(), &[]))
    }

    fn player(user_id: u32, entity_id: u32, first_seen_tick: i32) -> (u32, Player) {
        let p = Player {
            entity_id,
            user_id,
            name: String::new(),
            steam_id: String::new(),
            is_bot: false,
            first_seen_tick,
            neo_name: None,
        };
        (user_id, p)
    }

    fn carrier(tick: u32, entity_id: u32) -> CarrierChange {
        CarrierChange { tick, entity_id }
    }

    /// A player sample with only the fields the match-end rules read set;
    /// the rest take harmless defaults.
    fn sample(tick: u32, entity_id: u32, team: i64, class_num: i64, alive: bool) -> PlayerSample {
        PlayerSample {
            tick,
            entity_id,
            x: 0.0,
            y: 0.0,
            z: 0.0,
            eye_pitch: 0.0,
            eye_yaw: 0.0,
            vx: 0.0,
            vy: 0.0,
            vz: 0.0,
            weapon: None,
            health: if alive { 100 } else { -1 },
            team,
            class_num,
            camo: false,
            alive,
            in_pvs: true,
        }
    }

    fn death(tick: i32, victim_userid: u32, attacker_userid: u32) -> Death {
        Death {
            tick,
            victim_userid,
            attacker_userid,
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

    /// The one round from a start at tick 2 to a RoundResult naming `team`
    /// and carrying `message` at tick 100, with `evidence` behind it.
    fn match_end_round(team: &str, message: &str, evidence: &Evidence) -> Round {
        let anns = [ann(2, "- CTG ROUND 1 STARTED -")];
        let results = [res(100, team, message)];
        let mut rounds = derive(&anns, &results, evidence);
        assert_eq!(rounds.len(), 1);
        rounds.remove(0)
    }

    /// One round from tick 2 to tick 100, with capture zones 50 and 51
    /// switched on at tick 3 and off at each `(tick, zone)` in `zone_offs`.
    fn capturer_of(
        carriers: &[CarrierChange],
        zone_offs: &[(u32, u32)],
        players: &HashMap<u32, Player>,
    ) -> Option<u32> {
        let anns = [ann(2, "- CTG ROUND 1 STARTED -")];
        let results = [res(100, "nsf", "Team NSF wins by capturing the ghost!")];
        let mut zones = vec![
            ZoneChange { tick: 3, entity_id: 50, active: true },
            ZoneChange { tick: 3, entity_id: 51, active: true },
        ];
        for &(tick, entity_id) in zone_offs {
            zones.push(ZoneChange { tick, entity_id, active: false });
        }
        let evidence = Evidence {
            carrier_changes: carriers,
            zone_changes: &zones,
            players,
            player_samples: &[],
            deaths: &[],
            freeze_starts: &[],
        };
        let rounds = derive(&anns, &results, &evidence);
        assert_eq!(rounds.len(), 1);
        rounds[0].capturer_userid
    }

    #[test]
    fn results_supply_winners_including_ties() {
        let anns = [
            ann(2, "- CTG ROUND 1 STARTED -"),
            ann(100, "- CTG ROUND 2 STARTED -"),
            ann(200, "- CTG ROUND 3 STARTED -"),
        ];
        let results = [res(90, "tie", "TIE"), res(190, "nsf", "Team NSF wins by capturing the ghost!")];
        let rounds = derive_without_entities(&anns, &results);
        assert_eq!(rounds.len(), 3);
        assert_eq!(
            (rounds[0].number, rounds[0].end_tick, rounds[0].winner.as_deref(), rounds[0].reason.as_deref()),
            (Some(1), Some(90), Some("tie"), Some("tie"))
        );
        // Without entity state a capture win names no capturer.
        assert_eq!(
            (rounds[1].winner.as_deref(), rounds[1].reason.as_deref(), rounds[1].capturer_userid),
            (Some("nsf"), Some("objective"), None)
        );
        assert_eq!((rounds[2].number, rounds[2].end_tick), (Some(3), None));
    }

    /// Rounds from the given announcements and freeze starts, with no other
    /// entity state.
    fn derive_with_freezes(anns: &[Announcement], results: &[RoundResult], freezes: &[FreezeStart]) -> Vec<Round> {
        let roster = HashMap::new();
        let evidence = Evidence { freeze_starts: freezes, ..Evidence::new(None, &roster, &[]) };
        derive(anns, results, &evidence)
    }

    #[test]
    fn first_round_without_start_text_starts_at_its_freeze() {
        let anns = [ann(1000, "- CTG ROUND 2 STARTED -")];
        let results = [res(900, "nsf", "Team NSF wins by eliminating the other team!")];
        let freezes = [FreezeStart { tick: 4, round_number: 1 }, FreezeStart { tick: 1000, round_number: 2 }];
        let rounds = derive_with_freezes(&anns, &results, &freezes);
        let starts: Vec<_> = rounds.iter().map(|r| (r.number, r.start_tick)).collect();
        assert_eq!(starts, [(Some(1), Some(4)), (Some(2), Some(1000))]);
    }

    #[test]
    fn first_round_joined_after_its_freeze_keeps_no_start() {
        // The recording began while round 1 was live, so the first freeze
        // it saw is round 2's.
        let anns = [ann(1000, "- CTG ROUND 2 STARTED -")];
        let results = [res(900, "nsf", "Team NSF wins by eliminating the other team!")];
        let freezes = [FreezeStart { tick: 1000, round_number: 2 }];
        let rounds = derive_with_freezes(&anns, &results, &freezes);
        assert_eq!((rounds[0].number, rounds[0].start_tick), (Some(1), None));
    }

    #[test]
    fn start_text_wins_over_the_freeze() {
        let anns = [ann(3, "- CTG ROUND 1 STARTED -")];
        let freezes = [FreezeStart { tick: 2, round_number: 1 }];
        let rounds = derive_with_freezes(&anns, &[], &freezes);
        assert_eq!(rounds[0].start_tick, Some(3));
    }

    #[test]
    fn aborted_round_stays_open_when_restarted() {
        let anns = [ann(2, "- CTG ROUND 9 STARTED -"), ann(50, "- CTG ROUND 9 STARTED -")];
        let results = [res(120, "jinrai", "Team Jinrai wins by eliminating the other team!")];
        let rounds = derive_without_entities(&anns, &results);
        assert_eq!(rounds.len(), 2);
        assert_eq!(
            (rounds[0].start_tick, rounds[0].end_tick, rounds[0].winner.as_deref()),
            (Some(2), None, None)
        );
        assert_eq!(
            (rounds[1].start_tick, rounds[1].end_tick, rounds[1].winner.as_deref()),
            (Some(50), Some(120), Some("jinrai"))
        );
    }

    #[test]
    fn falls_back_to_win_announcements() {
        let anns = [
            ann(10, "Team NSF wins the match!"),
            ann(20, "- CTG ROUND 4 STARTED -"),
            ann(90, "Team Jinrai wins by numbers!"),
        ];
        let rounds = derive_without_entities(&anns, &[]);
        assert_eq!(rounds.len(), 2);
        // "Team NSF wins the match!" names the match's winner, not round 3's,
        // so neither the winner nor the reason is recoverable from it alone.
        assert_eq!(
            (rounds[0].number, rounds[0].winner.as_deref(), rounds[0].reason.as_deref()),
            (Some(3), None, None)
        );
        assert_eq!((rounds[1].number, rounds[1].winner.as_deref()), (Some(4), Some("jinrai")));
    }

    #[test]
    fn fallback_match_end_announcement_leaves_winner_and_reason_null() {
        let anns = [ann(10, "- CTG ROUND 5 STARTED -"), ann(90, "Team NSF wins the match!")];
        let rounds = derive_without_entities(&anns, &[]);
        assert_eq!(rounds.len(), 1);
        assert_eq!(
            (rounds[0].winner.as_deref(), rounds[0].reason.as_deref()),
            (None, None)
        );
    }

    #[test]
    fn win_reason_code_covers_every_victory_text() {
        let cases = [
            ("Team NSF wins by capturing the ghost!\n", Some("objective")),
            ("Team Jinrai wins by escorting the vip!\n", Some("objective")),
            ("Team NSF wins by eliminating the vip!\n", Some("objective")),
            ("Team Jinrai wins by eliminating the other team!\n", Some("elimination")),
            ("Team NSF wins by highest score!\n", Some("score")),
            ("Team Jinrai wins by numbers!\n", Some("score")),
            ("SomePlayer is the winner of the deathmatch!\n", Some("score")),
            ("Team NSF wins by forfeit!\n", Some("forfeit")),
            ("TIE\n", Some("tie")),
            ("Team NSF wins the match!\n", None),
            ("The match is tied!\n", None),
            ("Next round: Sudden death!\n", None),
            ("", None),
            ("Unknown Neotokyo victory reason 7\n", None),
        ];
        for (message, expected) in cases {
            assert_eq!(
                win_reason_code(message).as_deref(),
                expected,
                "message: {message:?}"
            );
        }
    }

    #[test]
    fn match_end_tie_gives_tie() {
        let roster = HashMap::new();
        let evidence = Evidence::new(None, &roster, &[]);
        let r = match_end_round("tie", "Next round: Sudden death!", &evidence);
        assert_eq!((r.winner.as_deref(), r.reason.as_deref()), (Some("tie"), Some("tie")));
    }

    #[test]
    fn match_end_texts_take_the_same_path() {
        let roster = HashMap::new();
        let evidence = Evidence::new(None, &roster, &[]);
        let tied = match_end_round("tie", "The match is tied!", &evidence);
        let sudden_death = match_end_round("tie", "Next round: Sudden death!", &evidence);
        let expected = (Some("tie"), Some("tie"));
        assert_eq!((tied.winner.as_deref(), tied.reason.as_deref()), expected);
        assert_eq!((sudden_death.winner.as_deref(), sudden_death.reason.as_deref()), expected);
    }

    #[test]
    fn match_end_zones_off_gives_objective() {
        let zones = [
            ZoneChange { tick: 3, entity_id: 50, active: true },
            ZoneChange { tick: 100, entity_id: 50, active: false },
        ];
        let evidence = Evidence {
            carrier_changes: &[],
            zone_changes: &zones,
            players: &HashMap::new(),
            player_samples: &[],
            deaths: &[],
            freeze_starts: &[],
        };
        let r = match_end_round("nsf", "Team NSF wins the match!", &evidence);
        assert_eq!((r.winner.as_deref(), r.reason.as_deref()), (Some("nsf"), Some("objective")));
    }

    #[test]
    fn match_end_killed_vip_gives_objective() {
        let players = HashMap::from([player(11, 21, 0)]);
        // The victim's class at the death tick is 3, the VIP.
        let samples = [sample(50, 21, 3, 3, false)];
        let deaths = [death(101, 11, 5)];
        let evidence = Evidence {
            carrier_changes: &[],
            zone_changes: &[],
            players: &players,
            player_samples: &samples,
            deaths: &deaths,
            freeze_starts: &[],
        };
        let r = match_end_round("jinrai", "Team Jinrai wins the match!", &evidence);
        assert_eq!((r.winner.as_deref(), r.reason.as_deref()), (Some("jinrai"), Some("objective")));
    }

    #[test]
    fn match_end_losers_dead_with_death_gives_elimination() {
        // Winner Jinrai (team 2); the losing team, NSF (team 3), has two
        // players, both dead by their last sample at or before the end tick.
        let players = HashMap::from([player(1, 10, 0), player(2, 11, 0)]);
        let samples = [sample(50, 10, 3, 0, false), sample(50, 11, 3, 0, false)];
        let deaths = [death(99, 1, 5)];
        let evidence = Evidence {
            carrier_changes: &[],
            zone_changes: &[],
            players: &players,
            player_samples: &samples,
            deaths: &deaths,
            freeze_starts: &[],
        };
        let r = match_end_round("jinrai", "Team Jinrai wins the match!", &evidence);
        assert_eq!((r.winner.as_deref(), r.reason.as_deref()), (Some("jinrai"), Some("elimination")));
    }

    #[test]
    fn match_end_losers_dead_without_death_in_window_gives_null() {
        let players = HashMap::from([player(1, 10, 0)]);
        let samples = [sample(50, 10, 3, 0, false)];
        let evidence = Evidence {
            carrier_changes: &[],
            zone_changes: &[],
            players: &players,
            player_samples: &samples,
            deaths: &[],
            freeze_starts: &[],
        };
        let r = match_end_round("jinrai", "Team Jinrai wins the match!", &evidence);
        assert_eq!((r.winner.as_deref(), r.reason.as_deref()), (Some("jinrai"), None));
    }

    #[test]
    fn match_end_without_evidence_gives_null() {
        let roster = HashMap::new();
        let evidence = Evidence::new(None, &roster, &[]);
        let r = match_end_round("jinrai", "Team Jinrai wins the match!", &evidence);
        assert_eq!((r.winner.as_deref(), r.reason.as_deref()), (Some("jinrai"), None));
    }

    /// Rounds 1 to 3 from start announcements at ticks 10, 100 and 200,
    /// with round 2 aborted and restarted at tick 150.
    fn rounds_with_restart() -> Vec<Round> {
        let anns = [
            ann(10, "- CTG ROUND 1 STARTED -"),
            ann(100, "- CTG ROUND 2 STARTED -"),
            ann(150, "- CTG ROUND 2 STARTED -"),
            ann(200, "- CTG ROUND 3 STARTED -"),
        ];
        let results = [res(90, "nsf", "TIE"), res(190, "jinrai", "TIE")];
        derive_without_entities(&anns, &results)
    }

    #[test]
    fn cut_after_keeps_rounds_up_to_last_and_cuts_at_the_next_start() {
        let mut rounds = rounds_with_restart();
        assert_eq!(cut_after(&mut rounds, 2).unwrap(), Some(200));
        let kept: Vec<_> = rounds.iter().map(|r| (r.number, r.start_tick)).collect();
        assert_eq!(kept, [(Some(1), Some(10)), (Some(2), Some(100)), (Some(2), Some(150))]);

        let mut rounds = rounds_with_restart();
        assert_eq!(cut_after(&mut rounds, 1).unwrap(), Some(100));
        assert_eq!(rounds.len(), 1);
    }

    #[test]
    fn cut_after_the_last_recorded_round_cuts_nothing() {
        let mut rounds = rounds_with_restart();
        assert_eq!(cut_after(&mut rounds, 3).unwrap(), None);
        assert_eq!(rounds.len(), 4);
        assert_eq!(cut_after(&mut rounds, 9).unwrap(), None);
    }

    fn at(rounds: &[Round], tick: i32) -> Option<(i32, bool)> {
        place(rounds, tick).map(|p| (p.number.unwrap(), p.post_round))
    }

    #[test]
    fn place_puts_end_ticks_in_the_round_and_the_gap_after_it_in_post_round() {
        let rounds = rounds_with_restart();
        assert_eq!(at(&rounds, 5), None);
        assert_eq!(at(&rounds, 10), Some((1, false)));
        assert_eq!(at(&rounds, 90), Some((1, false)));
        assert_eq!(at(&rounds, 91), Some((1, true)));
        assert_eq!(at(&rounds, 99), Some((1, true)));
        assert_eq!(at(&rounds, 190), Some((2, false)));
        assert_eq!(at(&rounds, 195), Some((2, true)));
        assert_eq!(at(&rounds, 200), Some((3, false)));
    }

    #[test]
    fn place_gives_a_restarted_round_its_own_index() {
        // Round 2 is aborted at its start and restarted, so its number
        // repeats while the index does not.
        let rounds = rounds_with_restart();
        let indices: Vec<_> =
            [10, 120, 150, 200].iter().map(|&t| place(&rounds, t).map(|p| (p.number, p.index))).collect();
        assert_eq!(indices, [Some((Some(1), 1)), Some((Some(2), 2)), Some((Some(2), 3)), Some((Some(3), 4))]);
    }

    #[test]
    fn place_runs_an_aborted_round_until_its_restart() {
        let rounds = rounds_with_restart();
        assert_eq!(place(&rounds, 149), Some(RoundPlace { number: Some(2), index: 2, post_round: false }));
        assert_eq!(at(&rounds, 150), Some((2, false)));
        // The last round never ended, so it runs to the end of the demo.
        assert_eq!(at(&rounds, 9000), Some((3, false)));
    }

    #[test]
    fn place_gives_a_round_without_a_start_everything_before_its_end() {
        let anns = [ann(100, "- CTG ROUND 2 STARTED -")];
        let results = [res(50, "nsf", "TIE"), res(190, "jinrai", "TIE")];
        let rounds = derive_without_entities(&anns, &results);
        assert_eq!(at(&rounds, 0), Some((1, false)));
        assert_eq!(at(&rounds, 50), Some((1, false)));
        assert_eq!(at(&rounds, 60), Some((1, true)));
        assert_eq!(at(&rounds, 100), Some((2, false)));
        assert_eq!(at(&rounds, 300), Some((2, true)));
        assert_eq!(place(&[], 300), None);
    }

    #[test]
    fn cut_after_a_round_before_the_recording_fails() {
        let anns = [ann(10, "- CTG ROUND 5 STARTED -")];
        let mut rounds = derive_without_entities(&anns, &[]);
        let err = cut_after(&mut rounds, 4).unwrap_err().to_string();
        assert!(err.contains("begins in round 5"), "{err}");
    }

    #[test]
    fn capture_names_the_carrier() {
        // Entity slot 4 passed from userid 3 to userid 9 before the capture,
        // and to userid 12 after it.
        let players = HashMap::from([player(3, 4, 0), player(9, 4, 20), player(12, 4, 150), player(5, 6, 0)]);
        let carriers = [carrier(40, 6), carrier(60, 4)];
        assert_eq!(capturer_of(&carriers, &[(100, 50), (101, 51)], &players), Some(9));
    }

    #[test]
    fn live_carrier_with_zones_active_is_not_a_capture() {
        let players = HashMap::from([player(7, 4, 0)]);
        let carriers = [carrier(60, 4)];
        assert_eq!(capturer_of(&carriers, &[], &players), None);
        // One zone switching off is not enough while another stays active.
        assert_eq!(capturer_of(&carriers, &[(100, 50)], &players), None);
    }

    #[test]
    fn carrier_cleared_on_capture_tick_falls_back_to_previous() {
        let players = HashMap::from([player(7, 4, 0)]);
        let offs = [(100, 50), (100, 51)];
        assert_eq!(capturer_of(&[carrier(60, 4), carrier(100, 0)], &offs, &players), Some(7));
        // A carrier cleared before the end tick carried nothing into a zone.
        assert_eq!(capturer_of(&[carrier(60, 4), carrier(99, 0)], &offs, &players), None);
    }

    #[test]
    fn zones_switching_off_without_a_carrier_is_not_a_capture() {
        let players = HashMap::from([player(7, 4, 0)]);
        assert_eq!(capturer_of(&[], &[(100, 50), (100, 51)], &players), None);
    }

    #[test]
    fn ghost_spawns_come_from_the_start_tick_sample() {
        let ghost = |tick: u32, x: f32| GhostSample { tick, entity_id: 90, x, y: 0.0, z: 0.0 };
        let mut rounds = vec![
            Round::unstarted(), // the recording began mid-round
            Round { start_tick: Some(100), ..Round::unstarted() },
            Round { start_tick: Some(200), ..Round::unstarted() }, // no ghost this round
        ];
        // The ghost lies at x = 5 before round 2 and moves to its spawn on the start tick.
        place_ghost_spawns(&mut rounds, &[ghost(50, 5.0), ghost(100, 1.0), ghost(102, 2.0)]);
        let spawns: Vec<_> = rounds.iter().map(|r| r.ghost_spawn).collect();
        assert_eq!(spawns, [None, Some([1.0, 0.0, 0.0]), None]);
    }
}
