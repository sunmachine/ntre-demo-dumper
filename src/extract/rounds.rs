//! Round reconstruction from announcements.
//!
//! Pairs "ROUND N STARTED" markers with "Team X wins ..." messages into
//! per-round records. Handles demos that begin mid-round (first round has no
//! start marker) or end mid-round (last round has no winner).

use regex::Regex;

use super::announcements::Announcement;

pub struct Round {
    pub number: Option<i32>,
    pub start_tick: Option<i32>,
    pub end_tick: Option<i32>,
    pub winner: Option<String>,
    pub reason: Option<String>,
}

pub fn derive(announcements: &[Announcement]) -> Vec<Round> {
    let start_re = Regex::new(r"ROUND (\d+) STARTED").unwrap();
    let win_re = Regex::new(r"Team (\w+) wins( [a-z ]*)?!").unwrap();
    let mut rounds: Vec<Round> = Vec::new();
    let mut open: Option<Round> = None;

    for a in announcements {
        if let Some(c) = start_re.captures(&a.text) {
            if let Some(r) = open.take() {
                rounds.push(r); // previous round never saw a win message
            }
            open = Some(Round {
                number: c[1].parse().ok(),
                start_tick: Some(a.tick),
                end_tick: None,
                winner: None,
                reason: None,
            });
        } else if let Some(c) = win_re.captures(&a.text) {
            let mut r = open.take().unwrap_or(Round {
                number: None,
                start_tick: None,
                end_tick: None,
                winner: None,
                reason: None,
            });
            r.end_tick = Some(a.tick);
            r.winner = Some(c[1].to_string());
            r.reason = c.get(2).map(|m| m.as_str().trim().to_string());
            rounds.push(r);
        }
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
