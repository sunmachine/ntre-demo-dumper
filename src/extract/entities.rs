//! All-player entity samples: position, eye angles, active weapon, health,
//! team, and life state, decoded from svc_PacketEntities via
//! tf-demo-parser's sendtable machinery. Decoding is definition-driven, so
//! NT;RE's custom classes and props need no TF2 assumptions (see
//! ARCHITECTURE.md; the library's typed game events are NOT safe, while its
//! entity decoding is).
//!
//! This is a whole-file pass, not a `FrameExtractor`, because tf-demo-parser
//! owns its own demo walk. A mid-file decode error degrades to a warning and
//! keeps every sample decoded so far; frame-level extractors are never
//! affected. See SCHEMA.md for the POV-demo PVS caveat and the meaning of
//! `in_pvs = 0` rows.

use anyhow::Result;
use std::collections::HashMap;

use tf_demo_parser::demo::data::DemoTick;
use tf_demo_parser::demo::message::packetentities::PacketEntity;
use tf_demo_parser::demo::message::{Message, MessageType};
use tf_demo_parser::demo::packet::datatable::{ParseSendTable, ServerClass};
use tf_demo_parser::demo::parser::MessageHandler;
use tf_demo_parser::demo::sendprop::{SendPropIdentifier, SendPropValue};
use tf_demo_parser::{Demo, DemoParser, ParserState};

/// One on-change sample of a player entity.
pub struct PlayerSample {
    pub tick: u32,
    pub entity_id: u32,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub eye_pitch: f32,
    pub eye_yaw: f32,
    pub vx: f32,
    pub vy: f32,
    pub vz: f32,
    pub weapon: String,
    pub health: i64,
    pub team: i64,
    pub class_num: i64,
    pub camo: bool,
    pub alive: bool,
    pub in_pvs: bool,
}

