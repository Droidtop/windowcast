//! The host's end of chat and notices (docs/COMMAND-STREAM.md, "Files"). The host application
//! gives the host a [`ChatHub`] ([`crate::HostControl::set_chat`]): it hears what clients say
//! through the hub's [`ChatSink`], and it speaks to clients with [`ChatHub::notice`] and
//! [`ChatHub::say_to`]. A host with no hub refuses chat channels. Nothing is stored.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windowcast_identity::PeerId;
use windowcast_protocol::chat::{decode, encode, problem_with, ChatMessage, MAX_PER_SECOND};

use crate::command::{Principal, RunningProcess};

/// What the host application does with a message from a client (show it on the host's screen,
/// say). Called off the session's task; keep it quick.
pub trait ChatSink: Send + Sync + 'static {
    fn received(&self, from: &Principal, text: &str);
}

type Send1 = Arc<dyn Fn(Vec<u8>) -> bool + Send + Sync>;

/// The chat channels open now, and where a client's words go.
pub struct ChatHub {
    sink: Arc<dyn ChatSink>,
    open: Mutex<HashMap<u64, (PeerId, Send1)>>,
    next: AtomicU64,
}

impl ChatHub {
    pub fn new(sink: Arc<dyn ChatSink>) -> Arc<ChatHub> {
        Arc::new(ChatHub {
            sink,
            open: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        })
    }

    /// Sends `text` to every client with a chat channel open; returns how many it reached. A
    /// notice is just this: the host speaking first.
    pub fn notice(&self, text: &str) -> Result<usize, String> {
        self.deliver(text, |_| true)
    }

    /// Sends `text` to one client's chat channels.
    pub fn say_to(&self, peer: PeerId, text: &str) -> Result<usize, String> {
        self.deliver(text, |p| *p == peer)
    }

    fn deliver(&self, text: &str, wanted: impl Fn(&PeerId) -> bool) -> Result<usize, String> {
        if let Some(problem) = problem_with(text) {
            return Err(problem);
        }
        let bytes = encode(&ChatMessage::Text {
            text: text.to_owned(),
        });
        let targets: Vec<Send1> = self
            .open
            .lock()
            .expect("chat")
            .values()
            .filter(|(peer, _)| wanted(peer))
            .map(|(_, send)| Arc::clone(send))
            .collect();
        Ok(targets.iter().filter(|send| send(bytes.clone())).count())
    }

    /// How many chat channels are open.
    pub fn open_channels(&self) -> usize {
        self.open.lock().expect("chat").len()
    }

    pub(crate) fn open(self: &Arc<Self>, who: Principal, send: Send1) -> ChatProcess {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        self.open
            .lock()
            .expect("chat")
            .insert(id, (who.peer, Arc::clone(&send)));
        ChatProcess {
            hub: Arc::clone(self),
            id,
            who,
            send,
            recent: Mutex::new(Vec::new()),
            ended: (Mutex::new(false), std::sync::Condvar::new()),
        }
    }
}

/// A client's chat channel as the channel table sees it.
pub(crate) struct ChatProcess {
    hub: Arc<ChatHub>,
    id: u64,
    who: Principal,
    send: Send1,
    /// When the last few messages arrived, for the rate limit.
    recent: Mutex<Vec<Instant>>,
    ended: (Mutex<bool>, std::sync::Condvar),
}

impl ChatProcess {
    fn reject(&self, reason: String) {
        (self.send)(encode(&ChatMessage::Rejected { reason }));
    }

    fn too_fast(&self) -> bool {
        let now = Instant::now();
        let mut recent = self.recent.lock().expect("chat");
        recent.retain(|at| now.duration_since(*at) < Duration::from_secs(1));
        if recent.len() >= MAX_PER_SECOND {
            return true;
        }
        recent.push(now);
        false
    }
}

