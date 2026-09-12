//! Round reconstruction from start markers and round-end messages.
//!
//! Pairs "ROUND N STARTED" announcements with round ends. Ends come from the
//! server's RoundResult user message when the demo carries it (it names the
//! winner, and "tie" for stalemates, which no on-screen text announces);
//! otherwise from "Team X wins ..." announcements. Handles demos that begin
//! mid-round (first round has no start marker) or end mid-round (last round
//! has no end).

use regex::Regex;

use super::announcements::Announcement;
use super::net::RoundResult;

pub struct Round {
    pub number: Option<i32>,
    pub start_tick: Option<i32>,
    pub end_tick: Option<i32>,
    pub winner: Option<String>,
    pub reason: Option<String>,
}

impl Round {
    fn unstarted() -> Self {
        Round { number: None, start_tick: None, end_tick: None, winner: None, reason: None }
    }
}

/// A round end from either source, normalized.
struct End {
    tick: i32,
    winner: String,
    reason: Option<String>,
}

pub fn derive(announcements: &[Announcement], results: &[RoundResult]) -> Vec<Round> {
    let start_re = Regex::new(r"ROUND (\d+) STARTED").unwrap();
    let win_re = Regex::new(r"Team (\w+) wins( [a-z ]*)?!").unwrap();

    let mut ends: Vec<End> = if results.is_empty() {
        announcements
            .iter()
            .filter_map(|a| {
                let c = win_re.captures(&a.text)?;
                Some(End {
                    tick: a.tick,
                    winner: c[1].to_string(),
                    reason: c.get(2).map(|m| m.as_str().trim().to_string()),
                })
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
                let reason = match win_re.captures(&r.message) {
                    Some(c) => c.get(2).map(|m| m.as_str().trim().to_string()),
                    None if r.message.is_empty() => None,
                    None => Some(r.message.to_lowercase()),
                };
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

    #[test]
    fn results_supply_winners_including_ties() {
        let anns = [
            ann(2, "- CTG ROUND 1 STARTED -"),
            ann(100, "- CTG ROUND 2 STARTED -"),
            ann(200, "- CTG ROUND 3 STARTED -"),
        ];
        let results = [res(90, "tie", "TIE"), res(190, "nsf", "Team NSF wins by capturing the ghost!")];
        let rounds = derive(&anns, &results);
        assert_eq!(rounds.len(), 3);
        assert_eq!(
            (rounds[0].number, rounds[0].end_tick, rounds[0].winner.as_deref(), rounds[0].reason.as_deref()),
            (Some(1), Some(90), Some("Tie"), Some("tie"))
        );
        assert_eq!(
            (rounds[1].winner.as_deref(), rounds[1].reason.as_deref()),
            (Some("NSF"), Some("by capturing the ghost"))
        );
        assert_eq!((rounds[2].number, rounds[2].end_tick), (Some(3), None));
    }

    #[test]
    fn aborted_round_stays_open_when_restarted() {
        let anns = [ann(2, "- CTG ROUND 9 STARTED -"), ann(50, "- CTG ROUND 9 STARTED -")];
        let results = [res(120, "jinrai", "Team Jinrai wins by eliminating the other team!")];
        let rounds = derive(&anns, &results);
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
        let rounds = derive(&anns, &[]);
        assert_eq!(rounds.len(), 2);
        assert_eq!(
            (rounds[0].number, rounds[0].winner.as_deref(), rounds[0].reason.as_deref()),
            (Some(3), Some("NSF"), Some("the match"))
        );
        assert_eq!((rounds[1].number, rounds[1].winner.as_deref()), (Some(4), Some("Jinrai")));
    }
}
