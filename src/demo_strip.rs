use anyhow::{bail, Result};

// Port of https://github.com/Nocrex/tf2-scripts/blob/main/strip_demo.py:
// walks the demo's message stream and cuts out every ConsoleCmd (type 4) packet
// in place, since those often contain exec'd cheat configs recorded into the demo.
const HEADER_SIZE: usize = 1072;
const HEADER_MAGIC: &[u8; 8] = b"HL2DEMO\0";

fn read_u32_le(data: &[u8], at: usize) -> Result<u32> {
    let bytes = data
        .get(at..at + 4)
        .ok_or_else(|| anyhow::anyhow!("demo data truncated"))?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

/// Remove every `dem_usercmd` packet while preserving the rest of the demo byte-for-byte.
pub fn strip_user_commands(data: &[u8]) -> Result<(Vec<u8>, usize)> {
    if data.len() < HEADER_SIZE {
        bail!("File is too small to be a valid Source demo");
    }
    if &data[..HEADER_MAGIC.len()] != HEADER_MAGIC {
        bail!("Not a valid HL2DEMO file (bad magic)");
    }

    let demo_protocol = i32::from_le_bytes(data[8..12].try_into().unwrap());
    let has_slot = demo_protocol >= 4;
    let mut out = Vec::with_capacity(data.len());
    out.extend_from_slice(&data[..HEADER_SIZE]);

    let mut offset = HEADER_SIZE;
    let mut stripped = 0;
    while offset < data.len() {
        let frame_start = offset;
        let command = data[offset];
        offset += 1;

        if command == 7 {
            // dem_stop has no tick or slot. Keep it and any trailing bytes verbatim.
            out.extend_from_slice(&data[frame_start..]);
            break;
        }

        if offset + 4 > data.len() {
            bail!("Unexpected EOF reading tick at offset {offset}");
        }
        offset += 4;

        if has_slot {
            if offset >= data.len() {
                bail!("Unexpected EOF reading player slot at offset {offset}");
            }
            offset += 1;
        }

        match command {
            1 | 2 => {
                // democmdinfo_t (76 bytes) plus incoming/outgoing sequences (8 bytes).
                if offset + 88 > data.len() {
                    bail!("Unexpected EOF reading packet header at offset {offset}");
                }
                offset += 84;
                let length = read_payload_length(data, &mut offset, command)?;
                advance_payload(data, &mut offset, length, command)?;
                out.extend_from_slice(&data[frame_start..offset]);
            }
            3 => out.extend_from_slice(&data[frame_start..offset]),
            4 | 6 | 8 => {
                let length = read_payload_length(data, &mut offset, command)?;
                advance_payload(data, &mut offset, length, command)?;
                out.extend_from_slice(&data[frame_start..offset]);
            }
            5 => {
                if offset + 8 > data.len() {
                    bail!("Unexpected EOF reading usercmd header at offset {offset}");
                }
                offset += 4;
                let length = read_payload_length(data, &mut offset, command)?;
                advance_payload(data, &mut offset, length, command)?;
                stripped += 1;
            }
            9 => {
                if offset + 8 > data.len() {
                    bail!("Unexpected EOF reading customdata header at offset {offset}");
                }
                offset += 4;
                let length = read_payload_length(data, &mut offset, command)?;
                advance_payload(data, &mut offset, length, command)?;
                out.extend_from_slice(&data[frame_start..offset]);
            }
            _ => bail!("Corrupted or unsupported demo command {command} at offset {frame_start}"),
        }
    }

    Ok((out, stripped))
}

fn read_payload_length(data: &[u8], offset: &mut usize, command: u8) -> Result<usize> {
    if *offset + 4 > data.len() {
        bail!("Unexpected EOF reading length for command {command} at offset {offset}");
    }
    let length = i32::from_le_bytes(data[*offset..*offset + 4].try_into().unwrap());
    *offset += 4;
    if length < 0 {
        bail!("Invalid negative length {length} for command {command}");
    }
    Ok(length as usize)
}

fn advance_payload(data: &[u8], offset: &mut usize, length: usize, command: u8) -> Result<()> {
    let end = offset
        .checked_add(length)
        .ok_or_else(|| anyhow::anyhow!("Payload length overflow for command {command}"))?;
    if end > data.len() {
        bail!("Truncated payload for command {command} at offset {offset}");
    }
    *offset = end;
    Ok(())
}

pub fn strip_console_commands(data: &[u8]) -> Result<(Vec<u8>, usize)> {
    let mut data = data.to_vec();
    let mut ind = HEADER_SIZE;
    let mut stripped = 0usize;

    while ind < data.len() {
        let packet_type = data[ind];
        ind += 1;
        // Stop's tick field is truncated to 3 bytes in this demo format.
        ind += if packet_type == 7 { 3 } else { 4 };

        match packet_type {
            1 | 2 => {
                // Signon / Message: fixed CmdInfo block, then a length-prefixed payload.
                ind += 84;
                let length = read_u32_le(&data, ind)? as usize;
                ind += 4 + length;
            }
            3 | 7 => {
                // SyncTick / Stop: no extra payload.
            }
            4 => {
                // ConsoleCmd: cut the whole record out (type + tick + length + payload).
                let length = read_u32_le(&data, ind)? as usize;
                ind -= 1 + 4;
                let end = ind + 1 + 4 + 4 + length;
                if end > data.len() {
                    bail!("console command packet extends past end of demo");
                }
                data.drain(ind..end);
                stripped += 1;
            }
            5 => {
                // UserCmd: sequence number, then a length-prefixed payload.
                ind += 4;
                let length = read_u32_le(&data, ind)? as usize;
                ind += 4 + length;
            }
            6 | 8 => {
                // DataTable / StringTable: length-prefixed payload.
                let length = read_u32_le(&data, ind)? as usize;
                ind += 4 + length;
            }
            other => bail!("unknown demo packet type {other}"),
        }
    }

    Ok((data, stripped))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(command: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![command];
        bytes.extend_from_slice(&123_i32.to_le_bytes());
        bytes.push(0); // player slot for demo protocol 4
        bytes.extend_from_slice(&(payload.len() as i32).to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn strips_only_user_commands() {
        let mut demo = vec![0; HEADER_SIZE];
        demo[..8].copy_from_slice(HEADER_MAGIC);
        demo[8..12].copy_from_slice(&4_i32.to_le_bytes());

        let mut sync_tick = vec![3];
        sync_tick.extend_from_slice(&100_i32.to_le_bytes());
        sync_tick.push(0);
        demo.extend_from_slice(&sync_tick);

        let mut user_command = vec![5];
        user_command.extend_from_slice(&101_i32.to_le_bytes());
        user_command.push(0);
        user_command.extend_from_slice(&42_i32.to_le_bytes());
        user_command.extend_from_slice(&3_i32.to_le_bytes());
        user_command.extend_from_slice(&[1, 2, 3]);
        demo.extend_from_slice(&user_command);

        let console_command = command(4, b"echo test\0");
        demo.extend_from_slice(&console_command);
        demo.push(7);

        let (stripped, count) = strip_user_commands(&demo).unwrap();
        let mut expected = demo[..HEADER_SIZE].to_vec();
        expected.extend_from_slice(&sync_tick);
        expected.extend_from_slice(&console_command);
        expected.push(7);

        assert_eq!(count, 1);
        assert_eq!(stripped, expected);
    }

    #[test]
    fn rejects_truncated_user_command_without_writing_partial_output() {
        let mut demo = vec![0; HEADER_SIZE];
        demo[..8].copy_from_slice(HEADER_MAGIC);
        demo[8..12].copy_from_slice(&4_i32.to_le_bytes());
        demo.push(5);
        demo.extend_from_slice(&101_i32.to_le_bytes());
        demo.push(0);

        assert!(strip_user_commands(&demo).is_err());
    }
}
