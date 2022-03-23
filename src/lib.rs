// Enabled for the `repr_bound!` macro.
#![feature(trait_alias)]
#![feature(async_closure)]

use std::{error::Error, marker, marker::PhantomData, mem::ManuallyDrop, thread, time::Duration};
use tokio::time::timeout as timeout_after;

// Type aliases so we don't have to update these in so many places if we switch the implementation.

pub type Receiver<T> = tokio::sync::mpsc::UnboundedReceiver<T>;
pub type Sender<T> = tokio::sync::mpsc::UnboundedSender<T>;

fn unbounded_channel<T>() -> (Sender<T>, Receiver<T>) {
    tokio::sync::mpsc::unbounded_channel()
}

/// Multiplexing multiple protocols over a single channel and dispatching to session handler instances.
#[cfg(feature = "mux")]
pub mod multiplexing;

mod repr;

pub use repr::{DynMessage, Repr};

#[derive(Debug)]
pub enum SessionError {
    /// Wrong message type was sent.
    UnexpectedMessage(DynMessage),
    /// The other end of the channel is closed.
    Disconnected,
    /// Did not receive a message within the timeout.
    Timeout,
    /// Abort due to the a violation of some protocol constraints.
    Abort(Box<dyn Error + marker::Send + 'static>),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Error for SessionError {}

pub type SessionResult<T> = Result<T, SessionError>;

pub fn ok<T>(value: T) -> SessionResult<T> {
    Ok(value)
}

fn downcast<T, R: Repr<T>>(msg: R) -> SessionResult<T> {
    msg.try_into()
        .map_err(|msg| SessionError::UnexpectedMessage(Box::new(msg)))
}

/// A session typed channel. `P` is the protocol and `E` is the environment,
/// containing potential recursion targets. `R` is the representation of
/// messages, which could be `DynMessage`, or perhaps something we know
/// statically how to turn into JSON or bytes.
pub struct Chan<P, E, R> {
    tx: ManuallyDrop<Sender<R>>,
    rx: ManuallyDrop<Receiver<R>>,
    stash: ManuallyDrop<Option<R>>,
    _phantom: PhantomData<(P, E)>,
}

impl<P, E, R> Chan<P, E, R> {
    fn new(tx: Sender<R>, rx: Receiver<R>) -> Chan<P, E, R> {
        Chan {
            tx: ManuallyDrop::new(tx),
            rx: ManuallyDrop::new(rx),
            stash: ManuallyDrop::new(None),
            _phantom: PhantomData,
        }
    }
}

fn write_chan<T, P, E, R: Repr<T>>(chan: &Chan<P, E, R>, v: T) -> SessionResult<()> {
    chan.tx
        .send(Repr::from(v))
        .map_err(|_| SessionError::Disconnected)
}

/// Read a message then cast it to the expected type.
async fn read_chan<T, P, E, R: Repr<T>>(
    chan: &mut Chan<P, E, R>,
    timeout: Duration,
) -> SessionResult<T> {
    let msg = read_chan_dyn(chan, timeout).await?;
    downcast(msg)
}

/// Try to read a dynamically typed message from the stay of from the channel.
async fn read_chan_dyn<P, E, R>(chan: &mut Chan<P, E, R>, timeout: Duration) -> SessionResult<R> {
    match chan.stash.take() {
        Some(msg) => Ok(msg),
        None if timeout == Duration::MAX => match chan.rx.recv().await {
            Some(msg) => Ok(msg),
            None => Err(SessionError::Disconnected),
        },
        None => match timeout_after(timeout, chan.rx.recv()).await {
            Ok(Some(msg)) => Ok(msg),
            Ok(None) => Err(SessionError::Disconnected),
            Err(_) => Err(SessionError::Timeout),
        },
    }
}

fn close_chan<P, E, R>(chan: Chan<P, E, R>) {
    // This method cleans up the channel without running the panicky destructor
    // In essence, it calls the drop glue bypassing the `Drop::drop` method.
    let mut this = ManuallyDrop::new(chan);
    unsafe {
        ManuallyDrop::drop(&mut this.tx);
        ManuallyDrop::drop(&mut this.rx);
        ManuallyDrop::drop(&mut this.stash);
    }
}

/// Peano numbers: Zero
pub struct Z;

/// Peano numbers: Increment
pub struct S<N>(PhantomData<N>);

/// End of communication session (epsilon)
pub struct Eps;

/// Receive `T`, then resume with protocol `P`.
pub struct Recv<T, P>(PhantomData<(T, P)>);

/// Send `T`, then resume with protocol `P`.
pub struct Send<T, P>(PhantomData<(T, P)>);

/// Active choice between `P` and `Q`
pub struct Choose<P: Outgoing, Q: Outgoing>(PhantomData<(P, Q)>);

/// Passive choice (offer) between `P` and `Q`
pub struct Offer<P: Incoming, Q: Incoming>(PhantomData<(P, Q)>);

/// Enter a recursive environment.
pub struct Rec<P>(PhantomData<P>);

/// Recurse. N indicates how many layers of the recursive environment we recurse out of.
pub struct Var<N>(PhantomData<N>);

/// Indicate that a protocol will receive a message, and specify what type it is,
/// so we can decide in an offer which arm we got a message for.
pub trait Incoming {
    type Expected;
}

impl<T, P> Incoming for Recv<T, P> {
    type Expected = T;
}
impl<P: Incoming, Q: Incoming> Incoming for Offer<P, Q> {
    type Expected = P::Expected;
}

/// Indicate that a protocol will send a message.
pub trait Outgoing {}

impl<T, P> Outgoing for Send<T, P> {}
impl<P: Outgoing, Q: Outgoing> Outgoing for Choose<P, Q> {}

/// The HasDual trait defines the dual relationship between protocols.
///
/// Any valid protocol has a corresponding dual.
pub trait HasDual {
    type Dual;
}

impl HasDual for Eps {
    type Dual = Eps;
}

impl<A, P: HasDual> HasDual for Send<A, P> {
    type Dual = Recv<A, P::Dual>;
}

impl<A, P: HasDual> HasDual for Recv<A, P> {
    type Dual = Send<A, P::Dual>;
}

impl<P: HasDual, Q: HasDual> HasDual for Choose<P, Q>
where
    P: Outgoing,
    Q: Outgoing,
    P::Dual: Incoming,
    Q::Dual: Incoming,
{
    type Dual = Offer<P::Dual, Q::Dual>;
}

impl<P: HasDual, Q: HasDual> HasDual for Offer<P, Q>
where
    P: Incoming,
    Q: Incoming,
    P::Dual: Outgoing,
    Q::Dual: Outgoing,
{
    type Dual = Choose<P::Dual, Q::Dual>;
}

impl HasDual for Var<Z> {
    type Dual = Var<Z>;
}

impl<N> HasDual for Var<S<N>> {
    type Dual = Var<S<N>>;
}

impl<P: HasDual> HasDual for Rec<P> {
    type Dual = Rec<P::Dual>;
}

pub enum Branch<L, R> {
    Left(L),
    Right(R),
}

/// A sanity check destructor that kicks in if we abandon the channel by
/// returning `Ok(_)` without closing it first.
impl<P, E, R> Drop for Chan<P, E, R> {
    fn drop(&mut self) {
        if !thread::panicking() {
            panic!("Session channel prematurely dropped. Must call `.close()`.");
        }
    }
}

impl<E, R> Chan<Eps, E, R> {
    /// Close a channel. Should always be used at the end of your program.
    pub fn close(self) -> SessionResult<()> {
        close_chan(self);
        Ok(())
    }
}

impl<P, E, R> Chan<P, E, R> {
    fn cast<P2, E2>(self) -> Chan<P2, E2, R> {
        let mut this = ManuallyDrop::new(self);
        unsafe {
            Chan {
                tx: ManuallyDrop::new(ManuallyDrop::take(&mut this.tx)),
                rx: ManuallyDrop::new(ManuallyDrop::take(&mut this.rx)),
                stash: ManuallyDrop::new(ManuallyDrop::take(&mut this.stash)),
                _phantom: PhantomData,
            }
        }
    }

    /// Close the channel and return an error due to some business logic violation.
    pub fn abort<T, F: Error + marker::Send + 'static>(self, e: F) -> SessionResult<T> {
        close_chan(self);
        Err(SessionError::Abort(Box::new(e)))
    }

    pub fn abort_dyn<T>(self, e: Box<dyn Error + marker::Send>) -> SessionResult<T> {
        close_chan(self);
        Err(SessionError::Abort(e))
    }
}

impl<P, E, T, R: Repr<T>> Chan<Send<T, P>, E, R> {
    /// Send a value of type `T` over the channel. Returns a channel with protocol `P`.
    pub fn send(self, v: T) -> SessionResult<Chan<P, E, R>> {
        match write_chan(&self, v) {
            Ok(()) => Ok(self.cast()),
            Err(e) => {
                close_chan(self);
                Err(e)
            }
        }
    }
}

impl<P, E, T, R: Repr<T>> Chan<Recv<T, P>, E, R> {
    /// Receives a value of type `T` from the channel. Returns a tuple
    /// containing the resulting channel and the received value.
    pub async fn recv(mut self, timeout: Duration) -> SessionResult<(Chan<P, E, R>, T)> {
        match read_chan(&mut self, timeout).await {
            Ok(v) => Ok((self.cast(), v)),
            Err(e) => {
                close_chan(self);
                Err(e)
            }
        }
    }
}

impl<P: Outgoing, Q: Outgoing, E, R> Chan<Choose<P, Q>, E, R> {
    /// Perform an active choice, selecting protocol `P`.
    /// We haven't sent any value yet, so the agency stays on our side.
    pub fn sel1(self) -> Chan<P, E, R> {
        self.cast()
    }

