//! A small blocking facade over zbus 6, which is async-only. clipt makes a
//! handful of sequential D-Bus calls, so each one is simply driven to
//! completion on the calling thread.

use std::pin::Pin;
use zbus::zvariant::DynamicType;

pub use zbus::Connection;

/// How long any method call may take. Every call clipt makes answers at once
/// (slow user interaction arrives later as signals), so a call that hasn't is
/// a hung service, such as a wedged xdg-desktop-portal; give up and let the
/// caller fall back rather than freezing.
pub const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A session-bus connection builder with [`CALL_TIMEOUT`] applied.
pub fn builder() -> zbus::connection::Builder<'static> {
    zbus::connection::Builder::session().method_timeout(CALL_TIMEOUT)
}

pub fn session() -> zbus::Result<Connection> {
    zbus::block_on(builder().build())
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
    /// Properties aren't cached: clipt reads each one once, and a cache fill
    /// waits for its GetAll outside [`CALL_TIMEOUT`], so a hung service would
    /// hang the read.
    pub fn new(conn: &Connection, dest: &'a str, path: &'a str, iface: &'a str) -> zbus::Result<Self> {
        let b = zbus::proxy::Builder::<zbus::Proxy>::new(conn)
            .destination(dest)
            .path(path)
            .interface(iface)
            .cache_properties(zbus::proxy::CacheProperties::No);
        zbus::block_on(b.build()).map(Self)
    }

    pub fn call<B, R>(&self, method: &str, body: &B) -> zbus::Result<R>
    where
        B: serde::Serialize + DynamicType,
        R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
    {
        zbus::block_on(self.0.call(method, body))
    }

    /// Send without waiting for (or getting) a reply.
    pub fn call_noreply<B>(&self, method: &str, body: &B) -> zbus::Result<()>
    where
        B: serde::Serialize + DynamicType,
    {
        zbus::block_on(self.0.call_noreply(method, body))
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
