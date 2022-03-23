use super::*;
use std::time::Instant;

pub mod protocols {
    pub mod ping_pong {
        use crate::*;
        pub struct Ping;
        pub struct Pong;

        // NOTE: This verison doesn't include looping.
        pub type Server = Recv<Ping, Send<Pong, Eps>>;
        pub type Client = <Server as HasDual>::Dual;
    }

    pub mod greetings {
        use std::time::Instant;

        use crate::*;

        pub struct Hail(pub String);
        pub struct Greetings(pub String);
        pub struct TimeRequest;
        pub struct TimeResponse(pub Instant);
        pub struct AddRequest(pub u32);
        pub struct AddResponse(pub u32);
        pub struct Quit;

        pub type TimeProtocol = Recv<TimeRequest, Send<TimeResponse, Var<Z>>>;
        pub type AddProtocol = Recv<AddRequest, Recv<AddRequest, Send<AddResponse, Var<Z>>>>;
        pub type QuitProtocol = Recv<Quit, Eps>;

        pub type ProtocolChoices = Offer<TimeProtocol, Offer<AddProtocol, QuitProtocol>>;

        pub type Server = Recv<Hail, Send<Greetings, Rec<ProtocolChoices>>>;
        pub type Client = <Server as HasDual>::Dual;
    }
}

#[tokio::test]
async fn ping_pong_basics() {
    use protocols::ping_pong::*;
    let t = Duration::from_millis(100);

    let srv = async move |c: Chan<Server, (), DynMessage>| {
        let (c, Ping) = c.recv(t).await?;
        c.send(Pong)?.close()
    };

    let cli = async move |c: Chan<Client, (), DynMessage>| {
        let c = c.send(Ping)?;
        let (c, Pong) = c.recv(t).await?;
        c.close()
    };

    let (server_chan, client_chan) = session_channel();

    let srv_t = tokio::spawn(srv(server_chan));
    let cli_t = tokio::spawn(cli(client_chan));

    srv_t.await.unwrap().unwrap();
    cli_t.await.unwrap().unwrap();
}

#[tokio::test]
async fn ping_pong_error() {
    use protocols::ping_pong::*;
    let t = Duration::from_secs(10);

    type WrongClient = Send<String, Recv<u64, Eps>>;

    let srv = async move |c: Chan<Server, (), DynMessage>| {
        let (c, Ping) = c.recv(t).await?;
        c.send(Pong)?.close()
    };

    let cli = async move |c: Chan<WrongClient, (), DynMessage>| {
        let c = c.send("Hello".into())?;
        let (c, _n) = c.recv(t).await?;
        c.close()
    };

    let (server_chan, client_chan) = session_channel();
    let wrong_client_chan = client_chan.cast::<WrongClient, ()>();

    let srv_t = tokio::spawn(srv(server_chan));
    let cli_t = tokio::spawn(cli(wrong_client_chan));

    let sr = srv_t.await.unwrap();
    let cr = cli_t.await.unwrap();

    assert!(sr.is_err());
    assert!(cr.is_err());
}

#[tokio::test]
async fn greetings() {
    use protocols::greetings::*;
    // It is at this point that an invalid protocol would fail to compile.
    let (server_chan, client_chan) = session_channel::<Server, DynMessage>();

    let srv = |c: Chan<Server, (), DynMessage>| async {
        let t = Duration::from_millis(100);
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
    };

    let cli = |c: Chan<Client, (), DynMessage>| async {
        let t = Duration::from_millis(100);
        let c = c.send(Hail("Rusty".into()))?;
        let (c, Greetings(_)) = c.recv(t).await?;
        let c = c.enter();
        let (c, AddResponse(r)) = c
            .sel2()
            .sel1()
            .send(AddRequest(1))?
            .send(AddRequest(2))?
            .recv(t)
            .await?;

        c.zero()?.sel2().sel2().send(Quit)?.close()?;

        ok(r)
    };

    let srv_t = tokio::spawn(srv(server_chan));
    let cli_t = tokio::spawn(cli(client_chan));

    let sr = srv_t.await.unwrap();
    let cr = cli_t.await.unwrap();

    assert!(sr.is_ok());
    assert_eq!(cr.unwrap(), 3);
}

#[tokio::test]
async fn ping_pong_generic_repr() {
    use protocols::ping_pong::*;

    // Define a wrapper type to be uses as Repr `R` in generic versions of `srv` and `cli`.
    enum Wrapper {
        Ping(Ping),
        Pong(Pong),
    }

    repr_bound! { PingPongReprs [Ping, Pong] }

    repr_impl! { Wrapper {
        Ping: ( Wrapper::Ping, Wrapper::Ping(x) => x ),
        Pong: ( |p| Wrapper::Pong(p), Wrapper::Pong(x) => x )
    } }

    async fn srv<R: PingPongReprs>(c: Chan<Server, (), R>) -> SessionResult<()> {
        let (c, Ping) = c.recv(Duration::from_millis(100)).await?;
        c.send(Pong)?.close()
    }

    async fn cli<R: PingPongReprs>(c: Chan<Client, (), R>) -> SessionResult<()> {
        let c = c.send(Ping)?;
        let (c, _pong) = c.recv(Duration::from_millis(100)).await?;
        c.close()
    }

    let (server_chan, client_chan) = session_channel::<Server, Wrapper>();

    let srv_t = tokio::spawn(srv(server_chan));
    let cli_t = tokio::spawn(cli(client_chan));

    srv_t.await.unwrap().unwrap();
    cli_t.await.unwrap().unwrap();
}
