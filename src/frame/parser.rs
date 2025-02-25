use r2d2;
use std::cell::RefCell;
use std::collections::HashMap;
use std::convert::TryInto;
use std::io::{Cursor, Read};
use std::ops::Deref;

use super::*;
use crate::compression::Compression;
use crate::error;
use crate::frame::frame_response::ResponseBody;
use crate::frame::FromCursor;
use crate::transport::CDRSTransport;
use crate::types::data_serialization_types::decode_timeuuid;
use crate::types::{from_bytes, from_u16_bytes, CStringList, UUID_LEN};

pub fn from_connection<M, T>(
    conn: &r2d2::PooledConnection<M>,
    compressor: &Compression,
) -> error::Result<Frame>
where
    T: CDRSTransport + 'static,
    M: r2d2::ManageConnection<Connection = RefCell<T>, Error = error::Error> + Sized,
{
    parse_frame(conn.deref(), compressor)
}

pub fn parse_frame(
    cursor_cell: &RefCell<dyn Read>,
    compressor: &Compression,
) -> error::Result<Frame> {
    let mut version_bytes = [0; Version::BYTE_LENGTH];
    let mut flag_bytes = [0; Flag::BYTE_LENGTH];
    let mut opcode_bytes = [0; Opcode::BYTE_LENGTH];
    let mut stream_bytes = [0; STREAM_LEN];
    let mut length_bytes = [0; LENGTH_LEN];
    let mut cursor = cursor_cell.borrow_mut();

    // NOTE: order of reads matters
    cursor.read_exact(&mut version_bytes)?;
    cursor.read_exact(&mut flag_bytes)?;
    cursor.read_exact(&mut stream_bytes)?;
    cursor.read_exact(&mut opcode_bytes)?;
    cursor.read_exact(&mut length_bytes)?;

    let version = Version::from(version_bytes.to_vec());
    let flags = Flag::get_collection(flag_bytes[0]);
    let stream = from_u16_bytes(&stream_bytes);
    let opcode = Opcode::from(opcode_bytes[0]);
    let length = from_bytes(&length_bytes) as usize;

    let mut body_bytes = Vec::with_capacity(length);
    unsafe {
        body_bytes.set_len(length);
    }

    cursor.read_exact(&mut body_bytes)?;

    let full_body = if flags.iter().any(|flag| flag == &Flag::Compression) {
        compressor.decode(body_bytes)?
    } else {
        Compression::None.decode(body_bytes)?
    };

    // Use cursor to get tracing id, warnings and actual body
    let mut body_cursor = Cursor::new(full_body.as_slice());

    let tracing_id = if flags.iter().any(|flag| flag == &Flag::Tracing) {
        let mut tracing_bytes = Vec::with_capacity(UUID_LEN);
        unsafe {
            tracing_bytes.set_len(UUID_LEN);
        }
        body_cursor.read_exact(&mut tracing_bytes)?;

        decode_timeuuid(tracing_bytes.as_slice()).ok()
    } else {
        None
    };

    let warnings = if flags.iter().any(|flag| flag == &Flag::Warning) {
        CStringList::from_cursor(&mut body_cursor)?.into_plain()
    } else {
        vec![]
    };

    let custom_payload = if flags.iter().any(|flag| flag == &Flag::CustomPayload) {
        let payload = read_bytes_map(&mut body_cursor).unwrap();
        Some(payload)
    } else {
        None
    };

    let mut body = vec![];

    body_cursor.read_to_end(&mut body)?;

    let frame = Frame {
        version: version,
        flags: flags,
        opcode: opcode,
        stream: stream,
        body: body,
        tracing_id: tracing_id,
        warnings: warnings,
    };

    convert_frame_into_result(frame)
}

fn convert_frame_into_result(frame: Frame) -> error::Result<Frame> {
    match frame.opcode {
        Opcode::Error => frame.get_body().and_then(|err| match err {
            ResponseBody::Error(err) => Err(error::Error::Server(err)),
            _ => unreachable!(),
        }),
        _ => Ok(frame),
    }
}

fn read_int(cursor: &mut Cursor<&[u8]>) -> error::Result<i32> {
    let mut body_bytes: Vec<u8> = Vec::with_capacity(4);
    unsafe {
        body_bytes.set_len(4);
    }

    cursor.read_exact(&mut body_bytes).unwrap();

    let v = i32::from_be_bytes(body_bytes.try_into().unwrap());
    Ok(v)
}

fn read_int_length(cursor: &mut Cursor<&[u8]>) -> error::Result<usize> {
    let v = read_int(cursor)?;
    let v: usize = v.try_into().unwrap();

    Ok(v)
}

fn read_bytes<'a>(cursor: &mut Cursor<&[u8]>) -> error::Result<Vec<u8>> {
    let len = read_int_length(cursor)?;
    let v = read_raw_bytes(len, cursor)?;
    Ok(v)
}

fn read_raw_bytes<'a>(count: usize, cursor: &mut Cursor<&[u8]>) -> error::Result<Vec<u8>> {
    let mut body_bytes: Vec<u8> = Vec::with_capacity(count);
    unsafe {
        body_bytes.set_len(count);
    }

    cursor.read_exact(&mut body_bytes).unwrap();

    Ok(body_bytes)
}

fn read_short(cursor: &mut Cursor<&[u8]>) -> error::Result<u16> {
    let mut body_bytes: Vec<u8> = Vec::with_capacity(2);
    unsafe {
        body_bytes.set_len(2);
    }

    cursor.read_exact(&mut body_bytes).unwrap();

    let v = u16::from_be_bytes(body_bytes[0..].try_into().unwrap());
    Ok(v)
}

fn read_short_length(buf: &mut Cursor<&[u8]>) -> error::Result<usize> {
    let v = read_short(buf)?;
    let v: usize = v.try_into().unwrap();
    Ok(v)
}

fn read_string<'a>(buf: &mut Cursor<&[u8]>) -> error::Result<String> {
    let len = read_short_length(buf)?;
    let raw = read_raw_bytes(len, buf)?;
    let v = std::str::from_utf8(&raw).unwrap();
    Ok(v.into())
}

fn read_bytes_map(buf: &mut Cursor<&[u8]>) -> error::Result<HashMap<String, Vec<u8>>> {
    let len = read_short_length(buf)?;
    let mut v = HashMap::with_capacity(len);
    for _ in 0..len {
        let key = read_string(buf)?.to_owned();
        let val = read_bytes(buf)?.to_owned();
        v.insert(key, val);
    }
    Ok(v)
}
