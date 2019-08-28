use bitstream_reader::{BitRead, BitReadSized, LittleEndian};
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

use crate::demo::message::stringtable::{log_base2, read_var_int};
use crate::demo::packet::datatable::{SendTable, SendTableName, ServerClass};
use crate::demo::parser::ParseBitSkip;
use crate::demo::sendprop::{SendProp, SendPropDefinition, SendPropValue};
use crate::{MalformedDemoError, Parse, ParseError, ParserState, ReadResult, Result, Stream};
use parse_display::Display;
use std::collections::HashMap;
use std::fmt;
use std::hint::unreachable_unchecked;
use std::num::ParseIntError;
use std::rc::Rc;
use std::str::FromStr;

#[derive(
    Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Display, Ord, PartialOrd,
)]
pub struct EntityId(u32);

impl From<u32> for EntityId {
    fn from(num: u32) -> Self {
        EntityId(num)
    }
}

impl FromStr for EntityId {
    type Err = ParseIntError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        u32::from_str(s).map(EntityId::from)
    }
}

#[derive(BitRead, Clone, Copy, Debug, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[discriminant_bits = 2]
#[repr(u8)]
pub enum PVS {
    Preserve = 0,
    Leave = 1,
    Enter = 2,
    Delete = 3,
}

#[derive(Debug)]
pub struct PacketEntity {
    pub server_class: Rc<ServerClass>,
    pub entity_index: EntityId,
    pub props: Vec<SendProp>,
    pub in_pvs: bool,
    pub pvs: PVS,
    pub serial_number: u32,
    pub delay: Option<u32>,
}

impl fmt::Display for PacketEntity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({}) {{\n", self.entity_index, self.server_class.name)?;
        for child in self.props.iter() {
            write!(f, "\t{}\n", child)?;
        }
        write!(f, "}}")
    }
}

impl PacketEntity {
    fn get_prop_by_definition(&mut self, definition: &SendPropDefinition) -> Option<&mut SendProp> {
        self.props
            .iter_mut()
            .find(|prop| prop.definition.as_ref().eq(definition))
    }

    pub fn apply_update(&mut self, props: Vec<SendProp>) {
        for prop in props {
            match self.get_prop_by_definition(&prop.definition) {
                Some(existing_prop) => existing_prop.value = prop.value,
                None => self.props.push(prop),
            }
        }
    }
}

fn read_bit_var<T: BitReadSized<LittleEndian>>(stream: &mut Stream) -> ReadResult<T> {
    let ty: u8 = stream.read_sized(2)?;

    let bits = match ty {
        0 => 4,
        1 => 8,
        2 => 12,
        3 => 32,
        _ => unsafe { unreachable_unchecked() },
    };
    stream.read_sized(bits)
}

#[derive(Debug)]
pub struct PacketEntitiesMessage {
    pub entities: Vec<PacketEntity>,
    pub removed_entities: Vec<EntityId>,
    pub max_entries: u16,
    pub delta: Option<u32>,
    pub base_line: u8,
    pub updated_base_line: bool,
}

fn get_send_table<'a>(state: &'a ParserState, table: &SendTableName) -> Result<&'a SendTable> {
    state
        .send_tables
        .get(table)
        .ok_or_else(|| MalformedDemoError::UnknownSendTable(table.clone()).into())
}

fn get_entity_for_update(
    state: &ParserState,
    entity_index: EntityId,
    pvs: PVS,
) -> Result<PacketEntity> {
    let server_class = state
        .entity_classes
        .get(&entity_index)
        .ok_or_else(|| MalformedDemoError::UnknownEntity(entity_index))?;

    Ok(PacketEntity {
        server_class: Rc::clone(server_class),
        entity_index,
        props: Vec::new(),
        in_pvs: false,
        pvs,
        serial_number: 0,
        delay: None,
    })
}

