// Copyright 2020 Nym Technologies SA
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::client::mix_traffic::BatchMixMessageSender;
use crate::client::real_messages_control::acknowledgement_control::SentPacketNotificationSender;
use crate::client::topology_control::TopologyAccessor;
use futures::channel::mpsc;
use futures::task::{Context, Poll};
use futures::{Future, Stream, StreamExt};
use log::*;
use nymsphinx::acknowledgements::AckKey;
use nymsphinx::addressing::clients::Recipient;
use nymsphinx::chunking::fragment::FragmentIdentifier;
use nymsphinx::cover::generate_loop_cover_packet;
use nymsphinx::forwarding::packet::MixPacket;
use nymsphinx::utils::sample_poisson_duration;
use rand::{CryptoRng, Rng};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::time;

/// Configurable parameters of the `OutQueueControl`
pub(crate) struct Config {
    /// Average delay an acknowledgement packet is going to get delay at a single mixnode.
    average_ack_delay: Duration,

    /// Average delay a data packet is going to get delay at a single mixnode.
    average_packet_delay: Duration,

    /// Average delay between sending subsequent packets.
    average_message_sending_delay: Duration,
}

impl Config {
    pub(crate) fn new(
        average_ack_delay: Duration,
        average_packet_delay: Duration,
        average_message_sending_delay: Duration,
    ) -> Self {
        Config {
            average_ack_delay,
            average_packet_delay,
            average_message_sending_delay,
        }
    }
}

pub(crate) struct OutQueueControl<R>
where
    R: CryptoRng + Rng,
{
    /// Configurable parameters of the `ActionController`
    config: Config,

    /// Key used to encrypt and decrypt content of an ACK packet.
    ack_key: Arc<AckKey>,

    /// Channel used for notifying of a real packet being sent out. Used to start up retransmission timer.
    sent_notifier: SentPacketNotificationSender,

    /// Internal state, determined by `average_message_sending_delay`,
    /// used to keep track of when a next packet should be sent out.
    next_delay: time::Delay,

    /// Channel used for sending prepared sphinx packets to `MixTrafficController` that sends them
    /// out to the network without any further delays.
    mix_tx: BatchMixMessageSender,

    /// Channel used for receiving real, prepared, messages that must be first sufficiently delayed
    /// before being sent out into the network.
    real_receiver: BatchRealMessageReceiver,

    /// Represents full address of this client.
    our_full_destination: Recipient,

    /// Instance of a cryptographically secure random number generator.
    rng: R,

    /// Accessor to the common instance of network topology.
    topology_access: TopologyAccessor,

    /// Buffer containing all real messages received. It is first exhausted before more are pulled.
    received_buffer: VecDeque<RealMessage>,
}

pub(crate) struct RealMessage {
    mix_packet: MixPacket,
    fragment_id: FragmentIdentifier,
}

impl RealMessage {
    pub(crate) fn new(mix_packet: MixPacket, fragment_id: FragmentIdentifier) -> Self {
        RealMessage {
            mix_packet,
            fragment_id,
        }
    }
}

// messages are already prepared, etc. the real point of it is to forward it to mix_traffic
// after sufficient delay
pub(crate) type BatchRealMessageSender = mpsc::UnboundedSender<Vec<RealMessage>>;
type BatchRealMessageReceiver = mpsc::UnboundedReceiver<Vec<RealMessage>>;

pub(crate) enum StreamMessage {
    Cover,
    Real(RealMessage),
}

impl<R> Stream for OutQueueControl<R>
where
    R: CryptoRng + Rng + Unpin,
{
    type Item = StreamMessage;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // it is not yet time to return a message
        if Pin::new(&mut self.next_delay).poll(cx).is_pending() {
            return Poll::Pending;
        };

        // we know it's time to send a message, so let's prepare delay for the next one
        // Get the `now` by looking at the current `delay` deadline
        let avg_delay = self.config.average_message_sending_delay;
        let now = self.next_delay.deadline();
        let next_poisson_delay = sample_poisson_duration(&mut self.rng, avg_delay);

        // The next interval value is `next_poisson_delay` after the one that just
        // yielded.
        let next = now + next_poisson_delay;
        self.next_delay.reset(next);

        // check if we have anything immediately available
        if let Some(real_available) = self.received_buffer.pop_front() {
            return Poll::Ready(Some(StreamMessage::Real(real_available)));
        }

        // decide what kind of message to send
        match Pin::new(&mut self.real_receiver).poll_next(cx) {
            // in the case our real message channel stream was closed, we should also indicate we are closed
            // (and whoever is using the stream should panic)
            Poll::Ready(None) => Poll::Ready(None),

            // if there are more messages available, return first one and store the rest
            Poll::Ready(Some(real_messages)) => {
                self.received_buffer = real_messages.into();
                // we MUST HAVE received at least ONE message
                Poll::Ready(Some(StreamMessage::Real(
                    self.received_buffer.pop_front().unwrap(),
                )))
            }

            // otherwise construct a dummy one
            Poll::Pending => Poll::Ready(Some(StreamMessage::Cover)),
        }
    }
}

