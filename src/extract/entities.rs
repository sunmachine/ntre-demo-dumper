//! All-player entity samples: position, eye angles, active weapon, health,
//! team, life state — decoded from svc_PacketEntities via tf-demo-parser's
//! sendtable machinery (definition-driven, so NT;RE's custom classes and
//! props decode without any TF2 assumptions; see ARCHITECTURE.md for why its
//! typed game events are NOT safe while its entity decoding is).
//!
//! This is a whole-file pass, not a `FrameExtractor`: tf-demo-parser owns
//! its own demo walk. The pipeline runs it alongside the frame pass. A demo
//! that trips the entity decoder mid-file still yields every sample decoded
//! up to that point, plus a warning — the frame-level extractors are never
//! affected.
//!
//! POV demos only contain entities inside the recorder's PVS: other players
//! are sampled while visible/nearby. `in_pvs = 0` rows mark when a player
//! left the recorder's PVS (position is their last known).

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
    pub weapon: String,
    pub health: i64,
    pub team: i64,
    pub alive: bool,
    pub in_pvs: bool,
}

pub struct EntityOutput {
    pub samples: Vec<PlayerSample>,
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
    weapon_entity: u32,
    health: i64,
    team: i64,
    life_state: i64,
}

#[derive(Default)]
struct EntityAnalyser {
    /// prop identifier -> bare prop name, for every prop in the sendtables
    prop_names: HashMap<SendPropIdentifier, String>,
    /// server class id (as usize) -> class name
    class_names: Vec<String>,
    /// class ids whose name ends with "Player" (the mod's player classes)
    player_classes: Vec<u16>,
    /// entity id -> class id, tracked from Enter updates (weapon lookups)
    entity_classes: HashMap<u32, u16>,
    players: HashMap<u32, PlayerState>,
    samples: Vec<PlayerSample>,
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
                ("m_lifeState", SendPropValue::Integer(life)) => {
                    player.life_state = *life;
                    changed = true;
                }
                _ => {}
            }
        }
        if changed {
            let p = player.clone();
            self.push_sample(tick, entity_id, &p, true);
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
            weapon,
            health: p.health,
            team: p.team,
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
            player_classes: player_class_names,
            warning: None,
        }
    }
}
