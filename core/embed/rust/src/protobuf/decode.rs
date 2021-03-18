use core::convert::{TryFrom, TryInto};
use core::str;

use crate::{
    error::Error,
    micropython::{buffer::Buffer, gc::Gc, list::List, map::Map, obj::Obj, qstr::Qstr},
    util,
};

use super::{
    defs::{self, FieldDef, FieldType, MsgDef},
    obj::{MsgDefObj, MsgObj, MSG_WIRE_ID_ATTR},
    zigzag,
};

#[no_mangle]
pub extern "C" fn protobuf_type(name: Obj) -> Obj {
    util::try_or_raise(|| {
        let name = Qstr::try_from(name)?;
        let def = MsgDef::for_name(name.to_u16()).ok_or(Error::Missing)?;
        let obj = MsgDefObj::alloc(def).into();
        Ok(obj)
    })
}

#[no_mangle]
pub extern "C" fn protobuf_decode(buf: Obj, msg_def: Obj) -> Obj {
    util::try_or_raise(|| {
        let def = Gc::<MsgDefObj>::try_from(msg_def)?;
        let buf = Buffer::try_from(buf)?;
        let stream = &mut InputStream::new(&buf);
        let decoder = Decoder {
            enable_experimental: false,
        };
        let obj = decoder.message_from_stream(stream, def.msg())?;
        Ok(obj)
    })
}

pub struct Decoder {
    pub enable_experimental: bool,
}

impl Decoder {
    /// Create a new message instance and decode `stream` into it, handling the
    /// default and required fields correctly.
    pub fn message_from_stream(
        &self,
        stream: &mut InputStream,
        msg: &MsgDef,
    ) -> Result<Obj, Error> {
        let mut dict = self.empty_message(msg);
        // SAFETY: We assume that `obj` is not alised here.
        let map = unsafe { dict.as_mut() }.map_mut();
        self.decode_fields_into(stream, msg, map)?;
        self.decode_defaults_into(msg, map)?;
        self.assign_required_into(msg, map)?;
        self.assign_wire_id(msg, map);
        Ok(dict.into())
    }

    /// Create a new message instance and fill it from `values`, handling the
    /// default and required fields correctly.
    pub fn message_from_values(&self, values: &Map, msg: &MsgDef) -> Result<Obj, Error> {
        let mut obj = self.empty_message(msg);
        // SAFETY: We assume that `obj` is not alised here.
        let map = unsafe { obj.as_mut() }.map_mut();
        for elem in values.elems() {
            map.set(elem.key, elem.value);
        }
        self.decode_defaults_into(msg, map)?;
        self.assign_required_into(msg, map)?;
        self.assign_wire_id(msg, map);
        Ok(obj.into())
    }

    /// Allocate the backing message object with enough pre-allocated space for
    /// all fields, including the special `MSG_WIRE_ID_ATTR` field.
    pub fn empty_message(&self, msg: &MsgDef) -> Gc<MsgObj> {
        MsgObj::alloc_with_capacity(msg.fields.len() + 1)
    }

