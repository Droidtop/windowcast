//! Lets keyframe requests reach the host's window tracks.
//!
//! webrtc's interceptors consume inbound RTCP; a packet only reaches a
//! track's event stream when an interceptor marks it for the application.
//! This one marks picture-loss and full-intra requests (what an encoder
//! needs to know about) and drops every other RTCP packet from the
//! application path after the default interceptors have acted on it.
//! Adapted from the `rtcp-processing` example in webrtc-rs (MIT/Apache-2.0).

use std::collections::VecDeque;
use std::time::Instant;

use rtc::interceptor::{Attribute, Interceptor, Packet, Registry, Slot, StreamInfo, TaggedPacket};
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::MediaEngine;
use rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
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
                .filter(|packet| is_keyframe_request(packet.as_ref()))
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
