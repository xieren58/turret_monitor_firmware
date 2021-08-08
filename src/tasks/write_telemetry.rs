use core::convert::TryInto;

use rtic::{mutex_prelude::*, time::duration::Seconds};
use rtt_target::rprintln;
use serde::{Deserialize, Serialize};
use stm32f4xx_hal::{crc32::Crc32, prelude::*};

use crate::app::{Usart1Buf, Usart1TransferTx, Usart1Tx, BUF_SIZE, MESSAGE_SIZE};
use crate::datamodel::telemetry_packet::TurretTelemetryPacket;
use crate::tasks::usart1_rx::compute_crc;

pub enum TxBufferState {
    // Ready, use the contained buffer for next transfer
    Running(Usart1TransferTx),
    // In flight, but here is the next buffer to use.
    Idle(Usart1TransferTx),
}

pub(crate) fn write_telemetry(
    // unfortunately, as a implementation detail we cannot take an immutable reference
    // to this shared resource since another task mutates it. we have to take it mutably even
    // if we are only reading it.
    mut context: crate::app::write_telemetry::Context,
) {
    rprintln!("tick!");

    /*
        entering critical section
    */
    let turret_position: f32 = context
        .shared
        .last_observed_turret_position
        .lock(|guard| *guard);

    /*
        leaving critical section
    */
    // retrieve the DMA state
    let dma_state: TxBufferState = context
        .shared
        .send
        .take()
        .expect("failed to aquire buffer state");

    // declare a buffer to fit the response in
    let mut payload_buffer: [u8; BUF_SIZE] = [0xFF; BUF_SIZE];
    // define the response
    let payload = TurretTelemetryPacket {
        turret_pos: turret_position,
    };
    // attempt to serialize the response
    let payload_size = match serde_json_core::to_slice(&payload, &mut payload_buffer) {
        Err(e) => {
            rprintln!("Failed to encode, error {:?}", e);
            // bail out without panicking.
            return;
        }
        Ok(size) => size,
    };
    rprintln!("payload  before CRC := {:?}", payload_buffer);
    // sanity check.
    if payload_size > MESSAGE_SIZE - 4 {
        rprintln!("Encoded payload is too big! need at least 4 bytes to fit the CRC32!");
        return;
    }

    /*
    entering critical section
     */
    let checksum: u32 = context.shared.crc.lock(|crc: &mut Crc32| {
        compute_crc(&payload_buffer[..payload_size],crc)
    });
    rprintln!("CRC := {}", checksum);
    /*
    exiting critical section
     */

    // append the CRC32 to the end.
    payload_buffer[payload_size..payload_size + 4].copy_from_slice(&checksum.to_be_bytes());
    rprintln!("buffer state before cobs := {:?}", payload_buffer);

    // if the DMA is idle, start a new transfer.
    if let TxBufferState::Idle(mut tx) = dma_state {
        rprintln!("DMA was idle, setting up next transfer...");
        // SAFETY: memory corruption can occur in double-buffer mode in the event of an overrun.
        //   - we are in single-buffer mode so this is safe.
        unsafe {
            // We re-use the existing DMA buffer, since the buffer has to live for 'static
            // in order to be safe. This was ensured during creation of the Transfer object.
            tx.next_transfer_with(|buf, _| {
                // populate the DMA buffer with the new buffer's content
                postcard_cobs::encode(&payload_buffer[0..payload_size + 4], buf);
                // log the TX buffer
                rprintln!("buf :: {:?}", buf);
                // calculate the buffer's length, if only to satisfy the closure's contract.
                let buf_len = buf.len();
                (buf, buf_len) // Don't know what the second argument is, but it seems to be ignored.
            })
            .expect("Something went horribly wrong setting up the transfer.");
        }
        // update the DMA state into the running phase
        *context.shared.send = Some(TxBufferState::Running(tx));
    } else {
        rprintln!("[WARNING] write_Telemetry called but a previous USART1 DMA was still active!");
    };
}