    /// Decode message fields one-by-one from the input stream, assigning them
    /// into `map`.
    pub fn decode_fields_into(
        &self,
        stream: &mut InputStream,
        msg: &MsgDef,
        map: &mut Map,
    ) -> Result<(), Error> {
        // Loop, trying to read the field key that contains the tag and primitive value
        // type. If we fail to read the key, we are at the end of the stream.
        while let Ok(field_key) = stream.read_uvarint() {
            let field_tag = u8::try_from(field_key >> 3)?;
            let prim_type = u8::try_from(field_key & 7)?;

            match msg.field(field_tag) {
                Some(field) => {
                    let field_value = self.decode_field(stream, field)?;
                    let field_name = Qstr::from(field.name);
                    if field.is_repeated() {
                        // Repeated field, values are stored in a list. First, look up the list
                        // object. If it exists, append to it. If it doesn't, create a new list with
                        // this field's value and assign it.
                        if let Ok(obj) = map.get(field_name) {
                            let mut list = Gc::<List>::try_from(obj)?;
                            // SAFETY: We assume that `list` is not aliased here.
                            unsafe { list.as_mut() }.append(field_value);
                        } else {
                            let list = List::alloc(&[field_value]);
                            map.set(field_name, list);
                        }
                    } else {
                        // Singular field, assign the value directly.
                        map.set(field_name, field_value);
                    }
                }
                None => {
                    // Unknown field, skip it.
                    match prim_type {
                        defs::PRIMITIVE_TYPE_VARINT => {
                            stream.read_uvarint()?;
                        }
                        defs::PRIMITIVE_TYPE_LENGTH_DELIMITED => {
                            let num = stream.read_uvarint()?;
                            let len = num.try_into()?;
                            stream.read(len)?;
                        }
                        _ => {
                            return Err(Error::InvalidType);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Fill in the default values by decoding them from the default stream.
    /// Only singular fields are allowed to have a default value, this is
    /// enforced in the blob compilation.
    pub fn decode_defaults_into(&self, msg: &MsgDef, map: &mut Map) -> Result<(), Error> {
        let stream = &mut InputStream::new(msg.defaults);

        // Because we are sure that our field tags fit in one byte, and because this is
        // a trusted stream, we encode the field tag directly as u8, without the
        // primitive type.
        while let Ok(field_tag) = stream.read_byte() {
            let field = msg.field(field_tag).ok_or(Error::Missing)?;
            let field_name = Qstr::from(field.name);
            if map.contains_key(field_name) {
                // Field already has a value assigned, skip it.
                match field.get_type().primitive_type() {
                    defs::PRIMITIVE_TYPE_VARINT => {
                        stream.read_uvarint()?;
                    }
                    defs::PRIMITIVE_TYPE_LENGTH_DELIMITED => {
                        let num = stream.read_uvarint()?;
                        let len = num.try_into()?;
                        stream.read(len)?;
                    }
                    _ => {
                        return Err(Error::InvalidType);
                    }
                }
            } else {
                // Decode the value and assign it.
                let field_value = self.decode_field(stream, field)?;
                map.set(field_name, field_value);
            }
        }
        Ok(())
    }

    /// Walk the fields definitions and make sure that all required fields are
    /// assigned and all optional missing fields are set to `None`.
    pub fn assign_required_into(&self, msg: &MsgDef, map: &mut Map) -> Result<(), Error> {
        for field in msg.fields {
            let field_name = Qstr::from(field.name);
            if map.contains_key(field_name) {
                // Field is assigned, skip.
                continue;
            }
            if field.is_required() {
                // Required field is missing, abort.
                return Err(Error::Missing);
            }
            // Optional field, set to None.
            map.set(field_name, Obj::const_none());
        }
        Ok(())
    }

    /// Assign the wire ID of this message def into the map, under a key
    /// designated by the `MSG_WIRE_ID_ATTR` QSTR.
    pub fn assign_wire_id(&self, msg: &MsgDef, map: &mut Map) {
        if let Some(wire_id) = msg.wire_id {
            map.set(MSG_WIRE_ID_ATTR, wire_id);
        } else {
            map.set(MSG_WIRE_ID_ATTR, Obj::const_none());
        }
    }

    /// Decode one field value from the input stream.
    pub fn decode_field(&self, stream: &mut InputStream, field: &FieldDef) -> Result<Obj, Error> {
        if field.is_experimental() && !self.enable_experimental {
            return Err(Error::InvalidType);
        }
        let num = stream.read_uvarint()?;
        match field.get_type() {
            FieldType::UVarInt => Ok(num.into()),
            FieldType::SVarInt => {
                let signed_int = zigzag::to_signed(num);
                Ok(signed_int.into())
            }
            FieldType::Bool => {
                let boolean = num != 0;
                Ok(boolean.into())
            }
            FieldType::Bytes => {
                let buf_len = num.try_into()?;
                let buf = stream.read(buf_len)?;
                Ok(buf.into())
            }
            FieldType::String => {
                let buf_len = num.try_into()?;
                let buf = stream.read(buf_len)?;
                let unicode = str::from_utf8(buf).map_err(|_| Error::InvalidType)?;
                Ok(unicode.into())
            }
            FieldType::Enum(enum_type) => {
                let enum_val = num.try_into()?;
                if enum_type.values.contains(&enum_val) {
                    Ok(enum_val.into())
                } else {
                    Err(Error::InvalidType)
                }
            }
            FieldType::Msg(msg_type) => {
                let msg_len = num.try_into()?;
                let sub_stream = &mut stream.read_stream(msg_len)?;
                self.message_from_stream(sub_stream, &msg_type)
            }
        }
    }
}

pub struct InputStream<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> InputStream<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn read_stream(&mut self, len: usize) -> Result<Self, Error> {
        let buf = self
            .buf
            .get(self.pos..self.pos + len)
            .ok_or(Error::Missing)?;
        self.pos += len;
        Ok(Self::new(buf))
    }

    pub fn read(&mut self, len: usize) -> Result<&[u8], Error> {
        let buf = self
            .buf
            .get(self.pos..self.pos + len)
            .ok_or(Error::Missing)?;
        self.pos += len;
        Ok(buf)
    }

    pub fn read_byte(&mut self) -> Result<u8, Error> {
        let val = self.buf.get(self.pos).copied().ok_or(Error::Missing)?;
        self.pos += 1;
        Ok(val)
    }

    pub fn read_uvarint(&mut self) -> Result<u64, Error> {
        let mut uint = 0;
        let mut shift = 0;
        loop {
            let byte = self.read_byte()?;
            uint += (byte as u64 & 0x7F) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                break;
            }
        }
        Ok(uint)
    }
}