/// One on-change position sample of a ghost entity.
pub struct GhostSample {
    pub tick: u32,
    pub entity_id: u32,
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// One on-change reading of a victim's per-attacker damage accumulator.
pub struct DamageSample {
    pub tick: u32,
    pub victim_entity_id: u32,
    pub attacker_entity_id: u32,
    pub damage: f32,
}

/// One on-change scoreboard sample from the player resource entity.
pub struct ResourceSample {
    pub tick: u32,
    pub entity_id: u32,
    pub xp: i64,
    pub score: i64,
    pub deaths: i64,
    pub ping: i64,
}

pub struct EntityOutput {
    pub samples: Vec<PlayerSample>,
    pub ghost_samples: Vec<GhostSample>,
    pub resource_samples: Vec<ResourceSample>,
    pub damage_samples: Vec<DamageSample>,
    pub player_classes: Vec<String>,
    pub warning: Option<String>,
}

/// Run the entity pass over the whole demo file.
pub fn run(data: &[u8]) -> Result<EntityOutput> {
    let demo = Demo::new(data);
    let parser = DemoParser::new_with_analyser(demo.get_stream(), EntityAnalyser::default());
    let (_header, mut ticker) = parser.ticker()?;
    let mut warning = None;
    loop {
        match ticker.tick() {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => {
                warning = Some(format!("entity pass stopped early: {e}"));
                break;
            }
        }
    }
    let mut output = ticker.into_state();
    output.warning = warning;
    Ok(output)
}

#[derive(Default, Clone)]
struct PlayerState {
    x: f32,
    y: f32,
    z: f32,
    eye_pitch: f32,
    eye_yaw: f32,
    vx: f32,
    vy: f32,
    vz: f32,
    weapon_entity: u32,
    health: i64,
    team: i64,
    class_num: i64,
    camo: bool,
    life_state: i64,
    /// last seen m_rfAttackersAccumlator values, indexed by attacker entity
    attackers: Vec<f32>,
}

#[derive(Default, Clone)]
struct ResourceState {
    xp: i64,
    score: i64,
    deaths: i64,
    ping: i64,
}

#[derive(Default)]
struct EntityAnalyser {
    /// prop identifier -> bare prop name, for every prop in the sendtables
    prop_names: HashMap<SendPropIdentifier, String>,
    /// array-element props (numeric names): identifier -> (owner table, index)
    array_props: HashMap<SendPropIdentifier, (String, u32)>,
    /// server class id (as usize) -> class name
    class_names: Vec<String>,
    /// class ids whose name ends with "Player" (the mod's player classes)
    player_classes: Vec<u16>,
    /// class ids whose name ends with "WeaponGhost" (the objective)
    ghost_classes: Vec<u16>,
    /// class ids whose name ends with "PlayerResource" (scoreboard arrays)
    resource_classes: Vec<u16>,
    /// entity id -> class id, tracked from Enter updates (weapon lookups)
    entity_classes: HashMap<u32, u16>,
    players: HashMap<u32, PlayerState>,
    ghosts: HashMap<u32, (f32, f32, f32)>,
    resource: HashMap<u32, ResourceState>,
    samples: Vec<PlayerSample>,
    ghost_samples: Vec<GhostSample>,
    resource_samples: Vec<ResourceSample>,
    damage_samples: Vec<DamageSample>,
}

const EHANDLE_ENTITY_MASK: i64 = (1 << 11) - 1;

fn weapon_name(class_name: &str) -> String {
    let stripped = class_name
        .strip_prefix("CNEO_Weapon")
        .or_else(|| class_name.strip_prefix("CNEOWeapon"))
        .or_else(|| class_name.strip_prefix("CWeapon"))
        .or_else(|| class_name.strip_prefix("C"))
        .unwrap_or(class_name);
    stripped.to_ascii_lowercase()
}

impl EntityAnalyser {
    fn apply_entity(&mut self, tick: u32, entity: &PacketEntity, state: &ParserState) {
        let entity_id: u32 = entity.entity_index.into();

        use tf_demo_parser::demo::message::packetentities::UpdateType;
        // Entity slots are reused; track the *current* class per slot. On
        // Delete, resolve the class from our map (the message's own class
        // field is not meaningful for deletes) and forget the slot.
        let class_id: u16 = if entity.update_type == UpdateType::Delete {
            match self.entity_classes.remove(&entity_id) {
                Some(id) => id,
                None => return,
            }
        } else {
            let id: u16 = entity.server_class.into();
            self.entity_classes.insert(entity_id, id);
            id
        };

        if self.ghost_classes.contains(&class_id) {
            self.apply_ghost(tick, entity_id, entity, state);
            return;
        }

        if self.resource_classes.contains(&class_id) {
            self.apply_resource(tick, entity, state);
            return;
        }

        if !self.player_classes.contains(&class_id) {
            return;
        }

        if entity.update_type == UpdateType::Leave || entity.update_type == UpdateType::Delete {
            if let Some(p) = self.players.get(&entity_id) {
                let p = p.clone();
                self.push_sample(tick, entity_id, &p, false);
            }
            if entity.update_type == UpdateType::Delete {
                self.players.remove(&entity_id);
            }
            return;
        }

        let mut changed = false;
        let player = self.players.entry(entity_id).or_default();
        for prop in entity.props(state) {
            let Some(name) = self.prop_names.get(&prop.identifier) else {
                continue;
            };
            match (name.as_str(), &prop.value) {
                ("m_vecOrigin", SendPropValue::Vector(v)) => {
                    (player.x, player.y, player.z) = (v.x, v.y, v.z);
                    changed = true;
                }
                ("m_vecOrigin", SendPropValue::VectorXY(v)) => {
                    (player.x, player.y) = (v.x, v.y);
                    changed = true;
                }
                ("m_vecOrigin[2]", SendPropValue::Float(z)) => {
                    player.z = *z;
                    changed = true;
                }
                ("m_vecVelocity[0]", SendPropValue::Float(vx)) => {
                    player.vx = *vx;
                    changed = true;
                }
                ("m_vecVelocity[1]", SendPropValue::Float(vy)) => {
                    player.vy = *vy;
                    changed = true;
                }
                ("m_vecVelocity[2]", SendPropValue::Float(vz)) => {
                    player.vz = *vz;
                    changed = true;
                }
                ("m_angEyeAngles[0]", SendPropValue::Float(pitch)) => {
                    player.eye_pitch = *pitch;
                    changed = true;
                }
                ("m_angEyeAngles[1]", SendPropValue::Float(yaw)) => {
                    player.eye_yaw = *yaw;
                    changed = true;
                }
                ("m_hActiveWeapon", SendPropValue::Integer(handle)) => {
                    let index = (handle & EHANDLE_ENTITY_MASK) as u32;
                    if index != EHANDLE_ENTITY_MASK as u32 && player.weapon_entity != index {
                        player.weapon_entity = index;
                        changed = true;
                    }
                }
                ("m_iHealth", SendPropValue::Integer(hp)) => {
                    player.health = *hp;
                    changed = true;
                }
                ("m_iTeamNum", SendPropValue::Integer(team)) => {
                    player.team = *team;
                    changed = true;
                }
                ("m_iNeoClass", SendPropValue::Integer(class_num)) => {
                    player.class_num = *class_num;
                    changed = true;
                }
                ("m_bInThermOpticCamo", SendPropValue::Integer(camo)) => {
                    player.camo = *camo != 0;
                    changed = true;
                }
                ("m_lifeState", SendPropValue::Integer(life)) => {
                    player.life_state = *life;
                    changed = true;
                }
                // Legacy SendPropArray: arrives whole, 1-indexed by attacker
                // entity id. The server keeps only the sub-1.0 fractional
                // carry of damage here, so a change marks a hit landing, not
                // an amount. Diff against the last reading; not a
                // player_samples column, so it never sets `changed`.
                ("m_rfAttackersAccumlator" | "m_rflAttackersAccumlator",
                    SendPropValue::Array(values)) => {
                    if player.attackers.len() < values.len() {
                        player.attackers.resize(values.len(), 0.0);
                    }
                    for (i, value) in values.iter().enumerate() {
                        let SendPropValue::Float(dmg) = value else {
                            continue;
                        };
                        if i > 0 && player.attackers[i] != *dmg {
                            player.attackers[i] = *dmg;
                            self.damage_samples.push(DamageSample {
                                tick,
                                victim_entity_id: entity_id,
                                attacker_entity_id: i as u32,
                                damage: *dmg,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        if changed {
            let p = player.clone();
            self.push_sample(tick, entity_id, &p, true);
        }
    }

    fn apply_ghost(&mut self, tick: u32, entity_id: u32, entity: &PacketEntity, state: &ParserState) {
        use tf_demo_parser::demo::message::packetentities::UpdateType;
        if entity.update_type == UpdateType::Delete {
            self.ghosts.remove(&entity_id);
            return;
        }
        let mut changed = false;
        let pos = self.ghosts.entry(entity_id).or_default();
        for prop in entity.props(state) {
            let Some(name) = self.prop_names.get(&prop.identifier) else {
                continue;
            };
            match (name.as_str(), &prop.value) {
                ("m_vecOrigin", SendPropValue::Vector(v)) => {
                    (pos.0, pos.1, pos.2) = (v.x, v.y, v.z);
                    changed = true;
                }
                ("m_vecOrigin", SendPropValue::VectorXY(v)) => {
                    (pos.0, pos.1) = (v.x, v.y);
                    changed = true;
                }
                ("m_vecOrigin[2]", SendPropValue::Float(z)) => {
                    pos.2 = *z;
                    changed = true;
                }
                _ => {}
            }
        }
        if changed {
            let (x, y, z) = *pos;
            self.ghost_samples.push(GhostSample { tick, entity_id, x, y, z });
        }
    }

    fn apply_resource(&mut self, tick: u32, entity: &PacketEntity, state: &ParserState) {
        use tf_demo_parser::demo::message::packetentities::UpdateType;
        if entity.update_type == UpdateType::Delete {
            return;
        }
        let mut changed: Vec<u32> = Vec::new();
        for prop in entity.props(state) {
            let Some((table, index)) = self.array_props.get(&prop.identifier) else {
                continue;
            };
            let SendPropValue::Integer(v) = &prop.value else {
                continue;
            };
            let entry = self.resource.entry(*index).or_default();
            let field = match table.as_str() {
                "m_iXP" => &mut entry.xp,
                "m_iScore" => &mut entry.score,
                "m_iDeaths" => &mut entry.deaths,
                "m_iPing" => &mut entry.ping,
                _ => continue,
            };
            if *field != *v {
                *field = *v;
                if !changed.contains(index) {
                    changed.push(*index);
                }
            }
        }
        for slot in changed {
            let s = self.resource[&slot].clone();
            self.resource_samples.push(ResourceSample {
                tick,
                entity_id: slot,
                xp: s.xp,
                score: s.score,
                deaths: s.deaths,
                ping: s.ping,
            });
        }
    }

    fn push_sample(&mut self, tick: u32, entity_id: u32, p: &PlayerState, in_pvs: bool) {
        // entity 0 is worldspawn, i.e. "no weapon handle seen yet"
        let weapon = if p.weapon_entity == 0 {
            String::new()
        } else {
            self.entity_classes
                .get(&p.weapon_entity)
                .and_then(|id| self.class_names.get(*id as usize))
                .map(|name| weapon_name(name))
                .unwrap_or_default()
        };
        self.samples.push(PlayerSample {
            tick,
            entity_id,
            x: p.x,
            y: p.y,
            z: p.z,
            eye_pitch: p.eye_pitch,
            eye_yaw: p.eye_yaw,
            vx: p.vx,
            vy: p.vy,
            vz: p.vz,
            weapon,
            health: p.health,
            team: p.team,
            class_num: p.class_num,
            camo: p.camo,
            alive: p.life_state == 0,
            in_pvs,
        });
    }
}

impl MessageHandler for EntityAnalyser {
    type Output = EntityOutput;

    fn does_handle(message_type: MessageType) -> bool {
        // PacketEntities only. Deliberately NOT GameEvent: tf-demo-parser's
        // typed game events misread NT;RE definitions (see ARCHITECTURE.md);
        // excluding them here makes the parser length-skip those messages.
        matches!(message_type, MessageType::PacketEntities)
    }

    fn handle_data_tables(
        &mut self,
        tables: &[ParseSendTable],
        server_classes: &[ServerClass],
        _state: &ParserState,
    ) {
        for table in tables {
            for prop in &table.props {
                let identifier = SendPropIdentifier::new(table.name.as_str(), prop.name.as_str());
                self.prop_names.insert(identifier, prop.name.to_string());
                // SendPropArray3 elements are numeric props inside a table
                // named after the array member (m_iPing.000, m_iPing.001, …).
                if let Ok(index) = prop.name.as_str().parse::<u32>() {
                    self.array_props
                        .insert(identifier, (table.name.to_string(), index));
                }
            }
        }
        self.class_names = server_classes
            .iter()
            .map(|c| c.name.as_str().to_string())
            .collect();
        self.player_classes = server_classes
            .iter()
            .filter(|c| c.name.as_str().ends_with("Player"))
            .map(|c| c.id.into())
            .collect();
        self.ghost_classes = server_classes
            .iter()
            .filter(|c| c.name.as_str().ends_with("WeaponGhost"))
            .map(|c| c.id.into())
            .collect();
        self.resource_classes = server_classes
            .iter()
            .filter(|c| c.name.as_str().ends_with("PlayerResource"))
            .map(|c| c.id.into())
            .collect();
    }

    fn handle_message(&mut self, message: &Message, tick: DemoTick, state: &ParserState) {
        if let Message::PacketEntities(msg) = message {
            for entity in &msg.entities {
                self.apply_entity(u32::from(tick), entity, state);
            }
        }
    }

    fn into_output(self, _state: &ParserState) -> Self::Output {
        let player_class_names = self
            .player_classes
            .iter()
            .filter_map(|id| self.class_names.get(*id as usize).cloned())
            .collect();
        EntityOutput {
            samples: self.samples,
            ghost_samples: self.ghost_samples,
            resource_samples: self.resource_samples,
            damage_samples: self.damage_samples,
            player_classes: player_class_names,
            warning: None,
        }
    }
}
