use futures::stream::FuturesUnordered;
use futures::StreamExt;
use std::{collections::btree_map::Entry, fmt::Debug};
use tokio::select;

use super::{Demultiplexer, ErrorChan, MultiMessage, Multiplexer};
use crate::{Receiver, Sender, SessionError};

/// An `IncomingMultiChannel` is established for each incoming TCP connection.
/// The local party will act as the server for the remote client.
///
/// The `demux` part takes messages from the incoming connection and dispatches
/// to the implementing protocols. If the protocol doesn't exist, a server
/// session is instantiated to handle the messages.
///
/// The `mux` part listens for outgoing messages from all instantiated server
/// sessions, relaying responses to the client.
pub struct IncomingMultiChannel<P, R> {
    demux: Demultiplexer<P, R>,
    mux: Multiplexer<P, R>,
    errors: ErrorChan,
}

impl<P: Ord + Copy + Debug, R> IncomingMultiChannel<P, R> {
    /// Create a new `MultiChannel` by passing it the channels it can use to
    /// send/receive messages to/from the underlying network connection.
    pub fn new(tx: Sender<MultiMessage<P, R>>, rx: Receiver<MultiMessage<P, R>>) -> Self {
        Self {
            errors: ErrorChan::new(),
            demux: Demultiplexer::new(rx),
            mux: Multiplexer::new(tx),
        }
    }

    /// Start consuming messages from the client, creating new protocol handlers
    /// where one doesn't exist already.
    ///
    /// We must also listen to an internal channel that signals the abortion
    /// of the whole connection due to some protocol violation.
    ///
    /// `init_server` should initiate the server side of the protocol, listening
    /// to incoming messages from the client; it should return the channels we
    /// can send the client messages to, and receive the replies over. When
    /// sending or receiving from these channels fail, the protocol is over
    /// and should be removed.
    pub async fn run<F>(mut self, mut init_server: F)
    where
        F: FnMut(P, Sender<SessionError>) -> (Sender<R>, Receiver<R>),
    {
        // In a loop, add the `rx` of `demux`, and all the `rxs` of `mux` to a `Select`, see which one is ready:
        // * If `error.rx` returns `Ok` then a protocol violation occurred and we can exit the loop.
        // * If `demux.rx` returns `Ok` then get or create the protocol in both `mux` and `demux`, and dispatch into the corresponding channel in `demux.txs`.
        // * If `demux.rx` returns `Err` then the connection is closed and we can exit the loop.
        // * If any of `mux.rxs` return `Ok` then wrap the message and send to `mux.tx`.
        // * If any of `mux.rxs` return `Err` then that protocol is finished and can be removed from both `mux` and `demux`.

        // We have to declare a `FuturesUnordered` here, can't store it in a field becuase it has an opaque type, `impl Future`.
        let mut mux = FuturesUnordered::new();

        loop {
            select! {
                // One of the sessions detection a violation of some rule and now all of them need to be closed.
                // This will never return `None` because we never close the error channel.
                Some(_) = self.errors.rx.recv() => break,

                // One of the session handlers has a message to send to the client.
                // This might return `None` when there are no sessions, but the pattern will discard this branch
                // and wait on the others. The first incoming message will then establish a handler and the next
                // loop should see it matching.
                Some((pid, outgoing, rx)) = mux.next() => {
                    match outgoing {
                        Some(msg) => {
                            self.handle_outgoing_reply(pid, msg);
                            // Put the receiver back in the mux, so further replies from the session are handled.
                            mux.push(Self::recv_outgoing(pid, rx));
                        }
                        // The outgoing channel was closed on our side.
                        None => self.remove(pid),
                    }
                },

                // There is an incoming request from the client.
                incoming = self.demux.recv() => {
                    match incoming {
                        Some((pid, msg)) => {
                            if let Some(rx) = self.handle_incoming_request(pid, msg, &mut init_server) {
                                // Register the receiver with the reply multiplexer.
                                mux.push(Self::recv_outgoing(pid, rx));
                            }
                        },
                        // The incoming channel is closed.
                        None => break,
                    }
                },
            }
        }
    }

    // We need exactly one function to produce the future that gets pushed into the `FuturesUnordered` otherwise it would have multiple types.
    async fn recv_outgoing(pid: P, mut rx: Receiver<R>) -> (P, Option<R>, Receiver<R>) {
        let o = rx.recv().await;
        (pid, o, rx)
    }

    /// Dispatch an incoming request to the corresponding session.
    ///
    /// Initialize the session handler if it doesn't exist yet.
    /// If we initiated a new handler, return the channel where
    /// we can receive messages from it.
    fn handle_incoming_request<F>(
        &mut self,
        pid: P,
        msg: R,
        init_server: &mut F,
    ) -> Option<Receiver<R>>
    where
        F: FnMut(P, Sender<SessionError>) -> (Sender<R>, Receiver<R>),
    {
        match self.demux.txs.entry(pid) {
            Entry::Vacant(e) => {
                let (tx, rx) = init_server(pid, self.errors.tx.clone());
                let _ = tx.send(msg);
                e.insert(tx);
                Some(rx)
            }
            Entry::Occupied(e) => {
                // Ignoring send errors here; it means the channel is closed, but we'll realize that in the loop.
                let _ = e.get().send(msg);
                None
            }
        }
    }

