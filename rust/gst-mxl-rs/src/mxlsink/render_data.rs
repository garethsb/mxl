// SPDX-FileCopyrightText: 2026 Contributors to the Media eXchange Layer project.
// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use crate::format;
use crate::mxlsink::{
    self,
    render_audio::WriteGrainData,
    state::{DataCommand, DataEngine, InitialTime, St2038Alignment, State},
};

use gstreamer::{self as gst, prelude::*};
use mxl::Rational;
use tracing::trace;

const LATENCY_CUSHION: u64 = 5_000_000;

pub(crate) fn data(
    state: &mut mxlsink::state::State,
    buffer: &gst::Buffer,
) -> Result<gst::FlowSuccess, gst::FlowError> {
    let data_state = state.data.as_mut().ok_or(gst::FlowError::Error)?;
    let flow = state.flow.as_ref().ok_or(gst::FlowError::Error)?;
    let grain_rate = flow
        .common()
        .grain_rate()
        .map_err(|_| gst::FlowError::Error)?;

    let clock = state.pipeline.clock().ok_or(gst::FlowError::Error)?;
    let gst_now = clock.time();
    let mxl_now = state.instance.get_time();
    let initial = data_state.initial_time.get_or_insert(InitialTime {
        mxl_pts_offset: mxl_now - gst_now.nseconds(),
    });

    let initial_pts_offset = initial.mxl_pts_offset;

    let gst_pts = buffer.pts().ok_or(gst::FlowError::Error)?;
    let mxl_pts = gst_pts.nseconds() + initial_pts_offset;
    let mut pts = mxl_pts + grains_to_ns(data_state.latency, &grain_rate);
    if pts < mxl_now {
        let diff_ns = mxl_now - pts;
        let diff_grains = ns_to_grains(diff_ns, &grain_rate);
        data_state.latency += diff_grains;
        let latency_ns = grains_to_ns(data_state.latency, &grain_rate);
        pts = mxl_pts + latency_ns + LATENCY_CUSHION;
    }
    trace!("DATA gst PTS: {:#?}", gst_pts);
    trace!("DATA mapped PTS: {:#?}", pts);
    let mxl_index = state
        .instance
        .timestamp_to_index(pts, &grain_rate)
        .map_err(|_| gst::FlowError::Error)?;
    trace!("DATA mapped mxl_index from pts: {:#?}", mxl_index);

    match data_state.st2038_alignment {
        St2038Alignment::Frame => {
            commit_buffer(buffer, data_state, mxl_index)?;
            data_state.grain_index = data_state.grain_index.wrapping_add(1);
        }
        St2038Alignment::Packet => {
            if let Some(pending_mxl_index) = data_state.pending_grain_mxl_index {
                if pending_mxl_index != mxl_index {
                    if let Some(builder) = data_state.pending_grain_builder.take() {
                        let buf = builder
                            .into_padded_grain()
                            .map_err(|_| gst::FlowError::Error)?;
                        commit_padded_grain(&data_state.tx, buf, pending_mxl_index)?;
                        data_state.grain_index = data_state.grain_index.wrapping_add(1);
                    }
                    data_state.pending_grain_mxl_index = None;
                }
            }

            if data_state.pending_grain_builder.is_none() {
                data_state.pending_grain_builder = Some(
                    format::data::MxlSmpte291GrainBuilder::new()
                        .map_err(|_| gst::FlowError::Error)?,
                );
                data_state.pending_grain_mxl_index = Some(mxl_index);
            }

            let builder = data_state
                .pending_grain_builder
                .as_mut()
                .ok_or(gst::FlowError::Error)?;
            let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
            builder
                .append_st2038_packets(map.as_slice())
                .map_err(|_| gst::FlowError::Error)?;

            if buffer.flags().contains(gst::BufferFlags::MARKER) {
                let pending_mxl_index = data_state
                    .pending_grain_mxl_index
                    .take()
                    .ok_or(gst::FlowError::Error)?;
                let builder = data_state
                    .pending_grain_builder
                    .take()
                    .ok_or(gst::FlowError::Error)?;
                let buf = builder
                    .into_padded_grain()
                    .map_err(|_| gst::FlowError::Error)?;
                commit_padded_grain(&data_state.tx, buf, pending_mxl_index)?;
                data_state.grain_index = data_state.grain_index.wrapping_add(1);
            }
        }
    }

    Ok(gst::FlowSuccess::Ok)
}

