//! Inferred hit attribution: who dealt each point of damage.
//!
//! NT;RE builds from 2026-07-25 onward no longer network the per-attacker
//! damage accumulator, so SourceTV demos show every health drop but not who
//! caused it. This pass reconstructs the attacker from what the demo still
//! carries: for each health drop, the enemy whose aim points closest at the
//! victim at that tick. The fatal hit is matched to the kill event instead,
//! which makes it exact. Rows carry a confidence and a method name because
//! they are guesses, and a future rule may change them.

use std::collections::HashMap;

use super::entities::PlayerSample;
use super::net::{Kill, Player};

pub struct InferredHit {
    pub tick: u32,
    pub victim_entity_id: u32,
    /// None when no enemy's aim came within [`MAX_AIM_ERROR`] degrees.
    pub attacker_entity_id: Option<u32>,
    pub damage: i64,
    /// Degrees between the attacker's view direction and the victim.
    pub aim_error: Option<f32>,
    pub confidence: f32,
    pub method: &'static str,
}

/// Name of the aim-based rule, stored in the `method` column.
pub const METHOD_AIM: &str = "aim-v1";
/// Name used when the kill event supplied the attacker.
pub const METHOD_KILL: &str = "kill-event";
/// Name used when nothing could be attributed.
pub const METHOD_NONE: &str = "none";

/// Beyond this angle the best candidate is not recorded as the attacker.
const MAX_AIM_ERROR: f32 = 15.0;
/// Confidence falls linearly from 1 at zero error to 0 at this angle.
const FULL_ERROR: f32 = 20.0;
/// Kill events land within this many ticks of the fatal health drop.
const KILL_WINDOW: u32 = 4;
/// Eye height above the player origin, and the aim point on a target.
const EYE_Z: f32 = 64.0;
const TARGET_Z: f32 = 40.0;

#[derive(Clone)]
struct State {
    x: f32,
    y: f32,
    z: f32,
    pitch: f32,
    yaw: f32,
    weapon: String,
    health: i64,
    team: i64,
    alive: bool,
}

/// Weapons that cannot land a hitscan or melee hit by aiming at the victim.
fn cannot_hit(weapon: &str) -> bool {
    matches!(weapon, "" | "ghost" | "smokegrenade" | "grenade" | "detpack" | "proxmine" | "remotedet")
}

/// Angle in degrees between the attacker's view and the vector to the target.
fn aim_error(attacker: &State, victim: &State) -> f32 {
    let dx = victim.x - attacker.x;
    let dy = victim.y - attacker.y;
    let dz = (victim.z + TARGET_Z) - (attacker.z + EYE_Z);
    let len = (dx * dx + dy * dy + dz * dz).sqrt();
    if len < 1.0 {
        return 0.0;
    }
    let (pitch, yaw) = (attacker.pitch.to_radians(), attacker.yaw.to_radians());
    // Source convention: positive pitch looks down.
    let (vx, vy, vz) = (pitch.cos() * yaw.cos(), pitch.cos() * yaw.sin(), -pitch.sin());
    let dot = (vx * dx + vy * dy + vz * dz) / len;
    dot.clamp(-1.0, 1.0).acos().to_degrees()
}

/// Reconstruct hits from on-change samples (in tick order) and the kill feed.
pub fn infer(samples: &[PlayerSample], kills: &[Kill], players: &HashMap<u32, Player>) -> Vec<InferredHit> {
    let entity_of: HashMap<u32, u32> = players.values().map(|p| (p.user_id, p.entity_id)).collect();
    // Kills keyed by victim entity, in tick order, for fatal-hit matching.
    let mut kills_by_victim: HashMap<u32, Vec<(u32, Option<u32>)>> = HashMap::new();
    for k in kills {
        if let Some(&victim) = entity_of.get(&k.victim_userid) {
            let attacker = entity_of.get(&k.attacker_userid).copied().filter(|_| k.attacker_userid != 0);
            kills_by_victim.entry(victim).or_default().push((k.tick as u32, attacker));
        }
    }

    let mut state: HashMap<u32, State> = HashMap::new();
    let mut hits = Vec::new();
    for s in samples {
        let prev = state.get(&s.entity_id).cloned();
        let next = State {
            x: s.x,
            y: s.y,
            z: s.z,
            pitch: s.eye_pitch,
            yaw: s.eye_yaw,
            weapon: s.weapon.clone(),
            health: s.health,
            team: s.team,
            alive: s.alive,
        };
        if let Some(prev) = prev {
            let dropped = prev.alive && s.health < prev.health && prev.health > 0;
            if dropped && (s.team == 2 || s.team == 3) {
                let damage = prev.health - s.health;
                let fatal = s.health <= 0 || !s.alive;
                let hit = fatal
                    .then(|| fatal_hit(s, damage, &kills_by_victim))
                    .flatten()
                    .unwrap_or_else(|| aim_hit(s, damage, &prev, &state));
                hits.push(hit);
            }
        }
        state.insert(s.entity_id, next);
    }
    hits
}