impl Parse for PacketEntitiesMessage {
    fn parse(stream: &mut Stream, state: &ParserState) -> Result<Self> {
        let max_entries = stream.read_sized(11)?;
        let delta: Option<u32> = stream.read()?;
        let base_line = stream.read_sized(1)?;
        let updated_entries: u16 = stream.read_sized(11)?;
        let length: u32 = stream.read_sized(20)?;
        let updated_base_line = stream.read()?;
        let mut data = stream.read_bits(length as usize)?;

        let mut entities = Vec::with_capacity(updated_entries as usize);
        let mut removed_entities = Vec::new();

        let mut last_index: i32 = -1;

        for _ in 0..updated_entries {
            let diff: u32 = read_bit_var(&mut data)?;
            last_index += diff as i32 + 1;
            let entity_index = EntityId::from(last_index as u32);

            let pvs = data.read()?;
            if pvs == PVS::Enter {
                let mut entity =
                    Self::read_enter(&mut data, entity_index, state, base_line as usize)?;
                let send_table = get_send_table(state, &entity.server_class.data_table)?;
                let updated_props = Self::read_update(&mut data, send_table)?;
                entity.apply_update(updated_props);

                entities.push(entity);
            } else if pvs == PVS::Preserve {
                let mut entity = get_entity_for_update(state, entity_index, pvs)?;
                let send_table = get_send_table(state, &entity.server_class.data_table)?;

                let updated_props = Self::read_update(&mut data, send_table)?;
                entity.props = updated_props;

                entities.push(entity);
            } else if state.entity_classes.contains_key(&entity_index) {
                let entity = get_entity_for_update(state, entity_index, pvs)?;
                entities.push(entity);
            }
        }

        if delta.is_some() {
            while data.read()? {
                removed_entities.push(data.read_sized::<u32>(11)?.into())
            }
        }

        Ok(PacketEntitiesMessage {
            entities,
            removed_entities,
            max_entries,
            delta,
            base_line,
            updated_base_line,
        })
    }
}

impl PacketEntitiesMessage {
    fn read_enter(
        stream: &mut Stream,
        entity_index: EntityId,
        state: &ParserState,
        baseline_index: usize,
    ) -> Result<PacketEntity> {
        let bits = log_base2(state.server_classes.len()) + 1;
        let class_index = stream.read_sized::<u16>(bits as usize)? as usize;
        let server_class = state
            .server_classes
            .get(class_index)
            .ok_or_else(|| ParseError::from(MalformedDemoError::UnknownServerClass(class_index)))?;

        let serial = stream.read_sized(10)?;
        let send_table = state
            .send_tables
            .get(&server_class.data_table)
            .ok_or_else(|| MalformedDemoError::UnknownSendTable(server_class.data_table.clone()))?;

        let props = match state.instance_baselines[baseline_index].get(&entity_index) {
            Some(baseline) => baseline.clone(),
            None => match state.static_baselines.get(&server_class.id) {
                Some(static_baseline) => {
                    state.get_static_baseline((class_index as u16).into(), send_table)?
                }
                None => Vec::new(),
            },
        };

        Ok(PacketEntity {
            server_class: Rc::clone(server_class),
            entity_index,
            props,
            in_pvs: true,
            pvs: PVS::Enter,
            serial_number: serial,
            delay: None,
        })
    }

    pub fn read_update(stream: &mut Stream, send_table: &SendTable) -> Result<Vec<SendProp>> {
        let mut index = -1;
        //let mut props: HashMap<i32, SendProp> = HashMap::new();
        let mut props = Vec::with_capacity(8);

        while stream.read()? {
            let diff: u32 = read_bit_var(stream)?;
            index += (diff as i32) + 1;

            match send_table.flattened_props.get(index as usize) {
                Some(definition) => {
                    let value = SendPropValue::parse(stream, definition)?;
                    props.push(SendProp {
                        definition: Rc::clone(definition),
                        value,
                    });
                }
                None => {
                    return Err(ParseError::from(MalformedDemoError::PropIndexOutOfBounds {
                        index,
                        prop_count: send_table.flattened_props.len(),
                    }))
                }
            }
        }

        Ok(props)
        //Ok(props.into_iter().map(|(_, prop)| prop).collect())
    }
}

impl ParseBitSkip for PacketEntitiesMessage {
    fn parse_skip(stream: &mut Stream) -> Result<()> {
        let _: u16 = stream.read_sized(11)?;
        let _: Option<u32> = stream.read()?;
        let _: u8 = stream.read_sized(1)?;
        let _: u16 = stream.read_sized(11)?;
        let length: u32 = stream.read_sized(20)?;
        let _: bool = stream.read()?;
        stream.skip_bits(length as usize).map_err(ParseError::from)
    }
}