/// EOS hook for the `meta/x-st-2038` data path. No-op if caps used `alignment=frame`.
pub(crate) fn eos(state: &mut State) {
    let Some(data) = state.data.as_mut() else {
        return;
    };
    if data.st2038_alignment == St2038Alignment::Frame {
        return;
    }
    let Some(pending_mxl_index) = data.pending_grain_mxl_index else {
        return;
    };
    let Some(builder) = data.pending_grain_builder.take() else {
        return;
    };
    let Ok(buf) = builder.into_padded_grain() else {
        trace!("packet-mode EOS: into_padded_grain failed");
        data.pending_grain_mxl_index = None;
        return;
    };
    if commit_padded_grain(&data.tx, buf, pending_mxl_index).is_err() {
        trace!("packet-mode EOS: commit failed");
        return;
    }
    data.pending_grain_mxl_index = None;
    data.grain_index = data.grain_index.wrapping_add(1);
}

fn commit_padded_grain(
    tx: &crossbeam::channel::Sender<DataCommand>,
    buf: Vec<u8>,
    index: u64,
) -> Result<(), gst::FlowError> {
    let data = WriteGrainData { buf, index };
    tx.send(DataCommand::Write { data })
        .map_err(|_| gst::FlowError::Error)?;
    Ok(())
}

fn commit_buffer(
    buffer: &gst::Buffer,
    data_state: &mut mxlsink::state::DataState,
    index: u64,
) -> Result<(), gst::FlowError> {
    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
    let buf = format::data::mxl_smpte291_grain_from_gst_st2038(map.as_slice())
        .map_err(|_| gst::FlowError::Error)?;
    commit_padded_grain(&data_state.tx, buf, index)
}

pub fn await_data_buffer(
    engine: &mut DataEngine,
    rx: crossbeam::channel::Receiver<DataCommand>,
) -> Result<(), gst::FlowError> {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            DataCommand::Write { data } => {
                if let Err(e) = write_buffer(engine, data) {
                    trace!("Data engine error: {:?}", e);
                }
            }
        }
    }
    trace!("DELETING DATA FLOW");
    engine
        .writer
        .take()
        .ok_or(gst::FlowError::Error)?
        .destroy()
        .map_err(|_| gst::FlowError::Error)?;
    Ok(())
}

fn write_buffer(engine: &mut DataEngine, data: WriteGrainData) -> Result<(), gst::FlowError> {
    trace!("DATA THREAD IS WRITING A BUFFER");
    let mut access = engine
        .writer
        .as_ref()
        .ok_or(gst::FlowError::Error)?
        .open_grain(data.index)
        .map_err(|_| gst::FlowError::Error)?;
    let payload = access.payload_mut();
    let copy_len = std::cmp::min(payload.len(), data.buf.len());
    payload[..copy_len].copy_from_slice(&data.buf[..copy_len]);
    let total_slices = access.total_slices();
    let mxl_now = engine.instance.get_time();
    let pts = engine
        .instance
        .index_to_timestamp(data.index, &engine.grain_rate)
        .map_err(|_| gst::FlowError::Error)?;

    if pts > mxl_now {
        trace!("Data sleeping: {:#?}", Duration::from_nanos(pts - mxl_now));
        let (lock, cvar) = &*engine.sleep_flag;

        let mut shutdown_guard = lock.lock().map_err(|_| gst::FlowError::Error)?;

        if !*shutdown_guard && pts > mxl_now {
            let remaining = Duration::from_nanos(pts - mxl_now);

            let (guard, _timeout_result) = cvar
                .wait_timeout(shutdown_guard, remaining)
                .map_err(|_| gst::FlowError::Error)?;

            shutdown_guard = guard;
        }

        if *shutdown_guard {
            return Ok(());
        }
    }
    access
        .commit(total_slices)
        .map_err(|_| gst::FlowError::Error)?;
    Ok(())
}

fn grains_to_ns(grains: u64, rate: &Rational) -> u64 {
    ((grains as u128 * 1_000_000_000u128 * rate.denominator as u128) / rate.numerator as u128)
        as u64
}

fn ns_to_grains(ns: u64, rate: &Rational) -> u64 {
    ((ns as u128 * rate.numerator as u128) / (rate.denominator as u128 * 1_000_000_000u128)) as u64
}
