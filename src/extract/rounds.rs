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
use super::entities::{CarrierChange, EntityOutput, ZoneChange};
use super::net::{Player, RoundResult};

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
}

impl<'a> Evidence<'a> {
    /// Collect the evidence from the entity pass, which is `None` when that
    /// pass failed, and the roster.
    pub fn new(entities: Option<&'a EntityOutput>, players: &'a HashMap<u32, Player>) -> Self {
        Evidence {
            carrier_changes: entities.map(|e| &e.carrier_changes[..]).unwrap_or_default(),
            zone_changes: entities.map(|e| &e.zone_changes[..]).unwrap_or_default(),
            players,
        }
    }
}

/// Ticks either side of a round end within which the capture zones must
/// switch off. The game switches them off in the think that ends the round,
/// so the window only absorbs a change landing in a neighbouring packet.
const CAPTURE_WINDOW: i64 = 2;

/// The userid of the ghost capturer for a round ending at `end_tick`, or
/// None when the round was not won by a capture.
///
/// On a capture the game switches every capture zone off in the think that
/// announces the win (upstream `neo_gamerules.cpp:1526-1547`), so a capture
/// is a round end where every zone that was active before the window
/// switched off within it. A VIP escort switches the zones off too
/// (`:1643-1656`), but it runs only while no ghost exists, so the carrier
/// is 0 and the rule gives None. The carrier is the one at the end tick, or
/// the one on the tick before when it was cleared at the end tick.
fn capturer(end_tick: i32, evidence: &Evidence) -> Option<u32> {
    let end = i64::from(end_tick);
    // Each zone's state before the window opens and after it closes.
    let mut zones: HashMap<u32, (bool, bool)> = HashMap::new();
    for z in evidence.zone_changes {
        let tick = i64::from(z.tick);
        let (before, after) = zones.entry(z.entity_id).or_default();
        if tick < end - CAPTURE_WINDOW {
            *before = z.active;
        }
        if tick <= end + CAPTURE_WINDOW {
            *after = z.active;
        }
    }
    let was_active: Vec<bool> =
        zones.values().filter(|(before, _)| *before).map(|(_, after)| *after).collect();
    if was_active.is_empty() || was_active.contains(&true) {
        return None;
    }

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

/// A round end from either source, normalized.
struct End {
    tick: i32,
    winner: String,
    reason: Option<String>,
}

/// Maps a round-end message to a short win-reason code.
///
/// The phrases are the victory strings NEO_VICTORY_* builds in upstream
/// NeotokyoRebuild/neo's `src/game/shared/neo/neo_gamerules.cpp` (lines
/// 3864-3899), plus the deathmatch message next to it (line 1733). NULL
/// means the reason could not be identified: an empty message, or a
/// match-end text such as "wins the match", which replaces the reason.
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
                Some(End { tick: a.tick, winner: c[1].to_string(), reason: win_reason_code(&a.text) })
            })
            .collect()
    } else {
        results
            .iter()
            .map(|r| {
                let winner = match r.team.as_str() {
                    "jinrai" => "Jinrai".to_string(),
                    "nsf" => "NSF".to_string(),
                    "tie" => "Tie".to_string(),
                    other => other.to_string(),
                };
                let reason = win_reason_code(&r.message);
                End { tick: r.tick, winner, reason }
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
        r.winner = Some(end.winner);
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
        derive(anns, results, &Evidence::new(None, &HashMap::new()))
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
        let evidence = Evidence { carrier_changes: carriers, zone_changes: &zones, players };
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
            (Some(1), Some(90), Some("Tie"), Some("tie"))
        );
        // Without entity state a capture win names no capturer.
        assert_eq!(
            (rounds[1].winner.as_deref(), rounds[1].reason.as_deref(), rounds[1].capturer_userid),
            (Some("NSF"), Some("objective"), None)
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
            (Some(50), Some(120), Some("Jinrai"))
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
        assert_eq!(
            (rounds[0].number, rounds[0].winner.as_deref(), rounds[0].reason.as_deref()),
            (Some(3), Some("NSF"), None)
        );
        assert_eq!((rounds[1].number, rounds[1].winner.as_deref()), (Some(4), Some("Jinrai")));
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
