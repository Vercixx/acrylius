//! `org.acrylius.share/1`: send a file.
//!
//! The bytes never touch this plugin, and never touch an envelope. All that
//! travels on the session is an offer, an endpoint and a result; the file goes
//! over a connection of its own, encrypted with a key the core derives from the
//! session and hands to the host. See [`crate::proto::bulk`].
//!
//! ## Who listens
//!
//! Not "the sender listens", which is what a design that had only ever run on
//! two computers would settle on. A phone cannot accept connections at all —
//! there is no background push on a free developer account and nothing to keep
//! a listener alive — so a rule that assumed the sender could listen would work
//! in exactly one direction.
//!
//! So the endpoint is negotiated. The receiver is asked whether it can listen;
//! if it can, it says where, and the sender connects. A phone therefore always
//! dials, in both directions, which is the same shape the session itself has
//! and for the same reason. If neither side can listen the transfer is refused
//! rather than left hanging.
//!
//! ## Accepting
//!
//! An offer is not accepted automatically. A device that wrote whatever a peer
//! sent it, wherever it liked, would be a file drop for anything that had ever
//! been paired with it. The host decides, and until it does the sender waits.

use std::collections::BTreeMap;

use crate::plugin::{Cx, Plugin, PluginError, PluginManifest};
use crate::proto::envelope::{Envelope, ErrorCode};
use crate::proto::ids::DeviceId;
use crate::vocab::{EffectKind, TransferId, UiEvent};

pub const CAP: &str = "org.acrylius.share/1";

/// Refused outright rather than attempted. Not a statement about disks: a
/// transfer this big over a link this project targets is a mistake, and finding
/// out four gigabytes in is worse than being told at the start.
pub const MAX_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// What a sender is offering.
#[derive(Clone, PartialEq, Eq, Debug, Default, minicbor::Encode, minicbor::Decode)]
pub struct Offer {
    /// Chosen by the sender and unique within the session.
    #[n(0)]
    pub transfer: u64,
    /// A file name, never a path. A receiver treats it as a suggestion and is
    /// responsible for making it safe: anything else would let a sender choose
    /// where its bytes landed.
    #[n(1)]
    pub name: String,
    #[n(2)]
    pub size: u64,
    #[n(3)]
    pub mime: String,
}

/// The receiver saying where to connect.
#[derive(Clone, PartialEq, Eq, Debug, Default, minicbor::Encode, minicbor::Decode)]
pub struct Accept {
    #[n(0)]
    pub transfer: u64,
    /// Transport-defined and opaque to this plugin.
    #[n(1)]
    pub endpoint: String,
}

/// How a transfer ended, from whichever side noticed.
#[derive(Clone, PartialEq, Eq, Debug, Default, minicbor::Encode, minicbor::Decode)]
pub struct Finished {
    #[n(0)]
    pub transfer: u64,
    #[n(1)]
    pub ok: bool,
    #[n(2)]
    pub detail: String,
}

static MANIFEST: PluginManifest = PluginManifest {
    id: "org.acrylius.share",
    outgoing: &[CAP],
    incoming: &[CAP],
    // Files are the host's business: this plugin never opens one. What it needs
    // is somewhere to put an incoming one and something to read an outgoing one
    // from, which is what a host declaring this effect kind is promising.
    requires: &[EffectKind::Share],
};

/// A transfer this device is part of.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Incoming {
    peer: DeviceId,
    offer: Offer,
    /// The envelope id of the offer, so a result can answer it.
    request: u32,
    /// What the sender calls this transfer.
    ///
    /// Kept because everything that goes back over the wire has to use it: the
    /// accept, the reject, the finished, and the greeting on the bulk socket.
    /// The sender numbers its transfers from one and so do we, so this is
    /// almost never the id we know it by, and using ours would name one of the
    /// sender's other transfers — or nothing at all.
    offered_as: u64,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Outgoing {
    peer: DeviceId,
    offer: Offer,
}

