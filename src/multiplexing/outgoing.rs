use futures::{stream::FuturesUnordered, StreamExt};
use tokio::select;

use super::{AddChan, AddMsg, Demultiplexer, ErrorChan, MultiMessage, Multiplexer};
use crate::{Receiver, Sender, SessionError};

/// Control signals for the running outgoing channel, allowing the rest of
/// the program to register new sessions and to abort all ongoing ones.
pub struct Control<P, R> {
    errors: Sender<SessionError>,
    adds: Sender<AddMsg<P, R>>,
}

impl<P, R> Control<P, R> {
    /// Register the client side of a session protocol, so that `run` starts listening
    /// to messages coming from the protocol and multiplexing them to the outgoing
    /// connection towards the server, and also starts dispaching server messages
    /// to this protocol.
    ///
    /// The session should be automatically removed when it closes the other sides of
    /// the channels.
    ///
    /// Returns `false` if the channel is already closed and the session could not be registered.
    pub fn add(&self, protocol_id: P, tx: Sender<R>, rx: Receiver<R>) -> bool {
        self.adds.send((protocol_id, tx, rx)).is_ok()
    }

    /// Remove all sessions due to protocol violation. Use an internal channel to notify `run`.
    pub fn error(&self, _protocol_id: P, error: SessionError) {
        // Ignoring send errors, it would mean the channel is already closed.
        let _ = self.errors.send(error);
    }
}

/// An `OutgoingMultiChannel` is established for each outgoing TCP connection.
/// The local party will be the client of the remote server.
///
/// The `mux` part listens to multiple sessions initiated by this side and
/// relay their messages to the common outgoing channel.
///
/// The `demux` part listens to the incoming replies from the server on the
/// connection and dispatches the messages to their corresponding protocol
/// handlers.
pub struct OutgoingMultiChannel<P, R> {
    mux: Multiplexer<P, R>,
    demux: Demultiplexer<P, R>,
    errors: ErrorChan,
    adds: AddChan<P, R>,
}

impl<P: Ord + Copy, R: 'static + Send + Sync> OutgoingMultiChannel<P, R> {
    /// Create a new `MultiChannel` by passing it the channels it can use to
    /// send/receive messages to/from the underlying network connection.
    pub fn new(tx: Sender<MultiMessage<P, R>>, rx: Receiver<MultiMessage<P, R>>) -> Self {
        Self {
            mux: Multiplexer::new(tx),
            demux: Demultiplexer::new(rx),
            errors: ErrorChan::new(),
            adds: AddChan::new(),
        }
    }

    /// Get a handler that can be used to send signals to the channel.
    ///
    /// Call this before the channel is run.
    pub fn control(&self) -> Control<P, R> {
        Control {
            errors: self.errors.tx.clone(),
            adds: self.adds.tx.clone(),
        }
    }

    /// Start consuming messages from the remote server, relaying them to
    /// the local client side of the protocols.
    ///
    /// We must also listen to an internal channel that signals the abortion
    /// of the whole connection due to some protocol violation.
    ///
    /// Yet another internal channel must be used to receive registration requests.
    pub async fn run(mut self) {
        // Futures aggregator for the session clients on our side.
        let mut mux = FuturesUnordered::new();

        loop {
            select! {
                // Abandon all channels if there's an error. This should never return `None`.
                Some(err) = self.errors.rx.recv() => {
                    // We can ignore disconnect errors, they can come from replaced sessions.
                    // We'll recognise a real disconnection by not being able to receive from
                    // the remote side here in the incoming arm.
                    match err {
                        SessionError::Disconnected => (),
                        _ => break,
                    }
                },

                // We are initiating a new protocol. This should never return `None`.
                Some(add) = self.adds.rx.recv() => {
                    let (pid, rx) = self.add(add);
                    mux.push(Self::recv_outgoing(pid, rx));
                }

                // We have an outgoing request from one of the sessions.
                // Initially `mux` is empty and this branch will be disabled by the `select!`.
                // Only after the first `add` message will it get another chaince.
                Some((pid, outgoing, rx)) = mux.next() => {
                    match outgoing {
                        Some(msg) => {
                            self.handle_outgoing_request(pid, msg);
                            // Re-queue the receiver.
                            mux.push(Self::recv_outgoing(pid, rx));
                        }
                        // The outgoing channel got closed on our side.
                        // The other sessions can keep going.
                        None =>self.remove(pid),
                    }
                }

                // There is an incoming reply from the server.
                incoming = self.demux.recv() => {
                    match incoming {
                        Some((pid, msg)) => self.handle_incoming_reply(pid, msg),
                        // The channel to server is closed.
                        None => break,
                    }
                }
            }
        }
    }

    // We need exactly one function to produce the future that gets pushed into the `FuturesUnordered` otherwise it would have multiple types.
    async fn recv_outgoing(pid: P, mut rx: Receiver<R>) -> (P, Option<R>, Receiver<R>) {
        let o = rx.recv().await;
        (pid, o, rx)
    }

    /// Add a new session, which is the client side of a protocol this side initiated.
    ///
    /// If a session with the same ID already exists it is overwritten.
    /// This should cause a disconnection error to be raised in the session,
    /// which is why we have to ignore those, and not abort all other sessions
    /// when such an error is reported.
    ///
    /// Returns the receiver we need to register with the multiplexer.
    fn add(&mut self, add: AddMsg<P, R>) -> (P, Receiver<R>) {
        let (pid, tx, rx) = add;
        self.demux.txs.insert(pid, tx);
        (pid, rx)
    }

    /// Dispatch an incoming reply to the corresponding session that sent the orginal request as a client.
    ///
    /// Abort if the protocol doesn't exist. This is an outgoing connection, the local party initiates.
    fn handle_incoming_reply(&mut self, pid: P, msg: R) {
        match self.demux.txs.get(&pid) {
            Some(tx) => {
                // Ignoring send errors here; it means the session has ended on our side,
                // but we'll realise this in the loop when trying to receive from it.
                let _ = tx.send(msg);
            }
            None => {
                // A message to a protocol we did not initiate.
                let _ = self
                    .errors
                    .tx
                    .send(SessionError::UnexpectedMessage(Box::new(msg)));
            }
        }
    }

    /// Wrap an outgoing request and relay to the connection channel.
    fn handle_outgoing_request(&self, pid: P, msg: R) {
        // Ignoring send errors here; it means the connection is closed,
        // but we'll realize that in the loop when trying to receive.
        let _ = self.mux.send(pid, msg);
    }

    /// Remove a session protocol that got closed on our side.
    fn remove(&mut self, pid: P) {
        self.demux.txs.remove(&pid);
    }
}