    /// Perform an active choice, selecting protocol `Q`.
    /// We haven't sent any value yet, so the agency stays on our side.
    pub fn sel2(self) -> Chan<Q, E, R> {
        self.cast()
    }
}

/// Branches offered between protocols `P` and `Q`.
type OfferBranch<P, Q, E, R> = Branch<Chan<P, E, R>, Chan<Q, E, R>>;

impl<P: Incoming, Q: Incoming, E, R> Chan<Offer<P, Q>, E, R>
where
    P::Expected: 'static,
    R: Repr<P::Expected>,
{
    /// Put the value we pulled from the channel back,
    /// so the next protocol step can read it and use it.
    fn stash(mut self, msg: R) -> Self {
        self.stash = ManuallyDrop::new(Some(msg));
        self
    }

    /// Passive choice. This allows the other end of the channel to select one
    /// of two options for continuing the protocol: either `P` or `Q`.
    /// Both options mean they will have to send a message to us,
    /// the agency is on their side.
    pub async fn offer(mut self, t: Duration) -> SessionResult<OfferBranch<P, Q, E, R>> {
        // The next message we read from the channel decides
        // which protocol we go with.
        let msg = match read_chan_dyn(&mut self, t).await {
            Ok(msg) => msg,
            Err(e) => {
                close_chan(self);
                return Err(e);
            }
        };

        // This variant casts then re-wraps.
        // match Repr::<P::Expected>::try_cast(msg) {
        //     Ok(exp) => Ok(Left(self.stash(exp.to_repr()).cast())),
        //     Err(msg) => Ok(Right(self.stash(msg).cast())),
        // }

        // This variant just checks, to avoid unwrapping and re-wrapping.
        if Repr::<P::Expected>::can_into(&msg) {
            Ok(Branch::Left(self.stash(msg).cast()))
        } else {
            Ok(Branch::Right(self.stash(msg).cast()))
        }
    }
}

impl<P, E, R> Chan<Rec<P>, E, R> {
    /// Enter a recursive environment, putting the current environment on the
    /// top of the environment stack.
    pub fn enter(self) -> Chan<P, (P, E), R> {
        self.cast()
    }
}

impl<P, E, R> Chan<Var<Z>, (P, E), R> {
    /// Recurse to the environment on the top of the environment stack.
    /// The agency must be kept, since there's no message exchange here,
    /// we just start from the top as a continuation of where we are.
    pub fn zero(self) -> SessionResult<Chan<P, (P, E), R>> {
        Ok(self.cast())
    }
}

impl<P, E, N, R> Chan<Var<S<N>>, (P, E), R> {
    /// Pop the top environment from the environment stack.
    pub fn succ(self) -> Chan<Var<N>, E, R> {
        self.cast()
    }
}

type ChanPair<P, R> = (Chan<P, (), R>, Chan<<P as HasDual>::Dual, (), R>);
type ChanDynPair<P, R> = (Chan<P, (), R>, (Sender<R>, Receiver<R>));

pub fn session_channel<P: HasDual, R>() -> ChanPair<P, R> {
    let (tx1, rx1) = unbounded_channel();
    let (tx2, rx2) = unbounded_channel();

    let c1 = Chan::new(tx1, rx2);
    let c2 = Chan::new(tx2, rx1);

    (c1, c2)
}

/// Similar to `session_channel`; create a typed channel for a protocol `P`,
/// but instead of creating a channel for its dual, return the raw sender
/// and receiver that can be used to communicate with the channel created.
///
/// These can be used in multiplexers to dispatch messages to/from the network.
pub fn session_channel_dyn<P, R>() -> ChanDynPair<P, R> {
    let (tx1, rx1) = unbounded_channel();
    let (tx2, rx2) = unbounded_channel();

    let c = Chan::new(tx1, rx2);

    (c, (tx2, rx1))
}

/// This macro is convenient for server-like protocols of the form:
///
/// `Offer<A, Offer<B, Offer<C, ... >>>`
///
/// # Examples
///
/// Assume we have a protocol `Offer<Recv<u64, Eps>, Offer<Recv<String, Eps>,Eps>>>`
/// we can use the `offer!` macro as follows:
///
/// ```rust
/// use async_session_types::offer;
/// use async_session_types::*;
/// use std::time::Duration;
///
/// struct Bye;
///
/// async fn srv(c: Chan<Offer<Recv<u64, Eps>, Offer<Recv<String, Eps>, Recv<Bye, Eps>>>, (), DynMessage>) -> SessionResult<()> {
///     let t = Duration::from_secs(1);
///     offer! { c, t,
///         Number => {
///             let (c, n) = c.recv(t).await?;
///             assert_eq!(42, n);
///             c.close()
///         },
///         String => {
///             c.recv(t).await?.0.close()
///         },
///         Quit => {
///             c.recv(t).await?.0.close()
///         }
///     }
/// }
///
/// async fn cli(c: Chan<Choose<Send<u64, Eps>, Choose<Send<String, Eps>, Send<Bye, Eps>>>, (), DynMessage>) -> SessionResult<()>{
///     c.sel1().send(42)?.close()
/// }
///
/// #[tokio::main]
/// async fn main() {
///     let (s, c) = session_channel();
///     tokio::spawn(cli(c));
///     srv(s).await.unwrap();
/// }
/// ```
///
/// The identifiers on the left-hand side of the arrows have no semantic
/// meaning, they only provide a meaningful name for the reader.
#[macro_export]
macro_rules! offer {
    (
        $id:ident, $timeout:expr, $branch:ident => $code:expr, $($t:tt)+
    ) => (
        match $id.offer($timeout).await? {
            $crate::Branch::Left($id) => $code,
            $crate::Branch::Right($id) => offer!{ $id, $timeout, $($t)+ }
        }
    );
    (
        $id:ident, $timeout:expr, $branch:ident => $code:expr
    ) => (
        $code
    )
}

#[cfg(test)]
mod test;
