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
* Using `tokio` channels so we can have multiple session types without blocking full threads while awaiting message.
* At any time, only one of the the server or the client are in a position to send the next message.
* Every decision has to be represented as a message, unlike in the original which sent binary flags to indicate choice.
* Added a [Repr](/home/aakoshh/projects/samples/rust/async-session-types-rs/src/repr.rs) type to be able to route messages to multiple session types using `enum` wrappers, instead of relying on dynamic casting.
* Added a pair of incoming/outgoing demultiplexer/multiplexer types to support dispatching to multiple session types over a single connection, like a Web Socket.

Please have a look at the [tests](src/test.rs) to see examples.

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