    /// Wrap an outgoing reply and relay to the connection channel.
    fn handle_outgoing_reply(&self, pid: P, msg: R) {
        // Ignoring send errors here; it means the connection is closed, but we'll realize that in the loop.
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
    use crate::DynMessage;
    use crate::Receiver;
    use crate::Sender;
    use crate::SessionResult;
    use crate::{offer, ok, Chan};
    use std::fmt::Debug;
    use std::time::Duration;
    use std::time::Instant;
    use tokio::time::timeout as timeout_after;

    use super::IncomingMultiChannel;

    #[derive(Ord, Clone, PartialEq, PartialOrd, Eq, Debug, Copy)]
    enum Protos {
        PingPong,
        Greetings,
    }

    async fn ping_pong_srv(
        c: Chan<ping_pong::Server, (), DynMessage>,
        t: Duration,
    ) -> SessionResult<()> {
        let (c, _ping) = c.recv(t).await?;
        c.send(Pong)?.close()
    }

    async fn greetings_srv(
        c: Chan<greetings::Server, (), DynMessage>,
        t: Duration,
    ) -> SessionResult<()> {
        let (c, Hail(cid)) = c.recv(t).await?;
        let c = c.send(Greetings(format!("Hello {}!", cid)))?;
        let mut c = c.enter();
        loop {
            c = offer! { c, t,
                Time => {
                    let (c, TimeRequest) = c.recv(t).await?;
                    let c = c.send(TimeResponse(Instant::now()))?;
                    c.zero()?
                },
                Add => {
                    let (c, AddRequest(a)) = c.recv(t).await?;
                    let (c, AddRequest(b)) = c.recv(t).await?;
                    let c = c.send(AddResponse(a + b))?;
                    c.zero()?
                },
                Quit => {
                    let (c, Quit) = c.recv(t).await?;
                    c.close()?;
                    break;
                }
            };
        }

        ok(())
    }

    #[tokio::test]
    async fn basics() {
        let timeout = Duration::from_millis(100);

        // Create an IncomingMultiChannel. It needs a pair of channels, an incoming and outgoing one.
        // Whichever side we are not passing to the constructor is what we're going to use in the test.
        let (tx_in, rx_in) = unbounded_channel();
        let (tx_out, mut rx_out) = unbounded_channel();
        let channel = IncomingMultiChannel::<Protos, DynMessage>::new(tx_out, rx_in);

        type TxIn = Sender<MultiMessage<Protos, DynMessage>>;
        type RxOut = Receiver<MultiMessage<Protos, DynMessage>>;

        // Start the channel by passing it a closure that tells it how to instantiate servers.
        tokio::spawn(channel.run(move |p, errors| match p {
            Protos::PingPong => {
                let (c, (tx, rx)) = session_channel_dyn();
                tokio::spawn(async move {
                    if let Err(e) = ping_pong_srv(c, timeout).await {
                        let _ = errors.send(e);
                    }
                });
                (tx, rx)
            }

            Protos::Greetings => {
                let (c, (tx, rx)) = session_channel_dyn();
                tokio::spawn(async move {
                    if let Err(e) = greetings_srv(c, timeout).await {
                        let _ = errors.send(e);
                    }
                });
                (tx, rx)
            }
        }));

        // Act as session clients, send some messages, verify that the responses arrive.

        async fn test_ping(tx_in: &TxIn, rx_out: &mut RxOut, timeout: Duration) {
            tx_in
                .send(MultiMessage::new(Protos::PingPong, Ping))
                .unwrap();

            let res = timeout_after(timeout, rx_out.recv())
                .await
                .unwrap()
                .unwrap();

            assert_eq!(res.protocol_id, Protos::PingPong);
            assert!(res.payload.downcast::<Pong>().is_ok());
        }

        async fn test_greetings(tx_in: &TxIn, rx_out: &mut RxOut, timeout: Duration) {
            let pid = Protos::Greetings;
            tx_in
                .send(MultiMessage::new(pid, Hail("Multi".into())))
                .unwrap();

            let res = timeout_after(timeout, rx_out.recv())
                .await
                .unwrap()
                .unwrap();

            assert!(res.payload.downcast::<Greetings>().is_ok());

            tx_in.send(MultiMessage::new(pid, AddRequest(1))).unwrap();
            tx_in.send(MultiMessage::new(pid, AddRequest(2))).unwrap();

            let res = timeout_after(timeout, rx_out.recv())
                .await
                .unwrap()
                .unwrap();

            assert!(res.payload.downcast::<AddResponse>().is_ok());
            tx_in.send(MultiMessage::new(pid, Quit)).unwrap();
        }

        async fn test_abort(tx_in: &TxIn, rx_out: &mut RxOut, timeout: Duration) {
            tx_in
                .send(MultiMessage::new(Protos::Greetings, Hail("Abort".into())))
                .unwrap();

            let res = timeout_after(timeout, rx_out.recv())
                .await
                .unwrap()
                .unwrap();

            assert!(res.payload.downcast::<Greetings>().is_ok());

            // Send an invalid message.
            tx_in
                .send(MultiMessage::new(Protos::PingPong, "Boom!!!"))
                .unwrap();

            // It should cause the other protocol to close as well.
            tokio::time::sleep(timeout / 2).await;
            let res = tx_in.send(MultiMessage::new(Protos::Greetings, AddRequest(10)));
            assert!(res.is_err());
        }

        test_ping(&tx_in, &mut rx_out, timeout).await;
        test_greetings(&tx_in, &mut rx_out, timeout).await;
        test_ping(&tx_in, &mut rx_out, timeout).await;
        test_abort(&tx_in, &mut rx_out, timeout).await;
    }
}