impl RunningProcess for ChatProcess {
    fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        match decode(bytes) {
            Ok(ChatMessage::Text { text }) => {
                if let Some(problem) = problem_with(&text) {
                    self.reject(problem);
                } else if self.too_fast() {
                    self.reject("too many messages; slow down".into());
                } else {
                    self.hub.sink.received(&self.who, &text);
                }
            }
            // A client does not reject the host's messages; ignore it rather than answer one.
            Ok(ChatMessage::Rejected { .. }) => {}
            Err(reason) => self.reject(reason),
        }
        Ok(())
    }

    fn eof(&self) {}

    fn kill(&self) {
        self.hub.open.lock().expect("chat").remove(&self.id);
        *self.ended.0.lock().expect("chat") = true;
        self.ended.1.notify_all();
    }

    fn wait(&self) -> Option<i32> {
        let mut ended = self.ended.0.lock().expect("chat");
        while !*ended {
            ended = self.ended.1.wait(ended).expect("chat");
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Collect(Mutex<Vec<String>>);

    impl ChatSink for Collect {
        fn received(&self, _from: &Principal, text: &str) {
            self.0.lock().unwrap().push(text.to_owned());
        }
    }

    fn principal() -> Principal {
        Principal {
            peer: windowcast_identity::PeerId([7; 32]),
            account: None,
        }
    }

    type Rig = (
        Arc<ChatHub>,
        Arc<Collect>,
        ChatProcess,
        Arc<Mutex<Vec<ChatMessage>>>,
    );

    fn rig() -> Rig {
        let sink = Arc::new(Collect(Mutex::new(Vec::new())));
        let hub = ChatHub::new(sink.clone());
        let sent = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&sent);
        let process = hub.open(
            principal(),
            Arc::new(move |bytes| {
                log.lock().unwrap().push(decode(&bytes).unwrap());
                true
            }),
        );
        (hub, sink, process, sent)
    }

    fn say(process: &ChatProcess, text: &str) {
        process
            .write(&encode(&ChatMessage::Text { text: text.into() }))
            .unwrap();
    }

    #[test]
    fn what_a_client_says_reaches_the_application() {
        let (_hub, sink, process, sent) = rig();
        say(&process, "hello host");
        assert_eq!(*sink.0.lock().unwrap(), ["hello host"]);
        assert!(sent.lock().unwrap().is_empty());
    }

    #[test]
    fn bad_messages_are_rejected_and_not_delivered() {
        let (_hub, sink, process, sent) = rig();
        say(&process, "");
        say(&process, &"x".repeat(5000));
        say(&process, "\u{1b}[2Jclear");
        process.write(&[0xff; 3]).unwrap();
        assert!(sink.0.lock().unwrap().is_empty());
        assert_eq!(sent.lock().unwrap().len(), 4);
        assert!(sent
            .lock()
            .unwrap()
            .iter()
            .all(|m| matches!(m, ChatMessage::Rejected { .. })));
    }

    #[test]
    fn a_flood_is_cut_off_at_the_limit() {
        let (_hub, sink, process, sent) = rig();
        for i in 0..(MAX_PER_SECOND + 5) {
            say(&process, &format!("m{i}"));
        }
        assert_eq!(sink.0.lock().unwrap().len(), MAX_PER_SECOND);
        assert_eq!(sent.lock().unwrap().len(), 5);
    }

    #[test]
    fn the_application_can_speak_to_everyone_or_one_client() {
        let (hub, _sink, process, sent) = rig();
        assert_eq!(hub.open_channels(), 1);
        assert_eq!(hub.notice("download finished"), Ok(1));
        assert_eq!(hub.say_to(principal().peer, "just you"), Ok(1));
        assert_eq!(
            hub.say_to(windowcast_identity::PeerId([8; 32]), "not you"),
            Ok(0)
        );
        assert_eq!(
            *sent.lock().unwrap(),
            [
                ChatMessage::Text {
                    text: "download finished".into()
                },
                ChatMessage::Text {
                    text: "just you".into()
                },
            ]
        );
        assert!(hub.notice("").is_err());
        process.kill();
        assert_eq!(hub.open_channels(), 0);
        assert_eq!(hub.notice("nobody there"), Ok(0));
    }
}