fn fatal_hit(
    s: &PlayerSample,
    damage: i64,
    kills_by_victim: &HashMap<u32, Vec<(u32, Option<u32>)>>,
) -> Option<InferredHit> {
    let kills = kills_by_victim.get(&s.entity_id)?;
    let (_, attacker) = kills.iter().find(|(t, _)| t.abs_diff(s.tick) <= KILL_WINDOW)?;
    Some(InferredHit {
        tick: s.tick,
        victim_entity_id: s.entity_id,
        attacker_entity_id: *attacker,
        damage,
        aim_error: None,
        confidence: 1.0,
        method: METHOD_KILL,
    })
}

fn aim_hit(s: &PlayerSample, damage: i64, victim: &State, state: &HashMap<u32, State>) -> InferredHit {
    let best = state
        .iter()
        .filter(|(&id, a)| id != s.entity_id && a.alive && a.team != victim.team && (a.team == 2 || a.team == 3))
        .filter(|(_, a)| !cannot_hit(&a.weapon))
        .map(|(&id, a)| (id, aim_error(a, victim)))
        .min_by(|a, b| a.1.total_cmp(&b.1));
    match best {
        Some((id, err)) if err <= MAX_AIM_ERROR => InferredHit {
            tick: s.tick,
            victim_entity_id: s.entity_id,
            attacker_entity_id: Some(id),
            damage,
            aim_error: Some(err),
            confidence: (1.0 - err / FULL_ERROR).clamp(0.0, 1.0),
            method: METHOD_AIM,
        },
        _ => InferredHit {
            tick: s.tick,
            victim_entity_id: s.entity_id,
            attacker_entity_id: None,
            damage,
            aim_error: best.map(|(_, err)| err),
            confidence: 0.0,
            method: METHOD_NONE,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(x: f32, y: f32, pitch: f32, yaw: f32) -> State {
        State { x, y, z: 0.0, pitch, yaw, weapon: "mx".into(), health: 100, team: 2, alive: true }
    }

    #[test]
    fn aim_error_is_zero_when_looking_straight_at_target() {
        // Target 1000 units along +x; eye sits 24 units above the aim point.
        let pitch = (24.0f32 / 1000.0).atan().to_degrees();
        let a = state(0.0, 0.0, pitch, 0.0);
        let v = state(1000.0, 0.0, 0.0, 0.0);
        assert!(aim_error(&a, &v) < 0.1);
        let a = state(0.0, 0.0, pitch, 90.0);
        assert!((aim_error(&a, &v) - 90.0).abs() < 0.1);
    }

    fn sample(tick: u32, entity_id: u32, x: f32, yaw: f32, health: i64, team: i64) -> PlayerSample {
        PlayerSample {
            tick,
            entity_id,
            x,
            y: 0.0,
            z: 0.0,
            eye_pitch: 0.0,
            eye_yaw: yaw,
            vx: 0.0,
            vy: 0.0,
            vz: 0.0,
            weapon: "mx".into(),
            health,
            team,
            class_num: 1,
            camo: false,
            alive: health > 0,
            in_pvs: true,
        }
    }

    #[test]
    fn attributes_drop_to_enemy_aiming_at_victim() {
        let samples = [
            sample(1, 1, 0.0, 0.0, 100, 2),   // attacker looks along +x
            sample(1, 2, 500.0, 180.0, 100, 3), // victim
            sample(1, 3, 0.0, 180.0, 100, 2),   // teammate of attacker looking away
            sample(5, 2, 500.0, 180.0, 70, 3),  // victim loses 30
            sample(9, 2, 500.0, 180.0, -5, 3),  // fatal
        ];
        let mut players = HashMap::new();
        for (uid, ent) in [(10, 1), (20, 2), (30, 3)] {
            players.insert(
                uid,
                Player { entity_id: ent, user_id: uid, name: String::new(), steam_id: String::new(), is_bot: false, first_seen_tick: 0 },
            );
        }
        let kills = [Kill {
            tick: 9,
            victim_userid: 20,
            attacker_userid: 30,
            assists: 0,
            weapon: "mx".into(),
            headshot: false,
            suicide: false,
            explosive: false,
            ghoster: false,
        }];
        let hits = infer(&samples, &kills, &players);
        assert_eq!(hits.len(), 2);
        assert_eq!((hits[0].attacker_entity_id, hits[0].damage, hits[0].method), (Some(1), 30, METHOD_AIM));
        assert!(hits[0].confidence > 0.8);
        assert_eq!((hits[1].attacker_entity_id, hits[1].damage, hits[1].method), (Some(3), 75, METHOD_KILL));
    }
}
