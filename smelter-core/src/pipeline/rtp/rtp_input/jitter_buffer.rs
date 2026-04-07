use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use tracing::{debug, trace};

use crate::pipeline::{
    rtp::{
        RtpInputEvent, RtpPacket,
        rtp_input::rtcp_sync::{RtpNtpSyncPoint, RtpTimestampSync},
    },
    utils::input_buffer::InputBuffer,
};

use crate::prelude::*;

struct JitterBufferPacket {
    packet: webrtc::rtp::packet::Packet,
    pts: Duration,
}

#[derive(Debug, Clone, Copy)]
pub enum RtpJitterBufferMode {
    /// If I receive packet for sample `x`, then don't wait
    /// for missing packets older than `(x-size)`
    FixedWindow(Duration),
    /// Packet needs to be returned from buffer before instant.elapsed() gets bigger
    /// than pts. (with some extra fixed buffer e.g. 80ms)
    RealTime,
}

// Value shared between both video and audio track, while actual RtpJitterBuffer
// should be created per track
#[derive(Debug, Clone)]
pub(crate) struct RtpJitterBufferSharedContext {
    mode: RtpJitterBufferMode,
    ntp_sync_point: Arc<RtpNtpSyncPoint>,
    input_buffer: InputBuffer,
}

impl RtpJitterBufferSharedContext {
    pub fn new(ctx: &Arc<PipelineCtx>, mode: RtpJitterBufferMode, reference_time: Instant) -> Self {
        let ntp_sync_point = RtpNtpSyncPoint::new(reference_time);
        Self {
            mode,
            ntp_sync_point,
            input_buffer: match mode {
                RtpJitterBufferMode::FixedWindow(window_size) => {
                    // PTS are synced on first packet, so if jitter buffer is the same as an
                    // input buffer then, at the worst case it would produce PTS at the time
                    // where queue already needs it, We are adding 80ms (`default_buffer_duration`)
                    // so packets can reach the queue on time.
                    //
                    // If input has an offset then above does not apply, however PTS should be
                    // normalized to zero, so adding a constant value should not affect anything
                    let effective_window = window_size + ctx.default_buffer_duration;
                    InputBuffer::new(
                        ctx,
                        InputBufferOptions::Const {
                            size: effective_window,
                        },
                    )
                }
                RtpJitterBufferMode::RealTime => {
                    InputBuffer::new(ctx, InputBufferOptions::LatencyOptimized { reference_time })
                }
            },
        }
    }
}

pub(crate) struct RtpJitterBuffer {
    shared_ctx: RtpJitterBufferSharedContext,
    timestamp_sync: RtpTimestampSync,
    seq_num_rollover: SequenceNumberRollover,
    packets: BTreeMap<u64, JitterBufferPacket>,
    /// Last sequence number returned from `pop_packets`
    next_seq_num: Option<u64>,
    on_stats_event: Box<dyn FnMut(RtpJitterBufferStatsEvent) + 'static + Send>,
}

/// We are assuming here that it is enough time to decode. Might be
/// problematic in case of B-frames, because it would require processing multiple
/// frames before
const MIN_DECODE_TIME: Duration = Duration::from_millis(30);

impl RtpJitterBuffer {
    pub fn new(
        shared_ctx: RtpJitterBufferSharedContext,
        clock_rate: u32,
        on_stats_event: Box<dyn FnMut(RtpJitterBufferStatsEvent) + 'static + Send>,
    ) -> Self {
        let timestamp_sync = RtpTimestampSync::new(shared_ctx.ntp_sync_point.clone(), clock_rate);

        Self {
            shared_ctx,
            timestamp_sync,
            seq_num_rollover: SequenceNumberRollover::default(),
            packets: BTreeMap::new(),
            next_seq_num: None,
            on_stats_event,
        }
    }

    pub fn on_sender_report(&mut self, ntp_time: u64, rtp_timestamp: u32) {
        self.timestamp_sync
            .on_sender_report(ntp_time, rtp_timestamp);
    }