#[derive(Default)]
pub struct SharePlugin {
    /// Offers made to us that a human has not answered yet.
    offered: BTreeMap<TransferId, Incoming>,
    /// Offers we made that have not finished.
    sending: BTreeMap<TransferId, Outgoing>,
}

impl SharePlugin {
    /// Offers waiting on a decision, for a host to show.
    pub fn pending(&self) -> impl Iterator<Item = (TransferId, &DeviceId, &Offer)> {
        self.offered.iter().map(|(id, i)| (*id, &i.peer, &i.offer))
    }

    /// Whether this outgoing transfer is the one we have with `peer`.
    ///
    /// A transfer id is chosen by whoever offered, so two peers hand out the
    /// same small numbers as a matter of course and an id on its own names
    /// nothing. Every answer about a transfer therefore has to come from the
    /// device the transfer is actually with. Without this, a second paired
    /// device could accept a file offered to the first — and an `accept`
    /// carries the address to send it to, so it would be handed the file — or
    /// simply cancel a transfer it had nothing to do with.
    fn is_sending_to(&self, peer: &DeviceId, transfer: TransferId) -> bool {
        self.sending.get(&transfer).is_some_and(|o| &o.peer == peer)
    }

    /// Our number for a transfer a peer is naming by its own.
    ///
    /// Answered against the peer as well as the number, for the reason
    /// [`Self::is_sending_to`] exists: a bare id names nothing, and every device
    /// hands out the same small ones.
    fn incoming_from(&self, peer: &DeviceId, offered_as: u64) -> Option<TransferId> {
        self.offered
            .iter()
            .find(|(_, i)| &i.peer == peer && i.offered_as == offered_as)
            .map(|(t, _)| *t)
    }

    /// What to call a transfer when speaking to the peer it is with.
    ///
    /// Ours for something we offered, and theirs for something they did. Every
    /// message that leaves this plugin goes through here, because a number that
    /// is right locally is wrong on the wire exactly half the time.
    fn as_the_peer_numbers_it(&self, transfer: TransferId) -> u64 {
        self.offered
            .get(&transfer)
            .map_or(transfer.0, |i| i.offered_as)
    }

    fn announce(cx: &mut Cx, peer: &DeviceId, ty: &str, body: &impl minicbor::Encode<()>) {
        if let Ok(encoded) = minicbor::to_vec(body) {
            cx.ui(UiEvent::Plugin {
                peer: peer.clone(),
                cap: CAP.to_string(),
                ty: ty.to_string(),
                body: encoded,
            });
        }
    }
}