impl<R> OutQueueControl<R>
where
    R: CryptoRng + Rng + Unpin,
{
    pub(crate) fn new(
        config: Config,
        ack_key: Arc<AckKey>,
        sent_notifier: SentPacketNotificationSender,
        mix_tx: BatchMixMessageSender,
        real_receiver: BatchRealMessageReceiver,
        rng: R,
        our_full_destination: Recipient,
        topology_access: TopologyAccessor,
    ) -> Self {
        OutQueueControl {
            config,
            ack_key,
            sent_notifier,
            next_delay: time::delay_for(Default::default()),
            mix_tx,
            real_receiver,
            our_full_destination,
            rng,
            topology_access,
            received_buffer: VecDeque::with_capacity(0), // we won't be putting any data into this guy directly
        }
    }

    fn sent_notify(&self, frag_id: FragmentIdentifier) {
        // well technically the message was not sent just yet, but now it's up to internal
        // queues and client load rather than the required delay. So realistically we can treat
        // whatever is about to happen as negligible additional delay.
        trace!("{} is about to get sent to the mixnet", frag_id);
        self.sent_notifier.unbounded_send(frag_id).unwrap();
    }

    async fn on_message(&mut self, next_message: StreamMessage) {
        trace!("created new message");

        let next_message = match next_message {
            StreamMessage::Cover => {
                // TODO for way down the line: in very rare cases (during topology update) we might have
                // to wait a really tiny bit before actually obtaining the permit hence messing with our
                // poisson delay, but is it really a problem?
                let topology_permit = self.topology_access.get_read_permit().await;
                // the ack is sent back to ourselves (and then ignored)
                let topology_ref_option = topology_permit.try_get_valid_topology_ref(
                    &self.our_full_destination,
                    Some(&self.our_full_destination),
                );
                if topology_ref_option.is_none() {
                    warn!(
                        "No valid topology detected - won't send any loop cover message this time"
                    );
                    return;
                }
                let topology_ref = topology_ref_option.unwrap();

                generate_loop_cover_packet(
                    &mut self.rng,
                    topology_ref,
                    &*self.ack_key,
                    &self.our_full_destination,
                    self.config.average_ack_delay,
                    self.config.average_packet_delay,
                )
                .expect("Somehow failed to generate a loop cover message with a valid topology")
            }
            StreamMessage::Real(real_message) => {
                self.sent_notify(real_message.fragment_id);
                real_message.mix_packet
            }
        };

        // if this one fails, there's no retrying because it means that either:
        // - we run out of memory
        // - the receiver channel is closed
        // in either case there's no recovery and we can only panic
        self.mix_tx.unbounded_send(vec![next_message]).unwrap();

        // JS: Not entirely sure why or how it fixes stuff, but without the yield call,
        // the UnboundedReceiver [of mix_rx] will not get a chance to read anything
        // JS2: Basically it was the case that with high enough rate, the stream had already a next value
        // ready and hence was immediately re-scheduled causing other tasks to be starved;
        // yield makes it go back the scheduling queue regardless of its value availability
        tokio::task::yield_now().await;
    }

    async fn on_batch_received(&mut self, real_messages: Vec<RealMessage>) {
        let mut mix_packets = Vec::with_capacity(real_messages.len());
        for real_message in real_messages.into_iter() {
            self.sent_notify(real_message.fragment_id);
            mix_packets.push(real_message.mix_packet);
        }
        self.mix_tx.unbounded_send(mix_packets).unwrap();
    }

    // Send messages at certain rate and if no real traffic is available, send cover message.
    async fn run_normal_out_queue(&mut self) {
        // we should set initial delay only when we actually start the stream
        self.next_delay = time::delay_for(sample_poisson_duration(
            &mut self.rng,
            self.config.average_message_sending_delay,
        ));

        while let Some(next_message) = self.next().await {
            self.on_message(next_message).await;
        }
    }

    // Send real message as soon as it's available and don't inject ANY cover traffic.
    async fn run_vpn_out_queue(&mut self) {
        while let Some(next_messages) = self.real_receiver.next().await {
            self.on_batch_received(next_messages).await
        }
    }

    pub(crate) async fn run_out_queue_control(&mut self, vpn_mode: bool) {
        if vpn_mode {
            debug!("Starting out queue controller in vpn mode...");
            self.run_vpn_out_queue().await
        } else {
            debug!("Starting out queue controller...");
            self.run_normal_out_queue().await
        }
    }
}
