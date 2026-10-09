//! Lets keyframe requests and receiver reports reach the host's window
//! tracks.
//!
//! webrtc's interceptors consume inbound RTCP; a packet only reaches a
//! track's event stream when an interceptor marks it for the application.
//! This one marks picture-loss and full-intra requests (what an encoder
//! needs to know about) and receiver reports (the client's packet loss,
//! which adaptive quality steers by), and drops every other RTCP packet
//! from the application path after the default interceptors have acted on
//! it.
//! Adapted from the `rtcp-processing` example in webrtc-rs (MIT/Apache-2.0).

use std::collections::VecDeque;
use std::time::Instant;

use rtc::interceptor::{Attribute, Interceptor, Packet, Registry, Slot, StreamInfo, TaggedPacket};
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::MediaEngine;
use rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtcp::receiver_report::ReceiverReport;
use rtc::sansio::Protocol;
use rtc::shared::error::{Error, Result};

/// After the default interceptors, which use slots below this.
const SLOT: usize = 14_000;

pub(crate) fn interceptor_registry(media_engine: &mut MediaEngine) -> Result<Registry> {
    Ok(
        register_default_interceptors(Registry::new(), media_engine)?
            .with(Slot::from(SLOT), KeyframeRequests::default()),
    )
}

pub(crate) fn is_keyframe_request(packet: &dyn rtc::rtcp::Packet) -> bool {
    let any = packet.as_any();
    any.is::<PictureLossIndication>() || any.is::<FullIntraRequest>()
}

/// The loss a client reports for `ssrc`, as a fraction, if `packet` is a
/// receiver report that covers it.
pub(crate) fn reported_loss(packet: &dyn rtc::rtcp::Packet, ssrc: u32) -> Option<f32> {
    let report = packet.as_any().downcast_ref::<ReceiverReport>()?;
    report
        .reports
        .iter()
        .find(|r| r.ssrc == ssrc)
        .map(|r| f32::from(r.fraction_lost) / 256.0)
}

fn for_application(packet: &dyn rtc::rtcp::Packet) -> bool {
    is_keyframe_request(packet) || packet.as_any().is::<ReceiverReport>()
}

#[derive(Default)]
pub(crate) struct KeyframeRequests {
    read_queue: VecDeque<TaggedPacket>,
    write_queue: VecDeque<TaggedPacket>,
}

impl Protocol<TaggedPacket, TaggedPacket, ()> for KeyframeRequests {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = Error;
    type Time = Instant;

    fn handle_read(&mut self, mut msg: TaggedPacket) -> std::result::Result<(), Error> {
        if let Packet::Rtcp(packets) = &msg.message.packet {
            let requests: Vec<Box<dyn rtc::rtcp::Packet>> = packets
                .iter()
                .filter(|packet| for_application(packet.as_ref()))
                .cloned()
                .collect();
            if requests.is_empty() {
                return Ok(());
            }
            msg.message.packet = Packet::Rtcp(requests);
            msg.message.add(Attribute::DeliverToApplication);
        }
        self.read_queue.push_back(msg);
        Ok(())
    }

    fn poll_read(&mut self) -> Option<TaggedPacket> {
        self.read_queue.pop_front()
    }

    fn handle_write(&mut self, msg: TaggedPacket) -> std::result::Result<(), Error> {
        self.write_queue.push_back(msg);
        Ok(())
    }

    fn poll_write(&mut self) -> Option<TaggedPacket> {
        self.write_queue.pop_front()
    }
}

impl Interceptor for KeyframeRequests {
    fn bind_local_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_local_stream(&mut self, _info: &StreamInfo) {}
    fn bind_remote_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_remote_stream(&mut self, _info: &StreamInfo) {}
}
