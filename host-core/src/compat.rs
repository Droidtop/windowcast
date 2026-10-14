//! Compatibility mode (docs/BACKENDS.md, "Third-party clients and
//! RemoteApp"). The owner: "We can have a compatibility mode for each
//! stream, so we can still cast to other clients". A window in
//! compatibility mode is also offered to third-party clients, through each
//! endpoint the host has: an RDP desktop of the window (mstsc, FreeRDP), a
//! GameStream app (Moonlight) and a VNC server. It is off by default and
//! turned on per window, by a client streaming that window (for as long as
//! it streams it) or by the host's owner (until the owner turns it off).
//! Each endpoint authenticates its clients its own way (a login made for
//! the window, Moonlight's PIN pairing typed on the host, a password made
//! for the window), and their input goes through the host's input rules.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use windowcast_protocol::{CompatEndpoint, WindowId};

/// One kind of endpoint for third-party clients.
pub trait CompatProvider: Send + Sync + 'static {
    /// Opens it for `window` (titled `title`): how a third-party client
    /// reaches it, and what keeps it open until dropped.
    fn open(
        &self,
        window: WindowId,
        title: &str,
    ) -> Result<(CompatEndpoint, Box<dyn Send + Sync>), String>;
}

/// Who turned a window's compatibility mode on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Owner {
    /// The host's owner, in the host application.
    Host,
    /// The windowcast session with this serial, while it streams the window.
    Session(u64),
}

struct Open {
    endpoints: Vec<CompatEndpoint>,
    /// Dropped (closing the endpoints) when the last owner lets go.
    _guards: Vec<Box<dyn Send + Sync>>,
    owners: HashSet<Owner>,
}

/// The host's windows in compatibility mode, and the endpoints it has.
#[derive(Default)]
pub struct Compatibility {
    providers: RwLock<Vec<Arc<dyn CompatProvider>>>,
    open: Mutex<HashMap<WindowId, Open>>,
}

impl Compatibility {
    /// The endpoints windows get from now on.
    pub fn set_providers(&self, providers: Vec<Arc<dyn CompatProvider>>) {
        *self.providers.write().expect("providers") = providers;
    }

    /// Whether this host has any endpoint to offer.
    pub fn offered(&self) -> bool {
        !self.providers.read().expect("providers").is_empty()
    }

    /// Turns compatibility mode on for `window` on behalf of `owner`:
    /// opens every endpoint the first time, and gives back what each says.
    /// An endpoint that cannot open is left out; with none, the reasons.
    pub fn turn_on(
        &self,
        window: WindowId,
        title: &str,
        owner: Owner,
    ) -> Result<Vec<CompatEndpoint>, String> {
        let mut open = self.open.lock().expect("compatibility");
        if let Some(entry) = open.get_mut(&window) {
            entry.owners.insert(owner);
            return Ok(entry.endpoints.clone());
        }
        let providers = self.providers.read().expect("providers").clone();
        if providers.is_empty() {
            return Err("this host has no compatibility mode".into());
        }
        let mut endpoints = Vec::new();
        let mut guards = Vec::new();
        let mut failures = Vec::new();
        for provider in providers {
            match provider.open(window, title) {
                Ok((endpoint, guard)) => {
                    endpoints.push(endpoint);
                    guards.push(guard);
                }
                Err(e) => failures.push(e),
            }
        }
        for failure in &failures {
            eprintln!("compatibility mode for window {}: {failure}", window.0);
        }
        if endpoints.is_empty() {
            return Err(failures.join("; "));
        }
        open.insert(
            window,
            Open {
                endpoints: endpoints.clone(),
                _guards: guards,
                owners: HashSet::from([owner]),
            },
        );
        Ok(endpoints)
    }

    /// `owner` lets go of `window`'s compatibility mode; the endpoints close
    /// when nobody holds it any more. True when they closed.
    pub fn turn_off(&self, window: WindowId, owner: Owner) -> bool {
        let mut open = self.open.lock().expect("compatibility");
        let Some(entry) = open.get_mut(&window) else {
            return false;
        };
        entry.owners.remove(&owner);
        if entry.owners.is_empty() {
            open.remove(&window);
            return true;
        }
        false
    }

    /// A session ended: it lets go of every window.
    pub fn session_ended(&self, serial: u64) {
        let mut open = self.open.lock().expect("compatibility");
        open.retain(|_, entry| {
            entry.owners.remove(&Owner::Session(serial));
            !entry.owners.is_empty()
        });
    }

    /// The windows in compatibility mode, their endpoints, and whether the
    /// host's owner holds each.
    pub fn list(&self) -> Vec<(WindowId, Vec<CompatEndpoint>, bool)> {
        let open = self.open.lock().expect("compatibility");
        let mut list: Vec<_> = open
            .iter()
            .map(|(window, entry)| {
                (
                    *window,
                    entry.endpoints.clone(),
                    entry.owners.contains(&Owner::Host),
                )
            })
            .collect();
        list.sort_by_key(|(window, ..)| window.0);
        list
    }

    /// `window`'s endpoints, when it is in compatibility mode.
    pub fn endpoints(&self, window: WindowId) -> Option<Vec<CompatEndpoint>> {
        let open = self.open.lock().expect("compatibility");
        open.get(&window).map(|entry| entry.endpoints.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts its open endpoints.
    struct Counting(Arc<AtomicUsize>);

    struct Closes(Arc<AtomicUsize>);

    impl Drop for Closes {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl CompatProvider for Counting {
        fn open(
            &self,
            _window: WindowId,
            _title: &str,
        ) -> Result<(CompatEndpoint, Box<dyn Send + Sync>), String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok((
                CompatEndpoint::Vnc {
                    port: 5900,
                    password: "pw".into(),
                },
                Box::new(Closes(Arc::clone(&self.0))),
            ))
        }
    }

    struct Failing;

    impl CompatProvider for Failing {
        fn open(
            &self,
            _window: WindowId,
            _title: &str,
        ) -> Result<(CompatEndpoint, Box<dyn Send + Sync>), String> {
            Err("no ports".into())
        }
    }

    #[test]
    fn endpoints_stay_open_while_anyone_holds_the_window() {
        let count = Arc::new(AtomicUsize::new(0));
        let compat = Compatibility::default();
        let window = WindowId(7);
        assert!(
            compat.turn_on(window, "w", Owner::Host).is_err(),
            "no providers"
        );
        compat.set_providers(vec![
            Arc::new(Counting(Arc::clone(&count))),
            Arc::new(Failing),
        ]);
        let endpoints = compat.turn_on(window, "w", Owner::Session(1)).unwrap();
        assert_eq!(endpoints.len(), 1, "the failing endpoint is left out");
        compat.turn_on(window, "w", Owner::Host).unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1, "opened once");
        compat.session_ended(1);
        assert_eq!(count.load(Ordering::SeqCst), 1, "the host still holds it");
        assert_eq!(compat.list().len(), 1);
        assert!(compat.turn_off(window, Owner::Host));
        assert_eq!(count.load(Ordering::SeqCst), 0, "closed");
        assert!(compat.endpoints(window).is_none());
    }

    #[test]
    fn with_no_endpoint_open_it_says_why() {
        let compat = Compatibility::default();
        compat.set_providers(vec![Arc::new(Failing)]);
        assert_eq!(
            compat.turn_on(WindowId(1), "w", Owner::Host),
            Err("no ports".into())
        );
    }
}