#[cfg(test)]
mod test {
    use crate::multiplexing::MultiMessage;
    use crate::session_channel_dyn;
    use crate::test::protocols::greetings::*;
    use crate::test::protocols::ping_pong::*;
    use crate::test::protocols::*;
    use crate::unbounded_channel;
    use crate::Chan;
    use crate::DynMessage;
    use crate::SessionResult;
    use std::time::Duration;
    use tokio::time::timeout as timeout_after;

    use super::OutgoingMultiChannel;

    type PID = u8;
    mod protos {
        pub const PING_PONG: u8 = 1;
        pub const GREETINGS: u8 = 2;
    }

    #[tokio::test]
    async fn basics() {
        let timeout = Duration::from_millis(100);

        // Create an IncomingMultiChannel. It needs a pair of channels,
        // one for requests going out and one for replies coming in. .
        let (tx_in, rx_in) = unbounded_channel();
        let (tx_out, mut rx_out) = unbounded_channel();
        let channel = OutgoingMultiChannel::<PID, DynMessage>::new(tx_out, rx_in);

        // Grab the control handle before moving the channel into a thread.
        let control = channel.control();

        // Start multiplexing in the background.
        tokio::spawn(channel.run());

        // Act as session server in the test. Start the clients in the background.
        async fn cli(
            c: Chan<greetings::Client, (), DynMessage>,
            timeout: Duration,
        ) -> SessionResult<()> {
            let c = c.send(Hail("Punter".into()))?;
            let (c, Greetings(_)) = c.recv(timeout).await?;
            let c = c.enter();
            let (c, AddResponse(_)) = c
                .sel2()
                .sel1()
                .send(AddRequest(1))?
                .send(AddRequest(2))?
                .recv(timeout)
                .await?;

            c.zero()?.sel2().sel2().send(Quit)?.close()
        }

        let start_greeting = || {
            let (c, (tx, rx)) = session_channel_dyn::<greetings::Client, DynMessage>();
            // Start the session interacting with the greeting server in a thread.
            tokio::spawn(cli(c, timeout));
            // Register the channels with the multiplexer.
            control.add(protos::GREETINGS, tx, rx);
        };

        start_greeting();

        // See what the outgoing multiplexer wants to send.
        let res = timeout_after(timeout, rx_out.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(res.protocol_id, protos::GREETINGS);
        assert!(res.payload.downcast::<Hail>().is_ok());

        // Now send an unexpected reply to a never requested ping. It should cause the whole thing to be closed.
        tx_in
            .send(MultiMessage::new(protos::PING_PONG, Pong))
            .unwrap();

        // Wait a bit for the message to take effect.
        tokio::time::sleep(timeout / 2).await;

        let res = tx_in.send(MultiMessage::new(
            protos::GREETINGS,
            Hail("Still there?".into()),
        ));
        assert!(res.is_err());
    }
}
