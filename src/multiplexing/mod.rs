use std::collections::BTreeMap;

use crate::{unbounded_channel, Receiver, Repr, Sender, SessionError};

pub mod incoming;
pub mod outgoing;

/// A multiplexed message, consisting of the ID of the protocol
/// and dynamic content that only the corresponding session will
/// know how to handle, and the check if it is indeed the next
/// expected message.
///
/// A message like this is coming from or going to a specific
/// network connection. It is assumed that there is no need to
/// identify the remote party, since the protocol would only
/// be talking to one party at a time that already got determined
/// at connection time.
///
/// We can probably assume that there will be only one instance
/// of each protocol per remote party, so `P` will be a simple
/// static identifier for the protocol. If that's not the case,
/// it could be more complex, like a unique session ID.
///
/// It is up to some higher level component to handle the wire
/// format of the message, including turning the payload and
/// the protocol ID into binary. For this we'll probably require
/// all messages to implement `serde`, and use a wrapper ADT
/// as the representation type, where we can apply tagging.
///
/// See https://serde.rs/enum-representations.html#internally-tagged
///
/// A network connection will be either incoming or outgoing,
/// i.e. for Alice to follow Bob and for Bob to follow Alice
/// they will need 2 TCP connections, one from Alice to Bob
/// and another from Bob to Alice. If it wasn't so we would
/// need a way to differentiate between two instances of the
/// same protocol, a unique session ID for each message.
///
/// Another way of doing it would be to add two flags to the message:
/// 1) indicate the direction of the connection, incoming or outgoing
/// 2) indicate whether the protocol was initiated by the source or
///    the target of the connection.
///
/// This would facilitate all communication over a single TCP connection,
/// however we would need to start with protocol negotiation to find out
/// if the other side even supports serving certain protocols. It would
/// also make it more difficult to tell if a connection can be closed,
/// because we wouldn't know if the other side has interest in keeping it
/// open even if currently there are no sessions.
#[derive(Debug)]
pub struct MultiMessage<P, R> {
    /// The protocol ID features explicitly in the message, rather than
    /// as a type class, because for example with `DynMessage` we don't
    /// have a unique mapping between a message type and its protocol.
    /// For example multiple protocols can expect a `String`.
    pub protocol_id: P,

    /// For now this is exactly the dynamic message that the session uses,
    /// but that won't work on the wire protocol without tagging. It will
    /// probably make more sense to turn it into an opaque representation
    /// that we have serialisers/deserialiser for based on the protocol ID.
    pub payload: R,
}

impl<P, R> MultiMessage<P, R> {
    pub fn new<T: Send + 'static>(protocol_id: P, msg: T) -> Self
    where
        R: Repr<T>,
    {
        Self {
            protocol_id,
            payload: Repr::from(msg),
        }
    }
}

/// The multiplexer takes messages from multiple channels that use
/// `DynMessage` for payload (each associated with a session),
/// attaches the protocol ID and relays the message into a common
/// outgoing channel using `MultiMessage`.
///
/// A multiplexer instance is unique to one connected party.
///
/// ```text
/// Protocol 1 ---> | \
///                 |  \
/// Protocol 2 ---> |mux| --> MultiMessage 1|2|3
///                 |  /
/// Protocol 3 ---> | /
/// ```
struct Multiplexer<P, R> {
    tx: Sender<MultiMessage<P, R>>,
    // NOTE: There used to be an `rxs` here similar to the `txs` in the `Demultiplexer`,
    // however since switching to `tokio`, waiting on multiple channels has to work
    // with a local `FuturesUnordered` in the runner loops that own the receives.
}

impl<P, R> Multiplexer<P, R> {
    pub fn new(tx: Sender<MultiMessage<P, R>>) -> Self {
        Self { tx }
    }

    /// Wrap and send a multiplexed message.
    ///
    /// Return `false` if the outgoing channel has already been closed.
    pub fn send(&self, protocol_id: P, payload: R) -> bool {
        self.tx
            .send(MultiMessage {
                protocol_id,
                payload,
            })
            .is_ok()
    }
}

/// The demultiplexer reads `MultiMessage` from an incoming channel
/// that is associated with a remote party (e.g. a TCP connection)
/// and dispatches the messages to protocol specific channels, each
/// associated with a different session with the same party.
///
/// ```text
///                            /   | ---> Protocol 1
///                           /    |
///  MultiMessage 1|2|3 ---> |demux| ---> Protocol 2
///                           \    |
///                            \   | ---> Protocol 3
/// ```
struct Demultiplexer<P, R> {
    rx: Receiver<MultiMessage<P, R>>,
    txs: BTreeMap<P, Sender<R>>,
}

impl<P, R> Demultiplexer<P, R> {
    pub fn new(rx: Receiver<MultiMessage<P, R>>) -> Self {
        Self {
            rx,
            txs: BTreeMap::new(),
        }
    }

    pub async fn recv(&mut self) -> Option<(P, R)> {
        self.rx.recv().await.map(|mm| (mm.protocol_id, mm.payload))
    }
}

/// A helper struct to hold on to both sides of a channel.
struct Chan<T> {
    rx: Receiver<T>,
    tx: Sender<T>,
}

impl<T> Chan<T> {
    pub fn new() -> Self {
        let (tx, rx) = unbounded_channel();
        Self { tx, rx }
    }
}

/// Channel we an use to allow protocols to signal the need to abort the
/// connection with the remote party.
///
/// Just by abandoning the loop that processes incoming messages from the
/// connection we should see the channel closing down, which the network
/// handler will be able to detect and close the physical connection.
type ErrorChan = Chan<SessionError>;

/// Channel we can use to register newly started sessions that want to  talk to a remote party.
type AddChan<P, R> = Chan<AddMsg<P, R>>;

/// A message we can send to over the `AddChan` to get a newly created session registered.
type AddMsg<P, R> = (P, Sender<R>, Receiver<R>);