impl Plugin for SharePlugin {
    fn manifest(&self) -> &'static PluginManifest {
        &MANIFEST
    }

    fn on_peer_disconnected(&mut self, _cx: &mut Cx, peer: &DeviceId) {
        // A transfer cannot outlive the session it was keyed from, so nothing
        // should still be waiting on one that has gone.
        self.offered.retain(|_, i| &i.peer != peer);
        self.sending.retain(|_, o| &o.peer != peer);
    }

    fn on_message(
        &mut self,
        cx: &mut Cx,
        peer: &DeviceId,
        env: &Envelope<'_>,
    ) -> Result<(), PluginError> {
        match env.ty {
            "offer" => {
                // A device with nowhere to put a file says so now, while the
                // sender is still listening. It cannot wait for a person to
                // decide, because there is no way for one to say yes: a phone
                // has no download directory and the capability is advertised
                // only so this refusal can be sent at all.
                if !cx.serves(EffectKind::Share) {
                    return Err(PluginError::NotAllowed);
                }
                let offer: Offer = minicbor::decode(env.body).map_err(|_| PluginError::BadBody)?;
                if offer.size > MAX_BYTES {
                    return Err(PluginError::TooLarge);
                }
                if offer.name.is_empty() {
                    return Err(PluginError::BadBody);
                }
                // Renumbered on arrival, and this is the only place it happens.
                //
                // The id in an offer was minted from the sender's counter,
                // which starts at one exactly like ours and like every other
                // device's. Keying anything by it meant two peers offering at
                // the same moment both called it transfer 1: this map, the
                // daemon's and the phone's all had one entry where they needed
                // two, the second offer replaced the first, and one device's
                // bytes could be written into the file another device had been
                // promised. Refusing the second was the stopgap; a number that
                // means something here is the fix.
                //
                // The sender's number is kept rather than discarded, because
                // every reply has to use it — see `Incoming::offered_as`.
                let transfer = cx.new_transfer();
                let offered_as = offer.transfer;
                self.offered.insert(
                    transfer,
                    Incoming {
                        peer: peer.clone(),
                        offer: offer.clone(),
                        request: env.id,
                        offered_as,
                    },
                );
                // Announced under our number, so that a host — and the person
                // answering — only ever handles ids that mean something on this
                // device. Nothing above this layer sees the sender's.
                Self::announce(
                    cx,
                    peer,
                    "offer",
                    &Offer {
                        transfer: transfer.0,
                        ..offer
                    },
                );
                // Nothing is accepted here. The host asks a person, and until
                // it answers the sender waits.
                Ok(())
            }

            "accept" => {
                let accept: Accept =
                    minicbor::decode(env.body).map_err(|_| PluginError::BadBody)?;
                let transfer = TransferId(accept.transfer);
                if !self.is_sending_to(peer, transfer) {
                    // An endpoint for a transfer we never offered *to this
                    // peer*. Refused rather than dialled: it is somewhere to
                    // connect chosen by someone else.
                    return Err(PluginError::NotAllowed);
                }
                cx.bulk_send(peer, transfer, &accept.endpoint);
                Ok(())
            }

            "reject" => {
                let f: Finished = minicbor::decode(env.body).map_err(|_| PluginError::BadBody)?;
                let transfer = TransferId(f.transfer);
                if !self.is_sending_to(peer, transfer) {
                    return Err(PluginError::NotAllowed);
                }
                self.sending.remove(&transfer);
                Self::announce(cx, peer, "reject", &f);
                Ok(())
            }

            "finished" => {
                let f: Finished = minicbor::decode(env.body).map_err(|_| PluginError::BadBody)?;
                // Either direction may be finishing, and the number means
                // different things in the two: a transfer we offered comes back
                // under our id, and one offered to us under the sender's.
                let transfer = if self.is_sending_to(peer, TransferId(f.transfer)) {
                    TransferId(f.transfer)
                } else {
                    // Only the device the transfer is actually with may end it.
                    self.incoming_from(peer, f.transfer)
                        .ok_or(PluginError::NotAllowed)?
                };
                self.sending.remove(&transfer);
                self.offered.remove(&transfer);
                Self::announce(
                    cx,
                    peer,
                    "finished",
                    &Finished {
                        transfer: transfer.0,
                        ..f
                    },
                );
                Ok(())
            }

            other => Err(PluginError::UnknownType(other.to_string())),
        }
    }

    fn on_local(
        &mut self,
        cx: &mut Cx,
        peer: &DeviceId,
        ty: &str,
        body: &[u8],
    ) -> Result<(), PluginError> {
        match ty {
            // The host has a file and an id for it. It keeps the path; this
            // plugin never learns one.
            "offer" => {
                let offer: Offer = minicbor::decode(body).map_err(|_| PluginError::BadBody)?;
                if offer.size > MAX_BYTES {
                    return Err(PluginError::TooLarge);
                }
                // Refused here, before the offer goes out, because here is the
                // only place a person will ever see it. A file moves over a
                // side channel, not over the session, so a link that carries
                // no bulk — Bluetooth — cannot finish this however willing
                // both ends are. The far end discovers that only when someone
                // there accepts, and its refusal is local to it: the sender
                // would sit on "offered" until the session ended.
                if !cx.peer_can_carry_bulk() {
                    cx.ui(UiEvent::Error {
                        code: ErrorCode::NotAllowed,
                        detail: format!(
                            "the link to {peer} cannot carry files. Reach it over the \
                             network for that."
                        ),
                    });
                    // And say the transfer is over, in the words a host already
                    // understands. A refusal here produces no traffic, so the
                    // "reject" that normally comes back from the far end never
                    // will — and a host that only learns of an ending from the
                    // wire would leave this file listed as sending for as long
                    // as it ran. It is a rejection; it just happens to be ours.
                    Self::announce(
                        cx,
                        peer,
                        "reject",
                        &Finished {
                            transfer: offer.transfer,
                            ok: false,
                            detail: "this link cannot carry files".to_string(),
                        },
                    );
                    return Ok(());
                }
                self.sending.insert(
                    TransferId(offer.transfer),
                    Outgoing {
                        peer: peer.clone(),
                        offer: offer.clone(),
                    },
                );
                cx.send(peer, CAP, "offer", body.to_vec());
                Ok(())
            }

            // A person said yes to something offered to us.
            "accept" => {
                let f: Finished = minicbor::decode(body).map_err(|_| PluginError::BadBody)?;
                let transfer = TransferId(f.transfer);
                let Some(incoming) = self.offered.get(&transfer) else {
                    return Err(PluginError::NotAllowed);
                };
                // Ask the host for somewhere to listen. The endpoint goes to
                // the peer only once the host has one, because a peer told to
                // connect to nothing has no way to tell that from a refusal.
                //
                // Both numbers: ours is what the host and everything above it
                // works in, and the sender's is what the bulk key is derived
                // from, which neither end may get wrong and neither end sends.
                cx.bulk_listen(
                    &incoming.peer.clone(),
                    transfer,
                    incoming.offered_as,
                    incoming.offer.size,
                );
                Ok(())
            }

            "reject" => {
                let f: Finished = minicbor::decode(body).map_err(|_| PluginError::BadBody)?;
                let transfer = TransferId(f.transfer);
                let Some(incoming) = self.offered.remove(&transfer) else {
                    return Err(PluginError::NotAllowed);
                };
                // Re-encoded rather than forwarded: the body a host hands down
                // names the transfer the way this device does, and the sender
                // would not recognise it.
                let body = minicbor::to_vec(Finished {
                    transfer: incoming.offered_as,
                    ..f
                })
                .map_err(|_| PluginError::BadBody)?;
                cx.send_reply(&incoming.peer, CAP, "reject", body, incoming.request);
                Ok(())
            }

            "cancel" => {
                let f: Finished = minicbor::decode(body).map_err(|_| PluginError::BadBody)?;
                let transfer = TransferId(f.transfer);
                let theirs = self.as_the_peer_numbers_it(transfer);
                self.offered.remove(&transfer);
                self.sending.remove(&transfer);
                cx.bulk_cancel(transfer);
                let body = minicbor::to_vec(Finished {
                    transfer: theirs,
                    ..f
                })
                .map_err(|_| PluginError::BadBody)?;
                cx.send(peer, CAP, "finished", body);
                Ok(())
            }

            other => Err(PluginError::UnknownType(other.to_string())),
        }
    }

    fn on_bulk_listening(&mut self, cx: &mut Cx, transfer: TransferId, endpoint: &str) {
        let Some(incoming) = self.offered.get(&transfer) else {
            return;
        };
        // Under the sender's number. It is the sender that has to match this
        // against something, and it has never heard of ours.
        let body = minicbor::to_vec(Accept {
            transfer: incoming.offered_as,
            endpoint: endpoint.to_string(),
        })
        .unwrap_or_default();
        cx.send_reply(
            &incoming.peer.clone(),
            CAP,
            "accept",
            body,
            incoming.request,
        );
    }

    fn on_bulk_finished(&mut self, cx: &mut Cx, transfer: TransferId, ok: bool, detail: &str) {
        let peer = self
            .offered
            .get(&transfer)
            .map(|i| i.peer.clone())
            .or_else(|| self.sending.get(&transfer).map(|o| o.peer.clone()));
        let theirs = self.as_the_peer_numbers_it(transfer);
        self.offered.remove(&transfer);
        self.sending.remove(&transfer);

        // The same ending, said twice in two numbering schemes: this device's
        // upwards, and the peer's outwards.
        let f = Finished {
            transfer: transfer.0,
            ok,
            detail: detail.to_string(),
        };
        if let Some(peer) = peer {
            // Both ends say how it went. Each knows only its own half: a sender
            // that finished writing does not know whether the receiver kept the
            // file, and a receiver cannot tell a cancelled send from a dropped
            // connection.
            if let Ok(body) = minicbor::to_vec(Finished {
                transfer: theirs,
                ..f.clone()
            }) {
                cx.send(&peer, CAP, "finished", body);
            }
            Self::announce(cx, &peer, "finished", &f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{
        BulkRequest,
        harness::{envelope, run},
    };

    fn peer() -> DeviceId {
        DeviceId::of(&[4u8; 32])
    }

    fn offer(size: u64) -> Vec<u8> {
        minicbor::to_vec(Offer {
            transfer: 1,
            name: "notes.txt".to_string(),
            size,
            mime: "text/plain".to_string(),
        })
        .unwrap()
    }

    /// The number this device gave an offer that arrived.
    ///
    /// Never the number in the offer: that one was the sender's. A host learns
    /// ours from the announcement and answers with it, and so does a test.
    fn ours(r: &crate::plugin::harness::Ran) -> u64 {
        r.ui.iter()
            .find_map(|e| match e {
                UiEvent::Plugin { ty, body, .. } if ty == "offer" => {
                    minicbor::decode::<Offer>(body).ok().map(|o| o.transfer)
                }
                _ => None,
            })
            .expect("an arriving offer is announced")
    }

    fn finished(transfer: u64) -> Vec<u8> {
        minicbor::to_vec(Finished {
            transfer,
            ok: true,
            detail: String::new(),
        })
        .unwrap()
    }

    #[test]
    fn an_offer_is_not_accepted_by_itself() {
        // A device that wrote whatever a peer sent it would be a file drop for
        // anything ever paired with it.
        let mut p = SharePlugin::default();
        let body = offer(1024);
        let r = run(0, |cx| {
            p.on_message(cx, &peer(), &envelope(9, CAP, "offer", &body))
                .unwrap();
        });
        assert!(
            r.bulk.is_empty(),
            "nothing is listened for until a person says so"
        );
        assert!(r.sent("accept").is_none(), "and nothing is accepted");
        assert!(
            r.ui.iter()
                .any(|e| matches!(e, UiEvent::Plugin { ty, .. } if ty == "offer")),
            "the host is asked"
        );
    }

    #[test]
    fn accepting_asks_for_somewhere_to_listen_before_answering() {
        // The endpoint reaches the peer only once the host has one. A peer told
        // to connect to nothing cannot tell that from a refusal.
        let mut p = SharePlugin::default();
        let body = offer(1024);
        let arrived = run(0, |cx| {
            p.on_message(cx, &peer(), &envelope(9, CAP, "offer", &body))
                .unwrap();
        });
        let yes = finished(ours(&arrived));
        let r = run(0, |cx| p.on_local(cx, &peer(), "accept", &yes).unwrap());
        assert!(r.sent("accept").is_none(), "not yet");
        assert!(matches!(
            r.bulk.first(),
            Some(BulkRequest::Listen {
                expect_bytes: 1024,
                ..
            })
        ));

        let r = run(0, |cx| {
            p.on_bulk_listening(cx, TransferId(ours(&arrived)), "127.0.0.1:5000")
        });
        let sent = r.sent("accept").expect("now the peer is told where");
        let a: Accept = minicbor::decode(&sent.body).unwrap();
        assert_eq!(a.endpoint, "127.0.0.1:5000");
        assert_eq!(sent.re, Some(9), "answering the offer");
        // Under the sender's number, not the one this device answered with.
        assert_eq!(
            a.transfer, 1,
            "the endpoint named a transfer the sender never offered"
        );
    }

    #[test]
    fn an_endpoint_for_a_transfer_we_never_offered_is_refused() {
        // Otherwise a peer names somewhere and this device connects to it.
        let mut p = SharePlugin::default();
        let body = minicbor::to_vec(Accept {
            transfer: 77,
            endpoint: "10.0.0.1:1234".to_string(),
        })
        .unwrap();
        run(0, |cx| {
            assert_eq!(
                p.on_message(cx, &peer(), &envelope(1, CAP, "accept", &body))
                    .unwrap_err(),
                PluginError::NotAllowed
            );
        });
    }

    #[test]
    fn two_devices_offering_the_same_id_each_get_one_of_their_own() {
        // Every device numbers its transfers from one, so "transfer 1" exists
        // for all of them at once. Keyed by that number alone, the second offer
        // replaced the first: the wrong offer was answered, and the accept —
        // which carries somewhere to send the file — went to the wrong device.
        //
        // Refusing the second was the stopgap. Numbering them here is the fix,
        // and it is the difference between two people being able to send you a
        // photo at the same time and not.
        let mut p = SharePlugin::default();
        let first = peer();
        let second = DeviceId::of(&[6u8; 32]);

        let body = offer(10);
        let r = run(0, |cx| {
            p.on_message(cx, &first, &envelope(1, CAP, "offer", &body))
                .unwrap();
        });
        // Carried on, the way the core carries it between dispatches.
        run(r.next_transfer, |cx| {
            p.on_message(cx, &second, &envelope(2, CAP, "offer", &body))
                .unwrap();
        });

        let pending: Vec<(TransferId, DeviceId)> =
            p.pending().map(|(t, who, _)| (t, who.clone())).collect();
        assert_eq!(pending.len(), 2, "both offers stand");
        assert_ne!(
            pending[0].0, pending[1].0,
            "under numbers that are not each other's"
        );
        assert!(
            pending.iter().any(|(_, who)| who == &first)
                && pending.iter().any(|(_, who)| who == &second),
            "and each is still attributed to the device that made it"
        );
        assert!(
            pending.iter().all(|(t, _)| t.0 != 10),
            "and neither is filed under the number the senders chose, \
             which is the number they collided on"
        );
    }

    #[test]
    fn another_paired_device_cannot_accept_a_file_offered_to_someone_else() {
        // Pairing a second phone must not make it able to read what you send to
        // the first. The id check alone was not enough: every device numbers its
        // own transfers from one, so "transfer 1" exists for all of them, and an
        // `accept` carries the address to deliver to.
        let mut p = SharePlugin::default();
        let intended = peer();
        let eavesdropper = DeviceId::of(&[8u8; 32]);
        let body = offer(10);
        run(0, |cx| p.on_local(cx, &intended, "offer", &body).unwrap());

        let accept = minicbor::to_vec(Accept {
            transfer: 1,
            endpoint: "10.6.6.6:4444".to_string(),
        })
        .unwrap();
        let r = run(0, |cx| {
            assert_eq!(
                p.on_message(cx, &eavesdropper, &envelope(2, CAP, "accept", &accept))
                    .unwrap_err(),
                PluginError::NotAllowed,
                "an accept from a device the offer was not made to"
            );
        });
        assert!(
            r.bulk.is_empty(),
            "and above all, nothing is dialled: the address came from the wrong device"
        );

        // The offer is untouched, so the device it was actually made to can
        // still accept it.
        let r2 = run(0, |cx| {
            p.on_message(cx, &intended, &envelope(3, CAP, "accept", &accept))
                .unwrap();
        });
        assert!(matches!(r2.bulk.first(), Some(BulkRequest::Send { .. })));
    }

    #[test]
    fn another_paired_device_cannot_cancel_a_transfer_it_has_nothing_to_do_with() {
        let mut p = SharePlugin::default();
        let intended = peer();
        let meddler = DeviceId::of(&[8u8; 32]);
        run(0, |cx| {
            p.on_local(cx, &intended, "offer", &offer(10)).unwrap()
        });

        let f = minicbor::to_vec(Finished {
            transfer: 1,
            ok: false,
            detail: "no thanks".to_string(),
        })
        .unwrap();
        run(0, |cx| {
            assert_eq!(
                p.on_message(cx, &meddler, &envelope(4, CAP, "reject", &f))
                    .unwrap_err(),
                PluginError::NotAllowed
            );
        });
        assert_eq!(p.sending.len(), 1, "the transfer survives a stranger's no");
    }

    #[test]
    fn only_the_device_a_transfer_is_with_may_finish_it() {
        // Both directions of `finished`, and a stranger refused in each. A
        // transfer id alone names nothing, so without the peer check any paired
        // device could close out somebody else's transfer.
        let mut p = SharePlugin::default();
        let mine = peer();
        let stranger = DeviceId::of(&[7u8; 32]);
        let f = minicbor::to_vec(Finished {
            transfer: 1,
            ok: true,
            detail: String::new(),
        })
        .unwrap();

        // Outgoing.
        run(0, |cx| p.on_local(cx, &mine, "offer", &offer(10)).unwrap());
        run(0, |cx| {
            assert_eq!(
                p.on_message(cx, &stranger, &envelope(9, CAP, "finished", &f))
                    .unwrap_err(),
                PluginError::NotAllowed
            );
        });
        run(0, |cx| {
            p.on_message(cx, &mine, &envelope(10, CAP, "finished", &f))
                .unwrap();
        });
        assert!(p.sending.is_empty(), "the right device closed it out");

        // Incoming. `offer(n)` sets the *size*; the id it carries is 1, which is
        // exactly why ids collide across devices. Naming any other id here would
        // be refused for the wrong reason and prove nothing.
        let body = offer(11);
        run(0, |cx| {
            p.on_message(cx, &mine, &envelope(11, CAP, "offer", &body))
                .unwrap();
        });
        assert_eq!(p.pending().count(), 1);
        run(0, |cx| {
            assert_eq!(
                p.on_message(cx, &stranger, &envelope(12, CAP, "finished", &f))
                    .unwrap_err(),
                PluginError::NotAllowed,
                "a stranger naming the id of an offer made to us"
            );
        });
        assert_eq!(p.pending().count(), 1, "the offer survives a stranger");
        run(0, |cx| {
            p.on_message(cx, &mine, &envelope(13, CAP, "finished", &f))
                .unwrap();
        });
        assert_eq!(p.pending().count(), 0, "and the right device closes it");
    }

    #[test]
    fn an_offer_of_exactly_the_largest_size_is_taken() {
        // A bound, not the first value outside it. Nothing pinned that, so the
        // check could quietly become `>=` and refuse a transfer that is exactly
        // allowed — with an error naming a size it does not exceed.
        let mut p = SharePlugin::default();
        let body = minicbor::to_vec(Offer {
            transfer: 5,
            name: "big.bin".to_string(),
            size: MAX_BYTES,
            mime: String::new(),
        })
        .unwrap();
        run(0, |cx| {
            p.on_message(cx, &peer(), &envelope(1, CAP, "offer", &body))
                .unwrap();
        });
        assert_eq!(p.pending().count(), 1);

        let too_big = minicbor::to_vec(Offer {
            transfer: 6,
            name: "bigger.bin".to_string(),
            size: MAX_BYTES + 1,
            mime: String::new(),
        })
        .unwrap();
        run(0, |cx| {
            assert_eq!(
                p.on_message(cx, &peer(), &envelope(2, CAP, "offer", &too_big))
                    .unwrap_err(),
                PluginError::TooLarge
            );
        });

        // The same bound on the way out, which is a separate check in a separate
        // function and needs saying separately. This is the one a person meets:
        // it is refused here, before anything is sent.
        let mut q = SharePlugin::default();
        run(0, |cx| {
            q.on_local(cx, &peer(), "offer", &body).unwrap();
        });
        run(0, |cx| {
            assert_eq!(
                q.on_local(cx, &peer(), "offer", &too_big).unwrap_err(),
                PluginError::TooLarge
            );
        });
    }

    #[test]
    fn a_sender_dials_the_endpoint_it_was_given() {
        let mut p = SharePlugin::default();
        let body = offer(10);
        run(0, |cx| p.on_local(cx, &peer(), "offer", &body).unwrap());

        let accept = minicbor::to_vec(Accept {
            transfer: 1,
            endpoint: "192.168.1.5:4444".to_string(),
        })
        .unwrap();
        let r = run(0, |cx| {
            p.on_message(cx, &peer(), &envelope(2, CAP, "accept", &accept))
                .unwrap();
        });
        assert!(matches!(
            r.bulk.first(),
            Some(BulkRequest::Send { endpoint, .. }) if endpoint == "192.168.1.5:4444"
        ));
    }

    #[test]
    fn something_absurdly_large_is_refused_at_the_offer() {
        // Finding out four gigabytes in is worse than being told at the start.
        let mut p = SharePlugin::default();
        let body = offer(MAX_BYTES + 1);
        run(0, |cx| {
            assert_eq!(
                p.on_message(cx, &peer(), &envelope(1, CAP, "offer", &body))
                    .unwrap_err(),
                PluginError::TooLarge
            );
        });
    }

    #[test]
    fn an_offer_with_no_name_is_refused() {
        let mut p = SharePlugin::default();
        let body = minicbor::to_vec(Offer {
            transfer: 1,
            name: String::new(),
            size: 1,
            mime: String::new(),
        })
        .unwrap();
        run(0, |cx| {
            assert_eq!(
                p.on_message(cx, &peer(), &envelope(1, CAP, "offer", &body))
                    .unwrap_err(),
                PluginError::BadBody
            );
        });
    }

    #[test]
    fn a_finished_transfer_is_forgotten_on_both_sides() {
        let mut p = SharePlugin::default();
        let body = offer(10);
        run(0, |cx| p.on_local(cx, &peer(), "offer", &body).unwrap());
        assert_eq!(p.sending.len(), 1);

        let r = run(0, |cx| p.on_bulk_finished(cx, TransferId(1), true, ""));
        assert!(
            p.sending.is_empty(),
            "nothing should still hold a key for it"
        );
        assert!(r.sent("finished").is_some(), "and the peer is told");
    }

    #[test]
    fn a_peer_going_away_takes_its_transfers_with_it() {
        // A transfer cannot outlive the session its key came from.
        let mut p = SharePlugin::default();
        let body = offer(10);
        run(0, |cx| p.on_local(cx, &peer(), "offer", &body).unwrap());
        run(0, |cx| {
            p.on_message(cx, &peer(), &envelope(9, CAP, "offer", &body))
                .unwrap();
        });
        run(0, |cx| p.on_peer_disconnected(cx, &peer()));
        assert!(p.sending.is_empty() && p.offered.is_empty());
    }

    #[test]
    fn rejecting_answers_the_offer_and_forgets_it() {
        let mut p = SharePlugin::default();
        let body = offer(10);
        let arrived = run(0, |cx| {
            p.on_message(cx, &peer(), &envelope(9, CAP, "offer", &body))
                .unwrap();
        });
        let no = finished(ours(&arrived));
        let r = run(0, |cx| p.on_local(cx, &peer(), "reject", &no).unwrap());
        assert_eq!(r.sent("reject").and_then(|s| s.re), Some(9));
        assert!(p.offered.is_empty());
    }
}
