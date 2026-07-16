use anyhow::{bail, Result};

// Port of https://github.com/Nocrex/tf2-scripts/blob/main/strip_demo.py:
// walks the demo's message stream and cuts out every ConsoleCmd (type 4) packet
// in place, since those often contain exec'd cheat configs recorded into the demo.
const HEADER_SIZE: usize = 1072;

fn read_u32_le(data: &[u8], at: usize) -> Result<u32> {
    let bytes = data
        .get(at..at + 4)
        .ok_or_else(|| anyhow::anyhow!("demo data truncated"))?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

/// Returns the stripped demo bytes and the number of console commands removed.
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
