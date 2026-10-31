//! A small blocking facade over zbus 6, which is async-only. glint makes a
//! handful of sequential D-Bus calls, so each one is simply driven to
//! completion on the calling thread.

use std::pin::Pin;
use zbus::zvariant::DynamicType;

pub use zbus::Connection;

pub fn session() -> zbus::Result<Connection> {
    zbus::block_on(Connection::session())
}

/// Fire a method call and wait for the reply.
pub fn call_method<B: serde::Serialize + DynamicType>(
    conn: &Connection,
    dest: &str,
    path: &str,
    iface: &str,
    method: &str,
    body: &B,
) -> zbus::Result<()> {
    zbus::block_on(conn.call_method(Some(dest), path, Some(iface), method, body)).map(drop)
}

pub fn name_has_owner(conn: &Connection, name: &str) -> bool {
    zbus::block_on(async {
        let p = zbus::fdo::DBusProxy::new(conn).await.ok()?;
        p.name_has_owner(name.try_into().ok()?).await.ok()
    })
    .unwrap_or(false)
}

pub struct Proxy<'a>(zbus::Proxy<'a>);

impl<'a> Proxy<'a> {
    pub fn new(conn: &Connection, dest: &'a str, path: &'a str, iface: &'a str) -> zbus::Result<Self> {
        zbus::block_on(zbus::Proxy::new(conn, dest, path, iface)).map(Self)
    }

    pub fn call<B, R>(&self, method: &str, body: &B) -> zbus::Result<R>
    where
        B: serde::Serialize + DynamicType,
        R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
    {
        zbus::block_on(self.0.call(method, body))
    }

    pub fn get_property<T>(&self, name: &str) -> zbus::Result<T>
    where
        T: serde::de::DeserializeOwned + zbus::zvariant::Type,
    {
        zbus::block_on(self.0.get_property(name))
    }

    /// Subscribe now; read later with [`Signals::next`].
    pub fn receive_signal(&self, name: &'static str) -> zbus::Result<Signals> {
        zbus::block_on(self.0.receive_signal(name)).map(Signals)
    }
}

pub struct Signals(zbus::proxy::SignalStream<'static>);

impl Signals {
    pub fn next(&mut self) -> Option<zbus::message::Message> {
        zbus::block_on(std::future::poll_fn(|cx| futures_core::Stream::poll_next(Pin::new(&mut self.0), cx)))
    }
}
