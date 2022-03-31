# Session Types

`async-session-types` is a library to express the interaction between two parties
as message passing with statically compiled correctness. Any violation
of the protocol, such as an unexpected message type, would result in
closing down the connection with the remote peer.

This allows both parties to protect themselves from malicious counterparts,
by maintaining a session where they can carry a context of what they know
about each other and what they can rightfully expect next.

It can also protect against one party trying to overwhelm the other with
messages by making back pressure an essential part of the protocols, although
this hasn't been implemented yet (it would need bounded channels).

Session Types are a way to achieve something similar to Cardano's Mini Protocols.

The library has been inspired by [session-types](https://github.com/Munksgaard/session-types),
but has been extended in the following ways:
* To work with unreliable parties, it breaks up the session rather than panic if an unexpected message arrives.
* Timeouts are required for every receive operation.
* The ability to abort a session with an error if some business rule has been violated; these are rules that aren't expressed in
the protocol.
* Using `tokio` channels so we can have multiple session types without blocking full threads while awaiting the next message.
* At any time, only one of the the server or the client are in a position to send the next message.
* Every decision has to be represented as a message, unlike in the original which sent binary flags to indicate choice.
* Added a [Repr](src/repr.rs) type to be able to route messages to multiple session types using `enum` wrappers, instead of relying on dynamic casting.
* Added a pair of [incoming](src/multiplexing/incoming.rs)/[outgoing](src/multiplexing/outgoing.rs) demultiplexer/multiplexer types to support dispatching to multiple session types over a single connection, like a Web Socket.

Please have a look at the [tests](src/test.rs) to see examples.

## Example

The following snippets demonstrate of hooking up some protocol with a Web Socket using JSON format.

Say we have a protocol to sync blocks from a blockchain:

<details>
  <summary>Expand protocol</summary>

```rust
/// Messages exchanged in the protocol.
///
/// They are generic in a way that should be compatible with full blocks or just headers.
pub mod messages {
    use core::property::{HasHash, HasParent};
    use async_session_types::{repr_bound, Repr};
    use serde::{Deserialize, Serialize};

    /// Ask the producer to find the newest point that exists on its blockchain.
    /// The block hashes are assumed to go from latest to oldest, for example
    /// with exponential gaps between them, e.g. tip, tip-1, tip-2, tip-4, tip-8, etc.
    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub struct FindIntersect<B: HasParent>(pub Vec<<B as HasHash>::Hash>);

    /// Tell the consumer about the first point that can be found on the producer's chain.
    /// They can start consuming from here, or try to find further points.
    /// The intersect found will become the read pointer for the consumer, so following
    /// up with a `RequestNext` message will go from here. But the consumer can also
    /// send further `FindIntersect` messages, bisecting until the best possible match
    /// is found; the better results will adjust the read pointer.
    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub struct IntersectFound<B: HasParent>(pub <B as HasHash>::Hash);

    /// Tell the consumer that none of the identifiers in `FindIntersect` are known.
    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub struct IntersectNotFound;

    /// Ask the producer to send the next block.
    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub struct RequestNext;

    /// Tell the consumer that they are caught up with the chain, and the next even is going
    /// to arrive when the producer's chain changes.
    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub struct AwaitReply;

    /// Tell the consumer to extend its chain with the next connecting block.
    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub struct RollForward<B: HasParent>(pub B);

    /// Tell the consumer to roll back to an earlier block hash.
    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub struct RollBackward<B: HasParent>(pub <B as HasHash>::Hash);

    /// The consumer signals the producer to terminate the protocol.
    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub struct Done;

    repr_bound! {
        pub LocalBlockSyncReprs<B: HasParent> [
            FindIntersect<B>,
            IntersectFound<B>,
            IntersectNotFound,
            RequestNext,
            AwaitReply,
            RollForward<B>,
            RollBackward<B>,
            Done
        ]
    }
}

/// Session Type protocols from the server's perspective.
///
/// The protocol allows the a trusted local client such as the wallet to
/// subscribe to the linearised version of the blockchain, i.e. not the
/// input and ranking blocks, but what is the result of their execution.
///
/// The protocol starts with the client establishing the intersection point
/// between the version of the chain it last synced to with where the server
/// is currently it, which may require a rollback before the subscription
/// can be resumed.
///
/// The search for intersection can be repeated any number of time, using
/// for example bisection to zoom in on the latest common block.
///
/// Once the intersection is known, the server feeds new blocks to the
/// client as and when the client is asking for them (with certain timeouts),
/// including notifications about potential rollbacks.
///
/// The server may start with a `RollBackward` message targetting the latest
/// known intersection known point, so the client doesn't have to remember
/// to do that step.
#[rustfmt::skip]
pub mod protocol {
    use async_session_types::*;
    use super::messages::*;

    /// Protocol to find the latest block that intersects in the chains of the client and the server.
    pub type Intersect<B> =
        Recv<
            FindIntersect<B>,
            Choose<
                Send<IntersectFound<B>, Var<Z>>,
                Send<IntersectNotFound, Var<Z>>
            >,
        >;

    /// Respond to the consumer with the next available block, or a rollback to an earlier one.
    pub type Roll<B> =
        Choose<
            Send<RollForward<B>, Var<Z>>,
            Send<RollBackward<B>, Var<Z>>
        >;

    /// Protocol to request the next available block.
    pub type Next<B> =
        Recv<
            RequestNext,
            Choose<
                Roll<B>,
                Send<AwaitReply, Roll<B>>
            >
        >;

    /// Receive a quit request to quit from the client.
    pub type Quit = Recv<Done, Eps>;

    // NOTE: Not surrounding with `Rec` because we will use a single `.enter()` and never return to the base.

    /// The server side of the local block sync protocol.
    pub type Server<B> =
        Offer<
            Intersect<B>,
            Offer<
                Next<B>,
                Quit
            >
        >;

    /// The client side of the local block sync protocol.
    pub type Client<B> = <Server<B> as HasDual>::Dual;
}
```
</details>
<br/>


And a message wrapper that handles JSON tagging:
<details>
<summary>Expand Wrapper</summary>

```rust
/// Identifiers used in the multiplexer.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Debug)]
pub enum ProtocolId {
    LocalBlockSync,
}

/// Alias for the message type used in the multiplexer.
pub type ProtocolMessage = MultiMessage<ProtocolId, Wrapper>;

/// Wrapper type for the Local Block Sync messages.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", content = "payload")]
pub enum LbsWrapper {
    FindIntersect(lbs::FindIntersect<WalletBlock>),
    IntersectFound(lbs::IntersectFound<WalletBlock>),
    IntersectNotFound,
    RequestNext,
    AwaitReply,
    RollForward(Box<lbs::RollForward<WalletBlock>>),
    RollBackward(lbs::RollBackward<WalletBlock>),
    Done,
}

/// Wrapper type we can use as to represent all protocol messages.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "protocol")]
pub enum Wrapper {
    LocalBlockSync(LbsWrapper),
}

// Produce all Repr implementations from the Wrapper to the individual messages.
repr_impl! {
  Wrapper {
    // Local Block Sync
    lbs::FindIntersect<WalletBlock> : (
        |x| Wrapper::LocalBlockSync(LbsWrapper::FindIntersect(x)),
        Wrapper::LocalBlockSync(LbsWrapper::FindIntersect(x)) => x
    ),
    lbs::IntersectFound<WalletBlock> : (
        |x| Wrapper::LocalBlockSync(LbsWrapper::IntersectFound(x)),
        Wrapper::LocalBlockSync(LbsWrapper::IntersectFound(x)) => x
    ),
    lbs::IntersectNotFound : (
        |_| Wrapper::LocalBlockSync(LbsWrapper::IntersectNotFound),
        Wrapper::LocalBlockSync(LbsWrapper::IntersectNotFound) => lbs::IntersectNotFound
    ),
    lbs::RequestNext : (
        |_| Wrapper::LocalBlockSync(LbsWrapper::RequestNext),
        Wrapper::LocalBlockSync(LbsWrapper::RequestNext) => lbs::RequestNext
    ),
    lbs::AwaitReply : (
        |_| Wrapper::LocalBlockSync(LbsWrapper::AwaitReply),
        Wrapper::LocalBlockSync(LbsWrapper::AwaitReply) => lbs::AwaitReply
    ),
    lbs::RollForward<WalletBlock> : (
        |x| Wrapper::LocalBlockSync(LbsWrapper::RollForward(Box::new(x))),
        Wrapper::LocalBlockSync(LbsWrapper::RollForward(x)) => *x
    ),
    lbs::RollBackward<WalletBlock> : (
        |x| Wrapper::LocalBlockSync(LbsWrapper::RollBackward(x)),
        Wrapper::LocalBlockSync(LbsWrapper::RollBackward(x)) => x
    ),
    lbs::Done : (
        |_| Wrapper::LocalBlockSync(LbsWrapper::Done),
        Wrapper::LocalBlockSync(LbsWrapper::Done) => lbs::Done
    ),
  }
}
```
</details>
<br/>


Then we can handle web socket connections by creating new channels and spawning a task with a multiplexer:

<details>
<summary>Expand socket handler</summary>

```rust
async fn handle_connection(stream: TcpStream, addr: SocketAddr, root: Arc<Root>) {
    debug!("Incoming TCP connection from: {}", addr);

    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
        Ok(s) => s,
        Err(e) => {
            error!("Error during handshake from {}: {}", addr, e);
            return;
        }
    };

    info!("WebSocket connection established: {}", addr);

    // Split the web socket stream so we can read and write independently.
    let (mut ws_out, ws_in) = ws_stream.split();

    // Create a channel which we can use to send messages to the session types world.
    // Ideally this would be bounded, but in fact this is just going into the multiplexer, which dispatches it to session specific channels,
    // so it would always consume the messages immediately. The bounds check would have to be in the multiplexer on a per-session basis.
    let (tx_in, rx_in) = tokio::sync::mpsc::unbounded_channel::<ProtocolMessage>();

    // Create a channel which we can use to receive messages from the session types world, going towards the web socket.
    let (tx_out, mut rx_out) = tokio::sync::mpsc::unbounded_channel::<ProtocolMessage>();

    // Start a task to run the multiplexer in the background.
    let _ = tokio::spawn(async move {
        let imc = IncomingMultiChannel::new(tx_out, rx_in);
        imc.run(|protocol_id, errors| root.init_server(addr, protocol_id, errors))
            .await;
        debug!("IncomingMultiChannel for {} stopped.", addr);
    });

    // Forward incoming web socket messages to the sync channel.
    // Each handler is a future, but it can send to the sync channel as long as its unbounded and so doesn't block.
    // This must return a `tungstenite::Error` because of the stream extension.
    let forward_incoming = ws_in.try_for_each(|msg| {
        if msg.is_text() || msg.is_binary() {
            match Wrapper::try_from(msg) {
                Ok(wrapper) => {
                    if tx_in.send(wrapper.into()).is_err() {
                        // The session handler is no longer listening.
                        return future::err(tungstenite::Error::ConnectionClosed);
                    }
                }
                Err(ProtocolError::SerdeJson(e)) => {
                    warn!("Could not deserialise message from {}: {}", addr, e);
                    // For now don't break the connection, to help debug JSON issues.
                }
                Err(ProtocolError::Tungstenite(e)) => {
                    error!("Unexpected tungstenite error from {}: {}", addr, e);
                    return future::err(e);
                }
                Err(e) => {
                    error!("Unexpected error from {}: {:?}", addr, e);
                    // Break the connection by returning an error.
                    return future::err(tungstenite::Error::ConnectionClosed);
                }
            }
        }
        future::ok(())
    });

    // Forward outgoing messages to the web socket. Running in a `Task` because it's waiting for messages asynchronously.
    let forward_outgoing = tokio::spawn(async move {
        while let Some(msg) = rx_out.recv().await {
            match serde_json::to_string(&msg.payload) {
                Ok(json_str) => {
                    let msg = tungstenite::Message::text(json_str);
                    if ws_out.send(msg).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    warn!("Could not convert reply to JSON: {}", e);
                }
            }
        }
    });

    // Turn them into futures we can await on.
    pin_mut!(forward_incoming, forward_outgoing);

    future::select(forward_incoming, forward_outgoing).await;

    info!("Disconnected from {}", addr);
}

```
</details>
<br/>


And spawn a session handler when we first see a message for a new protocol:
<details>
<summary>Expand session initializer</summary>

```rust
/// Sender and receiver for the channel that can be used to send/receive
/// messages to/from a session handler.
type WrapperChan = (Sender<Wrapper>, Receiver<Wrapper>);

impl Root {
    /// Start a task to handle a new protocol.
    pub fn init_server(
        &self,
        addr: SocketAddr,
        protocol_id: ProtocolId,
        errors: Sender<SessionError>,
    ) -> WrapperChan {
        let handle_result = move |result| {
            match result {
                Ok(()) => {
                    info!("Session {} from {} finished.", protocol_id, addr);
                }
                Err(SessionError::Disconnected) => {
                    info!("Session {} from {} disconnected.", protocol_id, addr);
                }
                Err(SessionError::Timeout) => {
                    warn!("Session {} from {} timed out.", protocol_id, addr);
                }
                Err(SessionError::UnexpectedMessage(msg)) => {
                    if let Some(msg) = msg.downcast_ref::<Wrapper>() {
                        error!(
                            "Session {} from {} ended because of an unexpected message: {:?}",
                            protocol_id, addr, msg
                        )
                    } else {
                        error!(
                            "Session {} from {} ended because of a completely unexpected message.",
                            protocol_id, addr
                        )
                    }
                    // This is an offence, but for now we can keep the other sessions alive.
                }
                Err(SessionError::Abort(e)) => {
                    error!("Session {} from {} aborted: {}", protocol_id, addr, e);
                    // This is a serious offence, abort all sessions.
                    let _ = errors.send(SessionError::Abort(e));
                }
            }
        };

        match protocol_id {
            ProtocolId::LocalBlockSync => self.init_local_block_sync_server(handle_result),
        }
    }

    fn init_local_block_sync_server<H>(&self, handle_result: H) -> WrapperChan
    where
        H: Send + 'static + FnOnce(Result<(), SessionError>),
    {
        let (chan, (tx, rx)) = session_channel_dyn::<Rec<Server<WalletBlock>>, Wrapper>();

        let mut server = Producer::new();

        tokio::spawn(async {
            handle_result(server.sync_chain(chan).await)
        });

        (tx, rx)
    }
}
```
</details>
<br/>

## Prerequisites

Install the following to be be able to build the project:
* [cargo](https://doc.rust-lang.org/cargo/getting-started/installation.html)
* [rust nightly](https://rust-lang.github.io/rustup/concepts/channels.html)

```shell
curl https://sh.rustup.rs -sSf | sh
rustup toolchain install nightly
rustup default nightly
rustup update
```

## See more

* https://github.com/Munksgaard/session-types
* https://ferrite-rs.github.io/ferrite-book/
* http://www.simonjf.com/2016/05/28/session-type-implementations.html

## License

This project is licensed under the [MIT license].

[MIT license]: https://github.com/aakoshh/async-session-types-rs/blob/master/LICENSE