    pub fn write_packet(&mut self, packet: webrtc::rtp::packet::Packet) {
        let sequence_number = self
            .seq_num_rollover
            .rolled_sequence_number(packet.header.sequence_number);

        if let Some(last_returned) = self.next_seq_num
            && last_returned > sequence_number
        {
            debug!(sequence_number, "Packet to old. Dropping.");
            return;
        }

        (self.on_stats_event)(RtpJitterBufferStatsEvent::RtpPacketReceived);
        (self.on_stats_event)(RtpJitterBufferStatsEvent::BytesReceived(
            packet.payload.len(),
        ));

        // pts relative to reference_time in ntp_sync_point
        let pts = self
            .timestamp_sync
            .pts_from_timestamp(packet.header.timestamp);

        self.shared_ctx.input_buffer.recalculate_buffer(pts);

        trace!(packet=?packet.header, ?pts, buffer_size=self.packets.len(), "Writing packet to jitter buffer");
        self.packets
            .insert(sequence_number, JitterBufferPacket { packet, pts });
    }

    pub fn pop(&mut self) -> Option<RtpInputEvent> {
        let (first_seq_num, _first_packet) = self.packets.first_key_value()?;

        if self.next_seq_num == Some(*first_seq_num) {
            return self.force_pop();
        }

        let wait_for_next_packet = match self.shared_ctx.mode {
            RtpJitterBufferMode::FixedWindow(window_size) => {
                let lowest_pts = self.packets.values().map(|packet| packet.pts).min()?;
                let highest_pts = self.packets.values().map(|packet| packet.pts).max()?;
                highest_pts.saturating_sub(lowest_pts) < window_size
            }
            RtpJitterBufferMode::RealTime => {
                let lowest_pts = self.packets.values().map(|packet| packet.pts).min()?;

                // TODO: if lowest pts is not first it means that we have B-frames
                //
                // It would be safer to use value based on index than constant, in the worst
                // case scenario this could be 16 frames that needs to decoded in that time
                let next_pts = lowest_pts + self.shared_ctx.input_buffer.size();
                let reference_time = self.shared_ctx.ntp_sync_point.reference_time;
                next_pts > reference_time.elapsed() + MIN_DECODE_TIME
            }
        };

        if wait_for_next_packet {
            return None;
        }

        self.force_pop()
    }

    pub fn force_pop(&mut self) -> Option<RtpInputEvent> {
        let (seq_num, packet) = self.packets.pop_first()?;

        if let Some(next) = self.next_seq_num
            && seq_num != next
        {
            (self.on_stats_event)(RtpJitterBufferStatsEvent::RtpPacketLost);
            self.next_seq_num = Some(next + 1);
            return Some(RtpInputEvent::LostPacket);
        }

        let input_buffer_size = self.shared_ctx.input_buffer.size();
        let timestamp = packet.pts + input_buffer_size;

        let reference_time = self.shared_ctx.ntp_sync_point.reference_time;
        (self.on_stats_event)(RtpJitterBufferStatsEvent::EffectiveBuffer(
            timestamp.saturating_sub(reference_time.elapsed()),
        ));
        (self.on_stats_event)(RtpJitterBufferStatsEvent::InputBufferSize(
            input_buffer_size,
        ));

        self.next_seq_num = Some(seq_num + 1);
        Some(RtpInputEvent::Packet(RtpPacket {
            packet: packet.packet,
            timestamp,
        }))
    }

    pub fn peek_next_pts(&self) -> Option<Duration> {
        let (_, packet) = self.packets.first_key_value()?;
        Some(packet.pts + self.shared_ctx.input_buffer.size())
    }
}

#[derive(Debug, Default)]
struct SequenceNumberRollover {
    rollover_count: u64,
    last_value: Option<u16>,
}

impl SequenceNumberRollover {
    fn rolled_sequence_number(&mut self, sequence_number: u16) -> u64 {
        let last_value = *self.last_value.get_or_insert(sequence_number);

        let diff = u16::abs_diff(last_value, sequence_number);
        if diff >= u16::MAX / 2 {
            if last_value > sequence_number {
                self.rollover_count += 1;
            } else {
                // We received a packet from before the rollover, so we need to decrement the count
                self.rollover_count = self.rollover_count.saturating_sub(1);
            }
        }
        self.last_value = Some(sequence_number);

        (self.rollover_count * (u16::MAX as u64 + 1)) + sequence_number as u64
    }
}
