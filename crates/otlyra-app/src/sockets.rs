//! Where a page's WebSockets and event streams are opened: a thread each,
//! running the network layer's connection until it ends, so neither the
//! content thread nor the window ever waits on one.

use std::sync::Arc;
use std::sync::mpsc::TryRecvError;

use otlyra_net::socket::{self, Next};
use otlyra_script::sockets::{SocketCommand, SocketEvent, SocketKind, SocketOpen};

/// Open every page's connections this way from now on.
pub fn install() {
    otlyra_net::install_crypto_provider();
    otlyra_script::sockets::set_opener(Arc::new(|open: SocketOpen| {
        let spawned = std::thread::Builder::new()
            .name("otlyra-socket".to_owned())
            .spawn(move || serve(open));
        if let Err(error) = spawned {
            tracing::warn!(%error, "a socket thread could not start");
        }
    }));
}

fn serve(open: SocketOpen) {
    let SocketOpen {
        kind,
        url,
        origin,
        commands,
        events,
    } = open;
    let Ok(url) = url::Url::parse(&url) else {
        events.send(SocketEvent::Error(format!("{url} is not an address")));
        events.send(SocketEvent::Close(1006, String::new(), false));
        return;
    };
    let mut next = || match commands.try_recv() {
        Ok(command) => Next::Command(match command {
            SocketCommand::Text(text) => socket::SocketCommand::Text(text),
            SocketCommand::Binary(bytes) => socket::SocketCommand::Binary(bytes),
            SocketCommand::Close(code, reason) => socket::SocketCommand::Close(code, reason),
        }),
        Err(TryRecvError::Empty) => Next::Empty,
        Err(TryRecvError::Disconnected) => Next::Gone,
    };
    let mut report = |event: socket::SocketEvent| {
        events.send(match event {
            socket::SocketEvent::Open { protocol } => SocketEvent::Open(protocol),
            socket::SocketEvent::Text(text) => SocketEvent::Text(text),
            socket::SocketEvent::Binary(bytes) => SocketEvent::Binary(bytes),
            socket::SocketEvent::Event { kind, data, id } => SocketEvent::Event(kind, data, id),
            socket::SocketEvent::Error(message) => SocketEvent::Error(message),
            socket::SocketEvent::Close {
                code,
                reason,
                clean,
            } => SocketEvent::Close(code, reason, clean),
        });
    };
    // No cookies yet: the jar answers for requests the fetcher makes, and a
    // connection held open on its own thread is not one of them.
    match kind {
        SocketKind::WebSocket { protocols } => {
            socket::run_websocket(&url, &protocols, &origin, None, &mut next, &mut report);
        }
        SocketKind::EventSource => socket::run_event_stream(&url, None, &mut next, &mut report),
    }
}
