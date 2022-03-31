use std::any::Any;

/// We can use a dynamic boxed message to pass messages, and represent all messages in a protocol as individual structs.
/// However if we want to send them over a network connection we will need to serialise to some wire format, and at that
/// point we have to use tagging, so we can recognise what type to deserialise into.
///
/// It helps then to use something like an enum to represent the data, but the way protocols work is to use the sent and
/// received types in their generic signatures, where enums would not work.
///
/// For this reason the channels have a generic parameter for the representation of the messages, which can be an outer
/// enum that handles all supported protocols, paired with `From` and `TryInto` traits for each possible message we want
/// to send during the sessions.
///
/// However for in-memory use cases `DynMessage` still works fine.
pub type DynMessage = Box<dyn Any + Send + Sync + 'static>;

/// Define for the wire representation, so that the raw messages can be lifted into it,
/// and later the representation can be cast back into the expected types.
///
/// Similar to `From<T> for R` plus `TryInto<T> for R`.
/// Unfortunately the built-in implementations lead to conflicts for `DynMessage`.
///
/// ```
/// use async_session_types::*;
///
/// let repr: DynMessage = Repr::from(123u8);
/// let msg: u8 = Repr::try_into(repr).unwrap();
/// ```
pub trait Repr<T>: Send + Sync + 'static
where
    Self: Sized,
{
    /// Convert a raw type to the common representation.
    fn from(v: T) -> Self;
    /// Try to convert the representation back into one of the raw message types.
    fn try_into(self) -> Result<T, Self>;
    /// Check whether the representation can be turned into this raw type, without consuming.
    fn can_into(&self) -> bool;
}

/// We can turn anything into a `DynMessage`.
impl<T: 'static + Send + Sync> Repr<T> for DynMessage {
    fn from(v: T) -> Self {
        Box::new(v)
    }
    fn try_into(self) -> Result<T, Self> {
        self.downcast::<T>().map(|b| *b)
    }
    fn can_into(&self) -> bool {
        self.is::<T>()
    }
}

/// The `repr_impl` macro creates `Repr` implementations for a type used on the wire,
/// for each protocol message type enumerated in the call.
///
/// The macro call consists of `<wrapper-type-name>`, followed by lines of:
/// `<raw-type-name>: ( <fn-raw-to-repr> , <pattern-match-repr> => <captured-raw> )`.
///
/// # Example
/// ```
/// use async_session_types::*;
///
/// struct Foo(pub u8);
/// struct Bar(pub String);
/// enum Message {
///   Foo(Foo),
///   Bar(Bar),
/// }
///
/// repr_impl! {
///   Message {
///     Foo: ( Message::Foo, Message::Foo(x) => x ),
///     Bar: ( |x| Message::Bar(x), Message::Bar(x) => x )
///   }
/// }
/// ```
///
/// Note that there's no comma after the last item!
#[macro_export]
macro_rules! repr_impl {
    ($repr:ty { $($msg:ty : ( $lift:expr, $pattern:pat => $x:expr ) ),+ }) => {
        $(
            impl Repr<$msg> for $repr {
                fn from(v: $msg) -> Self {
                    $lift(v)
                }

                fn try_into(self) -> Result<$msg, Self> {
                    match self {
                        $pattern => Ok($x),
                        other => Err(other)
                    }
                }

                fn can_into(&self) -> bool {
                    // Using the same pattern that normally extracts the raw type
                    // only for checking if it succeeds, without using the variables.
                    #[allow(unused_variables)]
                    match &self {
                        $pattern => true,
                        _ => false
                    }
                }
            }
        )*
    };
}

/// The `repr_bound` macro creates a type alias that can be used as bound for the generic
/// wire type, instead of listing all messages used by the protocol.
///
/// # Example
/// ```
/// #![feature(trait_alias)]
///
/// use async_session_types::*;
///
/// struct Foo(pub u8);
/// struct Bar(pub String);
///
/// repr_bound! { FooBarReprs [Foo, Bar] }
///
/// type Server = Recv<Foo, Send<Bar, Eps>>;
///
/// fn server<R: FooBarReprs>(c: Chan<Server, (), R>) -> SessionResult<()> {
///     unimplemented!()
/// }
/// ```
///
/// Note that there's no comma after the last item!
///
/// Requires `#![feature(trait_alias)]`.
#[macro_export]
macro_rules! repr_bound {
    // https://stackoverflow.com/a/61189128
    (
        // e.g. `pub MyReprs<T: Foo + Bar> [ Spam<T>, Eggs ]`
        $vis:vis $repr_traits:ident
        $(<
            $($gen:tt $(: $b:tt $(+ $bs:tt)* )? ),+
        >)?
        [
            $($msg:ty),+
        ]
    ) => {
        $vis trait $repr_traits
        $(<
            $($gen $(: $b $(+ $bs)* )? ),+
        >)? = 'static $( + Repr<$msg> )+;
    };
}
