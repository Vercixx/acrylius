//! `org.acrylius.share/1`: send a file.
//!
//! Only an offer, an endpoint and a result travel on the session; the bytes go
//! over a bulk connection keyed from it (see [`crate::proto::bulk`]). The
//! receiver says where to connect — a phone cannot listen, so it always dials.
//! Nothing is accepted until the host asks a person.

use std::collections::BTreeMap;

use crate::plugin::{Cx, Plugin, PluginError, PluginManifest};
use crate::proto::envelope::{Envelope, ErrorCode};
use crate::proto::ids::DeviceId;
use crate::vocab::{EffectKind, TransferId, UiEvent};

pub const CAP: &str = "org.acrylius.share/1";

/// Refused at the offer rather than discovered gigabytes in.
pub const MAX_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// What a sender is offering.
#[derive(Clone, PartialEq, Eq, Debug, Default, minicbor::Encode, minicbor::Decode)]
pub struct Offer {
    /// Chosen by the sender and unique within the session.
    #[n(0)]
    pub transfer: u64,
    /// A file name, never a path; the receiver must make it safe.
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
    // Files are the host's business: this plugin never opens one.
    requires: &[EffectKind::Share],
};

/// A transfer this device is part of.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Incoming {
    peer: DeviceId,
    offer: Offer,
    /// The envelope id of the offer, so a result can answer it.
    request: u32,
    /// The sender's number for this transfer. Everything back over the wire
    /// must use it; ours would name one of the sender's other transfers.
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

    /// Whether this outgoing transfer is the one we have with `peer`. A bare
    /// id names nothing, since every device numbers its own transfers from
    /// one, so an answer about a transfer must be checked against the peer.
    fn is_sending_to(&self, peer: &DeviceId, transfer: TransferId) -> bool {
        self.sending.get(&transfer).is_some_and(|o| &o.peer == peer)
    }

    /// Our number for a transfer a peer is naming by its own. Checked against
    /// the peer too, for the same reason as [`Self::is_sending_to`].
    fn incoming_from(&self, peer: &DeviceId, offered_as: u64) -> Option<TransferId> {
        self.offered
            .iter()
            .find(|(_, i)| &i.peer == peer && i.offered_as == offered_as)
            .map(|(t, _)| *t)
    }

    /// What to call a transfer when speaking to the peer it is with: ours for
    /// something we offered, theirs for something they did.
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
                // A device with nowhere to put a file refuses now rather than
                // waiting on a person: a phone has no download directory to offer.
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
                // Renumbered on arrival: the sender's id starts from one like
                // everyone else's, so two peers offering at once would collide
                // under it. Kept as `offered_as`, since every reply must use it.
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
                // Announced under our number: nothing above this layer ever
                // sees the sender's.
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
                    // An endpoint for a transfer never offered to this peer:
                    // somewhere to connect chosen by someone else.
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
                // Either direction may be finishing: a transfer we offered
                // comes back under our id, one offered to us under the sender's.
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
                // Refused here, before the offer goes out: a link with no bulk
                // support (Bluetooth) can never finish a transfer, and the far
                // end would only discover that after accepting.
                if !cx.peer_can_carry_bulk() {
                    cx.ui(UiEvent::Error {
                        peer: Some(peer.clone()),
                        code: ErrorCode::NotAllowed,
                        detail: format!(
                            "the link to {peer} cannot carry files. Reach it over the \
                             network for that."
                        ),
                    });
                    // Reported as an ordinary rejection: refusing here makes no
                    // wire traffic, so a host that learns endings from the wire
                    // would otherwise show this as sending forever.
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
                // The endpoint reaches the peer only once the host has one; a
                // peer told to connect to nothing couldn't tell that from a
                // refusal. `offered_as` (the sender's number) drives the bulk key.
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
                // Re-encoded rather than forwarded: the sender wouldn't
                // recognise the id this device uses.
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
            // Both ends say how it went; each knows only its own half (a sender
            // can't know if the receiver kept the file, a receiver can't tell a
            // cancel from a dropped connection).
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

    /// The number this device gave an offer that arrived; never the number in
    /// the offer itself, which is the sender's.
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
        // Every device numbers transfers from one, so two offers landing at
        // once would collide on "transfer 1" without renumbering on arrival.
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
        // Pairing a second phone must not make it able to read what you send
        // to the first, even though both number their transfers from one.
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
        // Both directions of `finished`, and a stranger refused in each: a
        // transfer id alone names nothing without the peer check.
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

        // Incoming. `offer(n)` sets the *size*; the id it carries is always 1,
        // which is why ids collide across devices.
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
        // A bound, not the first value outside it: guards against the check
        // quietly becoming `>=`.
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

        // The same bound on the way out, a separate check in a separate
        // function: refused here, before anything is sent.
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
