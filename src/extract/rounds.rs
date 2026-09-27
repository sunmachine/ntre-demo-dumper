//! Round reconstruction from start markers, round-end messages and entity
//! state.
//!
//! Pairs "ROUND N STARTED" announcements with round ends. Ends come from the
//! server's RoundResult user message when the demo carries it (it names the
//! winner, and "tie" for stalemates, which no on-screen text announces);
//! otherwise from "Team X wins ..." announcements. Handles demos that begin
//! mid-round (first round has no start marker) or end mid-round (last round
//! has no end). Facts the round-end message does not carry, such as who
//! captured the ghost, come from the entity state around each end.

use regex::Regex;
use std::collections::HashMap;

use super::announcements::Announcement;
use super::entities::{CarrierChange, EntityOutput, PlayerSample, ZoneChange};
use super::net::{Kill, Player, RoundResult};

pub struct Round {
    pub number: Option<i32>,
    pub start_tick: Option<i32>,
    pub end_tick: Option<i32>,
    pub winner: Option<String>,
    pub reason: Option<String>,
    /// The userid of the player who carried the ghost into the capture zone.
    pub capturer_userid: Option<u32>,
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
        }
    }
}

/// Game state that the rules in this module read around each round end.
/// Each field borrows what another pass produced, so a rule that needs more
/// end-of-round state, such as alive players or kills, adds a field here and
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
    /// The kill feed, for a kill landing near a round's end.
    pub kills: &'a [Kill],
}

impl<'a> Evidence<'a> {
    /// Collect the evidence from the entity pass, which is `None` when that
    /// pass failed, and the net pass's roster and kill feed.
    pub fn new(
        entities: Option<&'a EntityOutput>,
        players: &'a HashMap<u32, Player>,
        kills: &'a [Kill],
    ) -> Self {
        Evidence {
            carrier_changes: entities.map(|e| &e.carrier_changes[..]).unwrap_or_default(),
            zone_changes: entities.map(|e| &e.zone_changes[..]).unwrap_or_default(),
            players,
            player_samples: entities.map(|e| &e.samples[..]).unwrap_or_default(),
            kills,
        }
    }
}

/// Ticks either side of a round end within which the capture zones must
/// switch off, a VIP's death must fall, or a kill must fall, for that fact
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
/// every zone this way when they end a round (upstream
/// `neo_gamerules.cpp:1526-1547`, `:1643-1656`).
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

    // An entity slot passes to a later joiner when its player leaves, so the
    // latest arrival at or before the end tick holds it.
    evidence
        .players
        .values()
        .filter(|p| p.entity_id == carrier && p.first_seen_tick <= end_tick)
        .max_by_key(|p| (p.first_seen_tick, p.user_id))
        .map(|p| p.user_id)
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
/// leads into sudden death, instead of the round's own reason (upstream
/// `neo_gamerules.cpp:3813-3866`).
fn is_match_end_text(message: &str) -> bool {
    ["wins the match", "The match is tied!", "Next round: Sudden death!"]
        .iter()
        .any(|text| message.contains(text))
}

/// Maps a round-end message to a short win-reason code.
///
/// The phrases are the victory strings NEO_VICTORY_* builds in upstream
/// NeotokyoRebuild/neo's `src/game/shared/neo/neo_gamerules.cpp` (lines
/// 3864-3899), plus the deathmatch message next to it (line 1733). NULL
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

/// Whether a kill in `kills` fell within `END_WINDOW` of `end_tick`.
fn kill_near_end(end_tick: i32, kills: &[Kill]) -> bool {
    kills.iter().any(|k| near_end(k.tick, end_tick))
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

/// Whether a kill within the window around `end_tick` downed the VIP, NT;RE
/// class 3 (`neo_enums.h:21`), ending the round for the opposing team
/// (upstream `neo_gamerules.cpp:1619`).
fn vip_killed(end_tick: i32, evidence: &Evidence) -> bool {
    const VIP_CLASS: i64 = 3;
    evidence.kills.iter().filter(|k| near_end(k.tick, end_tick)).any(|k| {
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
/// (`neo_gamerules.cpp:1547`, `:1589`, `:1698`); the first that matches
/// wins, and forfeits and points wins hidden behind the match text stay
/// unrecoverable (None).
fn recovered_reason(winner: &str, end_tick: i32, evidence: &Evidence) -> Option<String> {
    if winner == "tie" {
        return Some("tie".to_string());
    }
    if zones_switched_off(end_tick, evidence) || vip_killed(end_tick, evidence) {
        return Some("objective".to_string());
    }
    let losing_team = losing_team(winner)?;
    if losers_eliminated(losing_team, end_tick, evidence) && kill_near_end(end_tick, evidence.kills) {
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
    rounds
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

    fn kill(tick: i32, victim_userid: u32, attacker_userid: u32) -> Kill {
        Kill {
            tick,
            victim_userid,
            attacker_userid,
            assists: 0,
            weapon: String::new(),
            headshot: false,
            suicide: false,
            explosive: false,
            ghoster: false,
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
            kills: &[],
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
            kills: &[],
        };
        let r = match_end_round("nsf", "Team NSF wins the match!", &evidence);
        assert_eq!((r.winner.as_deref(), r.reason.as_deref()), (Some("nsf"), Some("objective")));
    }

    #[test]
    fn match_end_killed_vip_gives_objective() {
        let players = HashMap::from([player(11, 21, 0)]);
        // The victim's class at the kill tick is 3, the VIP.
        let samples = [sample(50, 21, 3, 3, false)];
        let kills = [kill(101, 11, 5)];
        let evidence = Evidence {
            carrier_changes: &[],
            zone_changes: &[],
            players: &players,
            player_samples: &samples,
            kills: &kills,
        };
        let r = match_end_round("jinrai", "Team Jinrai wins the match!", &evidence);
        assert_eq!((r.winner.as_deref(), r.reason.as_deref()), (Some("jinrai"), Some("objective")));
    }

    #[test]
    fn match_end_losers_dead_with_kill_gives_elimination() {
        // Winner Jinrai (team 2); the losing team, NSF (team 3), has two
        // players, both dead by their last sample at or before the end tick.
        let players = HashMap::from([player(1, 10, 0), player(2, 11, 0)]);
        let samples = [sample(50, 10, 3, 0, false), sample(50, 11, 3, 0, false)];
        let kills = [kill(99, 1, 5)];
        let evidence = Evidence {
            carrier_changes: &[],
            zone_changes: &[],
            players: &players,
            player_samples: &samples,
            kills: &kills,
        };
        let r = match_end_round("jinrai", "Team Jinrai wins the match!", &evidence);
        assert_eq!((r.winner.as_deref(), r.reason.as_deref()), (Some("jinrai"), Some("elimination")));
    }

    #[test]
    fn match_end_losers_dead_without_kill_in_window_gives_null() {
        let players = HashMap::from([player(1, 10, 0)]);
        let samples = [sample(50, 10, 3, 0, false)];
        let evidence = Evidence {
            carrier_changes: &[],
            zone_changes: &[],
            players: &players,
            player_samples: &samples,
            kills: &[],
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
}
